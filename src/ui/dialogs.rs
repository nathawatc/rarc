use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_channel::Receiver;
use gtk4 as gtk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::archive::{
    create_archive, ArchiveError, ArchiveFormat, ConflictResolution, OperationSummary, WorkerEvent,
};
use crate::ui::{format_mtime, format_size, show_error_dialog};

/// Runs a worker `Receiver<WorkerEvent>` with a compact progress window and modal prompts.
pub fn run_worker_with_progress<F>(
    app: Option<&gtk::Application>,
    parent: Option<&gtk::Window>,
    title: &str,
    rx: Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    on_done: F,
) where
    F: FnOnce(Result<OperationSummary, ArchiveError>) + 'static,
{
    let win = gtk::Window::builder()
        .title(title)
        .modal(parent.is_some())
        .resizable(false)
        .default_width(420)
        .build();
    if let Some(a) = app {
        win.set_application(Some(a));
    }
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
    }

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 10);
    vbox.add_css_class("rarc-dialog-body");

    let heading = gtk::Label::new(Some(title));
    heading.set_xalign(0.0);
    heading.add_css_class("title-4");

    let file_label = gtk::Label::new(Some("Preparing…"));
    file_label.set_xalign(0.0);
    file_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    file_label.add_css_class("rarc-progress-file");

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_hexpand(true);

    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.set_halign(gtk::Align::End);

    {
        let cancel_flag = cancel.clone();
        let label = file_label.clone();
        cancel_btn.connect_clicked(move |btn| {
            cancel_flag.store(true, Ordering::Relaxed);
            btn.set_sensitive(false);
            label.set_text("Cancelling…");
        });
    }

    {
        let cancel_flag = cancel.clone();
        let label = file_label.clone();
        let btn = cancel_btn.clone();
        win.connect_close_request(move |_| {
            cancel_flag.store(true, Ordering::Relaxed);
            btn.set_sensitive(false);
            label.set_text("Cancelling…");
            glib::Propagation::Stop
        });
    }

    vbox.append(&heading);
    vbox.append(&file_label);
    vbox.append(&progress);
    vbox.append(&cancel_btn);
    win.set_child(Some(&vbox));
    win.present();

    let win_ref = win.clone();
    let app_hold = app.map(|a| a.hold());
    glib::spawn_future_local(async move {
        let _hold = app_hold;
        let mut final_res: Result<OperationSummary, ArchiveError> = Err(ArchiveError::Cancelled);
        while let Ok(ev) = rx.recv().await {
            match ev {
                WorkerEvent::Progress {
                    current_file,
                    bytes_done,
                    bytes_total,
                } => {
                    file_label.set_text(&current_file);
                    if bytes_total > 0 {
                        let frac = (bytes_done as f64 / bytes_total as f64).clamp(0.0, 1.0);
                        progress.set_fraction(frac);
                        progress.set_text(Some(&format!("{:.0}%", frac * 100.0)));
                    } else {
                        progress.pulse();
                    }
                }
                WorkerEvent::NeedPassword {
                    archive_name,
                    retry,
                    reply,
                } => {
                    let answer = prompt_password_async(&win_ref, &archive_name, retry).await;
                    let _ = reply.send(answer).await;
                }
                WorkerEvent::FileConflict { target_path, reply } => {
                    if let Some(res) = prompt_conflict_async(&win_ref, &target_path).await {
                        let _ = reply.send(res).await;
                    } else {
                        drop(reply);
                    }
                }
                WorkerEvent::Finished(res) => {
                    final_res = res;
                    break;
                }
            }
        }
        win_ref.destroy();
        on_done(final_res);
    });
}

