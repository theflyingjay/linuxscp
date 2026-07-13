//! WinSCP-style Properties dialog: file details plus editable owner, group
//! and permissions (with octal field), and an optional recursive apply to
//! everything below the selected directories.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::entry_object::{format_mtime, format_size};
use linuxscp::runtime::runtime;
use linuxscp::transfers::AttrRequest;
use linuxscp::types::{Backend, FsEntry};
use linuxscp::{fsops, sessions};

/// Show the dialog. When the user confirms with actual changes, `on_apply`
/// receives the request — the app runs it as a queued, watchable job.
pub fn show(
    parent: &impl IsA<gtk::Widget>,
    backend: Backend,
    entries: Vec<FsEntry>,
    on_apply: impl Fn(AttrRequest) + 'static,
) {
    if entries.is_empty() {
        return;
    }
    let parent = parent.clone().upcast::<gtk::Widget>();
    let has_dir = entries.iter().any(|e| e.is_dir && !e.is_symlink);

    // --- Read-only details -------------------------------------------------
    // A dense label grid (not ActionRows) so the whole dialog — including
    // the permission grid and the recursive checkbox — fits without
    // scrolling even on small screens.
    let info_grid = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(18)
        .build();
    let next_row = Rc::new(Cell::new(0i32));
    let add_info = {
        let info_grid = info_grid.clone();
        let next_row = next_row.clone();
        move |title: &str, value: &str| -> gtk::Label {
            let r = next_row.get();
            next_row.set(r + 1);
            let t = gtk::Label::builder()
                .label(title)
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Baseline)
                .build();
            t.add_css_class("dim-label");
            let v = gtk::Label::builder()
                .label(value)
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Baseline)
                .hexpand(true)
                .selectable(true)
                .max_width_chars(34)
                .build();
            v.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            info_grid.attach(&t, 0, r, 1, 1);
            info_grid.attach(&v, 1, r, 1, 1);
            v
        }
    };

    let where_label = match backend {
        Backend::Local => "Local".to_string(),
        Backend::Remote(id) => sessions::get(id)
            .map(|h| h.host.clone())
            .unwrap_or_else(|| "Remote".into()),
    };

    let heading;
    // Directory totals are only computed on demand: walking a big remote
    // tree costs a request per directory and would bog down both the dialog
    // and anything else using the session.
    let mut contents_value = None;
    let mut size_value = None;

    if entries.len() == 1 {
        let entry = &entries[0];
        heading = entry.name.clone();
        add_info("Location", &fsops::parent(&entry.path));
        add_info("Type", &type_description(entry));
        if entry.is_dir {
            contents_value = Some(add_info("Contents", "Not calculated"));
            size_value = Some(add_info("Size", "Not calculated"));
        } else {
            add_info("Size", &exact_size(entry.size));
        }
        add_info("Modified", &format_mtime(entry.mtime));
        if let Some(target) = &entry.link_target {
            add_info("Link target", target);
        }
        add_info("Stored on", &where_label);
    } else {
        heading = format!("{} Items", entries.len());
        add_info("Location", &fsops::parent(&entries[0].path));
        if has_dir {
            contents_value = Some(add_info("Contents", "Not calculated"));
            size_value = Some(add_info("Size", "Not calculated"));
        } else {
            // All plain files: the listing already knows the answer.
            add_info("Contents", &format!("{} files", entries.len()));
            add_info(
                "Size",
                &exact_size(entries.iter().map(|e| e.size).sum::<u64>()),
            );
        }
        add_info("Stored on", &where_label);
    }

    // "Calculate" fills the directory totals on demand; it rides beside the
    // Contents/Size rows so it costs no extra height.
    if let (Some(contents_value), Some(size_value)) = (&contents_value, &size_value) {
        let calc_btn = gtk::Button::builder()
            .label("Calculate")
            .valign(gtk::Align::Center)
            .build();
        let contents_grid_row = if entries.len() == 1 { 2 } else { 1 };
        info_grid.attach(&calc_btn, 2, contents_grid_row, 1, 2);
        let contents_value = contents_value.clone();
        let size_value = size_value.clone();
        let entries_for_scan = entries.clone();
        calc_btn.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            contents_value.set_label("Calculating…");
            size_value.set_label("Calculating…");
            let entries = entries_for_scan.clone();
            let handle = runtime().spawn(async move { fsops::disk_usage(backend, &entries).await });
            let contents_value = contents_value.clone();
            let size_value = size_value.clone();
            let btn = btn.clone();
            glib::spawn_future_local(async move {
                let usage = match handle.await {
                    Ok(Ok(usage)) => usage,
                    _ => {
                        contents_value.set_label("Unavailable");
                        size_value.set_label("Unavailable");
                        btn.set_sensitive(true);
                        return;
                    }
                };
                contents_value.set_label(&format!(
                    "{} {}, {} {}",
                    usage.files,
                    if usage.files == 1 { "file" } else { "files" },
                    usage.dirs,
                    if usage.dirs == 1 { "folder" } else { "folders" },
                ));
                size_value.set_label(&exact_size(usage.bytes));
                btn.set_visible(false);
            });
        });
    }

    // --- Owner / group (editable) ------------------------------------------
    let initial_owner = common_value(&entries, |e| e.owner.clone());
    let initial_group = common_value(&entries, |e| e.group.clone());

    let ident = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(18)
        .build();
    let ident_entry = |title: &str, initial: &str, grid_row: i32| {
        let label = gtk::Label::builder()
            .label(title)
            .halign(gtk::Align::Start)
            .build();
        label.add_css_class("dim-label");
        let entry = gtk::Entry::builder()
            .text(initial)
            .hexpand(true)
            .activates_default(true)
            .build();
        ident.attach(&label, 0, grid_row, 1, 1);
        ident.attach(&entry, 1, grid_row, 1, 1);
        entry
    };
    let owner_row = ident_entry("Owner", &initial_owner, 0);
    let group_row = ident_entry("Group", &initial_group, 1);

    // --- Permissions grid ---------------------------------------------------
    let perms = Permissions::new(initial_mode(&entries));

    let add_x_check = gtk::CheckButton::with_label("Add X to directories");
    add_x_check.set_visible(has_dir);
    let recursive_check =
        gtk::CheckButton::with_label("Set owner, group and permissions recursively");
    recursive_check.set_visible(has_dir);

    let perm_group = adw::PreferencesGroup::builder()
        .title("Permissions")
        .build();
    let perm_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    perm_box.append(&perms.grid);
    perm_box.append(&add_x_check);
    perm_box.append(&recursive_check);
    perm_group.add(&perm_box);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.append(&info_grid);
    content.append(&ident);
    content.append(&perm_group);

    let dialog = adw::AlertDialog::builder()
        .heading(&heading)
        .close_response("cancel")
        .default_response("ok")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_extra_child(Some(&content));

    let owner_focus = owner_row.clone();
    {
        let perms = perms.clone();
        dialog.connect_response(Some("ok"), move |_, _| {
            let owner_text = owner_row.text().trim().to_string();
            let group_text = group_row.text().trim().to_string();
            let owner =
                (owner_text != initial_owner && !owner_text.is_empty()).then_some(owner_text);
            let group =
                (group_text != initial_group && !group_text.is_empty()).then_some(group_text);
            let mode = perms.dirty.get().then(|| perms.collect());
            if owner.is_none() && group.is_none() && mode.is_none() {
                return; // nothing changed
            }
            on_apply(AttrRequest {
                owner,
                group,
                mode,
                add_x_dirs: add_x_check.is_active(),
                recursive: recursive_check.is_active(),
            });
        });
    }

    dialog.present(Some(&parent));
    // Keep focus off the selectable info labels (which would show an
    // all-selected highlight); start in the first editable field instead.
    owner_focus.grab_focus();
}

