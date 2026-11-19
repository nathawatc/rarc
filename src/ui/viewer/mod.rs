mod actions;
mod columns;
mod models;
mod tree;

use std::cell::{OnceCell, RefCell};
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
use crate::i18n::{tr, trn};
use crate::ui::dialogs::{run_worker_with_progress, show_create_dialog, show_properties_dialog};
use crate::ui::{format_size, show_message_dialog};

use actions::{
    activate_row, build_context_menu, install_context_actions, install_drag_out, install_drop_in,
    start_extract_dialog,
};
use columns::{append_columns, pick_row_pos};
use models::{
    index_archive_entries, refresh_visible_store, ArchiveRowObject, FolderTreeNode, NavFn, NavMode,
    ViewerState,
};
use tree::{build_tree_sidebar, install_thumb_buttons, make_navigate_to};

#[derive(Clone)]
pub(super) struct ViewerWidgets {
    pub window: gtk::ApplicationWindow,
    pub open_btn: gtk::Button,
    pub new_btn: gtk::Button,
    pub extract_btn: gtk::Button,
    pub test_btn: gtk::Button,
    pub props_btn: gtk::Button,
    pub search_entry: gtk::SearchEntry,
    pub up_btn: gtk::Button,
    pub crumbs_box: gtk::Box,
    pub stack: gtk::Stack,
}

struct ListPage {
    tree_root_store: gio::ListStore,
    tree_model: gtk::TreeListModel,
    selection: gtk::MultiSelection,
    navigate_to: NavFn,
}

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
        .label(tr("Open"))
        .icon_name("document-open-symbolic")
        .tooltip_text(tr("Open an archive"))
        .build();
    let new_btn = make_themed_icon_button(
        &[
            "add-files-to-archive-symbolic",
            "package-x-generic-symbolic",
            "document-new-symbolic",
        ],
        &tr("Create a new archive"),
    );
    let extract_btn = make_themed_icon_button(
        &[
            "extract-archive-symbolic",
            "archive-extract-symbolic",
            "folder-download-symbolic",
        ],
        &tr("Extract selected entries or entire archive"),
    );
    extract_btn.set_sensitive(false);
    let test_btn = gtk::Button::builder()
        .label(tr("Test"))
        .icon_name("security-high-symbolic")
        .tooltip_text(tr("Test archive integrity"))
        .sensitive(false)
        .build();
    let props_btn = gtk::Button::builder()
        .label(tr("Properties"))
        .icon_name("document-properties-symbolic")
        .tooltip_text(tr("Archive properties"))
        .sensitive(false)
        .build();

    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text(tr("Filter entries…"))
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
        .tooltip_text(tr("Up one folder"))
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

    let stack = build_empty_stack();
    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.append(&breadcrumb_bar);
    root_box.append(&stack);
    window.set_child(Some(&root_box));

    let widgets = ViewerWidgets {
        window,
        open_btn,
        new_btn,
        extract_btn,
        test_btn,
        props_btn,
        search_entry,
        up_btn,
        crumbs_box,
        stack,
    };

    let list_page: Rc<OnceCell<ListPage>> = Rc::new(OnceCell::new());
    let open_archive = make_open_archive(&widgets, &state, &list_page);
    install_header_actions(&widgets, &state, &list_page, &open_archive);
    install_search_unfocus(&widgets.window, &widgets.search_entry);
    install_drop_in(&widgets.window, &widgets.stack, &state, &open_archive);

    widgets.window.present();
    if let Some(path) = initial_archive {
        open_archive(path);
    }
}