/// Shows a modal passphrase prompt and resolves with `Some(password)` or `None` if cancelled.
pub async fn prompt_password_async(
    parent: &gtk::Window,
    archive_name: &str,
    retry: bool,
) -> Option<String> {
    let (tx, rx) = async_channel::bounded(1);
    let tx_cell = Rc::new(RefCell::new(Some(tx)));

    let win = gtk::Window::builder()
        .title("Password Required")
        .modal(true)
        .resizable(false)
        .transient_for(parent)
        .default_width(360)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 10);
    vbox.add_css_class("rarc-dialog-body");

    let msg = gtk::Label::new(Some(&format!(
        "Enter the passphrase for \"{archive_name}\":"
    )));
    msg.set_xalign(0.0);
    msg.set_wrap(true);
    vbox.append(&msg);

    if retry {
        let warn = gtk::Label::new(Some("Incorrect passphrase. Please try again."));
        warn.set_xalign(0.0);
        warn.add_css_class("error");
        warn.add_css_class("rarc-hint");
        vbox.append(&warn);
    }

    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    vbox.append(&entry);

    let btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btn_box.set_halign(gtk::Align::End);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let ok_btn = gtk::Button::with_label("Unlock");
    ok_btn.add_css_class("suggested-action");
    btn_box.append(&cancel_btn);
    btn_box.append(&ok_btn);
    vbox.append(&btn_box);

    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&ok_btn));

    let send_once = {
        let tx_cell = tx_cell.clone();
        let win_ref = win.clone();
        move |val: Option<String>| {
            if let Some(sender) = tx_cell.borrow_mut().take() {
                let _ = sender.try_send(val);
            }
            win_ref.close();
        }
    };

    {
        let send_once = send_once.clone();
        cancel_btn.connect_clicked(move |_| send_once(None));
    }
    {
        let send_once = send_once.clone();
        let entry_ref = entry.clone();
        ok_btn.connect_clicked(move |_| send_once(Some(entry_ref.text().to_string())));
    }
    {
        let tx_cell = tx_cell.clone();
        win.connect_close_request(move |_| {
            if let Some(sender) = tx_cell.borrow_mut().take() {
                let _ = sender.try_send(None);
            }
            glib::Propagation::Proceed
        });
    }

    win.present();
    entry.grab_focus();
    rx.recv().await.unwrap_or(None)
}

/// Shows a modal file conflict dialog (`Overwrite`, `Overwrite All`, `Skip`, `Skip All`, `Rename`).
async fn prompt_conflict_async(
    parent: &gtk::Window,
    target_path: &Path,
) -> Option<ConflictResolution> {
    let (tx, rx) = async_channel::bounded(1);
    let tx_cell = Rc::new(RefCell::new(Some(tx)));

    let win = gtk::Window::builder()
        .title("File Already Exists")
        .modal(true)
        .resizable(false)
        .transient_for(parent)
        .default_width(460)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.add_css_class("rarc-dialog-body");

    let msg = gtk::Label::new(Some(&format!(
        "A file already exists at:\n{}",
        target_path.display()
    )));
    msg.set_xalign(0.0);
    msg.set_wrap(true);
    msg.set_selectable(true);
    vbox.append(&msg);

    let orig_name = target_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("renamed")
        .to_string();
    let suggested = suggest_renamed_filename(&orig_name);

    let rename_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let rename_lbl = gtk::Label::new(Some("Rename to:"));
    let rename_entry = gtk::Entry::builder()
        .text(&suggested)
        .hexpand(true)
        .build();
    let rename_btn = gtk::Button::with_label("Rename");
    rename_row.append(&rename_lbl);
    rename_row.append(&rename_entry);
    rename_row.append(&rename_btn);
    vbox.append(&rename_row);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let skip_all_btn = gtk::Button::with_label("Skip All");
    let skip_btn = gtk::Button::with_label("Skip");
    let overwrite_all_btn = gtk::Button::with_label("Overwrite All");
    let overwrite_btn = gtk::Button::with_label("Overwrite");
    overwrite_btn.add_css_class("suggested-action");

    actions.append(&skip_all_btn);
    actions.append(&skip_btn);
    actions.append(&overwrite_all_btn);
    actions.append(&overwrite_btn);
    vbox.append(&actions);

    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&overwrite_btn));

    let send_once = {
        let tx_cell = tx_cell.clone();
        let win_ref = win.clone();
        move |res: Option<ConflictResolution>| {
            if let Some(sender) = tx_cell.borrow_mut().take() {
                let _ = sender.try_send(res);
            }
            win_ref.close();
        }
    };

    let bind_btn = |btn: &gtk::Button, resolution: ConflictResolution| {
        let send = send_once.clone();
        btn.connect_clicked(move |_| send(Some(resolution.clone())));
    };

    bind_btn(&skip_btn, ConflictResolution::Skip);
    bind_btn(&skip_all_btn, ConflictResolution::SkipAll);
    bind_btn(&overwrite_btn, ConflictResolution::Overwrite);
    bind_btn(&overwrite_all_btn, ConflictResolution::OverwriteAll);

    {
        let send = send_once.clone();
        let entry_ref = rename_entry.clone();
        rename_btn.connect_clicked(move |_| {
            let name = entry_ref.text().trim().to_string();
            if !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']) {
                send(Some(ConflictResolution::Rename(name)));
            }
        });
    }
    {
        let entry_ref = rename_entry.clone();
        let btn_ref = rename_btn.clone();
        entry_ref.connect_changed(move |e| {
            let txt = e.text();
            let t = txt.trim();
            let valid = !t.is_empty() && t != "." && t != ".." && !t.contains(['/', '\\']);
            btn_ref.set_sensitive(valid);
        });
    }
    {
        let tx_cell = tx_cell.clone();
        win.connect_close_request(move |_| {
            if let Some(sender) = tx_cell.borrow_mut().take() {
                let _ = sender.try_send(None);
            }
            glib::Propagation::Proceed
        });
    }

    win.present();
    rx.recv().await.unwrap_or(None)
}

