//! Main window: dual panes, transfer queue, keyboard model, event dispatch.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::ui::connect_dialog::{self, SiteManagerHandlers};
use crate::ui::pane::Pane;
use crate::ui::prompts;
use crate::ui::properties;
use crate::ui::queue::QueueView;
use crate::ui::site_editor::SecretUpdate;
use crate::ui::workspace::Workspace;
use linuxscp::runtime::runtime;
use linuxscp::settings::{Folder, Settings, Site};
use linuxscp::types::{Backend, ConnectSpec, Event, FsEntry, SessionId, TransferRequest};
use linuxscp::{fsops, secrets, sessions, ssh, transfers};

/// Files marked by Copy/Cut, pasted into a pane later.
#[derive(Clone)]
struct ClipboardState {
    backend: Backend,
    /// Directory the items were copied from (same-dir pastes are no-ops).
    dir: String,
    items: Vec<FsEntry>,
    cut: bool,
}

pub struct App {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    pub queue: Rc<QueueView>,
    /// WinSCP-style session tabs; each page holds a [`Workspace`].
    tabs: adw::TabView,
    workspaces: RefCell<Vec<Rc<Workspace>>>,
    /// Window-wide "show hidden files" state, applied to every pane.
    hidden: Cell<bool>,
    /// Header toggle mirroring [`Self::hidden`] so the keyboard shortcut and
    /// button stay in sync.
    hidden_btn: gtk::ToggleButton,
    /// Remembered split position, applied to each new tab's paned.
    left_width: Cell<Option<i32>>,
    events_tx: async_channel::Sender<Event>,
    settings: RefCell<Settings>,
    /// Last successful connect spec per session id, for reconnects.
    session_specs: RefCell<HashMap<SessionId, ConnectSpec>>,
    clipboard: RefCell<Option<ClipboardState>>,
    notifier: crate::ui::notify::Notifier,
}