fn build_list_page(widgets: &ViewerWidgets, state: &Rc<RefCell<ViewerState>>) -> ListPage {
    let tree = build_tree_sidebar(state);

    // ColumnView data pipeline (store holds only the currently visible directory or search results)
    let store = gio::ListStore::new::<ArchiveRowObject>();

    let column_view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
    column_view.set_show_row_separators(false);
    column_view.set_show_column_separators(true);
    column_view.set_vexpand(true);
    column_view.set_hexpand(true);

    let sort_model = gtk::SortListModel::new(Some(store.clone()), column_view.sorter());
    let selection = gtk::MultiSelection::new(Some(sort_model));

    let (ctx_actions, show_context_menu) =
        build_context_menu(&widgets.window, &column_view, &selection, state);

    append_columns(&column_view);
    column_view.set_model(Some(&selection));

    // Single right-click gesture on ColumnView (picks cell row position via BoundRowPos)
    {
        let right_click = gtk::GestureClick::builder().button(3).build();
        let col_ref = column_view.clone();
        let show_menu = show_context_menu.clone();
        right_click.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let pos = pick_row_pos(&col_ref, x, y);
            show_menu(x, y, pos);
        });
        column_view.add_controller(right_click);
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&column_view)
        .build();

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&tree.scroll)
        .end_child(&scrolled)
        .position(230)
        .resize_start_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .vexpand(true)
        .hexpand(true)
        .build();
    widgets.stack.add_named(&paned, Some("list"));

    let navigate_to = make_navigate_to(widgets, state, &store, &selection, &tree);

    install_thumb_buttons(&widgets.window, state, &navigate_to);

    // Left tree selection listener
    {
        let state_ref = state.clone();
        let nav = navigate_to.clone();
        tree.selection.connect_selected_item_notify(move |sel| {
            if state_ref.borrow().syncing_tree {
                return;
            }
            if let Some(item) = sel.selected_item()
                && let Ok(list_row) = item.downcast::<gtk::TreeListRow>()
                && let Some(node) = list_row.item().and_downcast::<FolderTreeNode>()
            {
                let target = node.full_path().to_string();
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
        widgets.up_btn.connect_clicked(move |_| {
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
        let store_ref = store.clone();
        widgets.search_entry.connect_search_changed(move |entry| {
            let query = entry.text().trim().to_ascii_lowercase();
            let changed = {
                let mut st = state_ref.borrow_mut();
                if st.search_query == query {
                    false
                } else {
                    st.search_query = query;
                    true
                }
            };
            if changed {
                refresh_visible_store(&store_ref, &state_ref.borrow());
            }
        });
    }

    install_context_actions(&widgets.window, state, &selection, &navigate_to, &ctx_actions);

    // Double-click activation: enter directory or extract & launch file
    {
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let nav = navigate_to.clone();
        let win_ref = widgets.window.clone();
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

    install_drag_out(&widgets.window, &column_view, state, &selection);

    ListPage {
        tree_root_store: tree.root_store,
        tree_model: tree.model,
        selection,
        navigate_to,
    }
}

fn run_viewer_worker<FWork, FOk>(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    expected_archive: Option<PathBuf>,
    progress_title: &str,
    error_title: String,
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
                show_message_dialog(Some(&win_err), &error_title, &err.to_string());
            }
        },
    );
}

fn make_themed_icon_button(icon_names: &[&str], tooltip: &str) -> gtk::Button {
    let icon = gio::ThemedIcon::from_names(icon_names);
    let img = gtk::Image::from_gicon(&icon);
    let btn = gtk::Button::builder()
        .child(&img)
        .tooltip_text(tooltip)
        .build();
    btn.add_css_class("image-button");
    btn
}

fn install_search_unfocus(window: &gtk::ApplicationWindow, search_entry: &gtk::SearchEntry) {
    let win_esc = window.clone();
    search_entry.connect_stop_search(move |entry| {
        if !entry.text().is_empty() {
            entry.set_text("");
        }
        gtk::prelude::GtkWindowExt::set_focus(&win_esc, gtk::Widget::NONE);
    });

    let click = gtk::GestureClick::builder()
        .button(0)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    let win_click = window.clone();
    let search_click = search_entry.clone();
    click.connect_pressed(move |_, _, x, y| {
        let search_focused = gtk::prelude::GtkWindowExt::focus(&win_click)
            .is_some_and(|w| w == search_click || w.is_ancestor(&search_click));
        if !search_focused {
            return;
        }
        let inside_search = win_click
            .pick(x, y, gtk::PickFlags::DEFAULT)
            .is_some_and(|w| w == search_click || w.is_ancestor(&search_click));
        if !inside_search {
            gtk::prelude::GtkWindowExt::set_focus(&win_click, gtk::Widget::NONE);
        }
    });
    window.add_controller(click);
}

fn build_empty_stack() -> gtk::Stack {
    let empty_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    empty_box.set_valign(gtk::Align::Center);
    empty_box.set_halign(gtk::Align::Center);
    empty_box.add_css_class("rarc-empty-state");
    let empty_icon = gtk::Image::from_icon_name("package-x-generic");
    empty_icon.set_pixel_size(64);
    let empty_title = gtk::Label::new(Some(&tr("Open an archive or drop files here")));
    empty_title.add_css_class("title-3");
    let empty_sub = gtk::Label::new(Some(&tr(
        "Drop an archive to inspect it, or drop files/folders to create a new archive.",
    )));
    empty_sub.add_css_class("rarc-hint");
    empty_box.append(&empty_icon);
    empty_box.append(&empty_title);
    empty_box.append(&empty_sub);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_hexpand(true);
    stack.add_named(&empty_box, Some("empty"));
    stack.set_visible_child_name("empty");
    stack
}

fn make_open_archive(
    widgets: &ViewerWidgets,
    state: &Rc<RefCell<ViewerState>>,
    list_page: &Rc<OnceCell<ListPage>>,
) -> Rc<dyn Fn(PathBuf)> {
    let widgets_ref = widgets.clone();
    let state_ref = state.clone();
    let list_page_ref = list_page.clone();

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
        let title = tr("Opening {name}").replace(
            "{name}",
            archive_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("archive"),
        );

        let w_done = widgets_ref.clone();
        let st_done = state_ref.clone();
        let list_page_done = list_page_ref.clone();

        run_viewer_worker(
            &widgets_ref.window,
            &state_ref,
            None,
            &title,
            tr("Unable to Open Archive"),
            move |tx, cancel| {
                let listing =
                    list_archive(&path_worker, initial_pw.as_deref(), Some(tx), cancel)?;
                Ok(OperationSummary::Listed(listing))
            },
            move |summary| {
                if let OperationSummary::Listed(listing) = summary {
                    let archive_name = path_done
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Archive");
                    let entry_count = listing.entries.len();
                    let total_uncompressed: u64 = listing.entries.iter().map(|e| e.size).sum();
                    let encrypted_count =
                        listing.entries.iter().filter(|e| e.is_encrypted).count();
                    let index = index_archive_entries(listing.entries);
                    let root_node = FolderTreeNode::new(archive_name.to_string(), String::new());

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
                        st.entries = index.entries;
                        st.dir_entries = index.dir_entries;
                        st.dir_children = index.dir_children;
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

                    let lp = list_page_done.get_or_init(|| build_list_page(&w_done, &st_done));
                    lp.tree_root_store
                        .splice(0, lp.tree_root_store.n_items(), &[root_node]);
                    if let Some(first_row) = lp.tree_model.row(0) {
                        first_row.set_expanded(true);
                    }

                    w_done.window.set_title(Some(&format!("{archive_name} - rarc")));
                    w_done.extract_btn.set_sensitive(true);
                    w_done.test_btn.set_sensitive(true);
                    w_done.props_btn.set_sensitive(true);
                    w_done.stack.set_visible_child_name("list");
                    (lp.navigate_to)(target_dir, true, nav_mode);
                    trim_glibc_heap();
                }
            },
        );
    })
}