fn suggest_renamed_filename(name: &str) -> String {
    let path = Path::new(name);
    match (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|s| s.to_str()),
    ) {
        (Some(stem), Some(ext)) if !stem.is_empty() => format!("{stem} (copy).{ext}"),
        _ => format!("{name} (copy)"),
    }
}

/// Opens the Create Archive dialog, runs creation with a progress window, and calls `on_created`.
pub fn show_create_dialog<F>(
    app: Option<&gtk::Application>,
    parent: Option<&gtk::Window>,
    initial_inputs: Vec<PathBuf>,
    default_dir: Option<PathBuf>,
    on_created: F,
) where
    F: Fn(Option<PathBuf>) + 'static,
{
    let win = gtk::Window::builder()
        .title("Create Archive")
        .modal(parent.is_some())
        .resizable(false)
        .default_width(460)
        .build();
    if let Some(a) = app {
        win.set_application(Some(a));
    }
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
    }

    let inputs_rc = Rc::new(RefCell::new(initial_inputs));
    let dest_dir_rc = Rc::new(RefCell::new(resolve_initial_dest_dir(
        &inputs_rc.borrow(),
        default_dir.as_deref(),
    )));

    let default_stem = {
        let inp = inputs_rc.borrow();
        if inp.len() == 1 {
            inp[0]
                .file_stem()
                .or_else(|| inp[0].file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("archive")
                .to_string()
        } else {
            "archive".to_string()
        }
    };

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.add_css_class("rarc-dialog-body");

    let grid = gtk::Grid::builder()
        .row_spacing(10)
        .column_spacing(12)
        .build();

    let name_lbl = gtk::Label::new(Some("Archive Name"));
    name_lbl.set_xalign(1.0);
    let name_entry = gtk::Entry::builder()
        .text(&default_stem)
        .hexpand(true)
        .activates_default(true)
        .build();

    let fmt_lbl = gtk::Label::new(Some("Format"));
    fmt_lbl.set_xalign(1.0);
    let fmt_labels: Vec<&str> = ArchiveFormat::ALL.iter().map(|f| f.label()).collect();
    let fmt_dropdown = gtk::DropDown::from_strings(&fmt_labels);
    fmt_dropdown.set_hexpand(true);

    let dir_lbl = gtk::Label::new(Some("Location"));
    dir_lbl.set_xalign(1.0);
    let dir_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let dir_display = gtk::Entry::builder()
        .text(dest_dir_rc.borrow().display().to_string())
        .editable(false)
        .hexpand(true)
        .build();
    let dir_btn = gtk::Button::with_label("Choose…");
    dir_box.append(&dir_display);
    dir_box.append(&dir_btn);

    let pass_lbl = gtk::Label::new(Some("Password"));
    pass_lbl.set_xalign(1.0);
    let pass_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let pass_entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .hexpand(true)
        .sensitive(false)
        .build();
    let pass_hint = gtk::Label::new(Some("Encryption is only supported for .zip archives"));
    pass_hint.set_xalign(0.0);
    pass_hint.add_css_class("rarc-hint");
    pass_box.append(&pass_entry);
    pass_box.append(&pass_hint);

    let files_lbl = gtk::Label::new(Some("Inputs"));
    files_lbl.set_xalign(1.0);
    let files_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let files_summary = gtk::Label::new(Some(&summarize_inputs(&inputs_rc.borrow())));
    files_summary.set_xalign(0.0);
    files_summary.set_hexpand(true);
    files_summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let add_files_btn = gtk::Button::with_label("Add Files…");
    let add_folder_btn = gtk::Button::with_label("Add Folder…");
    files_box.append(&files_summary);
    files_box.append(&add_files_btn);
    files_box.append(&add_folder_btn);

    grid.attach(&name_lbl, 0, 0, 1, 1);
    grid.attach(&name_entry, 1, 0, 1, 1);
    grid.attach(&fmt_lbl, 0, 1, 1, 1);
    grid.attach(&fmt_dropdown, 1, 1, 1, 1);
    grid.attach(&dir_lbl, 0, 2, 1, 1);
    grid.attach(&dir_box, 1, 2, 1, 1);
    grid.attach(&pass_lbl, 0, 3, 1, 1);
    grid.attach(&pass_box, 1, 3, 1, 1);
    grid.attach(&files_lbl, 0, 4, 1, 1);
    grid.attach(&files_box, 1, 4, 1, 1);
    vbox.append(&grid);

    let btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btn_box.set_halign(gtk::Align::End);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let create_btn = gtk::Button::with_label("Create");
    create_btn.add_css_class("suggested-action");
    create_btn.set_sensitive(!inputs_rc.borrow().is_empty());
    btn_box.append(&cancel_btn);
    btn_box.append(&create_btn);
    vbox.append(&btn_box);

    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&create_btn));

    {
        let pass_entry_ref = pass_entry.clone();
        let pass_hint_ref = pass_hint.clone();
        fmt_dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            let fmt = ArchiveFormat::ALL
                .get(idx)
                .copied()
                .unwrap_or(ArchiveFormat::TarZst);
            let enc_ok = fmt.supports_write_encryption();
            pass_entry_ref.set_sensitive(enc_ok);
            if !enc_ok {
                pass_entry_ref.set_text("");
                pass_hint_ref.set_text("Encryption is only supported for .zip archives");
            } else {
                pass_hint_ref.set_text("Optional AES-256 passphrase for .zip");
            }
        });
    }

    {
        let win_ref = win.clone();
        let dest_rc = dest_dir_rc.clone();
        let display_ref = dir_display.clone();
        dir_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Select Destination Folder")
                .initial_folder(&gio::File::for_path(&*dest_rc.borrow()))
                .build();
            let dest_rc = dest_rc.clone();
            let display_ref = display_ref.clone();
            dialog.select_folder(Some(&win_ref), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res
                    && let Some(path) = file.path()
                {
                    display_ref.set_text(&path.display().to_string());
                    *dest_rc.borrow_mut() = path;
                }
            });
        });
    }

    {
        let win_ref = win.clone();
        let inputs_ref = inputs_rc.clone();
        let summary_ref = files_summary.clone();
        let create_ref = create_btn.clone();
        add_files_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Select Files to Add").build();
            let inputs_ref = inputs_ref.clone();
            let summary_ref = summary_ref.clone();
            let create_ref = create_ref.clone();
            dialog.open_multiple(Some(&win_ref), gio::Cancellable::NONE, move |res| {
                if let Ok(list) = res {
                    let mut guard = inputs_ref.borrow_mut();
                    for i in 0..list.n_items() {
                        if let Some(obj) = list.item(i)
                            && let Ok(file) = obj.downcast::<gio::File>()
                            && let Some(path) = file.path()
                            && !guard.contains(&path)
                        {
                            guard.push(path);
                        }
                    }
                    summary_ref.set_text(&summarize_inputs(&guard));
                    create_ref.set_sensitive(!guard.is_empty());
                }
            });
        });
    }

    {
        let win_ref = win.clone();
        let inputs_ref = inputs_rc.clone();
        let summary_ref = files_summary.clone();
        let create_ref = create_btn.clone();
        add_folder_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Select Folder to Add").build();
            let inputs_ref = inputs_ref.clone();
            let summary_ref = summary_ref.clone();
            let create_ref = create_ref.clone();
            dialog.select_folder(Some(&win_ref), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res
                    && let Some(path) = file.path()
                {
                    let mut guard = inputs_ref.borrow_mut();
                    if !guard.contains(&path) {
                        guard.push(path);
                    }
                    summary_ref.set_text(&summarize_inputs(&guard));
                    create_ref.set_sensitive(!guard.is_empty());
                }
            });
        });
    }

    let on_created_rc = Rc::new(on_created);
    let completed = Rc::new(RefCell::new(false));
    {
        let win_ref = win.clone();
        cancel_btn.connect_clicked(move |_| win_ref.close());
    }
    {
        let completed_ref = completed.clone();
        let cb = on_created_rc.clone();
        win.connect_close_request(move |_| {
            if !*completed_ref.borrow() {
                cb(None);
            }
            glib::Propagation::Proceed
        });
    }

    {
        let app_cloned = app.cloned();
        let parent_cloned = parent.cloned();
        let win_ref = win.clone();
        let inputs_ref = inputs_rc.clone();
        let dest_ref = dest_dir_rc.clone();
        let completed_ref = completed.clone();
        let cb = on_created_rc.clone();
        create_btn.connect_clicked(move |_| {
            let raw_name = name_entry.text().trim().to_string();
            if raw_name.is_empty() || raw_name.contains(['/', '\\', '\0']) {
                show_error_dialog(
                    Some(&win_ref),
                    "Invalid Archive Name",
                    "Please enter a valid filename without path separators.",
                );
                return;
            }
            let fmt = ArchiveFormat::ALL
                .get(fmt_dropdown.selected() as usize)
                .copied()
                .unwrap_or(ArchiveFormat::TarZst);
            let output_path = build_output_path(&dest_ref.borrow(), &raw_name, fmt);
            let password = if fmt.supports_write_encryption() {
                let t = pass_entry.text().to_string();
                if t.is_empty() { None } else { Some(t) }
            } else {
                None
            };
            let inputs = inputs_ref.borrow().clone();

            *completed_ref.borrow_mut() = true;
            // Hold app lifetime across closing Create dialog and presenting Progress window.
            let _hold = app_cloned.as_ref().map(|a| a.hold());
            win_ref.destroy();

            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = cancel.clone();
            let out_worker = output_path.clone();
            let rx = crate::archive::spawn_worker(move |tx| {
                create_archive(
                    &out_worker,
                    fmt,
                    &inputs,
                    password.as_deref(),
                    Some(tx),
                    &cancel_worker,
                )?;
                Ok(OperationSummary::Created(out_worker))
            });

            let cb_done = cb.clone();
            let parent_err = parent_cloned.clone();
            run_worker_with_progress(
                app_cloned.as_ref(),
                parent_cloned.as_ref(),
                &format!(
                    "Creating {}",
                    output_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("archive")
                ),
                rx,
                cancel,
                move |res| match res {
                    Ok(OperationSummary::Created(output_path)) => cb_done(Some(output_path)),
                    Ok(_) => cb_done(None),
                    Err(ArchiveError::Cancelled) => cb_done(None),
                    Err(err) => {
                        show_error_dialog(
                            parent_err.as_ref(),
                            "Failed to Create Archive",
                            &err.to_string(),
                        );
                        cb_done(None);
                    }
                },
            );
        });
    }

    win.present();
}