impl App {
    pub fn build(app: &adw::Application) -> Rc<Self> {
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();

        let queue = QueueView::new();

        let header = adw::HeaderBar::new();
        let title = adw::WindowTitle::new("LinuxSCP", "");
        header.set_title_widget(Some(&title));

        let hidden_btn = gtk::ToggleButton::new();
        hidden_btn.set_icon_name("view-conceal-symbolic");
        hidden_btn.set_tooltip_text(Some("Show hidden files (Ctrl+H)"));
        header.pack_end(&hidden_btn);

        let menu = gtk::gio::Menu::new();
        menu.append(Some("Open Terminal Here"), Some("win.terminal"));
        menu.append(Some("Save as Site…"), Some("win.bookmark"));
        menu.append(Some("Preferences"), Some("win.preferences"));
        menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        menu.append(Some("About LinuxSCP"), Some("win.about"));
        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build();
        header.pack_end(&menu_btn);

        let terminal_btn = gtk::Button::from_icon_name("utilities-terminal-symbolic");
        terminal_btn.set_tooltip_text(Some("Open terminal here (Ctrl+T)"));
        terminal_btn.set_action_name(Some("win.terminal"));
        header.pack_start(&terminal_btn);

        // Session tabs (WinSCP-style): a bar under the header plus the view
        // that shows the selected tab's dual-pane workspace. The bar only
        // takes the width its tabs need so the "New Session" affordance sits
        // right next to the last tab, like WinSCP's "New Tab" tab.
        let tabs = adw::TabView::new();
        tabs.set_vexpand(true);
        let tab_bar = adw::TabBar::new();
        tab_bar.set_view(Some(&tabs));
        tab_bar.set_autohide(false);
        tab_bar.set_hexpand(false);
        // Boxed, bordered tabs (see main.rs CSS) so they read as tabs in
        // both light and dark mode.
        tab_bar.add_css_class("session-tabs");
        let new_session_btn = new_session_button();

        // Uniform WinSCP-style tab widths: the bar is sized to a fixed pitch
        // per tab (scrolling once it would exceed the cap) and the tabs
        // expand to fill it exactly.
        let update_strip_width = {
            let tabs = tabs.clone();
            let tab_bar = tab_bar.clone();
            move || {
                let n = tabs.n_pages().max(1);
                tab_bar.set_width_request((n * 200).min(640));
            }
        };
        update_strip_width();
        tabs.connect_n_pages_notify(move |_| update_strip_width());

        let tab_strip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tab_strip.add_css_class("session-strip");
        tab_strip.append(&tab_bar);
        tab_strip.append(&new_session_btn);
        let strip_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        strip_spacer.set_hexpand(true);
        tab_strip.append(&strip_spacer);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&tabs);
        content.append(&queue.revealer);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.add_top_bar(&tab_strip);
        toolbar_view.set_content(Some(&content));

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&toolbar_view));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("LinuxSCP")
            .default_width(1280)
            .default_height(760)
            .content(&toasts)
            .build();

        let settings = Settings::load();
        let notifier = crate::ui::notify::Notifier::new(app);
        let hidden = settings.show_hidden;
        let left_width = settings.left_width;

        let this = Rc::new(Self {
            window,
            toasts,
            queue,
            tabs,
            workspaces: RefCell::new(Vec::new()),
            hidden: Cell::new(hidden),
            hidden_btn: hidden_btn.clone(),
            left_width: Cell::new(left_width),
            events_tx,
            settings: RefCell::new(settings),
            session_specs: RefCell::new(HashMap::new()),
            clipboard: RefCell::new(None),
            notifier,
        });

        this.wire_tabs();
        this.wire_shortcuts();
        this.wire_actions();
        this.wire_hidden_toggle(&hidden_btn);
        this.spawn_event_loop(events_rx);

        // The first, always-present tab; the site manager opens over it at
        // launch (see main.rs) so a saved site is one click away.
        this.add_workspace("My Server");

        {
            let this = this.clone();
            new_session_btn.connect_clicked(move |_| this.new_session());
        }
        {
            let this = this.clone();
            this.queue
                .clone()
                .set_on_finished(move || this.reload_all_panes());
        }

        // Reflect persisted hidden-files preference on the toggle.
        if hidden {
            hidden_btn.set_active(true);
        }

        // Persist geometry and preferences on close.
        {
            let this = this.clone();
            this.window.clone().connect_close_request(move |_| {
                this.persist();
                glib::Propagation::Proceed
            });
        }

        this
    }

    fn persist(&self) {
        let width = self
            .active_workspace()
            .map(|ws| ws.root.position())
            .or_else(|| self.left_width.get());
        let mut settings = self.settings.borrow_mut();
        settings.show_hidden = self.hidden.get();
        if let Some(width) = width {
            settings.left_width = Some(width);
        }
        settings.save();
    }

    /// The workspace of the selected tab (falling back to the first tab).
    fn active_workspace(&self) -> Option<Rc<Workspace>> {
        let selected = self.tabs.selected_page();
        let workspaces = self.workspaces.borrow();
        workspaces
            .iter()
            .find(|ws| ws.page().as_ref() == selected.as_ref())
            .or_else(|| workspaces.first())
            .cloned()
    }

    /// The workspace containing `pane`, if any.
    fn workspace_of_pane(&self, pane: &Rc<Pane>) -> Option<Rc<Workspace>> {
        self.workspaces
            .borrow()
            .iter()
            .find(|ws| ws.has_pane(pane))
            .cloned()
    }

    fn active_pane(&self) -> Rc<Pane> {
        self.active_workspace()
            .map(|ws| ws.active_pane())
            .expect("there is always at least one workspace")
    }

    fn inactive_pane(&self) -> Rc<Pane> {
        self.active_workspace()
            .map(|ws| ws.inactive_pane())
            .expect("there is always at least one workspace")
    }

    /// Run `f` for every pane across all tabs (e.g. global refresh).
    fn for_each_pane(&self, f: impl Fn(&Rc<Pane>)) {
        for ws in self.workspaces.borrow().iter() {
            for pane in ws.panes() {
                f(&pane);
            }
        }
    }

    fn reload_all_panes(&self) {
        self.for_each_pane(|pane| pane.reload());
    }

    /// Reload every pane currently showing `dir` on `backend`. Used after a
    /// change so the pane that made it — and any other pane or tab viewing
    /// the same directory — both pick it up. Panes elsewhere (and unrelated
    /// remote sessions) are left alone.
    fn reload_views(&self, backend: Backend, dir: &str) {
        self.for_each_pane(|pane| {
            if pane.backend() == backend && pane.current_dir() == dir {
                pane.reload();
            }
        });
    }

    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    // ---- Tabs / workspaces ----------------------------------------------

    fn wire_tabs(self: &Rc<Self>) {
        // Clean up sessions when a tab is closed, and keep at least one tab.
        {
            let this = self.clone();
            self.tabs.connect_close_page(move |view, page| {
                if let Some(ws) = this.workspace_for_page(page) {
                    for pane in ws.panes() {
                        if let Some(id) = pane.session_id() {
                            sessions::close(id);
                            this.session_specs.borrow_mut().remove(&id);
                        }
                    }
                }
                view.close_page_finish(page, true);
                glib::Propagation::Stop
            });
        }
        {
            let this = self.clone();
            self.tabs.connect_page_detached(move |_, page, _| {
                this.workspaces
                    .borrow_mut()
                    .retain(|ws| ws.page().as_ref() != Some(page));
                // Never leave the window tab-less.
                if this.tabs.n_pages() == 0 {
                    this.add_workspace("My Server");
                }
            });
        }
    }

    fn workspace_for_page(&self, page: &adw::TabPage) -> Option<Rc<Workspace>> {
        self.workspaces
            .borrow()
            .iter()
            .find(|ws| ws.page().as_ref() == Some(page))
            .cloned()
    }

    /// Build a new tab (local|local), wire it, and select it.
    fn add_workspace(self: &Rc<Self>, default_title: &str) -> Rc<Workspace> {
        let ws = Workspace::new(default_title, self.left_width.get());
        let page = self.tabs.append(&ws.root);
        page.set_title(default_title);
        page.set_icon(Some(&themed_icon("computer-symbolic")));
        ws.set_page(page);
        self.workspaces.borrow_mut().push(ws.clone());

        self.wire_workspace(&ws);

        // Start both panes at the local home; apply the current hidden state.
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into());
        for pane in ws.panes() {
            pane.set_show_hidden(self.hidden.get());
        }
        ws.left.set_backend(Backend::Local, home.clone(), "Local");
        ws.right.set_backend(Backend::Local, home, "Local");

        if let Some(page) = ws.page() {
            self.tabs.set_selected_page(&page);
        }
        ws
    }

    /// Open a fresh session tab and immediately show the site manager for
    /// its right pane (the New Session action).
    fn new_session(self: &Rc<Self>) {
        let ws = self.add_workspace("New Session");
        self.open_site_manager(ws.right.clone());
    }

    fn wire_workspace(self: &Rc<Self>, ws: &Rc<Workspace>) {
        for (pane, is_left) in [(ws.left.clone(), true), (ws.right.clone(), false)] {
            // Track which pane is active within its workspace.
            let focus = gtk::EventControllerFocus::new();
            {
                let this = self.clone();
                let pane_for_focus = pane.clone();
                focus.connect_enter(move |_| {
                    if let Some(ws) = this.workspace_of_pane(&pane_for_focus) {
                        ws.set_left_active(is_left);
                    }
                });
            }
            pane.view.add_controller(focus);

            // Connect / disconnect buttons.
            {
                let this = self.clone();
                let pane_for_btn = pane.clone();
                pane.connect_button.connect_clicked(move |_| {
                    this.open_site_manager(pane_for_btn.clone());
                });
            }
            {
                let this = self.clone();
                let pane_for_btn = pane.clone();
                pane.disconnect_button.connect_clicked(move |_| {
                    pane_for_btn.to_local();
                    if let Some(ws) = this.workspace_of_pane(&pane_for_btn) {
                        this.refresh_tab_title(&ws);
                    }
                });
            }

            self.wire_context_menu(&pane);
            {
                let this = self.clone();
                pane.setup_drop_target(move |payload, dst_pane| {
                    this.transfer_into(payload.source, payload.items, &dst_pane);
                });
            }
        }
    }

    /// Set a tab's title/icon from its right pane's connection (or the
    /// workspace default when local).
    fn refresh_tab_title(&self, ws: &Rc<Workspace>) {
        let Some(page) = ws.page() else {
            return;
        };
        // The right pane is the conventional "session" side; fall back to a
        // remote left pane so an unusual setup still shows the host.
        let remote = [&ws.right, &ws.left]
            .into_iter()
            .find_map(|pane| pane.session_id());
        match remote {
            Some(id) => {
                let host = sessions::get(id)
                    .map(|h| h.host)
                    .unwrap_or_else(|| "Remote".into());
                page.set_title(&host);
                page.set_tooltip(&host);
                page.set_icon(Some(&themed_icon("network-server-symbolic")));
            }
            None => {
                page.set_title(&ws.default_title());
                page.set_tooltip("");
                page.set_icon(Some(&themed_icon("computer-symbolic")));
            }
        }
    }

    // ---- Context menu ----------------------------------------------------

    /// The right-click menu shared by both panes; per-item availability is
    /// controlled through the actions' enabled state at popup time.
    fn context_menu_model() -> gio::Menu {
        let clipboard = gio::Menu::new();
        clipboard.append(Some("Copy"), Some("ctx.copy"));
        clipboard.append(Some("Cut"), Some("ctx.cut"));
        clipboard.append(Some("Paste"), Some("ctx.paste"));
        clipboard.append(Some("Copy To…"), Some("ctx.copy-to"));

        let files = gio::Menu::new();
        files.append(Some("Rename…"), Some("ctx.rename"));
        files.append(Some("New Symlink…"), Some("ctx.symlink"));

        let danger = gio::Menu::new();
        danger.append(Some("Delete"), Some("ctx.delete"));

        let dir = gio::Menu::new();
        dir.append(Some("New Folder…"), Some("ctx.new-folder"));
        dir.append(Some("Refresh"), Some("ctx.refresh"));

        let props = gio::Menu::new();
        props.append(Some("Properties"), Some("ctx.properties"));

        let root = gio::Menu::new();
        root.append_section(None, &clipboard);
        root.append_section(None, &files);
        root.append_section(None, &danger);
        root.append_section(None, &dir);
        root.append_section(None, &props);
        root
    }

    fn wire_context_menu(self: &Rc<Self>, pane: &Rc<Pane>) {
        let group = gio::SimpleActionGroup::new();
        let action = |name: &str| {
            let action = gio::SimpleAction::new(name, None);
            group.add_action(&action);
            action
        };
        macro_rules! wire {
            ($action:expr, $this:ident, $pane:ident => $body:expr) => {{
                let $this = self.clone();
                let $pane = pane.clone();
                $action.connect_activate(move |_, _| $body);
            }};
        }

        let copy = action("copy");
        wire!(copy, this, pane => this.clip_selection(&pane, false));
        let cut = action("cut");
        wire!(cut, this, pane => this.clip_selection(&pane, true));
        let paste = action("paste");
        wire!(paste, this, pane => this.paste_into(&pane));
        let copy_to = action("copy-to");
        wire!(copy_to, this, pane => this.copy_selection_to(&pane));
        let rename = action("rename");
        wire!(rename, this, pane => this.rename_in(pane.clone()));
        let symlink = action("symlink");
        wire!(symlink, this, pane => this.symlink_selection(&pane));
        let delete = action("delete");
        wire!(delete, this, pane => this.delete_in(pane.clone()));
        let new_folder = action("new-folder");
        wire!(new_folder, this, pane => this.mkdir_in(pane.clone()));
        let refresh = action("refresh");
        {
            let pane = pane.clone();
            refresh.connect_activate(move |_, _| pane.reload());
        }
        let show_properties = action("properties");
        wire!(show_properties, this, pane => this.properties_of_selection(&pane));

        pane.view.insert_action_group("ctx", Some(&group));

        let popover = gtk::PopoverMenu::from_model(Some(&Self::context_menu_model()));
        popover.set_parent(&pane.view);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);

        let this = self.clone();
        let pane_for_menu = pane.clone();
        pane.set_context_menu(move |x, y| {
            let count = pane_for_menu.selected_entries().len();
            copy.set_enabled(count > 0);
            cut.set_enabled(count > 0);
            copy_to.set_enabled(count > 0);
            delete.set_enabled(count > 0);
            show_properties.set_enabled(count > 0);
            rename.set_enabled(count == 1);
            symlink.set_enabled(count == 1);
            paste.set_enabled(this.clipboard.borrow().is_some());
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        });
    }

    fn wire_actions(self: &Rc<Self>) {
        let group = gtk::gio::SimpleActionGroup::new();

        let terminal = gtk::gio::SimpleAction::new("terminal", None);
        {
            let this = self.clone();
            terminal.connect_activate(move |_, _| this.open_terminal());
        }
        group.add_action(&terminal);

        let bookmark = gtk::gio::SimpleAction::new("bookmark", None);
        {
            let this = self.clone();
            bookmark.connect_activate(move |_, _| this.bookmark_current());
        }
        group.add_action(&bookmark);

        let preferences = gtk::gio::SimpleAction::new("preferences", None);
        {
            let this = self.clone();
            preferences.connect_activate(move |_, _| this.show_preferences());
        }
        group.add_action(&preferences);

        let shortcuts = gtk::gio::SimpleAction::new("shortcuts", None);
        {
            let this = self.clone();
            shortcuts.connect_activate(move |_, _| this.show_shortcuts());
        }
        group.add_action(&shortcuts);

        let about = gtk::gio::SimpleAction::new("about", None);
        {
            let this = self.clone();
            about.connect_activate(move |_, _| this.show_about());
        }
        group.add_action(&about);

        self.window.insert_action_group("win", Some(&group));
    }

    fn show_preferences(self: &Rc<Self>) {
        let group = adw::PreferencesGroup::builder()
            .title("Notifications")
            .description("How LinuxSCP tells you a transfer has finished")
            .build();

        let (sound_on, desktop_on) = {
            let s = self.settings.borrow();
            (s.notify_sound, s.notify_desktop)
        };

        let sound_row = adw::SwitchRow::builder()
            .title("Play sound on completion")
            .subtitle("Plays a success chime when a transfer finishes")
            .active(sound_on)
            .build();
        let desktop_row = adw::SwitchRow::builder()
            .title("Desktop notifications")
            .subtitle("Show a GNOME notification when a transfer finishes")
            .active(desktop_on)
            .build();

        // Persist immediately on toggle; a "Test" button previews the sound.
        {
            let this = self.clone();
            let sound_row_ref = sound_row.clone();
            sound_row.connect_active_notify(move |row| {
                let active = row.is_active();
                this.with_settings_saved(|s| s.notify_sound = active);
                if active {
                    this.notifier.play_success();
                }
                let _ = &sound_row_ref;
            });
        }
        {
            let this = self.clone();
            desktop_row.connect_active_notify(move |row| {
                let active = row.is_active();
                this.with_settings_saved(|s| s.notify_desktop = active);
            });
        }

        group.add(&sound_row);
        group.add(&desktop_row);

        let page = adw::PreferencesPage::new();
        page.add(&group);
        let dialog = adw::PreferencesDialog::new();
        dialog.set_title("Preferences");
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }

    fn show_shortcuts(&self) {
        let shortcuts: &[(&str, &str)] = &[
            ("Tab", "Switch active pane"),
            ("Enter", "Open directory"),
            ("Backspace", "Parent directory"),
            ("F5", "Copy to other pane"),
            ("F6", "Move to other pane"),
            ("F7", "New directory"),
            ("F2", "Rename"),
            ("F8 / Delete", "Delete"),
            ("F9", "Properties (owner, group, permissions)"),
            ("Ctrl+C / Ctrl+X", "Copy / cut selection"),
            ("Ctrl+V", "Paste into active pane"),
            ("Right click", "File context menu"),
            ("Ctrl+H", "Toggle hidden files"),
            ("Ctrl+R", "Refresh"),
            ("Ctrl+L", "Edit path"),
            ("Ctrl+T", "Open terminal here"),
        ];
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("boxed-list");
        let group = adw::PreferencesGroup::new();
        for (keys, desc) in shortcuts {
            let row = adw::ActionRow::builder().title(*desc).build();
            let kbd = gtk::Label::new(Some(keys));
            kbd.add_css_class("dim-label");
            kbd.add_css_class("monospace");
            row.add_suffix(&kbd);
            group.add(&row);
        }
        let _ = list;

        let page = adw::PreferencesPage::new();
        page.add(&group);
        let dialog = adw::PreferencesDialog::new();
        dialog.set_title("Keyboard Shortcuts");
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("LinuxSCP")
            .application_icon("io.github.theflyingjay.LinuxSCP")
            .developer_name("Jacob Petrosky")
            .copyright("© 2026 Jacob Petrosky")
            .version(env!("CARGO_PKG_VERSION"))
            .license_type(gtk::License::Gpl30)
            .comments(
                "A commander-style SFTP client for GNOME. Works directly with \
                 your ~/.ssh/config and supports sudo/su elevation and resumable \
                 transfers.",
            )
            .website("https://github.com/theflyingjay/linuxscp")
            .build();
        about.present(Some(&self.window));
    }

    fn wire_hidden_toggle(self: &Rc<Self>, button: &gtk::ToggleButton) {
        let this = self.clone();
        button.connect_toggled(move |btn| {
            let show = btn.is_active();
            btn.set_icon_name(if show {
                "view-reveal-symbolic"
            } else {
                "view-conceal-symbolic"
            });
            this.hidden.set(show);
            this.for_each_pane(|pane| pane.set_show_hidden(show));
        });
    }

    fn wire_shortcuts(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = self.clone();
        keys.connect_key_pressed(move |_, key, _, modifier| {
            use gtk::gdk::Key;
            let ctrl = modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK);

            // Don't steal keys while typing in an entry.
            let editing =
                gtk::prelude::RootExt::focus(&this.window).is_some_and(|w| w.is::<gtk::Text>());

            match key {
                Key::F5 if !editing => this.start_transfer(false),
                Key::F6 if !editing => this.start_transfer(true),
                Key::F7 if !editing => this.mkdir(),
                Key::F2 if !editing => this.rename(),
                Key::F8 | Key::Delete if !editing => this.delete(),
                Key::F9 if !editing => {
                    this.properties_of_selection(&this.active_pane());
                }
                Key::Tab if !editing && modifier.is_empty() => {
                    let target = this.inactive_pane().clone();
                    target.view.grab_focus();
                }
                Key::c | Key::C if ctrl && !editing => {
                    this.clip_selection(&this.active_pane().clone(), false)
                }
                Key::x | Key::X if ctrl && !editing => {
                    this.clip_selection(&this.active_pane().clone(), true)
                }
                Key::v | Key::V if ctrl && !editing => this.paste_into(&this.active_pane().clone()),
                Key::h | Key::H if ctrl => {
                    // Drive the toggle so its icon and state stay in sync.
                    this.hidden_btn.set_active(!this.hidden_btn.is_active());
                }
                Key::r | Key::R if ctrl => {
                    this.active_pane().reload();
                }
                Key::l | Key::L if ctrl => {
                    this.active_pane().path_entry.grab_focus();
                }
                Key::t | Key::T if ctrl => {
                    this.open_terminal();
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);
    }

    // ---- Actions -------------------------------------------------------

    fn start_transfer(self: &Rc<Self>, move_src: bool) {
        let src = self.active_pane().clone();
        let dst = self.inactive_pane().clone();
        let items = src.selected_entries();
        if items.is_empty() {
            self.toast("Nothing selected");
            return;
        }
        if src.backend() == dst.backend() && src.current_dir() == dst.current_dir() {
            self.toast("Source and destination are the same directory");
            return;
        }
        let request = TransferRequest {
            src_backend: src.backend(),
            dst_backend: dst.backend(),
            items,
            dst_dir: dst.current_dir(),
            move_src,
        };
        transfers::start(request, self.events_tx.clone());
    }

    fn mkdir(self: &Rc<Self>) {
        self.mkdir_in(self.active_pane().clone());
    }

    fn mkdir_in(self: &Rc<Self>, pane: Rc<Pane>) {
        let this = self.clone();
        prompts::ask_text(&self.window, "New Directory", "", "Create", move |name| {
            let backend = pane.backend();
            let dir = pane.current_dir();
            let path = fsops::join(&dir, &name);
            let this = this.clone();
            glib::spawn_future_local(async move {
                let result = runtime()
                    .spawn(async move { fsops::mkdir_all(backend, &path).await })
                    .await;
                match result {
                    Ok(Ok(())) => this.reload_views(backend, &dir),
                    Ok(Err(err)) => this.toast(&format!("Create failed: {err:#}")),
                    Err(err) => this.toast(&err.to_string()),
                }
            });
        });
    }

    fn rename(self: &Rc<Self>) {
        self.rename_in(self.active_pane().clone());
    }

    fn rename_in(self: &Rc<Self>, pane: Rc<Pane>) {
        let Some(entry) = pane.focused_entry() else {
            self.toast("Nothing selected");
            return;
        };
        let this = self.clone();
        let initial = entry.name.clone();
        prompts::ask_text(
            &self.window,
            "Rename",
            &initial,
            "Rename",
            move |new_name| {
                if new_name == entry.name {
                    return;
                }
                let backend = pane.backend();
                let from = entry.path.clone();
                let dir = fsops::parent(&entry.path);
                let to = fsops::join(&dir, &new_name);
                let this = this.clone();
                glib::spawn_future_local(async move {
                    let result = runtime()
                        .spawn(async move { fsops::rename(backend, &from, &to).await })
                        .await;
                    match result {
                        Ok(Ok(())) => this.reload_views(backend, &dir),
                        Ok(Err(err)) => this.toast(&format!("Rename failed: {err:#}")),
                        Err(err) => this.toast(&err.to_string()),
                    }
                });
            },
        );
    }

    /// Open a terminal at the active pane's directory. For remote panes this
    /// launches ssh into the host, cd-ing to the current directory.
    fn open_terminal(self: &Rc<Self>) {
        let pane = self.active_pane();
        let dir = pane.current_dir();
        let terminal = self.settings.borrow().terminal.clone();
        let result = match pane.backend() {
            Backend::Local => spawn_terminal(terminal.as_deref(), Some(&dir), None),
            Backend::Remote(id) => {
                let Some(handle) = sessions::get(id) else {
                    self.toast("Session is closed");
                    return;
                };
                // ssh <host> -t "cd <dir>; exec $SHELL -l"
                let remote_cmd = format!(
                    "cd {} 2>/dev/null; exec \"$SHELL\" -l",
                    shell_words::quote(&dir)
                );
                let ssh_args = vec![
                    "-t".to_string(),
                    handle.host.clone(),
                    "--".to_string(),
                    remote_cmd,
                ];
                spawn_terminal(terminal.as_deref(), None, Some(ssh_args))
            }
        };
        if let Err(err) = result {
            self.toast(&format!("Could not open terminal: {err}"));
        }
    }

    /// Copy explicit entries into a pane's directory (used by drag & drop).
    pub fn transfer_into(
        self: &Rc<Self>,
        src_backend: Backend,
        items: Vec<crate::ui::pane::DropPayloadItem>,
        dst_pane: &Rc<Pane>,
    ) {
        let items: Vec<_> = items.into_iter().map(|i| i.0).collect();
        if items.is_empty() {
            return;
        }
        if src_backend == dst_pane.backend() {
            self.toast("Source and destination are the same");
            return;
        }
        let request = TransferRequest {
            src_backend,
            dst_backend: dst_pane.backend(),
            items,
            dst_dir: dst_pane.current_dir(),
            move_src: false,
        };
        transfers::start(request, self.events_tx.clone());
    }

    fn delete(self: &Rc<Self>) {
        self.delete_in(self.active_pane().clone());
    }

    fn delete_in(self: &Rc<Self>, pane: Rc<Pane>) {
        let entries = pane.selected_entries();
        if entries.is_empty() {
            self.toast("Nothing selected");
            return;
        }
        let body = if entries.len() == 1 {
            format!("Permanently delete \u{201c}{}\u{201d}?", entries[0].name)
        } else {
            format!("Permanently delete {} items?", entries.len())
        };
        let this = self.clone();
        let backend = pane.backend();
        prompts::confirm(&self.window, "Delete", &body, "Delete", move || {
            // Runs as a queue job: the row shows the removal count climbing
            // and can be cancelled; every pane refreshes when it finishes
            // (success, failure or cancel alike — reload_all_panes fires on
            // any terminal state, so partial deletions show up too).
            transfers::start_delete(backend, entries.clone(), this.events_tx.clone());
        });
    }

    // ---- Clipboard (copy / cut / paste) ---------------------------------

    /// Remember the selection for a later paste.
    fn clip_selection(self: &Rc<Self>, pane: &Rc<Pane>, cut: bool) {
        let items = pane.selected_entries();
        if items.is_empty() {
            self.toast("Nothing selected");
            return;
        }
        let noun = if items.len() == 1 {
            format!("\u{201c}{}\u{201d}", items[0].name)
        } else {
            format!("{} items", items.len())
        };
        self.toast(&format!(
            "{noun} ready to {}",
            if cut {
                "move — paste somewhere"
            } else {
                "copy — paste somewhere"
            }
        ));
        *self.clipboard.borrow_mut() = Some(ClipboardState {
            backend: pane.backend(),
            dir: pane.current_dir(),
            items,
            cut,
        });
    }

    fn paste_into(self: &Rc<Self>, pane: &Rc<Pane>) {
        let Some(clip) = self.clipboard.borrow().clone() else {
            self.toast("Nothing to paste");
            return;
        };
        let source_gone = match clip.backend {
            Backend::Remote(id) => sessions::get(id).is_none(),
            Backend::Local => false,
        };
        if source_gone {
            self.toast("The connection the files were copied from is closed");
            self.clipboard.borrow_mut().take();
            return;
        }
        let dst_dir = pane.current_dir();
        if clip.backend == pane.backend() && clip.dir == dst_dir {
            self.toast("Source and destination are the same directory");
            return;
        }

        if clip.cut && clip.backend == pane.backend() {
            // Same backend: a rename is instant. Items whose target already
            // exists (or that cross filesystems) fall back to a move job so
            // the user gets conflict prompts and progress.
            self.move_by_rename(pane.clone(), clip.clone(), dst_dir);
        } else {
            transfers::start(
                TransferRequest {
                    src_backend: clip.backend,
                    dst_backend: pane.backend(),
                    items: clip.items.clone(),
                    dst_dir,
                    move_src: clip.cut,
                },
                self.events_tx.clone(),
            );
        }
        if clip.cut {
            self.clipboard.borrow_mut().take();
        }
    }

    fn move_by_rename(self: &Rc<Self>, pane: Rc<Pane>, clip: ClipboardState, dst_dir: String) {
        let this = self.clone();
        let events = self.events_tx.clone();
        glib::spawn_future_local(async move {
            let backend = clip.backend;
            let items = clip.items.clone();
            let dst_for_task = dst_dir.clone();
            let result = runtime()
                .spawn(async move {
                    let mut leftover = Vec::new();
                    for item in items {
                        let target = fsops::join(&dst_for_task, &item.name);
                        // Never clobber silently; leave conflicts to the
                        // transfer engine and its prompts.
                        if fsops::exists(backend, &target).await
                            || fsops::rename(backend, &item.path, &target).await.is_err()
                        {
                            leftover.push(item);
                        }
                    }
                    leftover
                })
                .await;
            match result {
                Ok(leftover) => {
                    if !leftover.is_empty() {
                        transfers::start(
                            TransferRequest {
                                src_backend: backend,
                                dst_backend: backend,
                                items: leftover,
                                dst_dir,
                                move_src: true,
                            },
                            events,
                        );
                    }
                    this.reload_all_panes();
                    let _ = pane;
                }
                Err(err) => this.toast(&err.to_string()),
            }
        });
    }

    /// Prompt for a destination directory (same backend) and copy there.
    fn copy_selection_to(self: &Rc<Self>, pane: &Rc<Pane>) {
        let items = pane.selected_entries();
        if items.is_empty() {
            self.toast("Nothing selected");
            return;
        }
        let this = self.clone();
        let pane = pane.clone();
        let current = pane.current_dir();
        prompts::ask_text(
            &self.window,
            "Copy to Directory",
            &current,
            "Copy",
            move |dst_dir| {
                let dst_dir = dst_dir.trim().to_string();
                if dst_dir.is_empty() || dst_dir == pane.current_dir() {
                    this.toast("Destination is the current directory");
                    return;
                }
                let backend = pane.backend();
                let items = items.clone();
                let this = this.clone();
                glib::spawn_future_local(async move {
                    let dir_for_task = dst_dir.clone();
                    let created = runtime()
                        .spawn(async move { fsops::mkdir_all(backend, &dir_for_task).await })
                        .await;
                    match created {
                        Ok(Ok(())) => {
                            transfers::start(
                                TransferRequest {
                                    src_backend: backend,
                                    dst_backend: backend,
                                    items,
                                    dst_dir,
                                    move_src: false,
                                },
                                this.events_tx.clone(),
                            );
                        }
                        Ok(Err(err)) => {
                            this.toast(&format!("Could not create destination: {err:#}"))
                        }
                        Err(err) => this.toast(&err.to_string()),
                    }
                });
            },
        );
    }

    /// Create a symlink next to the selected entry, pointing at it.
    fn symlink_selection(self: &Rc<Self>, pane: &Rc<Pane>) {
        let Some(entry) = pane.focused_entry() else {
            self.toast("Nothing selected");
            return;
        };
        let this = self.clone();
        let pane = pane.clone();
        prompts::ask_text(
            &self.window,
            "New Symlink",
            &format!("{}-link", entry.name),
            "Create",
            move |name| {
                let name = name.trim().to_string();
                if name.is_empty() || name == entry.name {
                    return;
                }
                let backend = pane.backend();
                let dir = pane.current_dir();
                let link = fsops::join(&dir, &name);
                // Same-directory link, so a relative target is the most
                // portable thing to store.
                let target = entry.name.clone();
                let this = this.clone();
                glib::spawn_future_local(async move {
                    let result = runtime()
                        .spawn(async move { fsops::symlink(backend, &link, &target).await })
                        .await;
                    match result {
                        Ok(Ok(())) => this.reload_views(backend, &dir),
                        Ok(Err(err)) => this.toast(&format!("Symlink failed: {err:#}")),
                        Err(err) => this.toast(&err.to_string()),
                    }
                });
            },
        );
    }

    fn properties_of_selection(self: &Rc<Self>, pane: &Rc<Pane>) {
        let entries = pane.selected_entries();
        if entries.is_empty() {
            self.toast("Nothing selected");
            return;
        }
        let this = self.clone();
        let backend = pane.backend();
        let entries_for_apply = entries.clone();
        properties::show(&self.window, backend, entries, move |request| {
            // Runs as a queue job (recursive changes can take a while on a
            // real link): progress is visible, it can be cancelled, and all
            // panes refresh when it completes.
            transfers::start_attrs(
                backend,
                entries_for_apply.clone(),
                request,
                this.events_tx.clone(),
            );
        });
    }

    // ---- Connection ----------------------------------------------------

    /// Open the site manager targeting the active tab's right (remote) pane.
    pub fn open_site_manager_active(self: &Rc<Self>) {
        if let Some(ws) = self.active_workspace() {
            self.open_site_manager(ws.right.clone());
        }
    }

    /// Open the site manager (saved sites tree + ~/.ssh/config hosts).
    pub fn open_site_manager(self: &Rc<Self>, pane: Rc<Pane>) {
        let handlers = SiteManagerHandlers {
            tree: {
                let this = self.clone();
                Box::new(move || this.settings.borrow().sites.clone())
            },
            connect: {
                let this = self.clone();
                let pane = pane.clone();
                Box::new(move |spec| this.connect_session(pane.clone(), spec))
            },
            save_site: {
                let this = self.clone();
                Box::new(move |site, secret, parent_id| this.save_site(site, secret, &parent_id))
            },
            delete_site: {
                let this = self.clone();
                Box::new(move |id| this.delete_site(&id))
            },
            add_folder: {
                let this = self.clone();
                Box::new(move |parent_id, name| this.add_folder(&parent_id, &name))
            },
            move_site: {
                let this = self.clone();
                Box::new(move |site_id, dest_id| {
                    this.with_settings_saved(|settings| {
                        settings.sites.move_site_to(&site_id, &dest_id);
                    });
                })
            },
            move_folder: {
                let this = self.clone();
                Box::new(move |folder_id, dest_id| {
                    this.with_settings_saved(|settings| {
                        settings.sites.move_folder_to(&folder_id, &dest_id);
                    });
                })
            },
            delete_folder: {
                let this = self.clone();
                Box::new(move |id| this.delete_folder(&id))
            },
            spec_for_site: {
                let this = self.clone();
                Box::new(move |id| this.spec_for_site(&id))
            },
        };
        connect_dialog::show(&self.window, handlers);
    }

    // ---- Site manager persistence -------------------------------------

    fn with_settings_saved(self: &Rc<Self>, f: impl FnOnce(&mut Settings)) {
        let mut settings = self.settings.borrow_mut();
        f(&mut settings);
        settings.save();
    }

    /// Create or update a site under `parent_id` (empty = root), persisting
    /// any secret change to the keyring.
    fn save_site(self: &Rc<Self>, site: Site, secret: SecretUpdate, parent_id: &str) {
        match secret {
            SecretUpdate::Set(value) => {
                if let Err(err) = secrets::set(&site.id, &value) {
                    self.toast(&format!("Could not save password to keyring: {err}"));
                }
            }
            SecretUpdate::Clear => secrets::delete(&site.id),
            SecretUpdate::Keep => {}
        }
        let site_id = site.id.clone();
        self.with_settings_saved(|settings| {
            // Update in place if it already exists; otherwise add to parent.
            if !settings.sites.update_site(site.clone()) {
                settings
                    .sites
                    .folder_or_root_mut(parent_id)
                    .sites
                    .push(site);
            }
        });
        let _ = site_id;
        self.toast("Site saved");
    }

    fn delete_site(self: &Rc<Self>, id: &str) {
        secrets::delete(id);
        self.with_settings_saved(|settings| {
            settings.sites.remove_site(id);
        });
    }

    fn add_folder(self: &Rc<Self>, parent_id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let folder = Folder::new(name);
        self.with_settings_saved(|settings| {
            settings
                .sites
                .folder_or_root_mut(parent_id)
                .folders
                .push(folder);
        });
    }

    fn delete_folder(self: &Rc<Self>, id: &str) {
        // Delete keyring secrets for every site inside the folder first.
        {
            let settings = self.settings.borrow();
            if let Some(folder) = settings.sites.clone().find_folder_mut(id) {
                for site in folder.all_sites() {
                    secrets::delete(&site.id);
                }
            }
        }
        self.with_settings_saved(|settings| {
            settings.sites.remove_folder(id);
        });
    }

    /// Build a connect spec for a saved site, loading its secret.
    fn spec_for_site(self: &Rc<Self>, id: &str) -> Option<ConnectSpec> {
        let site = self
            .settings
            .borrow()
            .sites
            .all_sites()
            .into_iter()
            .find(|s| s.id == id)
            .cloned()?;
        let mut spec = site.to_spec();
        if site.has_secret {
            spec.secret = secrets::get(&site.id);
        }
        Some(spec)
    }

    pub fn connect_session(self: &Rc<Self>, pane: Rc<Pane>, spec: ConnectSpec) {
        let this = self.clone();
        let events = self.events_tx.clone();
        let id = sessions::next_id();
        self.toast(&format!("Connecting to {}…", spec.host));
        glib::spawn_future_local(async move {
            let spec_for_task = spec.clone();
            let events_for_task = events.clone();
            let result = runtime()
                .spawn(
                    async move { ssh::connect::connect(id, spec_for_task, events_for_task).await },
                )
                .await;
            match result {
                Ok(Ok(conn)) => {
                    sessions::register(
                        id,
                        sessions::SessionHandle::new(conn.sftp, conn.cancel, spec.host.clone()),
                    );
                    this.session_specs.borrow_mut().insert(id, spec.clone());
                    let label = match spec.elevation {
                        linuxscp::types::Elevation::None => spec.host.clone(),
                        _ => format!("root@{}", spec.host),
                    };
                    pane.set_backend(Backend::Remote(id), conn.initial_dir, &label);
                    if let Some(ws) = this.workspace_of_pane(&pane) {
                        this.refresh_tab_title(&ws);
                    }
                    this.toast(&format!("Connected to {}", spec.host));
                }
                Ok(Err(err)) => {
                    prompts::confirm(
                        &this.window,
                        "Connection Failed",
                        &format!("{err:#}"),
                        "Retry",
                        {
                            let this = this.clone();
                            move || this.connect_session(pane.clone(), spec.clone())
                        },
                    );
                }
                Err(err) => this.toast(&err.to_string()),
            }
        });
    }

    // ---- Event dispatch --------------------------------------------------

    /// Save the current remote pane as a site in the site manager (root
    /// folder), remembering the current directory. Credentials from the live
    /// spec are stored in the keyring when present.
    pub fn bookmark_current(self: &Rc<Self>) {
        let pane = self.active_pane();
        let Backend::Remote(id) = pane.backend() else {
            self.toast("Connect to a server first");
            return;
        };
        let Some(spec) = self.session_specs.borrow().get(&id).cloned() else {
            return;
        };
        let mut site = Site::new(spec.host.clone(), spec.host.clone());
        site.port = spec.port;
        site.user = spec.user.clone();
        site.auth = spec.auth;
        site.identity_file = spec.identity_file.clone();
        site.elevation = spec.elevation;
        site.remote_dir = Some(pane.current_dir());

        let secret = match &spec.secret {
            Some(value) if !value.is_empty() => SecretUpdate::Set(value.clone()),
            _ => SecretUpdate::Clear,
        };
        site.has_secret = matches!(secret, SecretUpdate::Set(_));
        self.save_site(site, secret, "");
    }

    /// Fire sound + desktop notification when a transfer finishes. Done and
    /// Failed notify (once each, since terminal states are emitted once);
    /// running/paused/cancelled stay quiet.
    fn on_transfer_snapshot(self: &Rc<Self>, snapshot: &linuxscp::types::TransferSnapshot) {
        let (sound, desktop) = {
            let s = self.settings.borrow();
            (s.notify_sound, s.notify_desktop)
        };
        let Some(plan) =
            crate::ui::notify::completion_plan(snapshot.kind, snapshot.state, sound, desktop)
        else {
            return;
        };
        let detail = if plan.success {
            format!(
                "{} — {}",
                snapshot.title,
                crate::ui::entry_object::format_size(snapshot.total_bytes)
            )
        } else {
            snapshot
                .error
                .clone()
                .unwrap_or_else(|| snapshot.title.clone())
        };
        crate::ui::notify::run_plan(&self.notifier, &plan, &detail);
    }

    #[allow(clippy::type_complexity)]
    fn spawn_event_loop(self: &Rc<Self>, events_rx: async_channel::Receiver<Event>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = events_rx.recv().await {
                match event {
                    Event::Prompt(request) => prompts::show_prompt(&this.window, request),
                    Event::Conflict(request) => prompts::show_conflict(&this.window, request),
                    Event::TransferUpdate(snapshot) => {
                        this.on_transfer_snapshot(&snapshot);
                        this.queue.update(snapshot);
                    }
                    Event::SessionClosed { id, reason } => {
                        sessions::forget(id);
                        let spec = this.session_specs.borrow_mut().remove(&id);
                        let mut affected: Vec<Rc<Pane>> = Vec::new();
                        for ws in this.workspaces.borrow().iter() {
                            for pane in ws.panes() {
                                if pane.session_id() == Some(id) {
                                    affected.push(pane);
                                }
                            }
                        }
                        for pane in &affected {
                            pane.to_local();
                            if let Some(ws) = this.workspace_of_pane(pane) {
                                this.refresh_tab_title(&ws);
                            }
                        }
                        // Offer a one-click reconnect for unexpected drops.
                        match (spec, affected.into_iter().next()) {
                            (Some(spec), Some(pane)) => {
                                let toast = adw::Toast::new(&format!("Connection lost: {reason}"));
                                toast.set_button_label(Some("Reconnect"));
                                toast.set_timeout(8);
                                let this2 = this.clone();
                                toast.connect_button_clicked(move |_| {
                                    this2.connect_session(pane.clone(), spec.clone());
                                });
                                this.toasts.add_toast(toast);
                            }
                            _ => this.toast(&format!("Connection lost: {reason}")),
                        }
                    }
                }
            }
        });
    }
}

