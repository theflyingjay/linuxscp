//! Site manager: a WinSCP-style login dialog in three columns. Left: the
//! hosts discovered from ~/.ssh/config (editable in-app). Middle: saved
//! sites (a folder tree, created and edited in-app, with credentials in the
//! OS keyring). Right: the selected site's details, edited inline —
//! creating a new site adds a draft row to the list whose name follows the
//! form as you type, instead of opening a separate modal. A single search
//! box filters the hosts, the folders, and the sites together.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::config_editor;
use super::site_editor::{SecretUpdate, SiteForm};
use super::tree_node::TreeNode;
use linuxscp::settings::{Folder, Site};
use linuxscp::ssh::config::{KnownHost, known_hosts};
use linuxscp::types::ConnectSpec;

/// Callbacks the dialog uses to talk back to the app. All site mutations go
/// through the app so it owns persistence and the keyring.
pub struct SiteManagerHandlers {
    /// Current site tree (re-fetched after each mutation).
    pub tree: Box<dyn Fn() -> Folder>,
    pub connect: Box<dyn Fn(ConnectSpec)>,
    /// Save a new or edited site under the given parent folder id.
    pub save_site: Box<dyn Fn(Site, SecretUpdate, String)>,
    pub delete_site: Box<dyn Fn(String)>,
    pub add_folder: Box<dyn Fn(String, String)>,
    /// Move a site (by id) into a folder (by id; "" = root).
    pub move_site: Box<dyn Fn(String, String)>,
    /// Move a folder (by id), subtree included, into a folder ("" = root).
    pub move_folder: Box<dyn Fn(String, String)>,
    pub delete_folder: Box<dyn Fn(String)>,
    /// Build a spec for a saved site id (loads its secret from the keyring).
    pub spec_for_site: Box<dyn Fn(String) -> Option<ConnectSpec>>,
}

