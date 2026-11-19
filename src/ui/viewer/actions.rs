use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::archive::{
    add_to_archive, extract_archive_with_session, is_supported_archive_path, ArchiveEntry,
    EntryKind, ExtractSession, OperationSummary,
};
use crate::ui::dialogs::{show_create_dialog, show_properties_dialog};
use crate::ui::{select_folder_dialog, session_temp_dir, show_error_dialog, show_info_dialog};

use super::columns::{build_context_menu_model, CellRightClickFn};
use super::models::{
    collect_selected_paths, collect_selected_rows, collect_staged_files, compute_selected_bytes,
    ArchiveDragProvider, ArchiveRowObject, NavFn, NavMode, ViewerState,
};
use super::run_viewer_worker;

pub(super) fn build_context_menu(
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
                && !(selected_rows[0].is_dir()
                    || selected_rows[0].entry_ref().kind == EntryKind::Symlink);
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

#[allow(clippy::too_many_arguments)]
pub(super) fn install_context_actions(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
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
                && !row.is_dir()
            {
                activate_row(&win_ref, &state_ref, row, true, &nav);
            }
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        act_extract.connect_activate(move |_, _| {
            start_extract_dialog(&win_ref, &state_ref, &selection_ref, true);
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        act_copy.connect_activate(move |_, _| {
            copy_selected_to_clipboard(&win_ref, &state_ref, &selection_ref);
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

pub(super) fn install_dnd(
    window: &gtk::ApplicationWindow,
    stack: &gtk::Stack,
    column_view: &gtk::ColumnView,
    state: &Rc<RefCell<ViewerState>>,
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
        let selection_ref = selection.clone();
        let win_ref = window.clone();
        drag_source.connect_prepare(move |_, _, _| {
            let st = state_ref.borrow();
            let arch = st.current_archive.as_ref()?;
            let selected = collect_selected_paths(&selection_ref);
            if selected.is_empty() {
                return None;
            }
            let total_bytes = compute_selected_bytes(&st.entries, Some(&selected));
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

pub(super) fn activate_row(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    row: &ArchiveRowObject,
    always_ask: bool,
    navigate_to: &NavFn,
) {
    let entry = row.entry_ref();
    let is_dir = row.is_dir();
    let is_symlink = entry.kind == EntryKind::Symlink;
    let full_path = entry.path.clone();
    let name = row.name().to_string();
    let parent_dir = row.parent_dir().to_string();
    let entry_size = entry.size;

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

pub(super) fn start_extract_dialog(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
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
            compute_selected_bytes(&st.entries, selected_opt.as_deref())
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
        let bytes = compute_selected_bytes(&st.entries, Some(&selected));
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