fn resolve_initial_dest_dir(inputs: &[PathBuf], fallback: Option<&Path>) -> PathBuf {
    if let Some(first) = inputs.first()
        && let Some(parent) = first.parent().filter(|p| !p.as_os_str().is_empty())
    {
        return parent.to_path_buf();
    }
    if let Some(dir) = fallback {
        return dir.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn summarize_inputs(inputs: &[PathBuf]) -> String {
    match inputs.len() {
        0 => "No files selected".to_string(),
        1 => inputs[0]
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("1 item")
            .to_string(),
        n => format!("{n} items selected"),
    }
}

fn build_output_path(dir: &Path, name: &str, fmt: ArchiveFormat) -> PathBuf {
    let ext = fmt.extension();
    let lower = name.to_ascii_lowercase();
    let final_name = if lower.ends_with(ext) {
        name.to_string()
    } else {
        format!("{name}{ext}")
    };
    dir.join(final_name)
}

#[derive(Clone, Debug)]
pub struct PropertyEntry {
    pub name: String,
    pub full_path: String,
    pub type_label: String,
    pub size: u64,
    pub compressed_size: Option<u64>,
    pub mtime: Option<i64>,
    pub mode: u32,
    pub symlink_target: Option<String>,
    pub is_dir: bool,
    pub is_encrypted: bool,
}

pub struct ArchivePropertiesInfo {
    pub archive_path: PathBuf,
    pub entry_count: usize,
    pub total_uncompressed: u64,
    pub encrypted_count: usize,
}

/// Shows a Properties modal for a single entry, multiple selected entries, or the archive itself.
pub fn show_properties_dialog(
    parent: &gtk::Window,
    selected: &[PropertyEntry],
    archive_info: &ArchivePropertiesInfo,
) {
    let title = match selected.len() {
        0 => "Archive Properties",
        1 => "Properties",
        _ => "Selection Properties",
    };

    let win = gtk::Window::builder()
        .title(title)
        .modal(true)
        .resizable(false)
        .transient_for(parent)
        .default_width(440)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.add_css_class("rarc-dialog-body");

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(14)
        .build();

    let mut rows: Vec<(&str, String)> = Vec::new();
    match selected {
        [entry] => {
            rows.push(("Name", entry.name.clone()));
            rows.push(("Archive Path", entry.full_path.clone()));
            rows.push(("Type", entry.type_label.clone()));
            if !entry.is_dir {
                rows.push((
                    "Uncompressed Size",
                    format!("{} ({} bytes)", format_size(entry.size), entry.size),
                ));
                let comp = entry
                    .compressed_size
                    .map(|c| format!("{} ({c} bytes)", format_size(c)))
                    .unwrap_or_else(|| "—".to_string());
                rows.push(("Compressed Size", comp));
            }
            rows.push(("Modified Date", format_mtime(entry.mtime)));
            rows.push(("Permissions", format_mode(entry.mode, entry.is_dir)));
            rows.push((
                "Encrypted",
                if entry.is_encrypted { "Yes" } else { "No" }.to_string(),
            ));
            if let Some(ref target) = entry.symlink_target {
                rows.push(("Symlink Target", target.clone()));
            }
        }
        [] => {
            let name = archive_info
                .archive_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("archive")
                .to_string();
            let disk_size = std::fs::metadata(&archive_info.archive_path)
                .map(|m| format!("{} ({} bytes)", format_size(m.len()), m.len()))
                .unwrap_or_else(|_| "—".to_string());
            rows.push(("Name", name));
            rows.push(("Location", archive_info.archive_path.display().to_string()));
            rows.push(("Archive Size", disk_size));
            rows.push(("Entries", archive_info.entry_count.to_string()));
            rows.push((
                "Uncompressed Size",
                format!(
                    "{} ({} bytes)",
                    format_size(archive_info.total_uncompressed),
                    archive_info.total_uncompressed
                ),
            ));
            rows.push((
                "Encrypted Entries",
                archive_info.encrypted_count.to_string(),
            ));
        }
        many => {
            let dirs = many.iter().filter(|e| e.is_dir).count();
            let files = many.len() - dirs;
            let total: u64 = many.iter().map(|e| e.size).sum();
            rows.push((
                "Selected",
                format!("{} items ({files} files, {dirs} folders)", many.len()),
            ));
            rows.push((
                "Uncompressed Size",
                format!("{} ({total} bytes)", format_size(total)),
            ));
            rows.push((
                "Encrypted Items",
                many.iter().filter(|e| e.is_encrypted).count().to_string(),
            ));
        }
    }

    for (idx, (key, val)) in rows.into_iter().enumerate() {
        let k_lbl = gtk::Label::new(Some(key));
        k_lbl.set_xalign(1.0);
        k_lbl.set_valign(gtk::Align::Start);
        k_lbl.add_css_class("rarc-muted");

        let v_lbl = gtk::Label::new(Some(&val));
        v_lbl.set_xalign(0.0);
        v_lbl.set_hexpand(true);
        v_lbl.set_wrap(true);
        v_lbl.set_selectable(true);

        grid.attach(&k_lbl, 0, idx as i32, 1, 1);
        grid.attach(&v_lbl, 1, idx as i32, 1, 1);
    }

    let close_btn = gtk::Button::with_label("Close");
    close_btn.set_halign(gtk::Align::End);
    close_btn.add_css_class("suggested-action");
    let win_ref = win.clone();
    close_btn.connect_clicked(move |_| win_ref.close());

    vbox.append(&grid);
    vbox.append(&close_btn);
    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&close_btn));
    win.present();
}

fn format_mode(mode: u32, is_dir: bool) -> String {
    let bits = mode & 0o777;
    if bits == 0 && mode == 0 {
        return "—".to_string();
    }
    let mut s = String::with_capacity(10);
    s.push(if is_dir { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        let tri = (bits >> shift) & 0o7;
        s.push(if (tri & 4) != 0 { 'r' } else { '-' });
        s.push(if (tri & 2) != 0 { 'w' } else { '-' });
        s.push(if (tri & 1) != 0 { 'x' } else { '-' });
    }
    format!("{s} ({bits:04o})")
}
