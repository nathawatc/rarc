use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::archive::{
    add_to_archive, extract_archive, list_archive, spawn_worker, ArchiveEntry, ArchiveError,
    EntryKind, OperationSummary, WorkerEvent,
};
use crate::ui::dialogs::{prompt_password_async, run_worker_with_progress, show_create_dialog};
use crate::ui::{format_mtime, format_size, session_temp_dir, show_error_dialog};

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct ArchiveRowData {
        pub name: String,
        pub full_path: String,
        pub parent_dir: String,
        pub size: u64,
        pub compressed_size: Option<u64>,
        pub type_label: String,
        pub mtime: Option<i64>,
        pub is_dir: bool,
        pub is_encrypted: bool,
    }

    #[derive(Default)]
    pub struct ArchiveRowObject {
        pub data: RefCell<ArchiveRowData>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArchiveRowObject {
        const NAME: &'static str = "RarcArchiveRow";
        type Type = super::ArchiveRowObject;
    }

    impl ObjectImpl for ArchiveRowObject {}

    #[derive(Default)]
    pub struct ArchiveDragProvider {
        pub archive_path: RefCell<PathBuf>,
        pub selected_paths: RefCell<Vec<String>>,
        pub strip_prefix: RefCell<Option<String>>,
        pub password: RefCell<Option<String>>,
        pub window: glib::WeakRef<gtk::Window>,
        pub seq: Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArchiveDragProvider {
        const NAME: &'static str = "RarcDragContentProvider";
        type Type = super::ArchiveDragProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for ArchiveDragProvider {}

    impl gdk::subclass::content_provider::ContentProviderImpl for ArchiveDragProvider {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormats::builder()
                .add_mime_type("text/uri-list")
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), glib::Error>> + 'static>> {
            let mime = mime_type.to_string();
            let stream = stream.clone();
            let archive_path = self.archive_path.borrow().clone();
            let selected = self.selected_paths.borrow().clone();
            let strip_prefix = self.strip_prefix.borrow().clone();
            let password = self.password.borrow().clone();
            let window = self.window.upgrade();
            let seq = self.seq.get().wrapping_add(1);
            self.seq.set(seq);

            Box::pin(async move {
                if mime != "text/uri-list" || selected.is_empty() {
                    return Err(glib::Error::new(
                        gio::IOErrorEnum::NotSupported,
                        "Unsupported drag mime type or empty selection",
                    ));
                }

                let stage_root = session_temp_dir().map_err(|e| {
                    glib::Error::new(gio::IOErrorEnum::Failed, &format!("Temp dir error: {e}"))
                })?;
                let stage_dir = stage_root.join(format!("dnd-{seq}"));
                let stage_worker = stage_dir.clone();
                let selected_worker = selected.clone();
                let cancel = Arc::new(AtomicBool::new(false));
                let cancel_worker = cancel.clone();

                let strip_worker = strip_prefix.clone();
                let rx = spawn_worker(move |tx| {
                    let (_, pw) = extract_archive(
                        &archive_path,
                        &stage_worker,
                        Some(&selected_worker),
                        strip_worker.as_deref(),
                        password.as_deref(),
                        Some(tx),
                        &cancel_worker,
                    )?;
                    Ok(OperationSummary::Extracted { password: pw })
                });

                while let Ok(ev) = rx.recv().await {
                    match ev {
                        WorkerEvent::NeedPassword {
                            archive_name,
                            retry,
                            reply,
                        } => {
                            let answer = if let Some(ref win) = window {
                                prompt_password_async(win, &archive_name, retry).await
                            } else {
                                None
                            };
                            let _ = reply.send(answer).await;
                        }
                        WorkerEvent::FileConflict { reply, .. } => {
                            let _ = reply.send(crate::archive::ConflictResolution::OverwriteAll).await;
                        }
                        WorkerEvent::Finished(Ok(_)) => break,
                        WorkerEvent::Finished(Err(err)) => {
                            return Err(glib::Error::new(
                                gio::IOErrorEnum::Failed,
                                &err.to_string(),
                            ));
                        }
                        WorkerEvent::Progress { .. } => {}
                    }
                }

                let prefix_slash = strip_prefix
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("{}/", s.trim_end_matches('/')));

                let mut uri_list = String::new();
                for sel in &selected {
                    let rel = if let Some(ref pre) = prefix_slash {
                        sel.strip_prefix(pre).unwrap_or(sel.as_str())
                    } else {
                        sel.as_str()
                    };
                    let extracted = stage_dir.join(rel);
                    if extracted.exists() {
                        let uri = gio::File::for_path(&extracted).uri();
                        uri_list.push_str(uri.as_str());
                        uri_list.push_str("\r\n");
                    }
                }

                stream
                    .write_all_future(uri_list.into_bytes(), io_priority)
                    .await
                    .map(|_| ())
                    .map_err(|(_, e)| e)
            })
        }
    }
}

