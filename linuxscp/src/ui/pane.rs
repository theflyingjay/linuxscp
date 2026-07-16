//! One file-browser pane (local or remote), commander style.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::entry_object::{EntryObject, format_mtime, format_size};
use linuxscp::runtime::runtime;
use linuxscp::types::{Backend, FsEntry};
use linuxscp::{fsops, sessions};

/// Wrapper so `FsEntry` can travel with a drag-and-drop payload.
#[derive(Debug, Clone)]
pub struct DropPayloadItem(pub FsEntry);

/// Drag payload: the source backend plus the dragged entries.
#[derive(Debug, Clone, glib::Boxed)]
#[boxed_type(name = "LinuxScpDragPayload")]
pub struct DragPayload {
    pub source: Backend,
    pub items: Vec<DropPayloadItem>,
}

pub struct PaneState {
    pub backend: Backend,
    pub dir: String,
    pub show_hidden: bool,
    /// Guards against stale async listings landing after a newer navigation.
    generation: u64,
}

/// Handler invoked on a row right-click: (model position, view x, view y).
type RowMenuSlot = Rc<RefCell<Option<Box<dyn Fn(u32, f64, f64)>>>>;

/// Builds the drag payload for a drag that started on the row at the given
/// model position; installed by the pane once it is constructed.
type RowDragSlot = Rc<RefCell<Option<Box<dyn Fn(u32) -> Option<gtk::gdk::ContentProvider>>>>>;

/// Opens the context menu at view coordinates (x, y).
type ContextMenuHandler = Box<dyn Fn(f64, f64)>;

/// Called when a non-directory row is activated (double-click / Enter).
type FileActivateHandler = Box<dyn Fn(FsEntry)>;

pub struct Pane {
    pub root: gtk::Box,
    pub view: gtk::ColumnView,
    pub path_entry: gtk::Entry,
    pub new_folder_button: gtk::Button,
    pub connect_button: gtk::Button,
    pub disconnect_button: gtk::Button,
    pub conn_chip: gtk::Label,
    store: gio::ListStore,
    /// Holds the single ".." row (empty at a filesystem root). Kept in its
    /// own model, flattened in front of the sorted listing, so it is pinned
    /// to the top no matter how the view is sorted or filtered.
    parent_store: gio::ListStore,
    filter: gtk::CustomFilter,
    selection: gtk::MultiSelection,
    status: gtk::Label,
    show_hidden_flag: Rc<Cell<bool>>,
    /// Opens the context menu at view coordinates; installed by the app.
    context_menu: RefCell<Option<ContextMenuHandler>>,
    /// Handles activating a regular file (edit); installed by the app.
    file_activate: RefCell<Option<FileActivateHandler>>,
    pub state: RefCell<PaneState>,
}

