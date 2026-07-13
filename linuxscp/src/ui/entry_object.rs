//! GObject wrapper so `FsEntry` values can live in a `gio::ListStore`.

use gtk::glib;
use gtk::subclass::prelude::*;

use linuxscp::types::FsEntry;

mod imp {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    pub struct EntryObject {
        pub entry: RefCell<Option<FsEntry>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EntryObject {
        const NAME: &'static str = "LinuxScpEntryObject";
        type Type = super::EntryObject;
    }

    impl ObjectImpl for EntryObject {}
}

glib::wrapper! {
    pub struct EntryObject(ObjectSubclass<imp::EntryObject>);
}

impl EntryObject {
    pub fn new(entry: FsEntry) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().entry.replace(Some(entry));
        obj
    }

    pub fn entry(&self) -> FsEntry {
        self.imp()
            .entry
            .borrow()
            .clone()
            .expect("EntryObject always holds an entry")
    }
}

pub fn format_size(size: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_mtime(mtime: Option<i64>) -> String {
    let Some(mtime) = mtime else {
        return String::new();
    };
    glib::DateTime::from_unix_local(mtime)
        .and_then(|dt| dt.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}
