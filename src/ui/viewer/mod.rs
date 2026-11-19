mod columns;
mod models;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_channel::Sender;
use gtk4 as gtk;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::archive::{
    add_to_archive, extract_archive_with_session, is_supported_archive_path, list_archive,
    spawn_worker, test_archive, ArchiveEntry, ArchiveError, EntryKind, ExtractSession,
    OperationSummary, WorkerEvent,
};
use crate::ui::dialogs::{
    run_worker_with_progress, show_create_dialog, show_properties_dialog,
};
use crate::ui::{
    format_size, select_folder_dialog, session_temp_dir, show_error_dialog, show_info_dialog,
};

use columns::{append_columns, build_context_menu_model, CellRightClickFn};
use models::{
    build_folder_tree, build_row_objects, collect_selected_paths, collect_selected_rows,
    collect_staged_files, compute_selected_bytes, ArchiveDragProvider, ArchiveRowObject,
    FolderTreeNode, NavFn, NavMode, NavWeak, ViewerState,
};

/// Launches the standalone archive viewer/manager window.
pub fn launch_standalone(app: &gtk::Application, initial_archive: Option<PathBuf>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("rarc")
        .default_width(980)
        .default_height(620)
        .build();

    let state = Rc::new(RefCell::new(ViewerState::default()));

    // HeaderBar controls
    let header = gtk::HeaderBar::new();
    let open_btn = gtk::Button::builder()
        .label("Open")
        .icon_name("document-open-symbolic")
        .tooltip_text("Open an archive")
        .build();
    let new_btn = gtk::Button::builder()
        .label("Create")
        .icon_name("archive-insert-symbolic")
        .tooltip_text("Create a new archive")
        .build();
    let extract_btn = gtk::Button::builder()
        .label("Extract")
        .icon_name("archive-extract-symbolic")
        .tooltip_text("Extract selected entries or entire archive")
        .sensitive(false)
        .build();
    let test_btn = gtk::Button::builder()
        .label("Test")
        .icon_name("security-high-symbolic")
        .tooltip_text("Test archive integrity")
        .sensitive(false)
        .build();
    let props_btn = gtk::Button::builder()
        .label("Properties")
        .icon_name("document-properties-symbolic")
        .tooltip_text("Archive properties")
        .sensitive(false)
        .build();

    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Filter entries…")
        .width_chars(22)
        .build();

    header.pack_start(&open_btn);
    header.pack_start(&new_btn);
    header.pack_start(&extract_btn);
    header.pack_start(&test_btn);
    header.pack_start(&props_btn);
    header.pack_end(&search_entry);
    // Keep title out of HeaderBar so it does not duplicate the window title bar.
    header.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.set_titlebar(Some(&header));

    // Breadcrumb / navigation bar
    let breadcrumb_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    breadcrumb_bar.add_css_class("rarc-breadcrumb-bar");
    let up_btn = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Up one folder")
        .sensitive(false)
        .build();
    up_btn.add_css_class("flat");
    let crumbs_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    crumbs_box.set_hexpand(true);
    let crumbs_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .child(&crumbs_box)
        .build();
    breadcrumb_bar.append(&up_btn);
    breadcrumb_bar.append(&crumbs_scroll);

    let (tree_root_store, tree_model, tree_selection, tree_view, tree_scroll) =
        build_tree_sidebar();

    // ColumnView data pipeline
    let store = gio::ListStore::new::<ArchiveRowObject>();
    let filter = {
        let state_ref = state.clone();
        gtk::CustomFilter::new(move |obj| {
            let row = obj.downcast_ref::<ArchiveRowObject>().unwrap();
            let st = state_ref.borrow();
            row.with_data(|d| {
                if st.search_query.is_empty() {
                    d.parent_dir == st.current_dir
                } else {
                    let q = st.search_query.as_str();
                    d.name.to_ascii_lowercase().contains(q)
                        || d.entry.path.to_ascii_lowercase().contains(q)
                }
            })
        })
    };
    let filter_model = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));

    let column_view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
    column_view.set_show_row_separators(false);
    column_view.set_show_column_separators(true);
    column_view.set_vexpand(true);
    column_view.set_hexpand(true);

    let sort_model = gtk::SortListModel::new(Some(filter_model), column_view.sorter());
    let selection = gtk::MultiSelection::new(Some(sort_model));

    let (act_open, act_open_with, act_extract, act_copy, act_props, show_context_menu) =
        build_context_menu(&window, &column_view, &selection, &state);

    append_columns(&column_view, &show_context_menu);
    column_view.set_model(Some(&selection));

    // Fallback right-click on empty whitespace below rows
    {
        let bg_click = gtk::GestureClick::builder().button(3).build();
        let col_ref = column_view.clone();
        let show_menu = show_context_menu.clone();
        bg_click.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_menu(col_ref.upcast_ref(), x, y, None);
        });
        column_view.add_controller(bg_click);
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&column_view)
        .build();

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&tree_scroll)
        .end_child(&scrolled)
        .position(230)
        .resize_start_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .vexpand(true)
        .hexpand(true)
        .build();

    let stack = build_main_stack(&paned);
    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.append(&breadcrumb_bar);
    root_box.append(&stack);
    window.set_child(Some(&root_box));

    let navigate_to = make_navigate_to(
        &state,
        &crumbs_box,
        &up_btn,
        &filter,
        &search_entry,
        &selection,
        &tree_view,
        &tree_model,
        &tree_selection,
    );

    install_thumb_buttons(&window, &state, &navigate_to);

    // Left tree selection listener
    {
        let state_ref = state.clone();
        let nav = navigate_to.clone();
        tree_selection.connect_selected_item_notify(move |sel| {
            if state_ref.borrow().syncing_tree {
                return;
            }
            if let Some(item) = sel.selected_item()
                && let Ok(list_row) = item.downcast::<gtk::TreeListRow>()
                && let Some(node) = list_row.item().and_downcast::<FolderTreeNode>()
            {
                let target = node.full_path();
                if state_ref.borrow().current_dir != target {
                    nav(target, false, NavMode::Push);
                }
            }
        });
    }

    // Up button action
    {
        let state_ref = state.clone();
        let nav = navigate_to.clone();
        up_btn.connect_clicked(move |_| {
            let next = {
                let st = state_ref.borrow();
                if let Some((parent, _)) = st.current_dir.rsplit_once('/') {
                    parent.to_string()
                } else {
                    String::new()
                }
            };
            nav(next, true, NavMode::Push);
        });
    }

    // Search entry filtering
    {
        let state_ref = state.clone();
        let filter_ref = filter.clone();
        search_entry.connect_search_changed(move |entry| {
            state_ref.borrow_mut().search_query = entry.text().trim().to_ascii_lowercase();
            filter_ref.changed(gtk::FilterChange::Different);
        });
    }

    let open_archive = make_open_archive(
        &window,
        &state,
        &store,
        &tree_root_store,
        &tree_model,
        &stack,
        &extract_btn,
        &test_btn,
        &props_btn,
        &navigate_to,
    );

    install_header_actions(
        &window,
        &state,
        &store,
        &selection,
        &open_btn,
        &new_btn,
        &extract_btn,
        &test_btn,
        &props_btn,
        &open_archive,
    );

    install_context_actions(
        &window,
        &state,
        &store,
        &selection,
        &navigate_to,
        &act_open,
        &act_open_with,
        &act_extract,
        &act_copy,
        &act_props,
    );

    // Double-click activation: enter directory or extract & launch file
    {
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let nav = navigate_to.clone();
        let win_ref = window.clone();
        column_view.connect_activate(move |_, pos| {
            let Some(item) = selection_ref.item(pos) else {
                return;
            };
            let Ok(row) = item.downcast::<ArchiveRowObject>() else {
                return;
            };
            activate_row(&win_ref, &state_ref, &row, false, &nav);
        });
    }

    install_dnd(&window, &stack, &column_view, &state, &store, &selection, &open_archive);

    window.present();
    if let Some(path) = initial_archive {
        open_archive(path);
    }
}

