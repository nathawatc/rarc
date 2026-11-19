use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gtk4 as gtk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::archive::{
    create_archive, sanitize_single_filename, ArchiveError, ArchiveFormat, OperationSummary,
};
use crate::ui::{select_folder_dialog, show_message_dialog};

use super::run_worker_with_progress;

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
            let dest_rc = dest_rc.clone();
            let display_ref = display_ref.clone();
            let initial = dest_rc.borrow().clone();
            select_folder_dialog(
                Some(&win_ref),
                "Select Destination Folder",
                Some(&initial),
                move |path_opt| {
                    if let Some(path) = path_opt {
                        display_ref.set_text(&path.display().to_string());
                        *dest_rc.borrow_mut() = path;
                    }
                },
            );
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
            let inputs_ref = inputs_ref.clone();
            let summary_ref = summary_ref.clone();
            let create_ref = create_ref.clone();
            select_folder_dialog(Some(&win_ref), "Select Folder to Add", None, move |path_opt| {
                if let Some(path) = path_opt {
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
            let Ok(raw_name) = sanitize_single_filename(&name_entry.text()) else {
                show_message_dialog(
                    Some(&win_ref),
                    "Invalid Archive Name",
                    "Please enter a valid filename without path separators.",
                );
                return;
            };
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

            let start_creation = {
                let app_cloned = app_cloned.clone();
                let parent_cloned = parent_cloned.clone();
                let win_ref = win_ref.clone();
                let completed_ref = completed_ref.clone();
                let cb = cb.clone();
                move |output_path: PathBuf,
                      fmt: ArchiveFormat,
                      inputs: Vec<PathBuf>,
                      password: Option<String>| {
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
                            Ok(OperationSummary::Created(output_path)) => {
                                cb_done(Some(output_path))
                            }
                            Ok(_) | Err(ArchiveError::Cancelled) => cb_done(None),
                            Err(err) => {
                                show_message_dialog(
                                    parent_err.as_ref(),
                                    "Failed to Create Archive",
                                    &err.to_string(),
                                );
                                cb_done(None);
                            }
                        },
                    );
                }
            };

            if output_path.symlink_metadata().is_ok() {
                let name = output_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("archive");
                let confirm = gtk::AlertDialog::builder()
                    .modal(true)
                    .message("Replace Existing Archive?")
                    .detail(format!(
                        "A file named \"{name}\" already exists. Do you want to overwrite it?"
                    ))
                    .buttons(["Cancel", "Replace"])
                    .cancel_button(0)
                    .default_button(0)
                    .build();
                confirm.choose(Some(&win_ref), gio::Cancellable::NONE, move |res| {
                    if res == Ok(1) {
                        start_creation(output_path, fmt, inputs, password);
                    }
                });
            } else {
                start_creation(output_path, fmt, inputs, password);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_path_appends_extension_unless_present() {
        let dir = Path::new("/tmp");
        assert_eq!(
            build_output_path(dir, "backup", ArchiveFormat::TarZst),
            PathBuf::from("/tmp/backup.tar.zst")
        );
        assert_eq!(
            build_output_path(dir, "backup.ZIP", ArchiveFormat::Zip),
            PathBuf::from("/tmp/backup.ZIP")
        );
        assert_eq!(summarize_inputs(&[]), "No files selected");
        assert_eq!(summarize_inputs(&[PathBuf::from("/tmp/a.txt")]), "a.txt");
    }
}