pub fn show(parent: &impl IsA<gtk::Widget>, handlers: SiteManagerHandlers) {
    let parent = parent.clone().upcast::<gtk::Widget>();
    let handlers = Rc::new(handlers);

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    let title = adw::WindowTitle::new("Site Manager", "Saved connections");
    header.set_title_widget(Some(&title));
    let close_button = gtk::Button::with_label("Close");
    header.pack_start(&close_button);
    toolbar.add_top_bar(&header);

    // Wide, WinSCP-style: the two list columns are fixed, so every extra
    // pixel goes to the details form on the right.
    let dialog = adw::Dialog::builder()
        .title("Site Manager")
        .content_width(1240)
        .content_height(690)
        .child(&toolbar)
        .build();
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| {
            dialog.close();
        });
    }

    // --- Shared state ---------------------------------------------------
    // Folder new sites/subfolders are created under, following the tree
    // selection (root when nothing folder-like is selected).
    let current_parent = Rc::new(RefCell::new(String::new()));
    let selected_site = Rc::new(RefCell::new(Option::<String>::None));
    let selected_folder = Rc::new(RefCell::new(Option::<String>::None));
    // Parent captured when a draft ("New site") row is opened.
    let draft_parent = Rc::new(RefCell::new(String::new()));
    // Lowercased search text applied to the tree and the ssh host list.
    let query = Rc::new(RefCell::new(String::new()));
    // Suppresses the selection handler during programmatic store updates.
    let syncing = Rc::new(Cell::new(false));

    // --- Top strip: one search box filtering hosts, folders, and sites ---
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search hosts, sites, and folders")
        .hexpand(true)
        .build();
    search.set_margin_top(10);
    search.set_margin_start(12);
    search.set_margin_end(12);

    // --- Left column: ~/.ssh/config hosts (full height, editable) --------
    let hosts_col = build_hosts_column(&dialog, &handlers, &query);

    // --- Middle column: saved sites tree with actions ---------------------
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add_site_btn = icon_button("list-add-symbolic", "New site");
    add_site_btn.add_css_class("suggested-action");
    let add_folder_btn = icon_button("folder-new-symbolic", "New folder");
    let delete_btn = icon_button("user-trash-symbolic", "Delete");
    delete_btn.set_sensitive(false);
    let sites_heading = gtk::Label::new(Some("Saved sites"));
    sites_heading.add_css_class("heading");
    sites_heading.set_xalign(0.0);
    sites_heading.set_hexpand(true);
    actions.append(&sites_heading);
    actions.append(&add_site_btn);
    actions.append(&add_folder_btn);
    actions.append(&delete_btn);

    let tree_store = gio::ListStore::new::<TreeNode>();
    let selection = gtk::SingleSelection::new(Some(tree_store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    selection.set_selected(gtk::INVALID_LIST_POSITION);

    // Drop handler slot: rows are built before `rebuild` exists, so the
    // actual move logic is filled in later. Args: drag payload
    // ("site:<id>" or "folder:<id>"), target row (None = the list
    // background, meaning the root).
    type MoveHandler = Box<dyn Fn(String, Option<TreeNode>)>;
    let on_move: Rc<RefCell<Option<MoveHandler>>> = Rc::new(RefCell::new(None));

    // Single-line rows (like the file lists) so large site collections stay
    // dense; the user@host summary is shown as a tooltip instead.
    let factory = gtk::SignalListItemFactory::new();
    {
        let on_move = on_move.clone();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            // Monospace guide so the box-drawing lines align across rows.
            let guide = gtk::Label::builder().xalign(0.0).build();
            guide.add_css_class("monospace");
            guide.add_css_class("dim-label");
            let icon = gtk::Image::new();
            icon.set_margin_end(4);
            let title = gtk::Label::builder().xalign(0.0).build();
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            row.append(&guide);
            row.append(&icon);
            row.append(&title);
            item.set_child(Some(&row));

            // The name is property-bound so the draft row follows the form
            // live while the user types.
            let node = item.property_expression("item");
            node.chain_property::<TreeNode>("name")
                .bind(&title, "label", gtk::Widget::NONE);

            // Sites and folders can be dragged (a folder takes its whole
            // subtree with it)…
            let drag = gtk::DragSource::new();
            drag.set_actions(gtk::gdk::DragAction::MOVE);
            {
                let item = item.downgrade();
                drag.connect_prepare(move |_, _, _| {
                    let node = item.upgrade()?.item().and_downcast::<TreeNode>()?;
                    if node.id().is_empty() {
                        return None; // the unsaved draft stays put
                    }
                    let kind = if node.is_folder() { "folder" } else { "site" };
                    Some(gtk::gdk::ContentProvider::for_value(
                        &format!("{kind}:{}", node.id()).to_value(),
                    ))
                });
            }
            {
                let row = row.clone();
                drag.connect_drag_begin(move |source, _| {
                    source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&row))), 0, 12);
                });
            }
            row.add_controller(drag);

            // …and dropped on any row: a folder means into that folder, a
            // site means next to it (its containing folder).
            let target =
                gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
            {
                let item = item.downgrade();
                let on_move = on_move.clone();
                target.connect_drop(move |_, value, _, _| {
                    let Ok(site_id) = value.get::<String>() else {
                        return false;
                    };
                    let Some(item) = item.upgrade() else {
                        return false;
                    };
                    let node = item.item().and_downcast::<TreeNode>();
                    match on_move.borrow().as_ref() {
                        Some(cb) => {
                            cb(site_id, node);
                            true
                        }
                        None => false,
                    }
                });
            }
            row.add_controller(target);
        });
    }
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let node = item.item().and_downcast::<TreeNode>().unwrap();
        let row = item.child().and_downcast::<gtk::Box>().unwrap();
        let guide = row.first_child().and_downcast::<gtk::Label>().unwrap();
        let icon = guide.next_sibling().and_downcast::<gtk::Image>().unwrap();

        guide.set_label(&node.prefix());
        guide.set_visible(!node.prefix().is_empty());
        icon.set_icon_name(Some(if node.is_folder() {
            "folder-symbolic"
        } else {
            "network-server-symbolic"
        }));
        let subtitle = node.subtitle();
        row.set_tooltip_text((!subtitle.is_empty()).then_some(subtitle.as_str()));
    });

    let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory));
    list_view.add_css_class("navigation-sidebar");
    list_view.add_css_class("compact-list");
    // Dropping onto empty list space moves the site to the root level.
    {
        let root_target =
            gtk::DropTarget::new(glib::types::Type::STRING, gtk::gdk::DragAction::MOVE);
        let on_move = on_move.clone();
        root_target.connect_drop(move |_, value, _, _| {
            let Ok(site_id) = value.get::<String>() else {
                return false;
            };
            match on_move.borrow().as_ref() {
                Some(cb) => {
                    cb(site_id, None);
                    true
                }
                None => false,
            }
        });
        list_view.add_controller(root_target);
    }
    let tree_scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(220)
        .has_frame(true)
        .child(&list_view)
        .build();

    let sites_col = gtk::Box::new(gtk::Orientation::Vertical, 8);
    sites_col.set_margin_top(12);
    sites_col.set_margin_bottom(12);
    sites_col.set_margin_start(12);
    sites_col.set_margin_end(12);
    sites_col.set_size_request(300, -1);
    // Fixed width: extra dialog space belongs to the details form, not the
    // lists (heading hexpand would otherwise propagate up and split it).
    sites_col.set_hexpand(false);
    sites_col.append(&actions);
    sites_col.append(&tree_scroller);

    // --- Right column: placeholder / inline site editor ------------------
    let form = SiteForm::new();
    form.root.set_margin_top(12);
    form.root.set_margin_bottom(12);
    form.root.set_margin_start(12);
    form.root.set_margin_end(12);
    let form_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&form.root)
        .build();

    let save_btn = gtk::Button::with_label("Save");
    let connect_btn = gtk::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    buttons.set_margin_bottom(12);
    buttons.set_margin_start(12);
    buttons.set_margin_end(12);
    buttons.append(&save_btn);
    buttons.append(&connect_btn);

    let form_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    form_page.append(&form_scroller);
    form_page.append(&buttons);

    let placeholder = adw::StatusPage::builder()
        .icon_name("network-server-symbolic")
        .title("No Site Selected")
        .description("Select a saved site, or create a new one to edit it here.")
        .build();
    placeholder.add_css_class("compact");

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&form_page, Some("form"));
    stack.set_visible_child_name("empty");

    let columns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    columns.set_vexpand(true);
    columns.append(&hosts_col.root);
    columns.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    columns.append(&sites_col);
    columns.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    columns.append(&stack);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&search);
    content.append(&columns);
    toolbar.set_content(Some(&content));

    // --- Selection → right pane ------------------------------------------
    // Single source of truth for what a (de)selection means. Also drops the
    // draft row whenever the selection moves off it.
    let apply_selection: Rc<dyn Fn(Option<TreeNode>)> = {
        let handlers = handlers.clone();
        let current_parent = current_parent.clone();
        let selected_site = selected_site.clone();
        let selected_folder = selected_folder.clone();
        let delete_btn = delete_btn.clone();
        let stack = stack.clone();
        let form = form.clone();
        let tree_store = tree_store.clone();
        let syncing = syncing.clone();
        Rc::new(move |node: Option<TreeNode>| {
            let keeps_draft = node
                .as_ref()
                .is_some_and(|n| !n.is_folder() && n.id().is_empty());
            let stale_draft = if keeps_draft {
                None
            } else {
                draft_position(&tree_store)
            };
            if let Some(pos) = stale_draft {
                syncing.set(true);
                tree_store.remove(pos);
                syncing.set(false);
            }
            match node {
                Some(node) if node.is_folder() => {
                    *current_parent.borrow_mut() = node.id();
                    *selected_folder.borrow_mut() = Some(node.id());
                    *selected_site.borrow_mut() = None;
                    delete_btn.set_sensitive(true);
                    stack.set_visible_child_name("empty");
                }
                Some(node) if node.id().is_empty() => {
                    // The unsaved draft; the form already holds its values.
                    *selected_site.borrow_mut() = None;
                    *selected_folder.borrow_mut() = None;
                    delete_btn.set_sensitive(true);
                    stack.set_visible_child_name("form");
                }
                Some(node) => {
                    // A saved site: new items go into its containing folder.
                    let root = (handlers.tree)();
                    *current_parent.borrow_mut() =
                        root.parent_id_of(&node.id()).unwrap_or_default();
                    *selected_site.borrow_mut() = Some(node.id());
                    *selected_folder.borrow_mut() = None;
                    delete_btn.set_sensitive(true);
                    match root.all_sites().into_iter().find(|s| s.id == node.id()) {
                        Some(site) => {
                            form.load(Some(site));
                            stack.set_visible_child_name("form");
                        }
                        None => stack.set_visible_child_name("empty"),
                    }
                }
                None => {
                    current_parent.borrow_mut().clear();
                    *selected_site.borrow_mut() = None;
                    *selected_folder.borrow_mut() = None;
                    delete_btn.set_sensitive(false);
                    stack.set_visible_child_name("empty");
                }
            }
        })
    };

    // Rebuilds the tree store from settings + search query, then restores
    // the selection to `select_id` when that row is still visible.
    let rebuild: Rc<dyn Fn(Option<String>)> = {
        let handlers = handlers.clone();
        let tree_store = tree_store.clone();
        let selection = selection.clone();
        let list_view = list_view.clone();
        let query = query.clone();
        let syncing = syncing.clone();
        let apply_selection = apply_selection.clone();
        Rc::new(move |select_id: Option<String>| {
            let pruned = (handlers.tree)().filtered(&query.borrow());
            let nodes = build_tree_model(&pruned);
            syncing.set(true);
            populate_store(&tree_store, &nodes);
            let target = select_id
                .and_then(|id| nodes.iter().position(|n| n.id() == id))
                .map(|i| i as u32);
            selection.set_selected(target.unwrap_or(gtk::INVALID_LIST_POSITION));
            syncing.set(false);
            if let Some(pos) = target {
                list_view.scroll_to(pos, gtk::ListScrollFlags::NONE, None);
            }
            apply_selection(selection.selected_item().and_downcast::<TreeNode>());
        })
    };

    // Drag & drop: move the dragged site or folder into the target folder
    // (dropping onto a site targets its containing folder; the background
    // = root), then rebuild with the moved row kept selected. Impossible
    // folder moves (into themselves or their own subtree) are no-ops.
    {
        let handlers = handlers.clone();
        let rebuild = rebuild.clone();
        *on_move.borrow_mut() = Some(Box::new(move |payload, target| {
            let Some((kind, id)) = payload.split_once(':') else {
                return;
            };
            let dest = match &target {
                None => String::new(),
                Some(node) if node.is_folder() => node.id(),
                Some(node) => (handlers.tree)()
                    .parent_id_of(&node.id())
                    .unwrap_or_default(),
            };
            match kind {
                "folder" => (handlers.move_folder)(id.to_string(), dest),
                _ => (handlers.move_site)(id.to_string(), dest),
            }
            rebuild(Some(id.to_string()));
        }));
    }

    {
        let syncing = syncing.clone();
        let apply_selection = apply_selection.clone();
        selection.connect_selection_changed(move |sel, _, _| {
            if syncing.get() {
                return;
            }
            apply_selection(sel.selected_item().and_downcast::<TreeNode>());
        });
    }

    // Mirror the form's name/host fields into the draft row as you type
    // (the row labels are property-bound to the node).
    {
        let selection = selection.clone();
        form.set_on_changed(move |name, summary| {
            let Some(node) = selection.selected_item().and_downcast::<TreeNode>() else {
                return;
            };
            if node.is_folder() || !node.id().is_empty() {
                return; // only the draft row tracks unsaved edits
            }
            node.set_name(name);
            node.set_subtitle(summary);
        });
    }

    // Connect on double-click / Enter (saved values, like before).
    {
        let handlers = handlers.clone();
        let dialog = dialog.clone();
        list_view.connect_activate(move |list, position| {
            let node = list
                .model()
                .and_then(|m| m.item(position))
                .and_downcast::<TreeNode>();
            let spec = node
                .filter(|node| !node.is_folder())
                .and_then(|node| (handlers.spec_for_site)(node.id()));
            if let Some(spec) = spec {
                dialog.close();
                (handlers.connect)(spec);
            }
        });
    }

    // New site: add a draft row at the top and edit it on the right.
    {
        let tree_store = tree_store.clone();
        let selection = selection.clone();
        let list_view = list_view.clone();
        let syncing = syncing.clone();
        let current_parent = current_parent.clone();
        let draft_parent = draft_parent.clone();
        let form = form.clone();
        let apply_selection = apply_selection.clone();
        add_site_btn.connect_clicked(move |_| {
            if let Some(pos) = draft_position(&tree_store) {
                selection.set_selected(pos);
                list_view.scroll_to(pos, gtk::ListScrollFlags::NONE, None);
                form.focus_name();
                return;
            }
            *draft_parent.borrow_mut() = current_parent.borrow().clone();
            form.load(None);
            let node = TreeNode::site("", "New site", "", "");
            syncing.set(true);
            tree_store.insert(0, &node);
            selection.set_selected(0);
            syncing.set(false);
            list_view.scroll_to(0, gtk::ListScrollFlags::NONE, None);
            apply_selection(Some(node));
            form.focus_name();
        });
    }

    // Save the form: creates the draft under its captured parent folder, or
    // updates the selected site in place.
    {
        let handlers = handlers.clone();
        let selection = selection.clone();
        let draft_parent = draft_parent.clone();
        let form = form.clone();
        let rebuild = rebuild.clone();
        save_btn.connect_clicked(move |_| {
            let Some((site, secret)) = form.collect() else {
                return;
            };
            let is_draft = selection
                .selected_item()
                .and_downcast::<TreeNode>()
                .is_some_and(|n| !n.is_folder() && n.id().is_empty());
            let parent_id = if is_draft {
                draft_parent.borrow().clone()
            } else {
                (handlers.tree)().parent_id_of(&site.id).unwrap_or_default()
            };
            (handlers.save_site)(site.clone(), secret, parent_id);
            rebuild(Some(site.id));
        });
    }

    // Connect with the form's current values (saving is not required). A
    // blank secret field falls back to the keyring entry, if any.
    {
        let handlers = handlers.clone();
        let dialog = dialog.clone();
        let form = form.clone();
        connect_btn.connect_clicked(move |_| {
            let Some((site, _secret)) = form.collect() else {
                return;
            };
            let mut spec = site.to_spec();
            let typed = form.secret_text();
            if !typed.is_empty() {
                spec.secret = Some(typed);
            } else if site.has_secret {
                spec.secret = (handlers.spec_for_site)(site.id.clone()).and_then(|s| s.secret);
            }
            dialog.close();
            (handlers.connect)(spec);
        });
    }

    // New folder.
    {
        let handlers = handlers.clone();
        let rebuild = rebuild.clone();
        let current_parent = current_parent.clone();
        let selected_site = selected_site.clone();
        let selected_folder = selected_folder.clone();
        let parent = parent.clone();
        add_folder_btn.connect_clicked(move |_| {
            let handlers = handlers.clone();
            let rebuild = rebuild.clone();
            let selected_site = selected_site.clone();
            let selected_folder = selected_folder.clone();
            let parent_id = current_parent.borrow().clone();
            super::prompts::ask_text(&parent, "New Folder", "", "Create", move |name| {
                (handlers.add_folder)(parent_id.clone(), name);
                let keep = selected_site
                    .borrow()
                    .clone()
                    .or_else(|| selected_folder.borrow().clone());
                rebuild(keep);
            });
        });
    }

    // Delete the selected site or folder (or just discard the draft),
    // shared by the trash button and the Delete key on the tree.
    let delete_selected: Rc<dyn Fn()> = {
        let handlers = handlers.clone();
        let rebuild = rebuild.clone();
        let selection = selection.clone();
        let selected_site = selected_site.clone();
        let selected_folder = selected_folder.clone();
        let parent = parent.clone();
        Rc::new(move || {
            let draft_selected = selection
                .selected_item()
                .and_downcast::<TreeNode>()
                .is_some_and(|n| !n.is_folder() && n.id().is_empty());
            if draft_selected {
                rebuild(None);
                return;
            }
            if let Some(id) = selected_site.borrow().clone() {
                let handlers = handlers.clone();
                let rebuild = rebuild.clone();
                super::prompts::confirm(
                    &parent,
                    "Delete Site",
                    "Delete this saved site and its stored password?",
                    "Delete",
                    move || {
                        (handlers.delete_site)(id.clone());
                        rebuild(None);
                    },
                );
            } else if let Some(id) = selected_folder.borrow().clone() {
                let handlers = handlers.clone();
                let rebuild = rebuild.clone();
                super::prompts::confirm(
                    &parent,
                    "Delete Folder",
                    "Delete this folder and every site inside it?",
                    "Delete",
                    move || {
                        (handlers.delete_folder)(id.clone());
                        rebuild(None);
                    },
                );
            }
        })
    };
    {
        let delete_selected = delete_selected.clone();
        delete_btn.connect_clicked(move |_| delete_selected());
    }
    // Delete key on the tree (only when the list has focus, so typing in
    // the form or the search box is unaffected).
    {
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            use gtk::gdk::Key;
            if matches!(key, Key::Delete | Key::KP_Delete) {
                delete_selected();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        list_view.add_controller(keys);
    }

    // Edit ~/.ssh/config in-app; the host list re-parses after a save.
    {
        let hosts_col = hosts_col.clone();
        let query = query.clone();
        let parent = parent.clone();
        hosts_col.edit_btn.clone().connect_clicked(move |_| {
            let hosts_col = hosts_col.clone();
            let query = query.clone();
            config_editor::show(&parent, move || {
                hosts_col.refresh(&query.borrow());
            });
        });
    }

    // Search filters the host list and the saved-site tree together.
    {
        let query = query.clone();
        let rebuild = rebuild.clone();
        let selected_site = selected_site.clone();
        let selected_folder = selected_folder.clone();
        let hosts_col = hosts_col.clone();
        search.connect_search_changed(move |entry| {
            *query.borrow_mut() = entry.text().to_lowercase();
            let keep = selected_site
                .borrow()
                .clone()
                .or_else(|| selected_folder.borrow().clone());
            rebuild(keep);
            hosts_col.apply_filter(&query.borrow());
        });
    }

    rebuild(None);
    dialog.present(Some(&parent));
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button
}