/// Launch a terminal emulator, optionally in a working directory or running
/// an ssh command. Tries a configured terminal first, then common ones.
fn spawn_terminal(
    configured: Option<&str>,
    workdir: Option<&str>,
    ssh_args: Option<Vec<String>>,
) -> std::io::Result<()> {
    // (binary, arg that precedes a command to execute)
    const CANDIDATES: &[(&str, &str)] = &[
        ("kgx", "-e"), // GNOME Console
        ("gnome-terminal", "--"),
        ("ptyxis", "--"),
        ("konsole", "-e"),
        ("xterm", "-e"),
        ("alacritty", "-e"),
        ("kitty", ""),
    ];

    let pick = |bin: &str| which(bin);

    let (bin, exec_flag) = configured
        .and_then(|c| pick(c).map(|_| (c.to_string(), "-e".to_string())))
        .or_else(|| {
            CANDIDATES
                .iter()
                .find(|(b, _)| pick(b).is_some())
                .map(|(b, f)| (b.to_string(), f.to_string()))
        })
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no terminal emulator found")
        })?;

    let mut cmd = std::process::Command::new(&bin);
    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }
    if let Some(ssh_args) = ssh_args {
        if !exec_flag.is_empty() {
            cmd.arg(&exec_flag);
        }
        cmd.arg("ssh");
        cmd.args(ssh_args);
    }
    cmd.spawn().map(|_| ())
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|p| p.join(bin))
            .find(|p| p.is_file())
    })
}

fn themed_icon(name: &str) -> gio::ThemedIcon {
    gio::ThemedIcon::new(name)
}

/// The "New Session" affordance shown after the last tab (a "+"-style tab).
fn new_session_button() -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&gtk::Image::from_icon_name("list-add-symbolic"));
    content.append(&gtk::Label::new(Some("New Session")));
    let button = gtk::Button::builder()
        .child(&content)
        .tooltip_text("Open a new session tab")
        .build();
    button.add_css_class("flat");
    button.add_css_class("new-session-tab");
    button.set_margin_start(4);
    button.set_margin_end(4);
    button.set_margin_top(4);
    button.set_margin_bottom(4);
    button
}