impl Pane {
    pub fn new() -> Rc<Self> {
        let store = gio::ListStore::new::<EntryObject>();

        let show_hidden = Rc::new(Cell::new(false));
        let filter = {
            let show_hidden = show_hidden.clone();
            gtk::CustomFilter::new(move |obj| {
                if show_hidden.get() {
                    return true;
                }
                let entry = obj.downcast_ref::<EntryObject>().unwrap().entry();
                !entry.name.starts_with('.')
            })
        };
        let filter_model = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));

        let view = gtk::ColumnView::builder()
            .show_row_separators(false)
            .reorderable(false)
            .build();
        view.add_css_class("data-table");

        let sort_model = gtk::SortListModel::new(Some(filter_model), view.sorter());

        // WinSCP-style ".." row: a one-item model flattened before the
        // listing so "go up" is always the first row.
        let parent_store = gio::ListStore::new::<EntryObject>();
        let models = gio::ListStore::with_type(gio::ListModel::static_type());
        models.append(&parent_store);
        models.append(&sort_model);
        let flatten = gtk::FlattenListModel::new(Some(models));

        let selection = gtk::MultiSelection::new(Some(flatten));
        view.set_model(Some(&selection));

        // Set true by a row's right-click gesture (which fires before the
        // view-level one in the bubble phase) so the empty-space handler
        // knows not to clear the selection it just made.
        let row_click_handled = Rc::new(Cell::new(false));
        let row_menu: RowMenuSlot = Rc::new(RefCell::new(None));
        let row_drag: RowDragSlot = Rc::new(RefCell::new(None));
        add_columns(&view, &row_menu, &row_drag);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&view)
            .build();

        // Header: navigation buttons + path + connection controls.
        let up_button = gtk::Button::from_icon_name("go-up-symbolic");
        up_button.set_tooltip_text(Some("Parent directory (Backspace)"));
        let home_button = gtk::Button::from_icon_name("go-home-symbolic");
        home_button.set_tooltip_text(Some("Home directory"));
        let refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh_button.set_tooltip_text(Some("Refresh (Ctrl+R)"));
        let new_folder_button = gtk::Button::from_icon_name("folder-new-symbolic");
        new_folder_button.set_tooltip_text(Some("New folder (F7)"));
        let nav_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        nav_box.add_css_class("linked");
        nav_box.append(&up_button);
        nav_box.append(&home_button);
        nav_box.append(&refresh_button);
        nav_box.append(&new_folder_button);

        let path_entry = gtk::Entry::builder().hexpand(true).build();

        let connect_button = gtk::Button::from_icon_name("network-server-symbolic");
        connect_button.set_tooltip_text(Some("Connect to server…"));
        let disconnect_button = gtk::Button::from_icon_name("window-close-symbolic");
        disconnect_button.set_tooltip_text(Some("Disconnect"));
        disconnect_button.set_visible(false);

        let conn_chip = gtk::Label::new(Some("Local"));
        conn_chip.add_css_class("caption");
        conn_chip.add_css_class("dim-label");
        conn_chip.set_margin_start(6);
        conn_chip.set_margin_end(6);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.set_margin_start(6);
        header.set_margin_end(6);
        header.append(&nav_box);
        header.append(&path_entry);
        header.append(&conn_chip);
        header.append(&connect_button);
        header.append(&disconnect_button);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("caption");
        status.add_css_class("dim-label");
        status.set_margin_start(8);
        status.set_margin_top(3);
        status.set_margin_bottom(3);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&scroller);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&status);

        let pane = Rc::new(Self {
            root,
            view,
            path_entry,
            new_folder_button,
            connect_button,
            disconnect_button,
            conn_chip,
            store,
            parent_store,
            filter,
            selection,
            status,
            show_hidden_flag: show_hidden,
            context_menu: RefCell::new(None),
            file_activate: RefCell::new(None),
            state: RefCell::new(PaneState {
                backend: Backend::Local,
                dir: "/".into(),
                show_hidden: false,
                generation: 0,
            }),
        });

        // Wiring.
        {
            let p = pane.clone();
            up_button.connect_clicked(move |_| p.go_up());
        }
        {
            let p = pane.clone();
            home_button.connect_clicked(move |_| p.go_home());
        }
        {
            let p = pane.clone();
            refresh_button.connect_clicked(move |_| p.reload());
        }
        {
            let p = pane.clone();
            pane.path_entry.connect_activate(move |entry| {
                let path = entry.text().to_string();
                p.navigate(path);
                p.view.grab_focus();
            });
        }
        {
            let p = pane.clone();
            pane.view.connect_activate(move |_, position| {
                p.activate_row(position);
            });
        }
        {
            let p = pane.clone();
            pane.selection
                .connect_selection_changed(move |_, _, _| p.update_status());
        }
        {
            // Backspace = up, when the list has focus.
            let p = pane.clone();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, modifier| {
                if modifier.is_empty() && key == gtk::gdk::Key::BackSpace {
                    p.go_up();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            pane.view.add_controller(keys);
        }

        // Right-click on a row: focus this pane, make the row part of the
        // selection (replacing it unless the row was already selected), and
        // open the context menu. The row gesture (on a descendant cell) runs
        // before the view-level one below; it sets `row_click_handled` so
        // that one knows the click already hit a row.
        {
            let p = pane.clone();
            let row_click_handled = row_click_handled.clone();
            *row_menu.borrow_mut() = Some(Box::new(move |position, x, y| {
                // Mark this event as row-handled, and clear the mark on the
                // next idle so the flag can't leak into a later click even
                // if claiming the sequence prevents the view gesture (which
                // would otherwise consume the flag) from ever firing.
                row_click_handled.set(true);
                {
                    let row_click_handled = row_click_handled.clone();
                    glib::idle_add_local_once(move || row_click_handled.set(false));
                }
                p.view.grab_focus();
                if !p.selection.is_selected(position) {
                    p.selection.select_item(position, true);
                }
                if let Some(open) = p.context_menu.borrow().as_ref() {
                    open(x, y);
                }
            }));
        }

        // Right-click on empty space (no row hit): clear the selection and
        // open the menu with only the directory-level items enabled.
        {
            let p = pane.clone();
            let row_click_handled = row_click_handled.clone();
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            // Fire after the per-row cell gestures have had their turn.
            gesture.set_propagation_phase(gtk::PropagationPhase::Bubble);
            gesture.connect_pressed(move |_, _, x, y| {
                if row_click_handled.get() {
                    return; // a row already handled this right-click
                }
                p.view.grab_focus();
                p.selection.unselect_all();
                if let Some(open) = p.context_menu.borrow().as_ref() {
                    open(x, y);
                }
            });
            pane.view.add_controller(gesture);
        }

        // Drag payload builder for the per-row drag sources (attached to the
        // cells in `add_columns`). Starting a drag on a row that isn't part
        // of the selection retargets the selection to just that row, so a
        // single press-and-drag works without selecting the file first;
        // dragging a row that is already selected carries the whole
        // selection along.
        {
            let p = pane.clone();
            *row_drag.borrow_mut() = Some(Box::new(move |position| {
                if position < p.parent_rows() {
                    return None; // the ".." row is not draggable
                }
                if !p.selection.is_selected(position) {
                    p.selection.select_item(position, true);
                }
                let items: Vec<DropPayloadItem> = p
                    .selected_entries()
                    .into_iter()
                    .map(DropPayloadItem)
                    .collect();
                let payload = DragPayload {
                    source: p.backend(),
                    items,
                };
                Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
            }));
        }

        pane
    }

    /// Install the context-menu opener (called with view coordinates after
    /// the pane has adjusted focus and selection).
    pub fn set_context_menu(&self, open: impl Fn(f64, f64) + 'static) {
        *self.context_menu.borrow_mut() = Some(Box::new(open));
    }

    pub fn set_on_file_activate(&self, open: impl Fn(FsEntry) + 'static) {
        *self.file_activate.borrow_mut() = Some(Box::new(open));
    }

    /// Wire this pane as a drop target. `on_drop` receives the payload and
    /// this pane (the destination).
    pub fn setup_drop_target(self: &Rc<Self>, on_drop: impl Fn(DragPayload, Rc<Pane>) + 'static) {
        let target = gtk::DropTarget::new(DragPayload::static_type(), gtk::gdk::DragAction::COPY);
        let this = self.clone();
        let on_drop = std::rc::Rc::new(on_drop);
        target.connect_drop(move |_, value, _, _| {
            if let Ok(payload) = value.get::<DragPayload>() {
                on_drop(payload, this.clone());
                return true;
            }
            false
        });
        self.view.add_controller(target);
    }

    pub fn backend(&self) -> Backend {
        self.state.borrow().backend
    }

    pub fn current_dir(&self) -> String {
        self.state.borrow().dir.clone()
    }

    pub fn set_show_hidden(self: &Rc<Self>, show: bool) {
        self.state.borrow_mut().show_hidden = show;
        self.show_hidden_flag.set(show);
        self.filter.changed(if show {
            gtk::FilterChange::LessStrict
        } else {
            gtk::FilterChange::MoreStrict
        });
        self.update_status();
    }

    /// Switch this pane to a (new) backend and open `dir`.
    pub fn set_backend(self: &Rc<Self>, backend: Backend, dir: String, label: &str) {
        {
            let mut state = self.state.borrow_mut();
            state.backend = backend;
        }
        self.conn_chip.set_text(label);
        self.disconnect_button.set_visible(backend.is_remote());
        self.connect_button.set_visible(!backend.is_remote());
        self.navigate(dir);
    }

    pub fn navigate(self: &Rc<Self>, path: String) {
        let backend = self.backend();
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation += 1;
            state.generation
        };
        let this = self.clone();
        glib::spawn_future_local(async move {
            let path_for_task = path.clone();
            let result = runtime()
                .spawn(async move {
                    let dir = fsops::canonicalize(backend, &path_for_task)
                        .await
                        .unwrap_or(path_for_task);
                    let entries = fsops::list_dir(backend, &dir).await?;
                    Ok::<_, anyhow::Error>((dir, entries))
                })
                .await;
            if this.state.borrow().generation != generation {
                return; // superseded by a newer navigation
            }
            match result {
                Ok(Ok((dir, entries))) => {
                    // Jump to the top when actually changing directory;
                    // refreshes of the same directory keep their scroll.
                    let changed_dir = this.state.borrow().dir != dir;
                    this.state.borrow_mut().dir = dir.clone();
                    this.path_entry.set_text(&dir);
                    this.populate(entries);
                    if changed_dir && this.selection.n_items() > 0 {
                        this.view
                            .scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
                    }
                }
                Ok(Err(err)) => this.show_error(&format!("{err:#}")),
                Err(join_err) => this.show_error(&join_err.to_string()),
            }
        });
    }

    pub fn reload(self: &Rc<Self>) {
        let dir = self.current_dir();
        self.navigate(dir);
    }

    pub fn go_up(self: &Rc<Self>) {
        let dir = self.current_dir();
        let parent = fsops::parent(&dir);
        if parent != dir {
            self.navigate(parent);
        }
    }

    pub fn go_home(self: &Rc<Self>) {
        let backend = self.backend();
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Ok(home)) = runtime()
                .spawn(async move { fsops::home_dir(backend).await })
                .await
            {
                this.navigate(home);
            }
        });
    }

    fn populate(&self, entries: Vec<FsEntry>) {
        let objects: Vec<EntryObject> = entries.into_iter().map(EntryObject::new).collect();
        self.store.remove_all();
        self.store.extend_from_slice(&objects);
        self.refresh_parent_row();
        self.update_status();
    }

    /// Keep the ".." row pointing at the parent of the current directory
    /// (and drop it entirely at a filesystem root).
    fn refresh_parent_row(&self) {
        self.parent_store.remove_all();
        let dir = self.current_dir();
        let parent = fsops::parent(&dir);
        if parent != dir {
            self.parent_store.append(&EntryObject::new(FsEntry {
                name: "..".into(),
                path: parent,
                is_dir: true,
                is_symlink: false,
                size: 0,
                mtime: None,
                mode: None,
                owner: None,
                group: None,
                link_target: None,
            }));
        }
    }

    /// Number of leading rows that are the ".." entry (0 or 1).
    fn parent_rows(&self) -> u32 {
        self.parent_store.n_items()
    }

    fn activate_row(self: &Rc<Self>, position: u32) {
        let Some(obj) = self.selection.item(position) else {
            return;
        };
        let entry = obj.downcast_ref::<EntryObject>().unwrap().entry();
        if entry.is_dir {
            self.navigate(entry.path);
        } else if let Some(open) = self.file_activate.borrow().as_ref() {
            open(entry);
        }
    }

    /// All selected entries, in view order. The ".." navigation row is
    /// never included — it is not a real file operations could act on.
    pub fn selected_entries(&self) -> Vec<FsEntry> {
        let mut out = Vec::new();
        let skip = self.parent_rows();
        let bitset = self.selection.selection();
        for i in 0..bitset.size() {
            let pos = bitset.nth(i as u32);
            if pos < skip {
                continue;
            }
            if let Some(obj) = self.selection.item(pos) {
                out.push(obj.downcast_ref::<EntryObject>().unwrap().entry());
            }
        }
        out
    }

    /// The focused entry (cursor row), if any — used for rename.
    pub fn focused_entry(&self) -> Option<FsEntry> {
        let selected = self.selected_entries();
        selected.into_iter().next()
    }

    fn update_status(&self) {
        // Don't count the ".." navigation row.
        let skip = self.parent_rows();
        let total = self.selection.n_items().saturating_sub(skip);
        let selected_entries = self.selected_entries();
        let selected = selected_entries.len();
        let mut text = format!("{total} items");
        if selected > 0 {
            let bytes: u64 = selected_entries
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| e.size)
                .sum();
            text = format!("{selected} of {total} selected, {}", format_size(bytes));
        }
        self.status.set_text(&text);
    }

    fn show_error(&self, message: &str) {
        self.status.set_text(message);
        self.status.remove_css_class("dim-label");
        self.status.add_css_class("error");
        let status = self.status.clone();
        glib::timeout_add_seconds_local_once(6, move || {
            status.remove_css_class("error");
            status.add_css_class("dim-label");
        });
    }

    /// Session id if this pane is remote.
    pub fn session_id(&self) -> Option<linuxscp::types::SessionId> {
        match self.backend() {
            Backend::Remote(id) => Some(id),
            Backend::Local => None,
        }
    }

    /// Fall back to the local home directory (used when a session dies).
    pub fn to_local(self: &Rc<Self>) {
        if let Some(id) = self.session_id() {
            sessions::close(id);
        }
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into());
        self.set_backend(Backend::Local, home, "Local");
    }
}