/// Position of the unsaved "New site" draft row (empty id), if present.
fn draft_position(store: &gio::ListStore) -> Option<u32> {
    (0..store.n_items()).find(|&i| {
        store
            .item(i)
            .and_downcast::<TreeNode>()
            .is_some_and(|n| !n.is_folder() && n.id().is_empty())
    })
}

/// The ~/.ssh/config hosts column: a full-height list with an edit button
/// that opens the in-app config editor, reloadable after saves.
#[derive(Clone)]
struct HostsColumn {
    root: gtk::Box,
    list: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    empty: gtk::Label,
    edit_btn: gtk::Button,
    hosts: Rc<RefCell<Vec<KnownHost>>>,
}

impl HostsColumn {
    /// Re-parse ~/.ssh/config and rebuild the rows. Rows are a single line
    /// of text (like the file lists) so long configs stay scannable; the
    /// user@host detail lives in the tooltip.
    fn refresh(&self, query: &str) {
        *self.hosts.borrow_mut() = known_hosts();
        self.list.remove_all();
        for host in self.hosts.borrow().iter() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let icon = gtk::Image::from_icon_name("network-workgroup-symbolic");
            let label = gtk::Label::new(Some(&host.alias));
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            row.append(&icon);
            row.append(&label);
            let detail = host.subtitle();
            if !detail.is_empty() {
                row.set_tooltip_text(Some(&detail));
            }
            self.list.append(&row);
        }
        self.apply_filter(query);
    }

    /// Re-run the search filter and toggle the empty-state message.
    fn apply_filter(&self, query: &str) {
        self.list.invalidate_filter();
        let hosts = self.hosts.borrow();
        let any = hosts.iter().any(|h| h.matches(query));
        self.scroller.set_visible(any);
        self.empty.set_visible(!any);
        self.empty.set_text(if hosts.is_empty() {
            "No Host entries yet.\n\nUse the edit button above to add some to ~/.ssh/config."
        } else {
            "No hosts match the search."
        });
    }
}

