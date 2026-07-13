//! GObject node backing the site-manager tree (folders and sites). `name`
//! and `subtitle` are real GObject properties so list rows bound with
//! property expressions update live while the user types in the site form.

use gtk::glib;
use gtk::subclass::prelude::*;

mod imp {
    use std::cell::{Cell, RefCell};

    use gtk::glib::Properties;
    use gtk::prelude::*;

    use super::*;

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::TreeNode)]
    pub struct TreeNode {
        pub id: RefCell<String>,
        #[property(get, set)]
        pub name: RefCell<String>,
        #[property(get, set)]
        pub subtitle: RefCell<String>,
        /// Box-drawing guide drawn before the icon (tree connector lines).
        pub prefix: RefCell<String>,
        pub is_folder: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TreeNode {
        const NAME: &'static str = "LinuxScpTreeNode";
        type Type = super::TreeNode;
    }

    #[glib::derived_properties]
    impl ObjectImpl for TreeNode {}
}

glib::wrapper! {
    pub struct TreeNode(ObjectSubclass<imp::TreeNode>);
}

impl TreeNode {
    pub fn folder(id: &str, name: &str, prefix: &str, site_count: usize) -> Self {
        let obj = Self::base(id, name, prefix);
        obj.imp().subtitle.replace(match site_count {
            1 => "1 site".to_string(),
            n => format!("{n} sites"),
        });
        obj.imp().is_folder.set(true);
        obj
    }

    pub fn site(id: &str, name: &str, prefix: &str, subtitle: &str) -> Self {
        let obj = Self::base(id, name, prefix);
        obj.imp().subtitle.replace(subtitle.to_owned());
        obj.imp().is_folder.set(false);
        obj
    }

    fn base(id: &str, name: &str, prefix: &str) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.id.replace(id.to_owned());
        imp.name.replace(name.to_owned());
        imp.prefix.replace(prefix.to_owned());
        obj
    }

    pub fn id(&self) -> String {
        self.imp().id.borrow().clone()
    }

    pub fn prefix(&self) -> String {
        self.imp().prefix.borrow().clone()
    }

    pub fn is_folder(&self) -> bool {
        self.imp().is_folder.get()
    }
}