glib::wrapper! {
    pub struct ArchiveRowObject(ObjectSubclass<imp::ArchiveRowObject>);
}

impl ArchiveRowObject {
    fn new(data: imp::ArchiveRowData) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().data.borrow_mut() = data;
        obj
    }

    fn with_data<R>(&self, f: impl FnOnce(&imp::ArchiveRowData) -> R) -> R {
        f(&self.imp().data.borrow())
    }
}

glib::wrapper! {
    pub struct ArchiveDragProvider(ObjectSubclass<imp::ArchiveDragProvider>)
        @extends gdk::ContentProvider;
}

impl ArchiveDragProvider {
    fn new(
        archive_path: PathBuf,
        selected_paths: Vec<String>,
        strip_prefix: Option<String>,
        password: Option<String>,
        window: &gtk::Window,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        *imp.archive_path.borrow_mut() = archive_path;
        *imp.selected_paths.borrow_mut() = selected_paths;
        *imp.strip_prefix.borrow_mut() = strip_prefix;
        *imp.password.borrow_mut() = password;
        imp.window.set(Some(window));
        obj
    }
}

#[derive(Default)]
struct ViewerState {
    current_archive: Option<PathBuf>,
    cached_password: Option<String>,
    current_dir: String,
    search_query: String,
    preview_seq: u64,
}