fn run_viewer_worker<FWork, FOk>(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    expected_archive: Option<PathBuf>,
    progress_title: &str,
    error_title: &'static str,
    work: FWork,
    on_ok: FOk,
) where
    FWork: FnOnce(&Sender<WorkerEvent>, &Arc<AtomicBool>) -> Result<OperationSummary, ArchiveError>
        + Send
        + 'static,
    FOk: FnOnce(OperationSummary) + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();
    let rx = spawn_worker(move |tx| work(tx, &cancel_worker));
    let win_err = window.clone();
    let state_pw = state.clone();
    run_worker_with_progress(
        None,
        Some(window.upcast_ref()),
        progress_title,
        rx,
        cancel,
        move |res| match res {
            Ok(summary) => {
                if let Some(ref arch) = expected_archive {
                    let pw = match &summary {
                        OperationSummary::Extracted { password }
                        | OperationSummary::Tested { password, .. }
                        | OperationSummary::Added { password, .. } => password.as_ref(),
                        OperationSummary::Listed(_) | OperationSummary::Created(_) => None,
                    };
                    if let Some(pw) = pw {
                        let mut st = state_pw.borrow_mut();
                        if st.current_archive.as_ref() == Some(arch) {
                            st.cached_password = Some(pw.clone());
                        }
                    }
                }
                on_ok(summary);
            }
            Err(ArchiveError::Cancelled) => {}
            Err(err) => {
                show_error_dialog(Some(&win_err), error_title, &err.to_string());
            }
        },
    );
}