fn build_hosts_column(
    dialog: &adw::Dialog,
    handlers: &Rc<SiteManagerHandlers>,
    query: &Rc<RefCell<String>>,
) -> HostsColumn {
    let heading = gtk::Label::new(Some("Hosts from ~/.ssh/config"));
    heading.set_xalign(0.0);
    heading.set_hexpand(true);
    heading.add_css_class("heading");
    let edit_btn = gtk::Button::from_icon_name("document-edit-symbolic");
    edit_btn.set_tooltip_text(Some("Edit ~/.ssh/config"));
    let heading_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    heading_row.append(&heading);
    heading_row.append(&edit_btn);

    let hosts = Rc::new(RefCell::new(Vec::<KnownHost>::new()));
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.add_css_class("compact-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    {
        let hosts = hosts.clone();
        let query = query.clone();
        list.set_filter_func(move |row| {
            let index = row.index();
            if index < 0 {
                return true;
            }
            match hosts.borrow().get(index as usize) {
                Some(host) => host.matches(&query.borrow()),
                None => true,
            }
        });
    }
    {
        let hosts = hosts.clone();
        let dialog = dialog.clone();
        let handlers = handlers.clone();
        list.connect_row_activated(move |_, row| {
            let Some(host) = hosts.borrow().get(row.index() as usize).cloned() else {
                return;
            };
            let spec = ConnectSpec::new(host.alias);
            dialog.close();
            (handlers.connect)(spec);
        });
    }
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .has_frame(true)
        .child(&list)
        .build();

    let empty = gtk::Label::new(None);
    empty.add_css_class("dim-label");
    empty.set_wrap(true);
    empty.set_justify(gtk::Justification::Center);
    empty.set_vexpand(true);
    empty.set_valign(gtk::Align::Center);
    empty.set_max_width_chars(28);
    empty.set_visible(false);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_size_request(270, -1);
    // Keep the hosts list at its fixed width (see sites_col).
    root.set_hexpand(false);
    root.append(&heading_row);
    root.append(&scroller);
    root.append(&empty);

    let column = HostsColumn {
        root,
        list,
        scroller,
        empty,
        edit_btn,
        hosts,
    };
    column.refresh(&query.borrow());
    column
}