/// Launches the standalone archive viewer/manager window.
pub fn launch_standalone(app: &gtk::Application, initial_archive: Option<PathBuf>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("rarc — Archive Manager")
        .default_width(920)
        .default_height(600)
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

    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Filter entries…")
        .width_chars(22)
        .build();

    header.pack_start(&open_btn);
    header.pack_start(&new_btn);
    header.pack_start(&extract_btn);
    header.pack_end(&search_entry);
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
                        || d.full_path.to_ascii_lowercase().contains(q)
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

    append_columns(&column_view);

    let sort_model = gtk::SortListModel::new(Some(filter_model), column_view.sorter());
    let selection = gtk::MultiSelection::new(Some(sort_model));
    column_view.set_model(Some(&selection));

    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&column_view)
        .build();

    // Empty state placeholder
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
    stack.add_named(&scrolled, Some("list"));
    stack.set_visible_child_name("empty");

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.append(&breadcrumb_bar);
    root_box.append(&stack);
    window.set_child(Some(&root_box));

    // Shared UI refresh closure
    let rebuild_breadcrumbs: Rc<dyn Fn()> = {
        let state_ref = state.clone();
        let crumbs_box = crumbs_box.clone();
        let up_btn = up_btn.clone();
        let filter_ref = filter.clone();
        let search_ref = search_entry.clone();
        let selection_ref = selection.clone();

        Rc::new(move || {
            update_breadcrumbs(
                &crumbs_box,
                &up_btn,
                &state_ref,
                &filter_ref,
                &search_ref,
                &selection_ref,
            );
        })
    };

    // Up button action
    {
        let state_ref = state.clone();
        let filter_ref = filter.clone();
        let rebuild = rebuild_breadcrumbs.clone();
        let selection_ref = selection.clone();
        up_btn.connect_clicked(move |_| {
            {
                let mut st = state_ref.borrow_mut();
                if let Some((parent, _)) = st.current_dir.rsplit_once('/') {
                    st.current_dir = parent.to_string();
                } else {
                    st.current_dir.clear();
                }
            }
            selection_ref.unselect_all();
            filter_ref.changed(gtk::FilterChange::Different);
            rebuild();
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

    // Archive loader closure
    let open_archive: Rc<dyn Fn(PathBuf)> = {
        let window_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let filter_ref = filter.clone();
        let stack_ref = stack.clone();
        let extract_ref = extract_btn.clone();
        let search_ref = search_entry.clone();
        let rebuild = rebuild_breadcrumbs.clone();

        Rc::new(move |archive_path: PathBuf| {
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = cancel.clone();
            let path_worker = archive_path.clone();
            let initial_pw = {
                let st = state_ref.borrow();
                if st.current_archive.as_ref() == Some(&archive_path) {
                    st.cached_password.clone()
                } else {
                    None
                }
            };

            let rx = spawn_worker(move |tx| {
                let listing = list_archive(
                    &path_worker,
                    initial_pw.as_deref(),
                    Some(tx),
                    &cancel_worker,
                )?;
                Ok(OperationSummary::Listed(listing))
            });

            let win_done = window_ref.clone();
            let st_done = state_ref.clone();
            let store_done = store_ref.clone();
            let filter_done = filter_ref.clone();
            let stack_done = stack_ref.clone();
            let extract_done = extract_ref.clone();
            let search_done = search_ref.clone();
            let rebuild_done = rebuild.clone();
            let path_done = archive_path.clone();

            run_worker_with_progress(
                None,
                Some(window_ref.upcast_ref()),
                &format!(
                    "Opening {}",
                    archive_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("archive")
                ),
                rx,
                cancel,
                move |res| match res {
                    Ok(OperationSummary::Listed(listing)) => {
                        let rows = build_row_objects(&listing.entries);
                        store_done.remove_all();
                        store_done.extend_from_slice(&rows);
                        {
                            let mut st = st_done.borrow_mut();
                            let same_archive = st.current_archive.as_ref() == Some(&path_done);
                            st.current_archive = Some(path_done.clone());
                            st.cached_password = listing.password;
                            if !same_archive {
                                st.current_dir.clear();
                            }
                            st.search_query.clear();
                        }
                        if !search_done.text().is_empty() {
                            search_done.set_text("");
                        }
                        let title_name = path_done
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("rarc");
                        win_done.set_title(Some(&format!("{title_name} — rarc")));
                        extract_done.set_sensitive(true);
                        stack_done.set_visible_child_name("list");
                        filter_done.changed(gtk::FilterChange::Different);
                        rebuild_done();
                    }
                    Ok(_) | Err(ArchiveError::Cancelled) => {}
                    Err(err) => {
                        show_error_dialog(
                            Some(&win_done),
                            "Unable to Open Archive",
                            &err.to_string(),
                        );
                    }
                },
            );
        })
    };

    // HeaderBar: Open button
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

    // HeaderBar: Create button
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

    // HeaderBar: Extract button
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        extract_btn.connect_clicked(move |_| {
            let Some(archive_path) = state_ref.borrow().current_archive.clone() else {
                return;
            };
            let selected = collect_selected_paths(&selection_ref);
            let selected_opt = if selected.is_empty() {
                None
            } else {
                Some(selected)
            };
            let (strip_prefix, cached_pw) = {
                let st = state_ref.borrow();
                let strip = if selected_opt.is_some() && !st.current_dir.is_empty() {
                    Some(st.current_dir.clone())
                } else {
                    None
                };
                (strip, st.cached_password.clone())
            };

            let initial_folder = archive_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(gio::File::for_path);
            let mut builder = gtk::FileDialog::builder().title("Extract To Folder");
            if let Some(ref f) = initial_folder {
                builder = builder.initial_folder(f);
            }
            let dialog = builder.build();

            let win_extract = win_ref.clone();
            let state_done = state_ref.clone();
            dialog.select_folder(Some(&win_ref), gio::Cancellable::NONE, move |res| {
                let Ok(folder) = res else { return };
                let Some(dest_dir) = folder.path() else { return };

                let cancel = Arc::new(AtomicBool::new(false));
                let cancel_worker = cancel.clone();
                let arch_worker = archive_path.clone();
                let dest_worker = dest_dir.clone();
                let rx = spawn_worker(move |tx| {
                    let (_, pw) = extract_archive(
                        &arch_worker,
                        &dest_worker,
                        selected_opt.as_deref(),
                        strip_prefix.as_deref(),
                        cached_pw.as_deref(),
                        Some(tx),
                        &cancel_worker,
                    )?;
                    Ok(OperationSummary::Extracted { password: pw })
                });

                let win_err = win_extract.clone();
                run_worker_with_progress(
                    None,
                    Some(win_extract.upcast_ref()),
                    "Extracting Archive",
                    rx,
                    cancel,
                    move |res| match res {
                        Ok(OperationSummary::Extracted { password, .. }) => {
                            if let Some(pw) = password {
                                state_done.borrow_mut().cached_password = Some(pw);
                            }
                        }
                        Ok(_) | Err(ArchiveError::Cancelled) => {}
                        Err(err) => {
                            show_error_dialog(
                                Some(&win_err),
                                "Extraction Failed",
                                &err.to_string(),
                            );
                        }
                    },
                );
            });
        });
    }

    // Double-click activation: enter directory or extract & launch file
    {
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let filter_ref = filter.clone();
        let search_ref = search_entry.clone();
        let rebuild = rebuild_breadcrumbs.clone();
        let win_ref = window.clone();

        column_view.connect_activate(move |_, pos| {
            let Some(item) = selection_ref.item(pos) else {
                return;
            };
            let Ok(row) = item.downcast::<ArchiveRowObject>() else {
                return;
            };
            let (is_dir, full_path, name, parent_dir) = row.with_data(|d| {
                (
                    d.is_dir,
                    d.full_path.clone(),
                    d.name.clone(),
                    d.parent_dir.clone(),
                )
            });

            if is_dir {
                state_ref.borrow_mut().current_dir = full_path;
                if !search_ref.text().is_empty() {
                    search_ref.set_text("");
                }
                selection_ref.unselect_all();
                filter_ref.changed(gtk::FilterChange::Different);
                rebuild();
                return;
            }

            let Some(archive_path) = state_ref.borrow().current_archive.clone() else {
                return;
            };
            let seq = {
                let mut st = state_ref.borrow_mut();
                st.preview_seq += 1;
                st.preview_seq
            };
            let Ok(temp_root) = session_temp_dir() else {
                return;
            };
            let preview_dir = temp_root.join(format!("preview-{seq}"));
            let cached_pw = state_ref.borrow().cached_password.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = cancel.clone();
            let target_file = preview_dir.join(&name);
            let strip = if parent_dir.is_empty() {
                None
            } else {
                Some(parent_dir)
            };

            let rx = spawn_worker(move |tx| {
                let (_, pw) = extract_archive(
                    &archive_path,
                    &preview_dir,
                    Some(&[full_path]),
                    strip.as_deref(),
                    cached_pw.as_deref(),
                    Some(tx),
                    &cancel_worker,
                )?;
                Ok(OperationSummary::Extracted { password: pw })
            });

            let win_launch = win_ref.clone();
            let state_pw = state_ref.clone();
            run_worker_with_progress(
                None,
                Some(win_ref.upcast_ref()),
                &format!("Opening {name}"),
                rx,
                cancel,
                move |res| match res {
                    Ok(OperationSummary::Extracted { password, .. }) => {
                        if let Some(pw) = password {
                            state_pw.borrow_mut().cached_password = Some(pw);
                        }
                        if target_file.exists() {
                            let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&target_file)));
                            let win_err = win_launch.clone();
                            launcher.launch(Some(&win_launch), gio::Cancellable::NONE, move |lres| {
                                if let Err(e) = lres {
                                    show_error_dialog(
                                        Some(&win_err),
                                        "Unable to Launch File",
                                        e.message(),
                                    );
                                }
                            });
                        }
                    }
                    Ok(_) | Err(ArchiveError::Cancelled) => {}
                    Err(err) => {
                        show_error_dialog(Some(&win_launch), "Preview Failed", &err.to_string());
                    }
                },
            );
        });
    }

    // Drag-and-Drop IN: drop files from Thunar into the window
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
                let cancel = Arc::new(AtomicBool::new(false));
                let cancel_worker = cancel.clone();
                let arch_worker = arch_path.clone();
                let rx = spawn_worker(move |tx| {
                    add_to_archive(
                        &arch_worker,
                        &paths,
                        &dest_subdir,
                        cached_pw.as_deref(),
                        Some(tx),
                        &cancel_worker,
                    )?;
                    Ok(OperationSummary::Added(arch_worker))
                });
                let win_err = win_ref.clone();
                let reload = open_fn.clone();
                run_worker_with_progress(
                    None,
                    Some(win_ref.upcast_ref()),
                    "Adding Files to Archive",
                    rx,
                    cancel,
                    move |res| match res {
                        Ok(OperationSummary::Added(archive_path)) => reload(archive_path),
                        Ok(_) | Err(ArchiveError::Cancelled) => {}
                        Err(err) => {
                            show_error_dialog(
                                Some(&win_err),
                                "Failed to Add Files",
                                &err.to_string(),
                            );
                        }
                    },
                );
                true
            } else if paths.len() == 1 && paths[0].is_file() && looks_like_archive(&paths[0]) {
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

    // Drag-and-Drop OUT: drag selected entries to Thunar / folders
    {
        let drag_source = gtk::DragSource::builder()
            .actions(gdk::DragAction::COPY)
            .build();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let win_ref = window.clone();
        drag_source.connect_prepare(move |_, _, _| {
            let st = state_ref.borrow();
            let arch = st.current_archive.as_ref()?;
            let selected = collect_selected_paths(&selection_ref);
            if selected.is_empty() {
                return None;
            }
            let strip = if st.current_dir.is_empty() {
                None
            } else {
                Some(st.current_dir.clone())
            };
            let provider = ArchiveDragProvider::new(
                arch.clone(),
                selected,
                strip,
                st.cached_password.clone(),
                win_ref.upcast_ref(),
            );
            Some(provider.upcast())
        });
        column_view.add_controller(drag_source);
    }

    window.present();
    if let Some(path) = initial_archive {
        open_archive(path);
    }
}

