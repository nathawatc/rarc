mod create;
mod properties;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_channel::Receiver;
use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;

use crate::archive::{ArchiveError, ConflictResolution, OperationSummary, WorkerEvent};

pub use create::show_create_dialog;
pub use properties::{show_properties_dialog, ArchivePropertiesInfo};

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

async fn run_modal_prompt<T: 'static>(
    parent: &gtk::Window,
    title: &str,
    spacing: i32,
    default_width: i32,
    build_ui: impl FnOnce(&gtk::Window, &gtk::Box, &Rc<dyn Fn(Option<T>)>),
) -> Option<T> {
    let (tx, rx) = async_channel::bounded(1);
    let tx_cell = Rc::new(RefCell::new(Some(tx)));

    let win = gtk::Window::builder()
        .title(title)
        .modal(true)
        .resizable(false)
        .transient_for(parent)
        .default_width(default_width)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, spacing);
    vbox.add_css_class("rarc-dialog-body");
    win.set_child(Some(&vbox));

    let send_once: Rc<dyn Fn(Option<T>)> = {
        let tx_cell = tx_cell.clone();
        let win_ref = win.clone();
        Rc::new(move |val| {
            if let Some(sender) = tx_cell.borrow_mut().take() {
                let _ = sender.try_send(val);
            }
            win_ref.close();
        })
    };

    win.connect_close_request(move |_| {
        if let Some(sender) = tx_cell.borrow_mut().take() {
            let _ = sender.try_send(None);
        }
        glib::Propagation::Proceed
    });

    build_ui(&win, &vbox, &send_once);
    win.present();
    rx.recv().await.unwrap_or(None)
}

/// Shows a modal passphrase prompt and resolves with `Some(password)` or `None` if cancelled.
pub async fn prompt_password_async(
    parent: &gtk::Window,
    archive_name: &str,
    retry: bool,
) -> Option<String> {
    let msg_text = format!("Enter the passphrase for \"{archive_name}\":");
    run_modal_prompt(parent, "Password Required", 10, 360, |win, vbox, send_once| {
        let msg = gtk::Label::new(Some(&msg_text));
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
        win.set_default_widget(Some(&ok_btn));

        let send_cancel = send_once.clone();
        cancel_btn.connect_clicked(move |_| send_cancel(None));

        let send_ok = send_once.clone();
        let entry_ref = entry.clone();
        ok_btn.connect_clicked(move |_| send_ok(Some(entry_ref.text().to_string())));

        entry.grab_focus();
    })
    .await
}

/// Shows a modal file conflict dialog (`Overwrite`, `Overwrite All`, `Skip`, `Skip All`, `Rename`).
async fn prompt_conflict_async(
    parent: &gtk::Window,
    target_path: &Path,
) -> Option<ConflictResolution> {
    let display_path = target_path.display().to_string();
    let orig_name = target_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("renamed")
        .to_string();
    let suggested = suggest_renamed_filename(&orig_name);

    run_modal_prompt(parent, "File Already Exists", 12, 460, |win, vbox, send_once| {
        let msg = gtk::Label::new(Some(&format!("A file already exists at:\n{display_path}")));
        msg.set_xalign(0.0);
        msg.set_wrap(true);
        msg.set_selectable(true);
        vbox.append(&msg);

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
        win.set_default_widget(Some(&overwrite_btn));

        let bind_btn = |btn: &gtk::Button, resolution: ConflictResolution| {
            let send = send_once.clone();
            btn.connect_clicked(move |_| send(Some(resolution.clone())));
        };

        bind_btn(&skip_btn, ConflictResolution::Skip);
        bind_btn(&skip_all_btn, ConflictResolution::SkipAll);
        bind_btn(&overwrite_btn, ConflictResolution::Overwrite);
        bind_btn(&overwrite_all_btn, ConflictResolution::OverwriteAll);

        let send_rename = send_once.clone();
        let entry_ref = rename_entry.clone();
        rename_btn.connect_clicked(move |_| {
            let name = entry_ref.text().trim().to_string();
            if !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']) {
                send_rename(Some(ConflictResolution::Rename(name)));
            }
        });

        let btn_ref = rename_btn.clone();
        rename_entry.connect_changed(move |e| {
            let txt = e.text();
            let t = txt.trim();
            let valid = !t.is_empty() && t != "." && t != ".." && !t.contains(['/', '\\']);
            btn_ref.set_sensitive(valid);
        });
    })
    .await
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