/// Flatten the folder tree into display rows with box-drawing guide lines
/// that connect each folder to its children (WinSCP / `tree` style). Folders
/// come first at each level, then sites. `ancestors[k]` is true when the
/// ancestor folder at that level has more siblings below it, so a vertical
/// bar continues through this row.
fn build_tree_model(root: &Folder) -> Vec<TreeNode> {
    fn guide(ancestors: &[bool], is_last: bool) -> String {
        let mut s = String::new();
        for &more in ancestors {
            s.push_str(if more {
                "\u{2502}\u{a0}\u{a0}"
            } else {
                "\u{a0}\u{a0}\u{a0}"
            });
        }
        s.push_str(if is_last {
            "\u{2514}\u{2500}\u{a0}" // └─
        } else {
            "\u{251c}\u{2500}\u{a0}" // ├─
        });
        s
    }

    // `top` items (direct children of the invisible root) get no guide, so
    // lines only appear where there is a visible parent folder to connect to.
    fn walk(folder: &Folder, ancestors: &[bool], top: bool, out: &mut Vec<TreeNode>) {
        let total = folder.folders.len() + folder.sites.len();
        let mut index = 0;
        for sub in &folder.folders {
            let is_last = index + 1 == total;
            let prefix = if top {
                String::new()
            } else {
                guide(ancestors, is_last)
            };
            out.push(TreeNode::folder(
                &sub.id,
                &sub.name,
                &prefix,
                sub.site_count(),
            ));
            let mut child_ancestors: Vec<bool> = if top { Vec::new() } else { ancestors.to_vec() };
            if !top {
                child_ancestors.push(!is_last);
            }
            walk(sub, &child_ancestors, false, out);
            index += 1;
        }
        for site in &folder.sites {
            let is_last = index + 1 == total;
            let prefix = if top {
                String::new()
            } else {
                guide(ancestors, is_last)
            };
            out.push(TreeNode::site(
                &site.id,
                &site.name,
                &prefix,
                &site.summary(),
            ));
            index += 1;
        }
    }

    let mut out = Vec::new();
    walk(root, &[], true, &mut out);
    out
}

fn populate_store(store: &gio::ListStore, nodes: &[TreeNode]) {
    store.remove_all();
    for node in nodes {
        store.append(node);
    }
}
