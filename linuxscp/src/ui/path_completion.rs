//! Folder suggestions for a pane's path entry: a dropdown under the entry
//! listing the sub-directories of whatever directory has been typed so far,
//! narrowing as the name is typed. It goes through `fsops`, so it behaves
//! the same over the local disk and an SFTP session.
//!
//! The entry keeps keyboard focus the whole time; the dropdown is driven
//! from it:
//! - typing `/mnt/` lists the folders in `/mnt`; `/mnt/da` narrows that to
//!   names starting with `da` (case-insensitively);
//! - Up/Down highlight a row; Enter or a double-click opens the highlighted
//!   folder in the pane;
//! - Tab (or Right at the end of the text) completes the entry to the
//!   highlighted row, the only match, or what all matches share — ending in
//!   `/` so the next level is listed straight away;
//! - Escape closes the dropdown, as does leaving the entry.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::entry_object::EntryObject;
use linuxscp::fsops;
use linuxscp::runtime::runtime;
use linuxscp::types::{Backend, FsEntry};

/// Tallest the dropdown grows before it scrolls.
const MAX_HEIGHT: i32 = 320;
/// Narrowest the dropdown gets, for very short entries.
const MIN_WIDTH: i32 = 240;
/// Horizontal padding libadwaita puts around a popover's contents; the list
/// is sized so the dropdown as a whole spans the entry.
const POPOVER_PADDING: i32 = 8;

/// Reports which backend to list and whether hidden folders are offered.
type SourceFn = Box<dyn Fn() -> (Backend, bool)>;
/// Opens the chosen directory in the pane.
type NavigateFn = Box<dyn Fn(String)>;

/// One directory listing, kept while the typed directory part stays the same
/// so each keystroke only re-filters instead of re-reading the directory.
struct Listing {
    backend: Backend,
    /// The directory exactly as typed, trailing slash included.
    dir: String,
    entries: Vec<FsEntry>,
}

pub struct PathCompletion {
    entry: gtk::Entry,
    popover: gtk::Popover,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListView,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    source: SourceFn,
    on_navigate: NavigateFn,
    cache: RefCell<Option<Listing>>,
    /// `(backend, dir)` of a listing currently being read.
    pending: RefCell<Option<(Backend, String)>>,
    /// Bumped by every listing request and by `dismiss`, so results that
    /// land late are dropped.
    generation: Cell<u64>,
    /// Set while `set_text_quiet` changes the entry, so the `changed`
    /// handler ignores programmatic updates.
    suppress: Cell<bool>,
}

