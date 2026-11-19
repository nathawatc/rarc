mod actions;
mod columns;
mod models;
mod tree;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_channel::Sender;
use gtk4 as gtk;
use gtk::gio;
use gtk::prelude::*;

use crate::archive::{
    list_archive, spawn_worker, test_archive, ArchiveError, OperationSummary, WorkerEvent,
};
use crate::ui::dialogs::{run_worker_with_progress, show_create_dialog, show_properties_dialog};
use crate::ui::{format_size, show_error_dialog, show_info_dialog};

use actions::{
    activate_row, build_context_menu, install_context_actions, install_dnd, start_extract_dialog,
};
use columns::append_columns;
use models::{
    build_folder_tree, build_row_objects, ArchiveRowObject, FolderTreeNode, NavFn, NavMode,
    ViewerState,
};
use tree::{build_tree_sidebar, install_thumb_buttons, make_navigate_to};

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
