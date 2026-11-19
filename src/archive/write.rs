use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::raw::{c_long, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_channel::Sender;

use super::ffi::{
    self, archive_err_string, check_read_write, entry_pathname_string, open_reader,
    path_to_cstring, reader_error, to_cstring, OwnedEntry, PassphraseCtx, WriteArchive,
};
use super::formats::{infer_writable_from_extension, WritableSpec};
use super::read::{list_archive, pump_archive_data, DataSink, ProgressThrottle};
use super::{sanitize_entry_path_str, ArchiveError, ArchiveFormat, EntryKind, WorkerEvent};

/// Creates a new archive at `output_path` from `inputs`.
pub fn create_archive(
    output_path: &Path,
    format: ArchiveFormat,
    inputs: &[PathBuf],
    password: Option<&str>,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<usize, ArchiveError> {
    if inputs.is_empty() {
        return Err(ArchiveError::Io("No input files selected".to_string()));
    }
    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    let disk_items = collect_disk_items(inputs, "", Some(output_path))?;
    let bytes_total: u64 = disk_items.iter().map(|item| item.size).sum();

    let tmp_path = temp_sibling_path(output_path);
    let writer = WriteArchive::new()?;
    let spec = WritableSpec::from_format(format);
    spec.apply(writer.as_ptr(), password)?;

    let tmp_cstr = path_to_cstring(&tmp_path)?;
    unsafe {
        check_read_write(
            ffi::raw::archive_write_open_filename(writer.as_ptr(), tmp_cstr.as_ptr()),
            writer.as_ptr(),
        )?;
    }

    let mut bytes_done = 0u64;
    let mut written_count = 0usize;
    let mut progress = ProgressThrottle::default();
    let mut io_buf = vec![0u8; 65536];
    let res = (|| -> Result<(), ArchiveError> {
        for item in &disk_items {
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            write_disk_item(
                writer.as_ptr(),
                item,
                &mut io_buf,
                &mut bytes_done,
                bytes_total,
                event_tx,
                &mut progress,
                cancel,
            )?;
            written_count += 1;
        }
        Ok(())
    })();

    if let Err(err) = res {
        writer.fail();
        drop(writer);
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    writer.close()?;
    fs::rename(&tmp_path, output_path)?;
    Ok(written_count)
}

/// Adds `inputs` into an existing writable archive under `dest_subdir` via atomic repacking.
pub fn add_to_archive(
    archive_path: &Path,
    inputs: &[PathBuf],
    dest_subdir: &str,
    initial_password: Option<&str>,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<(usize, Option<String>), ArchiveError> {
    if inputs.is_empty() {
        return Ok((0, initial_password.map(ToOwned::to_owned)));
    }
    let disk_items = collect_disk_items(inputs, dest_subdir, Some(archive_path))?;
    if disk_items.is_empty() {
        return Ok((0, initial_password.map(ToOwned::to_owned)));
    }
    let replace_set: HashSet<String> = disk_items.iter().map(|d| d.archive_path.clone()).collect();
    let new_bytes: u64 = disk_items.iter().map(|i| i.size).sum();

    let listing = list_archive(archive_path, initial_password, event_tx, cancel)?;
    let has_encrypted = listing.entries.iter().any(|e| e.is_encrypted);
    let mut effective_pw = listing
        .password
        .or_else(|| initial_password.filter(|p| !p.is_empty()).map(ToOwned::to_owned));
    if has_encrypted && effective_pw.is_none() {
        let mut probe_ctx = PassphraseCtx::for_path(archive_path, None, event_tx, cancel);
        effective_pw = probe_encrypted_password(archive_path, &mut probe_ctx, cancel)?;
        if effective_pw.is_none() {
            return Err(ArchiveError::PasswordRequired);
        }
    }

    let existing_bytes: u64 = listing
        .entries
        .iter()
        .filter(|e| !replace_set.contains(&e.path))
        .map(|e| e.size)
        .sum();
    let bytes_total = existing_bytes + new_bytes;

    let mut pass_ctx =
        PassphraseCtx::for_path(archive_path, effective_pw.as_deref(), event_tx, cancel);

    let reader = open_reader(archive_path, false, &mut pass_ctx)?;
    let tmp_path = temp_sibling_path(archive_path);
    let writer = WriteArchive::new()?;
    let mut writer_opened = false;
    let mut bytes_done = 0u64;
    let mut progress = ProgressThrottle::default();

    let res = (|| -> Result<(), ArchiveError> {
        unsafe {
            let mut entry: *mut ffi::raw::archive_entry = ptr::null_mut();
            let mut first = true;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ArchiveError::Cancelled);
                }
                let rc = ffi::raw::archive_read_next_header(reader.as_ptr(), &mut entry);
                if rc == ffi::raw::ARCHIVE_EOF {
                    if first {
                        let spec = infer_writable_from_extension(archive_path)?;
                        spec.apply(writer.as_ptr(), effective_pw.as_deref())?;
                        let tmp_cstr = path_to_cstring(&tmp_path)?;
                        check_read_write(
                            ffi::raw::archive_write_open_filename(
                                writer.as_ptr(),
                                tmp_cstr.as_ptr(),
                            ),
                            writer.as_ptr(),
                        )?;
                        writer_opened = true;
                    }
                    break;
                }
                if rc != ffi::raw::ARCHIVE_OK && rc != ffi::raw::ARCHIVE_WARN {
                    return Err(reader_error(reader.as_ptr(), &pass_ctx));
                }

                if first {
                    first = false;
                    let fmt = ffi::raw::archive_format(reader.as_ptr());
                    let flt = ffi::raw::archive_filter_code(reader.as_ptr(), 0);
                    let spec = WritableSpec::from_detected(fmt, flt)?;
                    spec.apply(writer.as_ptr(), effective_pw.as_deref())?;
                    let tmp_cstr = path_to_cstring(&tmp_path)?;
                    check_read_write(
                        ffi::raw::archive_write_open_filename(writer.as_ptr(), tmp_cstr.as_ptr()),
                        writer.as_ptr(),
                    )?;
                    writer_opened = true;
                }

                let raw_name = entry_pathname_string(entry)?;
                let Some(rel) = sanitize_entry_path_str(&raw_name)? else {
                    continue;
                };
                if replace_set.contains(&rel) {
                    let _ = ffi::raw::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                progress.send(event_tx, &rel, bytes_done, bytes_total);
                check_read_write(
                    ffi::raw::archive_write_header(writer.as_ptr(), entry),
                    writer.as_ptr(),
                )?;
                let filetype = ffi::raw::archive_entry_filetype(entry);
                if (filetype & ffi::raw::AE_IFMT) != ffi::raw::AE_IFDIR {
                    pump_archive_data(
                        reader.as_ptr(),
                        DataSink::Stream(writer.as_ptr()),
                        &pass_ctx,
                        &mut bytes_done,
                        bytes_total,
                        &rel,
                        event_tx,
                        &mut progress,
                        cancel,
                    )?;
                }
                check_read_write(
                    ffi::raw::archive_write_finish_entry(writer.as_ptr()),
                    writer.as_ptr(),
                )?;
            }
        }

        let mut io_buf = vec![0u8; 65536];
        for item in &disk_items {
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            write_disk_item(
                writer.as_ptr(),
                item,
                &mut io_buf,
                &mut bytes_done,
                bytes_total,
                event_tx,
                &mut progress,
                cancel,
            )?;
        }

        Ok(())
    })();

    if let Err(err) = res {
        if writer_opened {
            writer.fail();
        }
        drop(writer);
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    writer.close()?;
    fs::rename(&tmp_path, archive_path)?;
    Ok((disk_items.len(), pass_ctx.last_password().or(effective_pw)))
}

fn probe_encrypted_password(
    archive_path: &Path,
    pass_ctx: &mut PassphraseCtx,
    cancel: &Arc<AtomicBool>,
) -> Result<Option<String>, ArchiveError> {
    let probe = open_reader(archive_path, false, pass_ctx)?;
    unsafe {
        let mut entry: *mut ffi::raw::archive_entry = ptr::null_mut();
        let mut first = true;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            let rc = ffi::raw::archive_read_next_header(probe.as_ptr(), &mut entry);
            if rc == ffi::raw::ARCHIVE_EOF {
                break;
            }
            if rc != ffi::raw::ARCHIVE_OK && rc != ffi::raw::ARCHIVE_WARN {
                return Err(reader_error(probe.as_ptr(), pass_ctx));
            }
            if first {
                first = false;
                let fmt = ffi::raw::archive_format(probe.as_ptr());
                let flt = ffi::raw::archive_filter_code(probe.as_ptr(), 0);
                let spec = WritableSpec::from_detected(fmt, flt)?;
                if !matches!(spec, WritableSpec::Zip) {
                    return Err(ArchiveError::UnsupportedFormat(
                        "modifying encrypted non-ZIP archives is not supported".to_string(),
                    ));
                }
            }
            let is_dir = (ffi::raw::archive_entry_filetype(entry) & ffi::raw::AE_IFMT)
                == ffi::raw::AE_IFDIR;
            if !is_dir && ffi::raw::archive_entry_is_encrypted(entry) != 0 {
                let mut buf: *const c_void = ptr::null();
                let mut size: usize = 0;
                let mut offset: i64 = 0;
                let drc = ffi::raw::archive_read_data_block(
                    probe.as_ptr(),
                    &mut buf,
                    &mut size,
                    &mut offset,
                );
                if drc != ffi::raw::ARCHIVE_OK
                    && drc != ffi::raw::ARCHIVE_WARN
                    && drc != ffi::raw::ARCHIVE_EOF
                {
                    return Err(reader_error(probe.as_ptr(), pass_ctx));
                }
                break;
            }
            let _ = ffi::raw::archive_read_data_skip(probe.as_ptr());
        }
    }
    Ok(pass_ctx.last_password())
}

struct DiskItem {
    disk_path: PathBuf,
    archive_path: String,
    kind: EntryKind,
    size: u64,
    mode: u32,
    mtime: i64,
    symlink_target: Option<PathBuf>,
}

fn collect_disk_items(
    inputs: &[PathBuf],
    dest_subdir: &str,
    exclude_path: Option<&Path>,
) -> Result<Vec<DiskItem>, ArchiveError> {
    let mut items = Vec::new();
    let prefix = dest_subdir.trim_matches('/');
    let exclude_canon = exclude_path.and_then(|p| fs::canonicalize(p).ok());
    for input in inputs {
        let base_name = input
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| ArchiveError::Io(format!("Invalid input path: {}", input.display())))?;
        let archive_rel = if prefix.is_empty() {
            base_name.to_string()
        } else {
            format!("{prefix}/{base_name}")
        };
        collect_disk_recursive(input, &archive_rel, exclude_canon.as_deref(), &mut items)?;
    }
    Ok(items)
}

fn collect_disk_recursive(
    disk_path: &Path,
    archive_path: &str,
    exclude_canon: Option<&Path>,
    out: &mut Vec<DiskItem>,
) -> Result<(), ArchiveError> {
    let meta = fs::symlink_metadata(disk_path)?;
    let ft = meta.file_type();
    if ft.is_file()
        && let Some(ex) = exclude_canon
        && fs::canonicalize(disk_path).ok().as_deref() == Some(ex)
    {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o7777;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if ft.is_symlink() {
        let target = fs::read_link(disk_path)?;
        out.push(DiskItem {
            disk_path: disk_path.to_path_buf(),
            archive_path: archive_path.to_string(),
            kind: EntryKind::Symlink,
            size: 0,
            mode,
            mtime,
            symlink_target: Some(target),
        });
    } else if ft.is_dir() {
        out.push(DiskItem {
            disk_path: disk_path.to_path_buf(),
            archive_path: archive_path.to_string(),
            kind: EntryKind::Directory,
            size: 0,
            mode,
            mtime,
            symlink_target: None,
        });
        let mut children: Vec<_> = fs::read_dir(disk_path)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let child_name = child.file_name();
            let child_str = child_name.to_string_lossy();
            let next_archive = format!("{archive_path}/{child_str}");
            collect_disk_recursive(&child.path(), &next_archive, exclude_canon, out)?;
        }
    } else if ft.is_file() {
        out.push(DiskItem {
            disk_path: disk_path.to_path_buf(),
            archive_path: archive_path.to_string(),
            kind: EntryKind::File,
            size: meta.len(),
            mode,
            mtime,
            symlink_target: None,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_disk_item(
    writer: *mut ffi::raw::archive,
    item: &DiskItem,
    buf: &mut [u8],
    bytes_done: &mut u64,
    bytes_total: u64,
    event_tx: Option<&Sender<WorkerEvent>>,
    progress: &mut ProgressThrottle,
    cancel: &Arc<AtomicBool>,
) -> Result<(), ArchiveError> {
    progress.send(event_tx, &item.archive_path, *bytes_done, bytes_total);

    let entry = OwnedEntry::new()?;
    let c_path = to_cstring(&item.archive_path)?;
    unsafe {
        ffi::raw::archive_entry_set_pathname(entry.as_ptr(), c_path.as_ptr());
        ffi::raw::archive_entry_set_perm(entry.as_ptr(), item.mode);
        ffi::raw::archive_entry_set_mtime(entry.as_ptr(), item.mtime as c_long, 0);

        match item.kind {
            EntryKind::Directory => {
                ffi::raw::archive_entry_set_filetype(entry.as_ptr(), ffi::raw::AE_IFDIR);
                ffi::raw::archive_entry_set_size(entry.as_ptr(), 0);
                check_read_write(
                    ffi::raw::archive_write_header(writer, entry.as_ptr()),
                    writer,
                )?;
            }
            EntryKind::Symlink => {
                ffi::raw::archive_entry_set_filetype(entry.as_ptr(), ffi::raw::AE_IFLNK);
                ffi::raw::archive_entry_set_size(entry.as_ptr(), 0);
                if let Some(ref target) = item.symlink_target {
                    let c_target = path_to_cstring(target)?;
                    ffi::raw::archive_entry_set_symlink(entry.as_ptr(), c_target.as_ptr());
                }
                check_read_write(
                    ffi::raw::archive_write_header(writer, entry.as_ptr()),
                    writer,
                )?;
            }
            EntryKind::File | EntryKind::Other => {
                ffi::raw::archive_entry_set_filetype(entry.as_ptr(), ffi::raw::AE_IFREG);
                ffi::raw::archive_entry_set_size(entry.as_ptr(), item.size as i64);
                check_read_write(
                    ffi::raw::archive_write_header(writer, entry.as_ptr()),
                    writer,
                )?;

                let mut file = fs::File::open(&item.disk_path)?;
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(ArchiveError::Cancelled);
                    }
                    let n = file.read(buf)?;
                    if n == 0 {
                        break;
                    }
                    let written =
                        ffi::raw::archive_write_data(writer, buf.as_ptr() as *const c_void, n);
                    if written < 0 {
                        return Err(ArchiveError::LibArchive(archive_err_string(writer)));
                    }
                    *bytes_done += n as u64;
                    progress.send(event_tx, &item.archive_path, *bytes_done, bytes_total);
                }
            }
        }

        check_read_write(ffi::raw::archive_write_finish_entry(writer), writer)?;
    }

    Ok(())
}

fn temp_sibling_path(target: &Path) -> PathBuf {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("archive");
    let pid = std::process::id();
    for i in 0..1000 {
        let candidate = parent.join(format!(".{file_name}.rarc-tmp-{pid}-{i}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!(".{file_name}.rarc-tmp-{pid}"))
}