/// The WinSCP-style permission editor: R/W/X per class, the special bits,
/// and an octal entry kept in sync both ways. `dirty` flips once the user
/// touches any of it, so untouched permissions are never re-applied.
#[derive(Clone)]
struct Permissions {
    grid: gtk::Grid,
    checks: Rc<[[gtk::CheckButton; 3]; 3]>,
    specials: Rc<[gtk::CheckButton; 3]>,
    octal: gtk::Entry,
    dirty: Rc<Cell<bool>>,
    syncing: Rc<Cell<bool>>,
}

impl Permissions {
    fn new(initial: u32) -> Self {
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(14)
            .build();

        let checks: [[gtk::CheckButton; 3]; 3] = std::array::from_fn(|class| {
            std::array::from_fn(|perm| {
                let bit = 1u32 << (8 - (class * 3 + perm));
                gtk::CheckButton::builder()
                    .label(["R", "W", "X"][perm])
                    .active(initial & bit != 0)
                    .build()
            })
        });
        let specials: [gtk::CheckButton; 3] = std::array::from_fn(|i| {
            let bit = [0o4000u32, 0o2000, 0o1000][i];
            gtk::CheckButton::builder()
                .label(["Set UID", "Set GID", "Sticky bit"][i])
                .active(initial & bit != 0)
                .build()
        });

        for (class, class_name) in ["Owner", "Group", "Others"].iter().enumerate() {
            let label = gtk::Label::builder()
                .label(*class_name)
                .halign(gtk::Align::Start)
                .build();
            grid.attach(&label, 0, class as i32, 1, 1);
            for (perm, check) in checks[class].iter().enumerate() {
                grid.attach(check, perm as i32 + 1, class as i32, 1, 1);
            }
            grid.attach(&specials[class], 4, class as i32, 1, 1);
        }

        let octal_label = gtk::Label::builder()
            .label("Octal")
            .halign(gtk::Align::Start)
            .build();
        grid.attach(&octal_label, 0, 3, 1, 1);
        let octal = gtk::Entry::builder()
            .max_length(4)
            .width_chars(6)
            .max_width_chars(6)
            .activates_default(true)
            .build();
        octal.add_css_class("monospace");
        grid.attach(&octal, 1, 3, 2, 1);

        let this = Self {
            grid,
            checks: Rc::new(checks),
            specials: Rc::new(specials),
            octal,
            dirty: Rc::new(Cell::new(false)),
            syncing: Rc::new(Cell::new(false)),
        };
        this.write_octal(initial);

        // Checkbox toggles rewrite the octal field…
        for check in this.checks.iter().flatten().chain(this.specials.iter()) {
            let this2 = this.clone();
            check.connect_toggled(move |_| {
                if this2.syncing.get() {
                    return;
                }
                this2.dirty.set(true);
                this2.write_octal(this2.collect());
            });
        }
        // …and octal edits drive the checkboxes.
        {
            let this2 = this.clone();
            this.octal.connect_changed(move |entry| {
                if this2.syncing.get() {
                    return;
                }
                let text = entry.text();
                let Ok(mode) = u32::from_str_radix(text.trim(), 8) else {
                    return; // partial input; leave the boxes alone
                };
                this2.dirty.set(true);
                this2.set_checks(mode & 0o7777);
            });
        }
        this
    }