/// Returns freed temporary heap pages (from listing/indexing) to the OS on glibc.
fn trim_glibc_heap() {
    #[cfg(target_env = "gnu")]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::os::raw::c_int;
        }
        unsafe {
            malloc_trim(0);
        }
    }
}

fn install_header_actions(
    widgets: &ViewerWidgets,
    state: &Rc<RefCell<ViewerState>>,
    list_page: &Rc<OnceCell<ListPage>>,
    open_archive: &Rc<dyn Fn(PathBuf)>,
) {
    {
        let win_ref = widgets.window.clone();
        let open_fn = open_archive.clone();
        widgets.open_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title(tr("Open Archive")).build();
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
        let win_ref = widgets.window.clone();
        let open_fn = open_archive.clone();
        widgets.new_btn.connect_clicked(move |_| {
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
        let win_ref = widgets.window.clone();
        let state_ref = state.clone();
        let lp_ref = list_page.clone();
        widgets.extract_btn.connect_clicked(move |_| {
            if let Some(lp) = lp_ref.get() {
                start_extract_dialog(&win_ref, &state_ref, &lp.selection, false);
            }
        });
    }
    {
        let win_ref = widgets.window.clone();
        let state_ref = state.clone();
        widgets.test_btn.connect_clicked(move |_| {
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
                &tr("Testing Archive Integrity"),
                tr("Integrity Check Failed"),
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
                        let detail = trn(
                            "The archive is intact.\nVerified {count} entry ({size}) with 0 errors.",
                            "The archive is intact.\nVerified {count} entries ({size}) with 0 errors.",
                            entries_checked as u64,
                        )
                        .replace("{count}", &entries_checked.to_string())
                        .replace("{size}", &format_size(bytes_checked));
                        show_message_dialog(
                            Some(&win_done),
                            &tr("Integrity Check Passed"),
                            &detail,
                        );
                    }
                },
            );
        });
    }
    {
        let win_ref = widgets.window.clone();
        let state_ref = state.clone();
        widgets.props_btn.connect_clicked(move |_| {
            if let Some(info) = state_ref.borrow().properties_info() {
                show_properties_dialog(win_ref.upcast_ref(), &[], &info);
            }
        });
    }
}
