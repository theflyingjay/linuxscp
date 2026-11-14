//! One dual-pane session shown in a tab: local browser on the left, a
//! (possibly remote) browser on the right, exactly like the old single
//! window. The app keeps one of these per tab and routes actions to the
//! active tab's active pane.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::pane::Pane;

pub struct Workspace {
    /// The dual-pane widget handed to the tab page.
    pub root: gtk::Paned,
    pub left: Rc<Pane>,
    pub right: Rc<Pane>,
    /// true = the left pane is the active (source) pane in this tab.
    left_active: Cell<bool>,
    /// Tab page backing this workspace (set once it is added to the view).
    page: RefCell<Option<adw::TabPage>>,
    /// Title shown while no remote session is connected (e.g. "My Server").
    default_title: RefCell<String>,
}

impl Workspace {
    pub fn new(default_title: &str, left_width: Option<i32>) -> Rc<Self> {
        let left = Pane::new();
        let right = Pane::new();
        let root = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&left.root)
            .end_child(&right.root)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .build();
        if let Some(width) = left_width {
            root.set_position(width);
        }
        Rc::new(Self {
            root,
            left,
            right,
            left_active: Cell::new(true),
            page: RefCell::new(None),
            default_title: RefCell::new(default_title.to_owned()),
        })
    }

    pub fn set_page(&self, page: adw::TabPage) {
        *self.page.borrow_mut() = Some(page);
    }

    pub fn page(&self) -> Option<adw::TabPage> {
        self.page.borrow().clone()
    }

    pub fn set_left_active(&self, left_active: bool) {
        self.left_active.set(left_active);
    }

    pub fn active_pane(&self) -> Rc<Pane> {
        if self.left_active.get() {
            self.left.clone()
        } else {
            self.right.clone()
        }
    }

    pub fn inactive_pane(&self) -> Rc<Pane> {
        if self.left_active.get() {
            self.right.clone()
        } else {
            self.left.clone()
        }
    }

    pub fn panes(&self) -> [Rc<Pane>; 2] {
        [self.left.clone(), self.right.clone()]
    }

    /// True if `pane` is one of this workspace's two panes.
    pub fn has_pane(&self, pane: &Rc<Pane>) -> bool {
        Rc::ptr_eq(&self.left, pane) || Rc::ptr_eq(&self.right, pane)
    }

    pub fn default_title(&self) -> String {
        self.default_title.borrow().clone()
    }
}
