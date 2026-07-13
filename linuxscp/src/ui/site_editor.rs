//! Inline editor form for a saved site: host, port, user, auth (password or
//! PEM key), elevation and landing directory. Embedded in the site manager's
//! right-hand pane (WinSCP-style) rather than shown as a separate modal.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use linuxscp::settings::Site;
use linuxscp::types::{AuthMethod, Elevation};

/// What to do with the site's stored secret after editing.
#[derive(Debug, Clone)]
pub enum SecretUpdate {
    /// Leave the keyring entry untouched.
    Keep,
    /// Store this new secret.
    Set(String),
    /// Remove any stored secret.
    Clear,
}

/// Live-edit callback: (display name, "user@host:port" summary).
type ChangeHandler = Box<dyn Fn(String, String)>;

/// The embeddable site-details form. Fill it with [`SiteForm::load`] (a site
/// or a blank draft) and read it back with [`SiteForm::collect`]. The
/// `on_changed` callback reports live edits of the identity fields so the
/// site list can mirror the name while the user types.
pub struct SiteForm {
    pub root: gtk::Box,
    name_row: adw::EntryRow,
    host_row: adw::EntryRow,
    user_row: adw::EntryRow,
    port_row: adw::EntryRow,
    auth_row: adw::ComboRow,
    key_row: adw::ActionRow,
    secret_row: adw::PasswordEntryRow,
    elevation_row: adw::ComboRow,
    dir_row: adw::EntryRow,
    key_path: RefCell<Option<String>>,
    /// The site being edited; keeps id (and folder membership) stable.
    base: RefCell<Site>,
    had_secret: Cell<bool>,
    /// Suppresses `on_changed` while `load` fills the fields.
    loading: Cell<bool>,
    /// Called with (display name, "user@host:port" summary) on live edits.
    on_changed: RefCell<Option<ChangeHandler>>,
}

