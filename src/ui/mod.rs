pub mod dialogs;
pub mod viewer;

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use gtk4 as gtk;
use gtk::{gdk, gio};
use gtk::prelude::*;

use crate::archive::EntryKind;
use crate::i18n::tr;

static TEMP_ROOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../../resources/style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Returns a process-scoped temporary directory for previews and drag-out staging.
pub fn session_temp_dir() -> std::io::Result<PathBuf> {
    let cell = TEMP_ROOT.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();
    if let Some(ref dir) = *guard {
        if !dir.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(dir)?;
        }
        return Ok(dir.clone());
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("rarc-{}-{nanos}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    *guard = Some(dir.clone());
    Ok(dir)
}

/// Removes any temporary preview/DnD staging files created during this session.
pub fn cleanup_session_temp_dir() {
    if let Some(cell) = TEMP_ROOT.get()
        && let Some(dir) = cell.lock().unwrap().take()
    {
        let _ = fs::remove_dir_all(dir);
    }
}

pub fn translate_entry_kind(kind: EntryKind, name: &str) -> String {
    match kind {
        EntryKind::Directory => tr("Folder"),
        EntryKind::Symlink => tr("Symbolic Link"),
        EntryKind::Other => tr("Special"),
        EntryKind::File => match Path::new(name).extension().and_then(|e| e.to_str()) {
            Some(ext) if !ext.is_empty() => {
                tr("{ext} File").replace("{ext}", &ext.to_ascii_uppercase())
            }
            _ => tr("File"),
        },
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_mtime(mtime: Option<i64>) -> String {
    let Some(ts) = mtime else {
        return "—".to_string();
    };
    gtk::glib::DateTime::from_unix_local(ts)
        .and_then(|dt| dt.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "—".to_string())
}

pub fn select_folder_dialog(
    parent: Option<&gtk::Window>,
    title: &str,
    initial_dir: Option<&Path>,
    on_done: impl FnOnce(Option<PathBuf>) + 'static,
) {
    let mut builder = gtk::FileDialog::builder().title(title);
    if let Some(dir) = initial_dir.filter(|p| !p.as_os_str().is_empty()) {
        builder = builder.initial_folder(&gio::File::for_path(dir));
    }
    builder
        .build()
        .select_folder(parent, gio::Cancellable::NONE, move |res| {
            on_done(res.ok().and_then(|f| f.path()));
        });
}

pub fn show_message_dialog(parent: Option<&impl IsA<gtk::Window>>, title: &str, detail: &str) {
    let win = gtk::Window::builder()
        .title(title)
        .modal(true)
        .resizable(false)
        .build();
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
    } else if let Some(app) = gio::Application::default().and_then(|a| a.downcast::<gtk::Application>().ok()) {
        win.set_application(Some(&app));
    }

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.add_css_class("rarc-dialog-body");

    let heading = gtk::Label::new(Some(title));
    heading.set_xalign(0.0);
    heading.add_css_class("title-4");

    let body = gtk::Label::new(Some(detail));
    body.set_xalign(0.0);
    body.set_wrap(true);
    body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    body.set_width_chars(44);
    body.set_max_width_chars(44);
    body.set_selectable(true);

    let ok_btn = gtk::Button::with_label(&tr("Close"));
    ok_btn.set_halign(gtk::Align::End);
    ok_btn.add_css_class("suggested-action");
    let win_ref = win.clone();
    ok_btn.connect_clicked(move |_| win_ref.close());

    vbox.append(&heading);
    vbox.append(&body);
    vbox.append(&ok_btn);
    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&ok_btn));
    win.present();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn session_temp_dir_is_owner_only_and_cleans_up() {
        let dir = session_temp_dir().unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        cleanup_session_temp_dir();
        assert!(!dir.exists());
    }
}