fn add_columns(view: &gtk::ColumnView, row_menu: &RowMenuSlot, row_drag: &RowDragSlot) {
    // Name column with icon.
    let name_factory = gtk::SignalListItemFactory::new();
    {
        let view = view.clone();
        let row_menu = row_menu.clone();
        let row_drag = row_drag.clone();
        name_factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let icon = gtk::Image::new();
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            hbox.append(&icon);
            hbox.append(&label);
            item.set_child(Some(&hbox));
            attach_row_menu_gesture(item, hbox.upcast_ref(), &view, &row_menu);
            attach_row_drag_source(item, hbox.upcast_ref(), &row_drag);
        });
    }
    name_factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().and_downcast::<EntryObject>().unwrap().entry();
        let hbox = item.child().and_downcast::<gtk::Box>().unwrap();
        let icon = hbox.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
        label.set_text(&entry.name);
        if entry.is_symlink {
            label.set_tooltip_text(entry.link_target.as_deref());
        } else {
            label.set_tooltip_text(None);
        }
        icon.set_from_gicon(&icon_for(&entry));
    });
    let name_col = gtk::ColumnViewColumn::new(Some("Name"), Some(name_factory));
    name_col.set_expand(true);
    name_col.set_resizable(true);
    name_col.set_sorter(Some(&entry_sorter(|a, b| {
        a.name.to_lowercase().cmp(&b.name.to_lowercase())
    })));
    view.append_column(&name_col);

    let size_col = text_column(view, row_menu, row_drag, "Size", false, |e| {
        if e.is_dir {
            String::new()
        } else {
            format_size(e.size)
        }
    });
    size_col.set_sorter(Some(&entry_sorter(|a, b| a.size.cmp(&b.size))));
    view.append_column(&size_col);

    let mtime_col = text_column(view, row_menu, row_drag, "Modified", false, |e| {
        format_mtime(e.mtime)
    });
    mtime_col.set_sorter(Some(&entry_sorter(|a, b| a.mtime.cmp(&b.mtime))));
    view.append_column(&mtime_col);

    let perm_col = text_column(view, row_menu, row_drag, "Permissions", true, |e| {
        if is_parent_row(e) {
            String::new()
        } else {
            e.permissions_string()
        }
    });
    perm_col.set_sorter(Some(&entry_sorter(|a, b| a.mode.cmp(&b.mode))));
    view.append_column(&perm_col);

    let owner_col = text_column(view, row_menu, row_drag, "Owner", true, |e| {
        match (&e.owner, &e.group) {
            (Some(o), Some(g)) => format!("{o}:{g}"),
            (Some(o), None) => o.clone(),
            _ => String::new(),
        }
    });
    owner_col.set_sorter(Some(&entry_sorter(|a, b| a.owner.cmp(&b.owner))));
    view.append_column(&owner_col);

    // Default sort: by name, directories first.
    view.sort_by_column(Some(&name_col), gtk::SortType::Ascending);
}

