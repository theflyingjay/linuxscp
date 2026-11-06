//! The session tab strip under the header bar: WinSCP-style fixed-width
//! tabs with the "New Session" button right after the last one, and — only
//! once there are more tabs than fit — a pair of < > arrows to flip
//! between sessions.
//!
//! The tab bar itself is sized to a fixed pitch per tab (so tabs are
//! uniform) and sits inside a horizontal scroller whose *natural* width is
//! that size but whose *minimum* is tiny. With a few tabs the strip gives
//! the bar exactly the room its tabs need, so "New Session" hugs the last
//! tab; with many, the bar takes the whole strip and scrolls, and the
//! window can still be made narrower.

use std::rc::Rc;

use adw::prelude::*;

/// Width of one session tab; the bar is sized to this per tab and the tabs
/// expand to fill it exactly.
const TAB_PITCH: i32 = 200;

pub struct TabStrip {
    pub root: gtk::Box,
    /// The "New Session" affordance after the last tab (a "+"-style tab).
    pub new_session: gtk::Button,
    tabs: adw::TabView,
    tab_bar: adw::TabBar,
    scroller: gtk::ScrolledWindow,
    /// The < > pair. Kept allocated (just invisible and inert) while the
    /// tabs fit, so showing it never changes how much room the tabs have —
    /// which would otherwise make "do they fit?" flip-flop at the edge.
    arrows: gtk::Box,
    prev: gtk::Button,
    next: gtk::Button,
}

impl TabStrip {
    pub fn new(tabs: &adw::TabView) -> Rc<Self> {
        let tab_bar = adw::TabBar::new();
        tab_bar.set_view(Some(tabs));
        tab_bar.set_autohide(false);
        tab_bar.set_hexpand(false);
        // Boxed, bordered tabs (see main.rs CSS) so they read as tabs in
        // both light and dark mode.
        tab_bar.add_css_class("session-tabs");

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_width(true)
            .hexpand(false)
            .child(&tab_bar)
            .build();

        let new_session = new_session_button();

        let prev = flip_button("go-previous-symbolic", "Previous session");
        let next = flip_button("go-next-symbolic", "Next session");
        let arrows = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        arrows.add_css_class("linked");
        arrows.set_margin_start(4);
        arrows.set_margin_end(6);
        arrows.set_margin_top(4);
        arrows.set_margin_bottom(4);
        arrows.append(&prev);
        arrows.append(&next);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("session-strip");
        root.append(&scroller);
        root.append(&new_session);
        root.append(&spacer);
        root.append(&arrows);

        let this = Rc::new(Self {
            root,
            new_session,
            tabs: tabs.clone(),
            tab_bar,
            scroller,
            arrows,
            prev,
            next,
        });

        this.update_width();
        this.update_arrows();
        {
            let weak = Rc::downgrade(&this);
            tabs.connect_n_pages_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.update_width();
                    this.update_arrows();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            tabs.connect_selected_page_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.update_arrows();
                    this.scroll_to_selected();
                }
            });
        }
        {
            // Fires when the tabs' total width or the strip's width changes
            // (new/closed tabs, window resizes): re-check whether they
            // still fit and keep the selected tab in view.
            let weak = Rc::downgrade(&this);
            this.scroller.hadjustment().connect_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.update_arrows();
                    this.scroll_to_selected();
                }
            });
        }
        {
            let tabs = tabs.clone();
            this.prev.connect_clicked(move |_| {
                tabs.select_previous_page();
            });
        }
        {
            let tabs = tabs.clone();
            this.next.connect_clicked(move |_| {
                tabs.select_next_page();
            });
        }

        this
    }

    /// Uniform tab widths: size the bar to one pitch per tab and let the
    /// tabs expand to fill it.
    fn update_width(&self) {
        let n = self.tabs.n_pages().max(1);
        self.tab_bar.set_width_request(n * TAB_PITCH);
    }

    /// True when the tabs are wider than the room the strip gives them.
    fn overflowing(&self) -> bool {
        let adj = self.scroller.hadjustment();
        adj.upper() - adj.page_size() > 1.0
    }

    /// Show the arrows only while the tabs don't fit, and disable the one
    /// that has nowhere to go.
    fn update_arrows(&self) {
        let overflowing = self.overflowing();
        self.arrows.set_opacity(if overflowing { 1.0 } else { 0.0 });
        self.arrows.set_can_target(overflowing);
        let (index, count) = match self.tabs.selected_page() {
            Some(page) => (self.tabs.page_position(&page), self.tabs.n_pages()),
            None => (0, 0),
        };
        self.prev.set_sensitive(overflowing && index > 0);
        self.next.set_sensitive(overflowing && index + 1 < count);
    }

    /// Scroll the strip so the selected tab is fully in view.
    fn scroll_to_selected(&self) {
        let Some(page) = self.tabs.selected_page() else {
            return;
        };
        let adj = self.scroller.hadjustment();
        let page_size = adj.page_size();
        if page_size <= 0.0 || adj.upper() <= page_size {
            return;
        }
        let pitch = f64::from(TAB_PITCH);
        let start = f64::from(self.tabs.page_position(&page)) * pitch;
        let end = start + pitch;
        let value = adj.value();
        let target = if start < value {
            start
        } else if end > value + page_size {
            end - page_size
        } else {
            return;
        };
        adj.set_value(target.clamp(adj.lower(), adj.upper() - page_size));
    }
}

fn flip_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    button
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