impl PathCompletion {
    /// Attach folder suggestions to `entry`. `source` is asked for the
    /// backend to list and the pane's show-hidden flag whenever suggestions
    /// are computed; `on_navigate` receives the directory the user picked.
    pub fn attach(
        entry: &gtk::Entry,
        source: impl Fn() -> (Backend, bool) + 'static,
        on_navigate: impl Fn(String) + 'static,
    ) -> Rc<Self> {
        let store = gio::ListStore::new::<EntryObject>();
        let selection = gtk::SingleSelection::builder()
            .model(&store)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let list = gtk::ListView::new(Some(selection.clone()), Some(row_factory()));
        list.set_can_focus(false);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(MAX_HEIGHT)
            .child(&list)
            .build();
        // Not autohide: the popover must never take the keyboard focus
        // away from the entry, and it is closed from the entry's own
        // key and focus handling below.
        let popover = gtk::Popover::builder()
            .position(gtk::PositionType::Bottom)
            .has_arrow(false)
            .autohide(false)
            .child(&scroller)
            .build();
        popover.set_can_focus(false);
        popover.set_parent(entry);

        let this = Rc::new(Self {
            entry: entry.clone(),
            popover,
            scroller,
            list: list.clone(),
            store,
            selection: selection.clone(),
            source: Box::new(source),
            on_navigate: Box::new(on_navigate),
            cache: RefCell::new(None),
            pending: RefCell::new(None),
            generation: Cell::new(0),
            suppress: Cell::new(false),
        });

        // The GTK closures hold weak references: the pane owns the
        // completion, and the widgets outlive nothing they shouldn't.
        {
            let weak = Rc::downgrade(&this);
            entry.connect_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    if !this.suppress.get() {
                        this.refresh();
                    }
                }
            });
        }
        {
            // Double-click (or Enter while the list has focus) on a row.
            let weak = Rc::downgrade(&this);
            list.connect_activate(move |_, position| {
                if let Some(this) = weak.upgrade() {
                    this.open_row(position);
                }
            });
        }
        {
            // A single click only highlights; should the click have moved
            // the keyboard focus into the list, hand it straight back.
            let weak = Rc::downgrade(&this);
            selection.connect_selection_changed(move |_, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.keep_entry_focused();
                }
            });
        }
        {
            // Capture phase: runs before the entry's own text handling and
            // before the window-level shortcuts, for the keys handled here.
            let weak = Rc::downgrade(&this);
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            keys.connect_key_pressed(move |_, key, _, modifier| match weak.upgrade() {
                Some(this) => this.on_key(key, modifier),
                None => glib::Propagation::Proceed,
            });
            entry.add_controller(keys);
        }
        {
            // Leaving the entry closes the dropdown. Checked on idle so the
            // new focus location has settled (and so a focus bounce that
            // lands back in the entry keeps the dropdown open).
            let weak = Rc::downgrade(&this);
            let focus = gtk::EventControllerFocus::new();
            focus.connect_leave(move |_| {
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(this) = weak.upgrade() {
                        if !this.entry_has_focus() {
                            this.dismiss();
                        }
                    }
                });
            });
            entry.add_controller(focus);
        }

        this
    }

    /// Set the entry text without opening suggestions (directory loads).
    pub fn set_text_quiet(&self, text: &str) {
        self.suppress.set(true);
        self.entry.set_text(text);
        self.suppress.set(false);
        self.dismiss();
    }

    /// Close the dropdown and forget the cached listing, so the next typing
    /// session re-reads the directory.
    pub fn dismiss(&self) {
        self.hide();
        self.cache.take();
        self.pending.take();
        self.generation.set(self.generation.get() + 1);
    }

    fn is_open(&self) -> bool {
        self.popover.is_visible()
    }

    fn hide(&self) {
        if self.is_open() {
            self.popover.popdown();
        }
    }

    fn entry_has_focus(&self) -> bool {
        self.entry
            .state_flags()
            .contains(gtk::StateFlags::FOCUS_WITHIN)
    }

    fn keep_entry_focused(&self) {
        if self
            .popover
            .state_flags()
            .contains(gtk::StateFlags::FOCUS_WITHIN)
        {
            self.entry.grab_focus_without_selecting();
        }
    }

    /// Recompute the suggestions for the entry's current text: re-filter
    /// the cached listing when the directory part is unchanged, otherwise
    /// read that directory (once) and come back here when it lands.
    fn refresh(self: &Rc<Self>) {
        let text = self.entry.text();
        let Some((dir, prefix)) = split_typed(&text) else {
            self.hide();
            return;
        };
        let (backend, _) = (self.source)();
        let cached = self
            .cache
            .borrow()
            .as_ref()
            .is_some_and(|listing| listing.backend == backend && listing.dir == dir);
        if cached {
            self.show_matches(&prefix);
            return;
        }

        let key = (backend, dir);
        if self.pending.borrow().as_ref() == Some(&key) {
            return; // that listing is already on its way
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        *self.pending.borrow_mut() = Some(key.clone());

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (backend, dir) = key;
            let dir_for_task = dir.clone();
            let result = runtime()
                .spawn(async move { fsops::list_dir(backend, &dir_for_task).await })
                .await;
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.generation.get() != generation {
                return; // superseded by newer typing or a dismiss
            }
            this.pending.take();
            // A directory that doesn't exist or can't be read offers nothing
            // (and is remembered as such until the dropdown is dismissed).
            let entries = match result {
                Ok(Ok(entries)) => entries,
                _ => Vec::new(),
            };
            *this.cache.borrow_mut() = Some(Listing {
                backend,
                dir,
                entries,
            });
            this.refresh();
        });
    }

    fn show_matches(&self, prefix: &str) {
        let (_, show_hidden) = (self.source)();
        let matches = match self.cache.borrow().as_ref() {
            Some(listing) => matching_dirs(&listing.entries, prefix, show_hidden),
            None => Vec::new(),
        };
        if matches.is_empty() {
            self.hide();
            return;
        }
        let objects: Vec<EntryObject> = matches.into_iter().map(EntryObject::new).collect();
        self.store.splice(0, self.store.n_items(), &objects);
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.present();
    }

    fn present(&self) {
        // Span the entry so the dropdown reads as part of it; long names
        // ellipsize rather than widen it.
        let width = (self.entry.width() - 2 * POPOVER_PADDING).max(MIN_WIDTH);
        self.scroller.set_size_request(width, -1);
        if self.is_open() {
            // An open popover keeps its surface size when its content
            // changes; re-present it so the dropdown shrinks and grows
            // with the number of rows.
            self.list.queue_resize();
            self.popover.present();
        } else {
            self.popover.popup();
        }
    }

    fn on_key(
        self: &Rc<Self>,
        key: gtk::gdk::Key,
        modifier: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        use gtk::gdk::{Key, ModifierType};

        if modifier.intersects(
            ModifierType::SHIFT_MASK
                | ModifierType::CONTROL_MASK
                | ModifierType::ALT_MASK
                | ModifierType::SUPER_MASK,
        ) {
            return glib::Propagation::Proceed;
        }
        let open = self.is_open();
        match key {
            Key::Escape if open => {
                self.dismiss();
                glib::Propagation::Stop
            }
            Key::Down | Key::KP_Down => {
                if open {
                    self.move_highlight(1);
                } else {
                    self.refresh(); // re-open suggestions for the current text
                }
                glib::Propagation::Stop
            }
            Key::Up | Key::KP_Up if open => {
                self.move_highlight(-1);
                glib::Propagation::Stop
            }
            Key::Return | Key::KP_Enter | Key::ISO_Enter if open => match self.highlighted() {
                Some(position) => {
                    self.open_row(position);
                    glib::Propagation::Stop
                }
                // Nothing highlighted: let the entry's `activate` open
                // whatever was typed.
                None => glib::Propagation::Proceed,
            },
            Key::Tab | Key::KP_Tab if open => {
                self.complete();
                glib::Propagation::Stop
            }
            Key::Right | Key::KP_Right if open && self.cursor_at_end() => {
                self.complete();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    /// True when the cursor sits after the last character with nothing
    /// selected, so Right has no cursor movement left to do.
    fn cursor_at_end(&self) -> bool {
        self.entry.selection_bounds().is_none()
            && self.entry.position() == self.entry.text().chars().count() as i32
    }

    fn highlighted(&self) -> Option<u32> {
        let position = self.selection.selected();
        (position != gtk::INVALID_LIST_POSITION).then_some(position)
    }

    /// Move the highlight by `delta` rows, cycling through "nothing
    /// highlighted" between the last and first rows (like a browser's
    /// location bar, so Enter can still open exactly what was typed).
    fn move_highlight(&self, delta: i32) {
        let count = self.store.n_items() as i32;
        if count == 0 {
            return;
        }
        let current = self.highlighted().map_or(-1, |p| p as i32);
        let next = (current + 1 + delta).rem_euclid(count + 1) - 1;
        if next < 0 {
            self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        } else {
            self.highlight(next as u32);
        }
    }

    fn highlight(&self, position: u32) {
        self.selection.set_selected(position);
        self.list
            .scroll_to(position, gtk::ListScrollFlags::NONE, None);
    }

    fn row_name(&self, position: u32) -> Option<String> {
        let obj = self.store.item(position)?;
        Some(obj.downcast_ref::<EntryObject>()?.entry().name)
    }

    /// Complete the typed text: to the highlighted row or the only match
    /// (as `.../name/`, which lists the next level), else to the longest
    /// prefix all matches share; when even that adds nothing, highlight the
    /// first row so the next Tab takes it.
    fn complete(&self) {
        let text = self.entry.text();
        let Some((dir, prefix)) = split_typed(&text) else {
            return;
        };
        let names: Vec<String> = (0..self.store.n_items())
            .filter_map(|position| self.row_name(position))
            .collect();
        if names.is_empty() {
            return;
        }
        let chosen = match self.highlighted() {
            Some(position) => names.get(position as usize).cloned(),
            None if names.len() == 1 => Some(names[0].clone()),
            None => None,
        };
        if let Some(name) = chosen {
            self.set_text_live(&format!("{dir}{name}/"));
            return;
        }
        let shared = common_prefix(&names);
        if shared.chars().count() > prefix.chars().count() {
            self.set_text_live(&format!("{dir}{shared}"));
        } else {
            self.highlight(0);
        }
    }

    /// Replace the entry text as if typed: the cursor goes to the end and
    /// the suggestions refresh through the `changed` handler.
    fn set_text_live(&self, text: &str) {
        self.entry.set_text(text);
        self.entry.set_position(-1);
    }

    fn open_row(&self, position: u32) {
        let Some(obj) = self.store.item(position) else {
            return;
        };
        let entry = obj.downcast_ref::<EntryObject>().unwrap().entry();
        self.dismiss();
        (self.on_navigate)(entry.path);
    }
}

fn row_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        // Rows are driven from the entry and must never take the focus.
        item.set_focusable(false);
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);
        hbox.set_margin_start(6);
        hbox.set_margin_end(6);
        let icon = gtk::Image::from_icon_name("folder");
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        hbox.append(&icon);
        hbox.append(&label);
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let entry = item.item().and_downcast::<EntryObject>().unwrap().entry();
        let hbox = item.child().and_downcast::<gtk::Box>().unwrap();
        let label = hbox.last_child().and_downcast::<gtk::Label>().unwrap();
        label.set_text(&entry.name);
        label.set_tooltip_text(if entry.is_symlink {
            entry.link_target.as_deref()
        } else {
            None
        });
    });
    factory
}