impl SiteForm {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);

        // --- Connection group ---
        let conn_group = adw::PreferencesGroup::builder().title("Connection").build();
        let name_row = adw::EntryRow::builder().title("Name").build();
        let host_row = adw::EntryRow::builder().title("Host").build();
        let user_row = adw::EntryRow::builder().title("Username").build();
        let port_row = adw::EntryRow::builder().title("Port (blank = 22)").build();
        conn_group.add(&name_row);
        conn_group.add(&host_row);
        conn_group.add(&user_row);
        conn_group.add(&port_row);
        root.append(&conn_group);

        // --- Authentication group ---
        let auth_group = adw::PreferencesGroup::builder()
            .title("Authentication")
            .build();
        let auth_model = gtk::StringList::new(&AuthMethod::ALL.map(|a| a.label()));
        let auth_row = adw::ComboRow::builder()
            .title("Method")
            .model(&auth_model)
            .build();
        auth_group.add(&auth_row);

        // Key file chooser (visible for Key auth).
        let key_row = adw::ActionRow::builder()
            .title("Key file")
            .subtitle("None selected")
            .build();
        let key_button = gtk::Button::from_icon_name("document-open-symbolic");
        key_button.set_valign(gtk::Align::Center);
        key_button.add_css_class("flat");
        key_row.add_suffix(&key_button);
        key_row.set_activatable_widget(Some(&key_button));
        auth_group.add(&key_row);

        // Secret entry (password, or key passphrase).
        let secret_row = adw::PasswordEntryRow::builder().title("Password").build();
        auth_group.add(&secret_row);
        root.append(&auth_group);

        // --- Session group ---
        let session_group = adw::PreferencesGroup::builder().title("Session").build();
        let elevation_model = gtk::StringList::new(&Elevation::ALL.map(|e| e.label()));
        let elevation_row = adw::ComboRow::builder()
            .title("Login as")
            .model(&elevation_model)
            .build();
        let dir_row = adw::EntryRow::builder()
            .title("Remote directory (optional)")
            .build();
        session_group.add(&elevation_row);
        session_group.add(&dir_row);
        root.append(&session_group);

        let form = Rc::new(Self {
            root,
            name_row,
            host_row,
            user_row,
            port_row,
            auth_row,
            key_row,
            secret_row,
            elevation_row,
            dir_row,
            key_path: RefCell::new(None),
            base: RefCell::new(Site::new("", "")),
            had_secret: Cell::new(false),
            loading: Cell::new(false),
            on_changed: RefCell::new(None),
        });

        // Signal wiring uses weak refs so widget closures never keep the
        // form (and thus the whole widget tree) alive in a cycle.
        {
            let weak = Rc::downgrade(&form);
            form.auth_row.connect_selected_notify(move |_| {
                if let Some(form) = weak.upgrade() {
                    form.refresh_auth_rows();
                }
            });
        }
        for row in [
            &form.name_row,
            &form.host_row,
            &form.user_row,
            &form.port_row,
        ] {
            let weak = Rc::downgrade(&form);
            row.connect_changed(move |_| {
                if let Some(form) = weak.upgrade() {
                    form.emit_changed();
                }
            });
        }
        {
            let weak = Rc::downgrade(&form);
            key_button.connect_clicked(move |button| {
                if weak.upgrade().is_none() {
                    return;
                }
                let dialog = gtk::FileDialog::builder()
                    .title("Select private key")
                    .build();
                if let Some(dir) = dirs::home_dir() {
                    let ssh_dir = dir.join(".ssh");
                    let start = if ssh_dir.is_dir() { ssh_dir } else { dir };
                    dialog.set_initial_folder(Some(&gtk::gio::File::for_path(start)));
                }
                let win = button.root().and_downcast::<gtk::Window>();
                let weak = weak.clone();
                dialog.open(win.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                    let Some(form) = weak.upgrade() else { return };
                    if let Ok(file) = res {
                        if let Some(path) = file.path() {
                            let text = path.to_string_lossy().into_owned();
                            form.key_row.set_subtitle(&text);
                            *form.key_path.borrow_mut() = Some(text);
                        }
                    }
                });
            });
        }

        form.refresh_auth_rows();
        form
    }

    /// Report live edits of name/host/user/port as (display name, summary).
    pub fn set_on_changed(&self, callback: impl Fn(String, String) + 'static) {
        *self.on_changed.borrow_mut() = Some(Box::new(callback));
    }

    fn emit_changed(&self) {
        if self.loading.get() {
            return;
        }
        if let Some(callback) = self.on_changed.borrow().as_ref() {
            callback(self.display_name(), self.summary());
        }
    }

    /// Fill the form from `site`; `None` starts a blank draft (fresh id).
    pub fn load(&self, site: Option<&Site>) {
        self.loading.set(true);
        let site = site.cloned().unwrap_or_else(|| Site::new("", ""));
        self.had_secret.set(site.has_secret);
        self.name_row.set_text(&site.name);
        self.host_row.remove_css_class("error");
        self.host_row.set_text(&site.host);
        self.user_row.set_text(site.user.as_deref().unwrap_or(""));
        self.port_row
            .set_text(&site.port.map(|p| p.to_string()).unwrap_or_default());
        self.auth_row.set_selected(
            AuthMethod::ALL
                .iter()
                .position(|a| *a == site.auth)
                .unwrap_or(0) as u32,
        );
        self.key_row
            .set_subtitle(site.identity_file.as_deref().unwrap_or("None selected"));
        *self.key_path.borrow_mut() = site.identity_file.clone();
        self.elevation_row.set_selected(
            Elevation::ALL
                .iter()
                .position(|e| *e == site.elevation)
                .unwrap_or(0) as u32,
        );
        self.dir_row
            .set_text(site.remote_dir.as_deref().unwrap_or(""));
        self.secret_row.set_text("");
        self.base.replace(site);
        self.refresh_auth_rows();
        self.loading.set(false);
    }

    /// Name for the site list row while editing: the name field, falling
    /// back to the host, then a placeholder.
    pub fn display_name(&self) -> String {
        let name = self.name_row.text().trim().to_string();
        if !name.is_empty() {
            return name;
        }
        let host = self.host_row.text().trim().to_string();
        if !host.is_empty() {
            return host;
        }
        "New site".into()
    }

    /// Live "user@host:port" preview from the current field values.
    pub fn summary(&self) -> String {
        let mut site = Site::new("", self.host_row.text().trim());
        site.user = Some(self.user_row.text().trim().to_string()).filter(|s| !s.is_empty());
        site.port = self.port_row.text().trim().parse::<u16>().ok();
        site.summary()
    }

    /// Current text of the secret field (used to connect without saving).
    pub fn secret_text(&self) -> String {
        self.secret_row.text().to_string()
    }

    /// Validate and collect the edited site plus the secret action to
    /// apply. Marks the host row and returns `None` when the host is empty.
    pub fn collect(&self) -> Option<(Site, SecretUpdate)> {
        let name = self.name_row.text().trim().to_string();
        let host = self.host_row.text().trim().to_string();
        if host.is_empty() {
            self.host_row.add_css_class("error");
            self.host_row.grab_focus();
            return None;
        }
        self.host_row.remove_css_class("error");

        let mut site = self.base.borrow().clone();
        site.name = if name.is_empty() { host.clone() } else { name };
        site.host = host;
        site.user = Some(self.user_row.text().trim().to_string()).filter(|s| !s.is_empty());
        site.port = self.port_row.text().trim().parse::<u16>().ok();
        site.auth = AuthMethod::ALL[self.auth_row.selected() as usize];
        site.identity_file = self.key_path.borrow().clone().filter(|s| !s.is_empty());
        site.elevation = Elevation::ALL[self.elevation_row.selected() as usize];
        site.remote_dir = Some(self.dir_row.text().trim().to_string()).filter(|s| !s.is_empty());

        // Decide what happens to the stored secret. Clear only when there
        // is actually something to clear, so the common no-secret case
        // never touches the keyring.
        let secret_text = self.secret_row.text().to_string();
        let wants_secret = matches!(site.auth, AuthMethod::Password | AuthMethod::Key);
        let secret_update = if !wants_secret {
            if self.had_secret.get() {
                SecretUpdate::Clear
            } else {
                SecretUpdate::Keep
            }
        } else if !secret_text.is_empty() {
            SecretUpdate::Set(secret_text)
        } else {
            SecretUpdate::Keep
        };
        site.has_secret = match &secret_update {
            SecretUpdate::Set(_) => true,
            SecretUpdate::Clear => false,
            SecretUpdate::Keep => self.had_secret.get(),
        };

        Some((site, secret_update))
    }

    pub fn focus_name(&self) {
        let row = self.name_row.clone();
        glib::idle_add_local_once(move || {
            row.grab_focus();
        });
    }

    /// Show/hide auth-specific rows for the selected method.
    fn refresh_auth_rows(&self) {
        let method = AuthMethod::ALL[self.auth_row.selected() as usize];
        self.key_row.set_visible(method == AuthMethod::Key);
        let show_secret = matches!(method, AuthMethod::Password | AuthMethod::Key);
        self.secret_row.set_visible(show_secret);
        self.secret_row
            .set_title(match (method, self.had_secret.get()) {
                (AuthMethod::Key, true) => "Passphrase (leave blank to keep saved)",
                (AuthMethod::Key, false) => "Passphrase (optional)",
                (_, true) => "Password (leave blank to keep saved)",
                (_, false) => "Password",
            });
    }
}