fn build_tree_sidebar() -> (
    gio::ListStore,
    gtk::TreeListModel,
    gtk::SingleSelection,
    gtk::ListView,
    gtk::ScrolledWindow,
) {
    let tree_root_store = gio::ListStore::new::<FolderTreeNode>();
    let tree_model = gtk::TreeListModel::new(tree_root_store.clone(), false, false, |obj| {
        let node = obj.downcast_ref::<FolderTreeNode>()?;
        let children = node.children();
        if children.n_items() > 0 {
            Some(children.upcast())
        } else {
            None
        }
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
        label.set_text(&node.name());
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

    (tree_root_store, tree_model, tree_selection, tree_view, tree_scroll)
}

fn build_context_menu(
    window: &gtk::ApplicationWindow,
    column_view: &gtk::ColumnView,
    selection: &gtk::MultiSelection,
    state: &Rc<RefCell<ViewerState>>,
) -> (
    gio::SimpleAction,
    gio::SimpleAction,
    gio::SimpleAction,
    gio::SimpleAction,
    gio::SimpleAction,
    CellRightClickFn,
) {
    let item_actions = gio::SimpleActionGroup::new();
    let act_open = gio::SimpleAction::new("open", None);
    let act_open_with = gio::SimpleAction::new("open-with", None);
    let act_extract = gio::SimpleAction::new("extract", None);
    let act_copy = gio::SimpleAction::new("copy", None);
    let act_props = gio::SimpleAction::new("properties", None);
    item_actions.add_action(&act_open);
    item_actions.add_action(&act_open_with);
    item_actions.add_action(&act_extract);
    item_actions.add_action(&act_copy);
    item_actions.add_action(&act_props);
    window.insert_action_group("item", Some(&item_actions));

    let menu_model = build_context_menu_model();
    let popover = gtk::PopoverMenu::from_model(Some(&menu_model));
    popover.set_parent(column_view);
    popover.set_has_arrow(false);
    {
        let pop_cleanup = popover.clone();
        column_view.connect_destroy(move |_| pop_cleanup.unparent());
    }

    let show_context_menu: CellRightClickFn = {
        let column_ref = column_view.clone();
        let selection_ref = selection.clone();
        let popover_ref = popover.clone();
        let state_ref = state.clone();
        let act_open = act_open.clone();
        let act_open_with = act_open_with.clone();
        let act_extract = act_extract.clone();
        let act_copy = act_copy.clone();
        let act_props = act_props.clone();

        Rc::new(move |src_widget, x, y, pos_opt| {
            if state_ref.borrow().current_archive.is_none() {
                return;
            }
            if let Some(pos) = pos_opt
                && !selection_ref.is_selected(pos)
            {
                selection_ref.select_item(pos, true);
            }

            let selected_rows = collect_selected_rows(&selection_ref);
            let count = selected_rows.len();
            let single_file = count == 1
                && !selected_rows[0]
                    .with_data(|d| d.is_dir() || d.entry.kind == EntryKind::Symlink);
            act_open.set_enabled(count == 1);
            act_open_with.set_enabled(single_file);
            act_extract.set_enabled(count > 0);
            act_copy.set_enabled(count > 0);
            act_props.set_enabled(count > 0);

            let rect = if let Some(pt) =
                src_widget.compute_point(&column_ref, &gtk::graphene::Point::new(x as f32, y as f32))
            {
                gdk::Rectangle::new(pt.x().round() as i32, pt.y().round() as i32, 1, 1)
            } else {
                gdk::Rectangle::new(x.round() as i32, y.round() as i32, 1, 1)
            };
            popover_ref.set_pointing_to(Some(&rect));
            popover_ref.popup();
        })
    };

    (
        act_open,
        act_open_with,
        act_extract,
        act_copy,
        act_props,
        show_context_menu,
    )
}

fn build_main_stack(paned: &gtk::Paned) -> gtk::Stack {
    let empty_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    empty_box.set_valign(gtk::Align::Center);
    empty_box.set_halign(gtk::Align::Center);
    empty_box.add_css_class("rarc-empty-state");
    let empty_icon = gtk::Image::from_icon_name("package-x-generic");
    empty_icon.set_pixel_size(64);
    let empty_title = gtk::Label::new(Some("Open an archive or drop files here"));
    empty_title.add_css_class("title-3");
    let empty_sub = gtk::Label::new(Some(
        "Drop an archive to inspect it, or drop files/folders to create a new archive.",
    ));
    empty_sub.add_css_class("rarc-hint");
    empty_box.append(&empty_icon);
    empty_box.append(&empty_title);
    empty_box.append(&empty_sub);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_hexpand(true);
    stack.add_named(&empty_box, Some("empty"));
    stack.add_named(paned, Some("list"));
    stack.set_visible_child_name("empty");
    stack
}

#[allow(clippy::too_many_arguments)]
fn make_navigate_to(
    state: &Rc<RefCell<ViewerState>>,
    crumbs_box: &gtk::Box,
    up_btn: &gtk::Button,
    filter: &gtk::CustomFilter,
    search_entry: &gtk::SearchEntry,
    selection: &gtk::MultiSelection,
    tree_view: &gtk::ListView,
    tree_model: &gtk::TreeListModel,
    tree_selection: &gtk::SingleSelection,
) -> NavFn {
    let state_ref = state.clone();
    let crumbs_box = crumbs_box.clone();
    let up_btn = up_btn.clone();
    let filter_ref = filter.clone();
    let search_ref = search_entry.clone();
    let selection_ref = selection.clone();
    let tree_view_ref = tree_view.clone();
    let tree_model_ref = tree_model.clone();
    let tree_sel_ref = tree_selection.clone();

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
            filter_ref.changed(gtk::FilterChange::Different);

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

fn install_thumb_buttons(
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

#[allow(clippy::too_many_arguments)]
fn make_open_archive(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    tree_root_store: &gio::ListStore,
    tree_model: &gtk::TreeListModel,
    stack: &gtk::Stack,
    extract_btn: &gtk::Button,
    test_btn: &gtk::Button,
    props_btn: &gtk::Button,
    navigate_to: &NavFn,
) -> Rc<dyn Fn(PathBuf)> {
    let window_ref = window.clone();
    let state_ref = state.clone();
    let store_ref = store.clone();
    let tree_root_ref = tree_root_store.clone();
    let tree_model_ref = tree_model.clone();
    let stack_ref = stack.clone();
    let extract_ref = extract_btn.clone();
    let test_ref = test_btn.clone();
    let props_ref = props_btn.clone();
    let nav = navigate_to.clone();

    Rc::new(move |archive_path: PathBuf| {
        let initial_pw = {
            let st = state_ref.borrow();
            if st.current_archive.as_ref() == Some(&archive_path) {
                st.cached_password.clone()
            } else {
                None
            }
        };
        let path_worker = archive_path.clone();
        let path_done = archive_path.clone();
        let title = format!(
            "Opening {}",
            archive_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("archive")
        );

        let win_done = window_ref.clone();
        let st_done = state_ref.clone();
        let store_done = store_ref.clone();
        let tree_root_done = tree_root_ref.clone();
        let tree_model_done = tree_model_ref.clone();
        let stack_done = stack_ref.clone();
        let extract_done = extract_ref.clone();
        let test_done = test_ref.clone();
        let props_done = props_ref.clone();
        let nav_done = nav.clone();

        run_viewer_worker(
            &window_ref,
            &state_ref,
            None,
            &title,
            "Unable to Open Archive",
            move |tx, cancel| {
                let listing =
                    list_archive(&path_worker, initial_pw.as_deref(), Some(tx), cancel)?;
                Ok(OperationSummary::Listed(listing))
            },
            move |summary| {
                if let OperationSummary::Listed(listing) = summary {
                    let rows = build_row_objects(&listing.entries);
                    let archive_name = path_done
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Archive");
                    let root_node = build_folder_tree(archive_name, &listing.entries);
                    let entry_count = listing.entries.len();
                    let total_uncompressed: u64 = listing.entries.iter().map(|e| e.size).sum();
                    let encrypted_count =
                        listing.entries.iter().filter(|e| e.is_encrypted).count();

                    store_done.remove_all();
                    store_done.extend_from_slice(&rows);
                    tree_root_done.remove_all();
                    tree_root_done.append(&root_node);
                    if let Some(first_row) = tree_model_done.row(0) {
                        first_row.set_expanded(true);
                    }

                    let (target_dir, nav_mode) = {
                        let mut st = st_done.borrow_mut();
                        let same_archive = st.current_archive.as_ref() == Some(&path_done);
                        st.current_archive = Some(path_done.clone());
                        st.cached_password = listing.password;
                        st.format_name = listing.format_name;
                        st.is_raw = listing.is_raw;
                        st.entry_count = entry_count;
                        st.total_uncompressed = total_uncompressed;
                        st.encrypted_count = encrypted_count;
                        if !same_archive {
                            st.current_dir.clear();
                        }
                        let mode = if same_archive {
                            NavMode::History
                        } else {
                            NavMode::Reset
                        };
                        (st.current_dir.clone(), mode)
                    };

                    win_done.set_title(Some(&format!("{archive_name} - rarc")));
                    extract_done.set_sensitive(true);
                    test_done.set_sensitive(true);
                    props_done.set_sensitive(true);
                    stack_done.set_visible_child_name("list");
                    nav_done(target_dir, true, nav_mode);
                }
            },
        );
    })
}

#[allow(clippy::too_many_arguments)]
fn install_header_actions(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
    open_btn: &gtk::Button,
    new_btn: &gtk::Button,
    extract_btn: &gtk::Button,
    test_btn: &gtk::Button,
    props_btn: &gtk::Button,
    open_archive: &Rc<dyn Fn(PathBuf)>,
) {
    {
        let win_ref = window.clone();
        let open_fn = open_archive.clone();
        open_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Open Archive").build();
            let open_fn = open_fn.clone();
            dialog.open(Some(&win_ref), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res
                    && let Some(path) = file.path()
                {
                    open_fn(path);
                }
            });
        });
    }
    {
        let win_ref = window.clone();
        let open_fn = open_archive.clone();
        new_btn.connect_clicked(move |_| {
            let open_fn = open_fn.clone();
            show_create_dialog(
                None,
                Some(win_ref.upcast_ref()),
                Vec::new(),
                None,
                move |created| {
                    if let Some(path) = created {
                        open_fn(path);
                    }
                },
            );
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        extract_btn.connect_clicked(move |_| {
            start_extract_dialog(&win_ref, &state_ref, &store_ref, &selection_ref, false);
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        test_btn.connect_clicked(move |_| {
            let (Some(archive_path), cached_pw, total_uncompressed, is_raw) = ({
                let st = state_ref.borrow();
                (
                    st.current_archive.clone(),
                    st.cached_password.clone(),
                    st.total_uncompressed,
                    st.is_raw,
                )
            }) else {
                return;
            };
            let arch_worker = archive_path.clone();
            let win_done = win_ref.clone();
            run_viewer_worker(
                &win_ref,
                &state_ref,
                Some(archive_path),
                "Testing Archive Integrity",
                "Integrity Check Failed",
                move |tx, cancel| {
                    let (entries_checked, bytes_checked, pw) = test_archive(
                        &arch_worker,
                        cached_pw.as_deref(),
                        Some((total_uncompressed, is_raw)),
                        Some(tx),
                        cancel,
                    )?;
                    Ok(OperationSummary::Tested {
                        entries_checked,
                        bytes_checked,
                        password: pw,
                    })
                },
                move |summary| {
                    if let OperationSummary::Tested {
                        entries_checked,
                        bytes_checked,
                        ..
                    } = summary
                    {
                        show_info_dialog(
                            Some(&win_done),
                            "Integrity Check Passed",
                            &format!(
                                "The archive is intact.\nVerified {entries_checked} entries ({}) with 0 errors.",
                                format_size(bytes_checked)
                            ),
                        );
                    }
                },
            );
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        props_btn.connect_clicked(move |_| {
            if let Some(info) = state_ref.borrow().properties_info() {
                show_properties_dialog(win_ref.upcast_ref(), &[], &info);
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn install_context_actions(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
    navigate_to: &NavFn,
    act_open: &gio::SimpleAction,
    act_open_with: &gio::SimpleAction,
    act_extract: &gio::SimpleAction,
    act_copy: &gio::SimpleAction,
    act_props: &gio::SimpleAction,
) {
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let nav = navigate_to.clone();
        act_open.connect_activate(move |_, _| {
            let rows = collect_selected_rows(&selection_ref);
            if let Some(row) = rows.first() {
                activate_row(&win_ref, &state_ref, row, false, &nav);
            }
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let nav = navigate_to.clone();
        act_open_with.connect_activate(move |_, _| {
            let rows = collect_selected_rows(&selection_ref);
            if let Some(row) = rows.first()
                && !row.with_data(|d| d.is_dir())
            {
                activate_row(&win_ref, &state_ref, row, true, &nav);
            }
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        act_extract.connect_activate(move |_, _| {
            start_extract_dialog(&win_ref, &state_ref, &store_ref, &selection_ref, true);
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        act_copy.connect_activate(move |_, _| {
            copy_selected_to_clipboard(&win_ref, &state_ref, &store_ref, &selection_ref);
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        act_props.connect_activate(move |_, _| {
            let Some(info) = state_ref.borrow().properties_info() else {
                return;
            };
            let selected: Vec<ArchiveEntry> = collect_selected_rows(&selection_ref)
                .into_iter()
                .map(|r| r.entry())
                .collect();
            if !selected.is_empty() {
                show_properties_dialog(win_ref.upcast_ref(), &selected, &info);
            }
        });
    }
}

fn install_dnd(
    window: &gtk::ApplicationWindow,
    stack: &gtk::Stack,
    column_view: &gtk::ColumnView,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
    open_archive: &Rc<dyn Fn(PathBuf)>,
) {
    // Drag-and-Drop IN
    {
        let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let stack_enter = stack.clone();
        drop_target.connect_enter(move |_, _, _| {
            stack_enter.add_css_class("rarc-drop-active");
            gdk::DragAction::COPY
        });
        let stack_leave = stack.clone();
        drop_target.connect_leave(move |_| {
            stack_leave.remove_css_class("rarc-drop-active");
        });

        let stack_drop = stack.clone();
        let state_ref = state.clone();
        let win_ref = window.clone();
        let open_fn = open_archive.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            stack_drop.remove_css_class("rarc-drop-active");
            let Ok(file_list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<PathBuf> = file_list
                .files()
                .into_iter()
                .filter_map(|f| f.path())
                .collect();
            if paths.is_empty() {
                return false;
            }

            let open_archive_path = state_ref.borrow().current_archive.clone();
            if let Some(arch_path) = open_archive_path {
                let (dest_subdir, cached_pw) = {
                    let st = state_ref.borrow();
                    (st.current_dir.clone(), st.cached_password.clone())
                };
                let arch_worker = arch_path.clone();
                let reload = open_fn.clone();
                run_viewer_worker(
                    &win_ref,
                    &state_ref,
                    Some(arch_path),
                    "Adding Files to Archive",
                    "Failed to Add Files",
                    move |tx, cancel| {
                        let (_, password) = add_to_archive(
                            &arch_worker,
                            &paths,
                            &dest_subdir,
                            cached_pw.as_deref(),
                            Some(tx),
                            cancel,
                        )?;
                        Ok(OperationSummary::Added {
                            path: arch_worker,
                            password,
                        })
                    },
                    move |summary| {
                        if let OperationSummary::Added { path, .. } = summary {
                            reload(path);
                        }
                    },
                );
                true
            } else if paths.len() == 1
                && paths[0].is_file()
                && is_supported_archive_path(&paths[0])
            {
                open_fn(paths[0].clone());
                true
            } else {
                let open_after = open_fn.clone();
                show_create_dialog(
                    None,
                    Some(win_ref.upcast_ref()),
                    paths,
                    None,
                    move |created| {
                        if let Some(path) = created {
                            open_after(path);
                        }
                    },
                );
                true
            }
        });
        stack.add_controller(drop_target);
    }

    // Drag-and-Drop OUT
    {
        let drag_source = gtk::DragSource::builder()
            .actions(gdk::DragAction::COPY)
            .build();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        let win_ref = window.clone();
        drag_source.connect_prepare(move |_, _, _| {
            let st = state_ref.borrow();
            let arch = st.current_archive.as_ref()?;
            let selected = collect_selected_paths(&selection_ref);
            if selected.is_empty() {
                return None;
            }
            let total_bytes = compute_selected_bytes(&store_ref, Some(&selected));
            let strip = st.active_strip_prefix();
            let provider = ArchiveDragProvider::new(
                arch.clone(),
                selected,
                strip,
                st.cached_password.clone(),
                st.is_raw,
                total_bytes,
                Rc::downgrade(&state_ref),
                win_ref.upcast_ref(),
            );
            Some(provider.upcast())
        });
        column_view.add_controller(drag_source);
    }
}

fn activate_row(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    row: &ArchiveRowObject,
    always_ask: bool,
    navigate_to: &NavFn,
) {
    let (is_dir, is_symlink, full_path, name, parent_dir, entry_size) = row.with_data(|d| {
        (
            d.is_dir(),
            d.entry.kind == EntryKind::Symlink,
            d.entry.path.clone(),
            d.name.clone(),
            d.parent_dir.clone(),
            d.entry.size,
        )
    });

    if is_dir {
        navigate_to(full_path, true, NavMode::Push);
        return;
    }
    if is_symlink {
        show_info_dialog(
            Some(window),
            "Cannot Open Symbolic Link",
            "Symbolic links inside archives cannot be opened directly.",
        );
        return;
    }

    let (archive_path, seq, cached_pw, is_raw) = {
        let mut st = state.borrow_mut();
        let Some(arch) = st.current_archive.clone() else {
            return;
        };
        st.stage_seq += 1;
        (arch, st.stage_seq, st.cached_password.clone(), st.is_raw)
    };
    let Ok(temp_root) = session_temp_dir() else {
        return;
    };
    let preview_dir = temp_root.join(format!("preview-{seq}"));
    let target_file = preview_dir.join(&name);
    let strip = if parent_dir.is_empty() {
        None
    } else {
        Some(parent_dir)
    };

    let arch_worker = archive_path.clone();
    let win_launch = window.clone();
    run_viewer_worker(
        window,
        state,
        Some(archive_path),
        &format!("Opening {name}"),
        "Preview Failed",
        move |tx, cancel| {
            let mut session = ExtractSession::new(cached_pw, entry_size);
            extract_archive_with_session(
                &arch_worker,
                &preview_dir,
                Some(&[full_path]),
                strip.as_deref(),
                Some(is_raw),
                &mut session,
                Some(tx),
                cancel,
            )?;
            Ok(OperationSummary::Extracted {
                password: session.password,
            })
        },
        move |_| {
            if target_file.exists() {
                let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&target_file)));
                launcher.set_always_ask(always_ask);
                let win_err = win_launch.clone();
                launcher.launch(Some(&win_launch), gio::Cancellable::NONE, move |lres| {
                    if let Err(e) = lres
                        && !is_user_dismissed_error(&e)
                    {
                        show_error_dialog(Some(&win_err), "Unable to Launch File", e.message());
                    }
                });
            }
        },
    );
}

fn is_user_dismissed_error(err: &glib::Error) -> bool {
    err.matches(gtk::DialogError::Dismissed)
        || err.matches(gtk::DialogError::Cancelled)
        || err.matches(gio::IOErrorEnum::Cancelled)
        || err.message().to_ascii_lowercase().contains("dismissed")
}

fn start_extract_dialog(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
    require_selection: bool,
) {
    let Some(archive_path) = state.borrow().current_archive.clone() else {
        return;
    };
    let selected = collect_selected_paths(selection);
    if require_selection && selected.is_empty() {
        return;
    }
    let selected_opt = if selected.is_empty() {
        None
    } else {
        Some(selected)
    };
    let (strip_prefix, cached_pw, is_raw, total_bytes) = {
        let st = state.borrow();
        let strip = if selected_opt.is_some() {
            st.active_strip_prefix()
        } else {
            None
        };
        let bytes = if selected_opt.is_some() {
            compute_selected_bytes(store, selected_opt.as_deref())
        } else {
            st.total_uncompressed
        };
        (strip, st.cached_password.clone(), st.is_raw, bytes)
    };

    let initial_folder = archive_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from);
    let win_extract = window.clone();
    let state_done = state.clone();
    select_folder_dialog(
        Some(window.upcast_ref()),
        "Extract To Folder",
        initial_folder.as_deref(),
        move |folder_opt| {
            let Some(dest_dir) = folder_opt else { return };
            let arch_worker = archive_path.clone();
            run_viewer_worker(
                &win_extract,
                &state_done,
                Some(archive_path),
                "Extracting Archive",
                "Extraction Failed",
                move |tx, cancel| {
                    let mut session = ExtractSession::new(cached_pw, total_bytes);
                    extract_archive_with_session(
                        &arch_worker,
                        &dest_dir,
                        selected_opt.as_deref(),
                        strip_prefix.as_deref(),
                        Some(is_raw),
                        &mut session,
                        Some(tx),
                        cancel,
                    )?;
                    Ok(OperationSummary::Extracted {
                        password: session.password,
                    })
                },
                |_| {},
            );
        },
    );
}

fn copy_selected_to_clipboard(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
) {
    let Some(archive_path) = state.borrow().current_archive.clone() else {
        return;
    };
    let selected = collect_selected_paths(selection);
    if selected.is_empty() {
        return;
    }

    let (seq, strip_prefix, cached_pw, is_raw, total_bytes) = {
        let mut st = state.borrow_mut();
        st.stage_seq += 1;
        let strip = st.active_strip_prefix();
        let bytes = compute_selected_bytes(store, Some(&selected));
        (
            st.stage_seq,
            strip,
            st.cached_password.clone(),
            st.is_raw,
            bytes,
        )
    };
    let Ok(temp_root) = session_temp_dir() else {
        return;
    };
    let stage_dir = temp_root.join(format!("copy-{seq}"));
    let stage_worker = stage_dir.clone();
    let selected_worker = selected.clone();
    let strip_worker = strip_prefix.clone();
    let arch_worker = archive_path.clone();

    run_viewer_worker(
        window,
        state,
        Some(archive_path),
        "Copying to Clipboard",
        "Copy Failed",
        move |tx, cancel| {
            let mut session = ExtractSession::new(cached_pw, total_bytes);
            extract_archive_with_session(
                &arch_worker,
                &stage_worker,
                Some(&selected_worker),
                strip_worker.as_deref(),
                Some(is_raw),
                &mut session,
                Some(tx),
                cancel,
            )?;
            Ok(OperationSummary::Extracted {
                password: session.password,
            })
        },
        move |_| {
            let gio_files = collect_staged_files(&stage_dir, &selected, strip_prefix.as_deref());
            if gio_files.is_empty() {
                return;
            }
            let uri_lines: Vec<String> = gio_files.iter().map(|f| f.uri().to_string()).collect();
            let uri_list = format!("{}\r\n", uri_lines.join("\r\n"));
            let gnome_copied = format!("copy\n{}", uri_lines.join("\n"));

            let file_list = gdk::FileList::from_array(&gio_files);
            let provider = gdk::ContentProvider::new_union(&[
                gdk::ContentProvider::for_value(&file_list.to_value()),
                gdk::ContentProvider::for_bytes(
                    "x-special/gnome-copied-files",
                    &glib::Bytes::from_owned(gnome_copied.into_bytes()),
                ),
                gdk::ContentProvider::for_bytes(
                    "text/uri-list",
                    &glib::Bytes::from_owned(uri_list.into_bytes()),
                ),
            ]);

            if let Some(display) = gdk::Display::default() {
                let _ = display.clipboard().set_content(Some(&provider));
            }
        },
    );
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