/// Right-click on a cell reports the row position and view coordinates to
/// the pane's row-menu handler; the press is claimed so the view-level
/// blank-space gesture stays quiet.
fn attach_row_menu_gesture(
    item: &gtk::ListItem,
    cell: &gtk::Widget,
    view: &gtk::ColumnView,
    row_menu: &RowMenuSlot,
) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3);
    let item = item.clone();
    let cell_for_handler = cell.clone();
    let view = view.downgrade();
    let row_menu = row_menu.clone();
    gesture.connect_pressed(move |gesture, _, x, y| {
        let Some(view) = view.upgrade() else { return };
        let position = item.position();
        if position == gtk::INVALID_LIST_POSITION {
            return;
        }
        let Some(point) =
            cell_for_handler.compute_point(&view, &gtk::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if let Some(open) = row_menu.borrow().as_ref() {
            open(position, point.x() as f64, point.y() as f64);
        }
    });
    cell.add_controller(gesture);
}

/// Per-cell drag source so a drag can start on any row in one motion — no
/// need to select the file first. The cell knows its row through the
/// `ListItem`, and the pane's `RowDragSlot` handler turns that row (or the
/// existing selection containing it) into the drag payload.
fn attach_row_drag_source(item: &gtk::ListItem, cell: &gtk::Widget, row_drag: &RowDragSlot) {
    let drag = gtk::DragSource::new();
    drag.set_actions(gtk::gdk::DragAction::COPY);
    let item = item.clone();
    let row_drag = row_drag.clone();
    drag.connect_prepare(move |_, _, _| {
        let position = item.position();
        if position == gtk::INVALID_LIST_POSITION {
            return None;
        }
        row_drag.borrow().as_ref().and_then(|build| build(position))
    });
    cell.add_controller(drag);
}

