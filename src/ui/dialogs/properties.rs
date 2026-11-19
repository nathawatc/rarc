use std::path::{Path, PathBuf};

use gtk4 as gtk;
use gtk::prelude::*;

use crate::archive::{ArchiveEntry, EntryKind};
use crate::ui::{format_mtime, format_size};

pub struct ArchivePropertiesInfo {
    pub archive_path: PathBuf,
    pub format_name: String,
    pub entry_count: usize,
    pub total_uncompressed: u64,
    pub encrypted_count: usize,
}

/// Shows a Properties modal for a single entry, multiple selected entries, or the archive itself.
pub fn show_properties_dialog(
    parent: &gtk::Window,
    selected: &[ArchiveEntry],
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
            let name = Path::new(&entry.path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&entry.path);
            let is_dir = entry.kind == EntryKind::Directory;
            rows.push(("Name", name.to_string()));
            rows.push(("Archive Path", entry.path.clone()));
            rows.push(("Type", entry.kind.label(name)));
            if !is_dir {
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
            rows.push(("Permissions", format_mode(entry.mode, is_dir)));
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
            let meta = std::fs::metadata(&archive_info.archive_path).ok();
            let disk_bytes = meta.as_ref().map(|m| m.len());
            let disk_size = disk_bytes
                .map(|len| format!("{} ({len} bytes)", format_size(len)))
                .unwrap_or_else(|| "—".to_string());
            let modified = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let ratio = match disk_bytes {
                Some(len) if len > 0 => {
                    format!("{:.2}:1", archive_info.total_uncompressed as f64 / len as f64)
                }
                _ => "—".to_string(),
            };
            let fmt = if archive_info.format_name.is_empty() {
                "Archive".to_string()
            } else {
                archive_info.format_name.clone()
            };
            rows.push(("Name", name));
            rows.push(("Location", archive_info.archive_path.display().to_string()));
            rows.push(("Format", fmt));
            rows.push(("Modified Date", format_mtime(modified)));
            rows.push(("Archive Size", disk_size));
            rows.push((
                "Content Size",
                format!(
                    "{} ({} bytes)",
                    format_size(archive_info.total_uncompressed),
                    archive_info.total_uncompressed
                ),
            ));
            rows.push(("Compression Ratio", ratio));
            rows.push(("Entries", archive_info.entry_count.to_string()));
            rows.push((
                "Encrypted Entries",
                archive_info.encrypted_count.to_string(),
            ));
        }
        many => {
            let dirs = many
                .iter()
                .filter(|e| e.kind == EntryKind::Directory)
                .count();
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