fn update_breadcrumbs(
    crumbs_box: &gtk::Box,
    up_btn: &gtk::Button,
    state_ref: &Rc<RefCell<ViewerState>>,
    filter_ref: &gtk::CustomFilter,
    search_ref: &gtk::SearchEntry,
    selection_ref: &gtk::MultiSelection,
) {
    while let Some(child) = crumbs_box.first_child() {
        crumbs_box.remove(&child);
    }
    let current_dir = state_ref.borrow().current_dir.clone();
    up_btn.set_sensitive(!current_dir.is_empty());

    let archive_label = state_ref
        .borrow()
        .current_archive
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("Archive")
        .to_string();

    let mut segments: Vec<(String, String)> = vec![(archive_label, String::new())];
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
        let st_click = state_ref.clone();
        let filter_click = filter_ref.clone();
        let crumbs_ref = crumbs_box.clone();
        let up_ref = up_btn.clone();
        let search_click = search_ref.clone();
        let sel_click = selection_ref.clone();
        btn.connect_clicked(move |_| {
            st_click.borrow_mut().current_dir = target_dir.clone();
            if !search_click.text().is_empty() {
                search_click.set_text("");
            }
            sel_click.unselect_all();
            filter_click.changed(gtk::FilterChange::Different);
            update_breadcrumbs(
                &crumbs_ref,
                &up_ref,
                &st_click,
                &filter_click,
                &search_click,
                &sel_click,
            );
        });
        crumbs_box.append(&btn);
    }
}

