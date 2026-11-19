use std::path::{Path, PathBuf};

use gtk4 as gtk;
use gtk::prelude::*;

use crate::archive::{ArchiveEntry, EntryKind};
use crate::i18n::tr;
use crate::ui::{format_mtime, format_size, translate_entry_kind};

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
        0 => tr("Archive Properties"),
        1 => tr("Properties"),
        _ => tr("Selection Properties"),
    };

    let win = gtk::Window::builder()
        .title(&title)
        .modal(true)
        .resizable(false)
        .transient_for(parent)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.add_css_class("rarc-dialog-body");

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(14)
        .build();

    let mut rows: Vec<(String, String)> = Vec::new();
    match selected {
        [entry] => {
            let name = Path::new(&entry.path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&entry.path);
            let is_dir = entry.kind == EntryKind::Directory;
            rows.push((tr("Name"), name.to_string()));
            rows.push((tr("Archive Path"), entry.path.clone()));
            rows.push((tr("Type"), translate_entry_kind(entry.kind, name)));
            if !is_dir {
                rows.push((
                    tr("Uncompressed Size"),
                    tr("{size} ({bytes} bytes)")
                        .replace("{size}", &format_size(entry.size))
                        .replace("{bytes}", &entry.size.to_string()),
                ));
                let comp = entry
                    .compressed_size
                    .map(|c| {
                        tr("{size} ({bytes} bytes)")
                            .replace("{size}", &format_size(c))
                            .replace("{bytes}", &c.to_string())
                    })
                    .unwrap_or_else(|| "—".to_string());
                rows.push((tr("Compressed Size"), comp));
            }
            rows.push((tr("Modified Date"), format_mtime(entry.mtime)));
            rows.push((tr("Permissions"), format_mode(entry.mode, is_dir)));
            rows.push((
                tr("Encrypted"),
                if entry.is_encrypted { tr("Yes") } else { tr("No") },
            ));
            if let Some(ref target) = entry.symlink_target {
                rows.push((tr("Symlink Target"), target.clone()));
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
                .map(|len| {
                    tr("{size} ({bytes} bytes)")
                        .replace("{size}", &format_size(len))
                        .replace("{bytes}", &len.to_string())
                })
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
                tr("Archive")
            } else {
                archive_info.format_name.clone()
            };
            rows.push((tr("Name"), name));
            rows.push((tr("Location"), archive_info.archive_path.display().to_string()));
            rows.push((tr("Format"), fmt));
            rows.push((tr("Modified Date"), format_mtime(modified)));
            rows.push((tr("Archive Size"), disk_size));
            rows.push((
                tr("Content Size"),
                tr("{size} ({bytes} bytes)")
                    .replace("{size}", &format_size(archive_info.total_uncompressed))
                    .replace("{bytes}", &archive_info.total_uncompressed.to_string()),
            ));
            rows.push((tr("Compression Ratio"), ratio));
            rows.push((tr("Entries"), archive_info.entry_count.to_string()));
            rows.push((
                tr("Encrypted Entries"),
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
                tr("Selected"),
                tr("{count} items ({files} files, {dirs} folders)")
                    .replace("{count}", &many.len().to_string())
                    .replace("{files}", &files.to_string())
                    .replace("{dirs}", &dirs.to_string()),
            ));
            rows.push((
                tr("Uncompressed Size"),
                tr("{size} ({bytes} bytes)")
                    .replace("{size}", &format_size(total))
                    .replace("{bytes}", &total.to_string()),
            ));
            rows.push((
                tr("Encrypted Items"),
                many.iter().filter(|e| e.is_encrypted).count().to_string(),
            ));
        }
    }

    for (idx, (key, val)) in rows.into_iter().enumerate() {
        let k_lbl = gtk::Label::new(Some(&key));
        k_lbl.set_xalign(1.0);
        k_lbl.set_valign(gtk::Align::Start);
        k_lbl.add_css_class("rarc-muted");

        let v_lbl = gtk::Label::new(Some(&val));
        v_lbl.set_xalign(0.0);
        v_lbl.set_hexpand(true);
        v_lbl.set_wrap(true);
        v_lbl.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        v_lbl.set_width_chars(36);
        v_lbl.set_max_width_chars(36);
        v_lbl.set_selectable(true);

        grid.attach(&k_lbl, 0, idx as i32, 1, 1);
        grid.attach(&v_lbl, 1, idx as i32, 1, 1);
    }

    let close_btn = gtk::Button::with_label(&tr("Close"));
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