/// Split the typed text into the directory to list (exactly as typed,
/// trailing slash included) and the partial name after it. Only absolute
/// paths are completed; anything else yields `None`.
fn split_typed(text: &str) -> Option<(String, String)> {
    if !text.starts_with('/') {
        return None;
    }
    let idx = text.rfind('/')?;
    Some((text[..=idx].to_owned(), text[idx + 1..].to_owned()))
}

/// The directories among `entries` whose names start with `prefix`
/// (case-insensitively), sorted by name. Hidden folders are offered only
/// when the pane shows hidden files or the prefix itself starts with a dot.
fn matching_dirs(entries: &[FsEntry], prefix: &str, show_hidden: bool) -> Vec<FsEntry> {
    let want = prefix.to_lowercase();
    let offer_hidden = show_hidden || prefix.starts_with('.');
    let mut out: Vec<FsEntry> = entries
        .iter()
        .filter(|e| e.is_dir)
        .filter(|e| offer_hidden || !e.name.starts_with('.'))
        .filter(|e| e.name.to_lowercase().starts_with(&want))
        .cloned()
        .collect();
    out.sort_by_cached_key(|e| e.name.to_lowercase());
    out
}

/// Longest prefix shared by all `names`, compared case-insensitively; the
/// characters come from the first name.
fn common_prefix(names: &[String]) -> String {
    let Some((first, rest)) = names.split_first() else {
        return String::new();
    };
    let rest: Vec<Vec<char>> = rest.iter().map(|name| name.chars().collect()).collect();
    let shared = first
        .chars()
        .enumerate()
        .take_while(|(i, ch)| {
            rest.iter().all(|other| {
                other
                    .get(*i)
                    .is_some_and(|c| c.to_lowercase().eq(ch.to_lowercase()))
            })
        })
        .count();
    first.chars().take(shared).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> FsEntry {
        FsEntry {
            name: name.into(),
            path: format!("/x/{name}"),
            is_dir,
            is_symlink: false,
            size: 0,
            mtime: None,
            mode: None,
            owner: None,
            group: None,
            link_target: None,
        }
    }

    fn names(entries: Vec<FsEntry>) -> Vec<String> {
        entries.into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn split_typed_separates_directory_and_partial_name() {
        assert_eq!(split_typed("/"), Some(("/".into(), "".into())));
        assert_eq!(split_typed("/mnt"), Some(("/".into(), "mnt".into())));
        assert_eq!(split_typed("/mnt/"), Some(("/mnt/".into(), "".into())));
        assert_eq!(split_typed("/mnt/da"), Some(("/mnt/".into(), "da".into())));
        assert_eq!(split_typed("/a/b/c/"), Some(("/a/b/c/".into(), "".into())));
        // Only absolute paths are completed.
        assert_eq!(split_typed(""), None);
        assert_eq!(split_typed("mnt/data"), None);
        assert_eq!(split_typed("~/src"), None);
    }

    #[test]
    fn matching_dirs_offers_sorted_folders_by_prefix() {
        let entries = vec![
            entry("Videos", true),
            entry("data", true),
            entry("Data2", true),
            entry("notes.txt", false),
            entry(".cache", true),
            entry(".dotfile", false),
        ];
        assert_eq!(
            names(matching_dirs(&entries, "", false)),
            ["data", "Data2", "Videos"]
        );
        assert_eq!(
            names(matching_dirs(&entries, "DA", false)),
            ["data", "Data2"]
        );
        assert_eq!(names(matching_dirs(&entries, "vid", false)), ["Videos"]);
        // Files are never offered, even when the prefix matches one.
        assert!(matching_dirs(&entries, "notes", false).is_empty());
        assert!(matching_dirs(&entries, "x", false).is_empty());
    }

    #[test]
    fn matching_dirs_hidden_folders_follow_the_pane_or_a_typed_dot() {
        let entries = vec![
            entry("data", true),
            entry(".cache", true),
            entry(".git", true),
        ];
        assert_eq!(names(matching_dirs(&entries, "", false)), ["data"]);
        assert_eq!(
            names(matching_dirs(&entries, "", true)),
            [".cache", ".git", "data"]
        );
        assert_eq!(
            names(matching_dirs(&entries, ".", false)),
            [".cache", ".git"]
        );
        assert_eq!(names(matching_dirs(&entries, ".g", false)), [".git"]);
    }

    #[test]
    fn common_prefix_is_case_insensitive_and_keeps_the_first_spelling() {
        let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            common_prefix(&strings(&["data1", "Data2", "DATABASE"])),
            "data"
        );
        assert_eq!(common_prefix(&strings(&["Data2", "data1"])), "Data");
        assert_eq!(common_prefix(&strings(&["only"])), "only");
        assert_eq!(common_prefix(&strings(&["abc", "abd", "xyz"])), "");
        assert_eq!(common_prefix(&[]), "");
    }
}