fn collect_selected_paths(selection: &gtk::MultiSelection) -> Vec<String> {
    let bitset = selection.selection();
    let mut paths = Vec::new();
    if let Some((iter, first)) = gtk::BitsetIter::init_first(&bitset) {
        if let Some(obj) = selection.item(first)
            && let Ok(row) = obj.downcast::<ArchiveRowObject>()
        {
            paths.push(row.with_data(|d| d.full_path.clone()));
        }
        for pos in iter {
            if let Some(obj) = selection.item(pos)
                && let Ok(row) = obj.downcast::<ArchiveRowObject>()
            {
                paths.push(row.with_data(|d| d.full_path.clone()));
            }
        }
    }
    paths
}

fn build_row_objects(entries: &[ArchiveEntry]) -> Vec<ArchiveRowObject> {
    let mut rows = Vec::with_capacity(entries.len());
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut needed_dirs: Vec<String> = Vec::new();

    for entry in entries {
        let is_dir = entry.kind == EntryKind::Directory;
        let (parent_dir, name) = split_parent_and_name(&entry.path);
        seen_paths.insert(entry.path.clone());

        // Ensure all ancestor virtual folders exist for folder navigation.
        if !parent_dir.is_empty() {
            let mut acc = String::new();
            for part in parent_dir.split('/') {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                if !seen_paths.contains(&acc) {
                    needed_dirs.push(acc.clone());
                }
            }
        }

        rows.push(ArchiveRowObject::new(imp::ArchiveRowData {
            type_label: entry.kind.label(&name),
            name,
            full_path: entry.path.clone(),
            parent_dir,
            size: entry.size,
            compressed_size: entry.compressed_size,
            mtime: entry.mtime,
            is_dir,
            is_encrypted: entry.is_encrypted,
        }));
    }

    for dir_path in needed_dirs {
        if seen_paths.insert(dir_path.clone()) {
            let (parent_dir, name) = split_parent_and_name(&dir_path);
            rows.push(ArchiveRowObject::new(imp::ArchiveRowData {
                name,
                full_path: dir_path,
                parent_dir,
                size: 0,
                compressed_size: None,
                type_label: "Folder".to_string(),
                mtime: None,
                is_dir: true,
                is_encrypted: false,
            }));
        }
    }

    rows
}

