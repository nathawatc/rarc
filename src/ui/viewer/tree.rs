use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use super::models::{
    path_basename, refresh_visible_store, FolderTreeNode, NavFn, NavMode, NavWeak, ViewerState,
};
use super::ViewerWidgets;

pub(super) struct TreeSidebar {
    pub root_store: gio::ListStore,
    pub model: gtk::TreeListModel,
    pub selection: gtk::SingleSelection,
    pub view: gtk::ListView,
    pub scroll: gtk::ScrolledWindow,
}

pub(super) fn build_tree_sidebar(state: &Rc<RefCell<ViewerState>>) -> TreeSidebar {
    let tree_root_store = gio::ListStore::new::<FolderTreeNode>();
    let state_tree = state.clone();
    let tree_model = gtk::TreeListModel::new(tree_root_store.clone(), false, false, move |obj| {
        let node = obj.downcast_ref::<FolderTreeNode>()?;
        let st = state_tree.borrow();
        let children = st.dir_children.get(node.full_path())?;
        if children.is_empty() {
            return None;
        }
        let store = gio::ListStore::new::<FolderTreeNode>();
        let nodes: Vec<FolderTreeNode> = children
            .iter()
            .map(|child_path| {
                FolderTreeNode::new(path_basename(child_path).to_string(), child_path.clone())
            })
            .collect();
        store.extend_from_slice(&nodes);
        Some(store.upcast())
    });
    let tree_selection = gtk::SingleSelection::new(Some(tree_model.clone()));
    tree_selection.set_autoselect(false);

    let tree_factory = gtk::SignalListItemFactory::new();
    tree_factory.connect_setup(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let expander = gtk::TreeExpander::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = gtk::Image::from_icon_name("folder-symbolic");
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        hbox.append(&icon);
        hbox.append(&label);
        expander.set_child(Some(&hbox));
        item.set_child(Some(&expander));
    });
    tree_factory.connect_bind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let list_row = item.item().and_downcast::<gtk::TreeListRow>().unwrap();
        let node = list_row.item().and_downcast::<FolderTreeNode>().unwrap();
        let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
        expander.set_list_row(Some(&list_row));
        let hbox = expander.child().and_downcast::<gtk::Box>().unwrap();
        let icon = hbox.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
        let full_path = node.full_path();
        icon.set_icon_name(Some(if full_path.is_empty() {
            "package-x-generic"
        } else {
            "folder-symbolic"
        }));
        label.set_text(node.name());
    });
    tree_factory.connect_unbind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        if let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() {
            expander.set_list_row(None);
        }
    });

    let tree_view = gtk::ListView::new(Some(tree_selection.clone()), Some(tree_factory));
    {
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let sel_key = tree_selection.clone();
        let tv_key = tree_view.clone();
        key_ctrl.connect_key_pressed(move |_, key, _, _| {
            let Some(row) = sel_key.selected_item().and_downcast::<gtk::TreeListRow>() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Right | gdk::Key::KP_Right => {
                    let pos = row.position();
                    if row.is_expandable() && !row.is_expanded() {
                        row.set_expanded(true);
                        tv_key.scroll_to(
                            pos,
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                    if row.is_expanded() && row.children().is_some_and(|c| c.n_items() > 0) {
                        tv_key.scroll_to(
                            pos + 1,
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                }
                gdk::Key::Left | gdk::Key::KP_Left => {
                    if row.is_expanded() {
                        let pos = row.position();
                        row.set_expanded(false);
                        tv_key.scroll_to(
                            pos,
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                    if let Some(parent) = row.parent() {
                        tv_key.scroll_to(
                            parent.position(),
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        tree_view.add_controller(key_ctrl);
    }

    let tree_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_width(180)
        .child(&tree_view)
        .build();
    tree_scroll.add_css_class("rarc-tree-sidebar");

    TreeSidebar {
        root_store: tree_root_store,
        model: tree_model,
        selection: tree_selection,
        view: tree_view,
        scroll: tree_scroll,
    }
}

pub(super) fn make_navigate_to(
    widgets: &ViewerWidgets,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
    tree: &TreeSidebar,
) -> NavFn {
    let state_ref = state.clone();
    let crumbs_box = widgets.crumbs_box.clone();
    let up_btn = widgets.up_btn.clone();
    let store_ref = store.clone();
    let search_ref = widgets.search_entry.clone();
    let selection_ref = selection.clone();
    let tree_view_ref = tree.view.clone();
    let tree_model_ref = tree.model.clone();
    let tree_sel_ref = tree.selection.clone();

    Rc::new_cyclic(|nav_weak| {
        let nav_weak = nav_weak.clone();
        Box::new(move |target_dir: String, sync_tree: bool, mode: NavMode| {
            {
                let mut st = state_ref.borrow_mut();
                match mode {
                    NavMode::Push => {
                        if st.current_dir != target_dir {
                            let prev = st.current_dir.clone();
                            st.history_back.push(prev);
                            st.history_forward.clear();
                        }
                    }
                    NavMode::History => {}
                    NavMode::Reset => {
                        st.history_back.clear();
                        st.history_forward.clear();
                    }
                }
                st.current_dir = target_dir.clone();
                st.search_query.clear();
            }
            if !search_ref.text().is_empty() {
                search_ref.set_text("");
            }
            selection_ref.unselect_all();
            refresh_visible_store(&store_ref, &state_ref.borrow());

            if sync_tree {
                state_ref.borrow_mut().syncing_tree = true;
                sync_tree_selection(&tree_view_ref, &tree_model_ref, &tree_sel_ref, &target_dir);
                state_ref.borrow_mut().syncing_tree = false;
            }

            let archive_label = state_ref
                .borrow()
                .current_archive
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("Archive")
                .to_string();
            render_breadcrumbs(&crumbs_box, &up_btn, &archive_label, &target_dir, &nav_weak);
        })
    })
}

pub(super) fn install_thumb_buttons(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    navigate_to: &NavFn,
) {
    let thumb_click = gtk::GestureClick::builder()
        .button(0)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    let state_ref = state.clone();
    let nav = navigate_to.clone();
    thumb_click.connect_pressed(move |gesture, _, _, _| {
        let btn = gesture.current_button();
        if btn != 8 && btn != 9 {
            return;
        }
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let target = {
            let mut st = state_ref.borrow_mut();
            if st.current_archive.is_none() {
                return;
            }
            if btn == 8 {
                st.history_back.pop().inspect(|_| {
                    let cur = st.current_dir.clone();
                    st.history_forward.push(cur);
                })
            } else {
                st.history_forward.pop().inspect(|_| {
                    let cur = st.current_dir.clone();
                    st.history_back.push(cur);
                })
            }
        };
        if let Some(dir) = target {
            nav(dir, true, NavMode::History);
        }
    });
    window.add_controller(thumb_click);
}

fn render_breadcrumbs(
    crumbs_box: &gtk::Box,
    up_btn: &gtk::Button,
    archive_label: &str,
    current_dir: &str,
    nav_weak: &NavWeak,
) {
    while let Some(child) = crumbs_box.first_child() {
        crumbs_box.remove(&child);
    }
    up_btn.set_sensitive(!current_dir.is_empty());

    let mut segments: Vec<(String, String)> = vec![(archive_label.to_string(), String::new())];
    if !current_dir.is_empty() {
        let mut acc = String::new();
        for part in current_dir.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            segments.push((part.to_string(), acc.clone()));
        }
    }

    let last_idx = segments.len().saturating_sub(1);
    for (idx, (label, target_dir)) in segments.into_iter().enumerate() {
        if idx > 0 {
            let sep = gtk::Label::new(Some("/"));
            sep.add_css_class("rarc-muted");
            crumbs_box.append(&sep);
        }
        let btn = gtk::Button::with_label(&label);
        btn.add_css_class("flat");
        btn.add_css_class("rarc-crumb-btn");
        if idx == last_idx {
            btn.add_css_class("current");
        }
        let nav_weak = nav_weak.clone();
        btn.connect_clicked(move |_| {
            if let Some(nav) = nav_weak.upgrade() {
                nav(target_dir.clone(), true, NavMode::Push);
            }
        });
        crumbs_box.append(&btn);
    }
}

fn sync_tree_selection(
    tree_view: &gtk::ListView,
    tree_model: &gtk::TreeListModel,
    tree_selection: &gtk::SingleSelection,
    target_dir: &str,
) {
    // Expand ancestor rows top-down while advancing scan offset monotonically.
    if let Some(root_row) = tree_model.row(0) {
        root_row.set_expanded(true);
    }

    let mut target_idx = if target_dir.is_empty() { Some(0) } else { None };
    if !target_dir.is_empty() {
        let mut start = 1u32;
        let mut acc = String::new();
        for part in target_dir.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            let Some(idx) = find_tree_row_index_from(tree_model, start, &acc) else {
                break;
            };
            if acc == target_dir {
                target_idx = Some(idx);
                break;
            }
            if let Some(row) = tree_model.row(idx) {
                row.set_expanded(true);
            }
            start = idx + 1;
        }
    }

    if let Some(idx) = target_idx {
        tree_selection.set_selected(idx);
        tree_view.scroll_to(
            idx,
            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
            None,
        );
    }
}

fn find_tree_row_index_from(
    tree_model: &gtk::TreeListModel,
    start: u32,
    target_dir: &str,
) -> Option<u32> {
    for idx in start..tree_model.n_items() {
        if let Some(row) = tree_model.row(idx)
            && let Some(node) = row.item().and_downcast::<FolderTreeNode>()
            && node.full_path() == target_dir
        {
            return Some(idx);
        }
    }
    None
}