    fn collect(&self) -> u32 {
        let mut mode = 0u32;
        for (class, row) in self.checks.iter().enumerate() {
            for (perm, check) in row.iter().enumerate() {
                if check.is_active() {
                    mode |= 1 << (8 - (class * 3 + perm));
                }
            }
        }
        for (i, check) in self.specials.iter().enumerate() {
            if check.is_active() {
                mode |= [0o4000, 0o2000, 0o1000][i];
            }
        }
        mode
    }

    fn set_checks(&self, mode: u32) {
        self.syncing.set(true);
        for (class, row) in self.checks.iter().enumerate() {
            for (perm, check) in row.iter().enumerate() {
                let bit = 1u32 << (8 - (class * 3 + perm));
                check.set_active(mode & bit != 0);
            }
        }
        for (i, check) in self.specials.iter().enumerate() {
            check.set_active(mode & [0o4000, 0o2000, 0o1000][i] != 0);
        }
        self.syncing.set(false);
    }

    fn write_octal(&self, mode: u32) {
        self.syncing.set(true);
        self.octal.set_text(&format!("{mode:04o}"));
        self.syncing.set(false);
    }
}

/// The value shared by every entry, or "" when they differ / are unknown.
fn common_value(entries: &[FsEntry], get: impl Fn(&FsEntry) -> Option<String>) -> String {
    let first = get(&entries[0]);
    if entries.iter().all(|e| get(e) == first) {
        first.unwrap_or_default()
    } else {
        String::new()
    }
}

/// Seed for the permission boxes: the mode every entry agrees on, else the
/// first known one, else a sensible default.
fn initial_mode(entries: &[FsEntry]) -> u32 {
    let first = entries[0].mode;
    if entries.iter().all(|e| e.mode == first) {
        return first.unwrap_or(0o644);
    }
    entries.iter().find_map(|e| e.mode).unwrap_or(0o644)
}

/// "1.5 MB (1,572,864 bytes)" style size text.
fn exact_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    format!("{} ({} bytes)", format_size(bytes), group_digits(bytes))
}

fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn type_description(entry: &FsEntry) -> String {
    if entry.is_symlink {
        return if entry.is_dir {
            "Symbolic link to folder".into()
        } else {
            "Symbolic link".into()
        };
    }
    if entry.is_dir {
        return "Folder".into();
    }
    let (content_type, uncertain) =
        gio::content_type_guess(Some(std::path::Path::new(&entry.name)), None);
    if uncertain && content_type == "application/octet-stream" {
        return "File".into();
    }
    gio::content_type_get_description(&content_type).to_string()
}