fn split_parent_and_name(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

fn append_columns(view: &gtk::ColumnView) {
    // Name column (icon + lock badge + label)
    let name_factory = gtk::SignalListItemFactory::new();
    name_factory.connect_setup(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = gtk::Image::new();
        let lock = gtk::Image::from_icon_name("channel-secure-symbolic");
        lock.set_visible(false);
        lock.set_tooltip_text(Some("Encrypted"));
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_hexpand(true);
        hbox.append(&icon);
        hbox.append(&label);
        hbox.append(&lock);
        item.set_child(Some(&hbox));
    });
    name_factory.connect_bind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let row = item.item().and_downcast::<ArchiveRowObject>().unwrap();
        let hbox = item.child().and_downcast::<gtk::Box>().unwrap();
        let icon = hbox.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
        let lock = label.next_sibling().and_downcast::<gtk::Image>().unwrap();
        row.with_data(|d| {
            icon.set_icon_name(Some(if d.is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            label.set_text(&d.name);
            lock.set_visible(d.is_encrypted);
        });
    });
    let name_sorter = gtk::CustomSorter::new(|a, b| {
        let ra = a.downcast_ref::<ArchiveRowObject>().unwrap();
        let rb = b.downcast_ref::<ArchiveRowObject>().unwrap();
        ra.with_data(|da| {
            rb.with_data(|db| {
                db.is_dir
                    .cmp(&da.is_dir)
                    .then_with(|| da.name.to_ascii_lowercase().cmp(&db.name.to_ascii_lowercase()))
                    .into()
            })
        })
    });
    let name_col = gtk::ColumnViewColumn::builder()
        .title("Name")
        .factory(&name_factory)
        .sorter(&name_sorter)
        .expand(true)
        .resizable(true)
        .build();
    view.append_column(&name_col);
    view.sort_by_column(Some(&name_col), gtk::SortType::Ascending);

    // Uncompressed Size
    let size_col = make_text_column(
        "Uncompressed Size",
        1.0,
        true,
        130,
        |d| {
            if d.is_dir {
                "—".to_string()
            } else {
                format_size(d.size)
            }
        },
        |da, db| da.size.cmp(&db.size),
    );
    view.append_column(&size_col);

    // Compressed Size
    let comp_col = make_text_column(
        "Compressed Size",
        1.0,
        true,
        130,
        |d| match d.compressed_size {
            Some(sz) if !d.is_dir => format_size(sz),
            _ => "—".to_string(),
        },
        |da, db| da.compressed_size.cmp(&db.compressed_size),
    );
    view.append_column(&comp_col);

    // Type
    let type_col = make_text_column(
        "Type",
        0.0,
        false,
        120,
        |d| d.type_label.clone(),
        |da, db| da.type_label.cmp(&db.type_label),
    );
    view.append_column(&type_col);

    // Modified Date
    let mtime_col = make_text_column(
        "Modified Date",
        0.0,
        true,
        150,
        |d| format_mtime(d.mtime),
        |da, db| da.mtime.cmp(&db.mtime),
    );
    view.append_column(&mtime_col);
}

fn make_text_column<FText, FSorter>(
    title: &str,
    xalign: f32,
    numeric: bool,
    fixed_width: i32,
    format_fn: FText,
    sort_fn: FSorter,
) -> gtk::ColumnViewColumn
where
    FText: Fn(&imp::ArchiveRowData) -> String + 'static,
    FSorter: Fn(&imp::ArchiveRowData, &imp::ArchiveRowData) -> std::cmp::Ordering + 'static,
{
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.set_xalign(xalign);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if numeric {
            label.add_css_class("rarc-numeric-cell");
        }
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let row = item.item().and_downcast::<ArchiveRowObject>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        row.with_data(|d| label.set_text(&format_fn(d)));
    });

    let sorter = gtk::CustomSorter::new(move |a, b| {
        let ra = a.downcast_ref::<ArchiveRowObject>().unwrap();
        let rb = b.downcast_ref::<ArchiveRowObject>().unwrap();
        ra.with_data(|da| rb.with_data(|db| sort_fn(da, db).into()))
    });

    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(&sorter)
        .fixed_width(fixed_width)
        .resizable(true)
        .build()
}

fn looks_like_archive(path: &Path) -> bool {
    let lower = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    [
        ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tzst", ".tar.bz2", ".tbz2",
        ".zip", ".7z", ".rar", ".iso", ".cab", ".cpio", ".ar", ".deb", ".rpm", ".jar", ".apk",
        ".gz", ".xz", ".zst", ".bz2", ".lz4",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}