fn text_column(
    view: &gtk::ColumnView,
    row_menu: &RowMenuSlot,
    row_drag: &RowDragSlot,
    title: &str,
    monospace: bool,
    getter: impl Fn(&FsEntry) -> String + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    {
        let view = view.clone();
        let row_menu = row_menu.clone();
        let row_drag = row_drag.clone();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            if monospace {
                label.add_css_class("monospace");
            }
            label.add_css_class("numeric");
            item.set_child(Some(&label));
            attach_row_menu_gesture(item, label.upcast_ref(), &view, &row_menu);
            attach_row_drag_source(item, label.upcast_ref(), &row_drag);
        });
    }
    let getter = Rc::new(getter);
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().and_downcast::<EntryObject>().unwrap().entry();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        label.set_text(&getter(&entry));
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col
}

/// True for the synthetic ".." row (no real file can carry that name).
fn is_parent_row(entry: &FsEntry) -> bool {
    entry.name == ".."
}

/// Sorter that keeps directories before files, then applies `cmp`.
fn entry_sorter(
    cmp: impl Fn(&FsEntry, &FsEntry) -> std::cmp::Ordering + 'static,
) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| {
        let a = a.downcast_ref::<EntryObject>().unwrap().entry();
        let b = b.downcast_ref::<EntryObject>().unwrap().entry();
        match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less.into(),
            (false, true) => std::cmp::Ordering::Greater.into(),
            _ => cmp(&a, &b).into(),
        }
    })
}

fn icon_for(entry: &FsEntry) -> gio::Icon {
    if is_parent_row(entry) {
        return gio::ThemedIcon::new("go-up-symbolic").upcast();
    }
    if entry.is_dir {
        return gio::ThemedIcon::new("folder").upcast();
    }
    let (content_type, _uncertain) =
        gio::content_type_guess(Some(std::path::Path::new(&entry.name)), None);
    gio::content_type_get_icon(&content_type)
}
