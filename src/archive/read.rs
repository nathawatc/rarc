use std::fs;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_channel::Sender;

use super::ffi::{
    self, archive_err_string, describe_reader_format, entry_hardlink_string,
    entry_pathname_string, entry_symlink_string, filetype_to_kind, open_reader_step,
    path_to_cstring, read_next_header_step, reader_error, HeaderStep, PassphraseCtx, ReaderOpen,
    WriteDiskArchive,
};
use super::formats::archive_stem;
use super::{
    apply_strip_prefix, sanitize_entry_path, sanitize_entry_path_str, sanitize_single_filename,
    validate_symlink_target, ArchiveEntry, ArchiveError, ArchiveListing, ConflictAction,
    ConflictPolicy, ConflictResolution, EntryKind, ExtractOptions, ExtractSession, SelectionFilter,
    WorkerEvent,
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
pub(super) struct ProgressThrottle {
    last_sent: Option<Instant>,
}

impl ProgressThrottle {
    pub(super) fn send(
        &mut self,
        event_tx: Option<&Sender<WorkerEvent>>,
        label: &str,
        bytes_done: u64,
        bytes_total: u64,
    ) {
        let Some(tx) = event_tx else {
            return;
        };
        let now = Instant::now();
        if let Some(last) = self.last_sent
            && now.duration_since(last) < PROGRESS_INTERVAL
        {
            return;
        }
        self.last_sent = Some(now);
        let _ = tx.try_send(WorkerEvent::Progress {
            current_file: label.to_string(),
            bytes_done,
            bytes_total,
        });
    }
}

pub(super) enum DataSink {
    Discard,
    Disk(*mut ffi::raw::archive),
    Stream(*mut ffi::raw::archive),
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn pump_archive_data(
    reader: *mut ffi::raw::archive,
    sink: DataSink,
    pass_ctx: &PassphraseCtx,
    bytes_done: &mut u64,
    bytes_total: u64,
    label: &str,
    event_tx: Option<&Sender<WorkerEvent>>,
    progress: &mut ProgressThrottle,
    cancel: &Arc<AtomicBool>,
) -> Result<(), ArchiveError> {
    let mut buffer: *const c_void = ptr::null();
    let mut size: usize = 0;
    let mut offset: i64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let rc = unsafe {
            ffi::raw::archive_read_data_block(reader, &mut buffer, &mut size, &mut offset)
        };
        if rc == ffi::raw::ARCHIVE_EOF {
            return Ok(());
        }
        if rc != ffi::raw::ARCHIVE_OK && rc != ffi::raw::ARCHIVE_WARN {
            return Err(reader_error(reader, pass_ctx));
        }

        match sink {
            DataSink::Discard => {}
            DataSink::Disk(disk_writer) => {
                let wrc = unsafe {
                    ffi::raw::archive_write_data_block(disk_writer, buffer, size, offset)
                };
                if wrc < 0 {
                    return Err(ArchiveError::LibArchive(unsafe {
                        archive_err_string(disk_writer)
                    }));
                }
            }
            DataSink::Stream(writer) => {
                if size > 0 {
                    let wrc = unsafe { ffi::raw::archive_write_data(writer, buffer, size) };
                    if wrc < 0 {
                        return Err(ArchiveError::LibArchive(unsafe {
                            archive_err_string(writer)
                        }));
                    }
                }
            }
        }

        *bytes_done += size as u64;
        progress.send(event_tx, label, *bytes_done, bytes_total);
    }
}

fn resolve_raw_entry_path(raw_mode: bool, rel: String, archive_path: &Path) -> String {
    if raw_mode && rel == "data" {
        archive_stem(archive_path)
    } else {
        rel
    }
}

/// Lists all entries in `archive_path`, prompting over `event_tx` if encrypted.
pub fn list_archive(
    archive_path: &Path,
    initial_password: Option<&str>,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<ArchiveListing, ArchiveError> {
    let mut raw_mode = false;
    let file_size = fs::metadata(archive_path).ok().map(|m| m.len());
    let mut pass_ctx = PassphraseCtx::for_path(archive_path, initial_password, event_tx, cancel);

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }

        let reader = match open_reader_step(archive_path, raw_mode, !raw_mode, &mut pass_ctx)? {
            ReaderOpen::Ready(r) => r,
            ReaderOpen::RetryRaw => {
                raw_mode = true;
                continue;
            }
        };
        let mut entries = Vec::new();
        let mut needs_raw_retry = false;
        let mut first = true;
        let mut format_name = String::new();

        unsafe {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ArchiveError::Cancelled);
                }
                let entry = match read_next_header_step(
                    reader.as_ptr(),
                    archive_path,
                    &pass_ctx,
                    &mut first,
                    !raw_mode,
                )? {
                    HeaderStep::Eof => break,
                    HeaderStep::RetryRaw => {
                        needs_raw_retry = true;
                        break;
                    }
                    HeaderStep::Entry(entry) => entry,
                };
                if first {
                    format_name = describe_reader_format(reader.as_ptr());
                }
                first = false;

                let raw_name = entry_pathname_string(entry)?;
                let Some(rel_path) = sanitize_entry_path_str(&raw_name)? else {
                    continue;
                };
                let path_str = resolve_raw_entry_path(raw_mode, rel_path, archive_path);

                let filetype = ffi::raw::archive_entry_filetype(entry);
                let kind = filetype_to_kind(filetype);
                let size = if ffi::raw::archive_entry_size_is_set(entry) != 0 {
                    ffi::raw::archive_entry_size(entry).max(0) as u64
                } else {
                    0
                };
                let compressed_size = if raw_mode { file_size } else { None };
                let mtime = if ffi::raw::archive_entry_mtime_is_set(entry) != 0 {
                    Some(ffi::raw::archive_entry_mtime(entry) as i64)
                } else {
                    None
                };
                let is_encrypted = ffi::raw::archive_entry_is_encrypted(entry) != 0;
                let mode = ffi::raw::archive_entry_perm(entry);
                let symlink_target = if kind == EntryKind::Symlink {
                    entry_symlink_string(entry)
                } else {
                    None
                };

                entries.push(ArchiveEntry {
                    path: path_str,
                    size,
                    compressed_size,
                    kind,
                    mtime,
                    mode,
                    symlink_target,
                    is_encrypted,
                });
            }
        }

        if needs_raw_retry {
            raw_mode = true;
            continue;
        }

        return Ok(ArchiveListing {
            entries,
            password: pass_ctx.last_password(),
            is_raw: raw_mode,
            format_name,
        });
    }
}

/// Verifies archive integrity by reading and decompressing every entry stream in memory.
pub fn test_archive(
    archive_path: &Path,
    initial_password: Option<&str>,
    known: Option<(u64, bool)>,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<(usize, u64, Option<String>), ArchiveError> {
    let (bytes_total, mut raw_mode) = known.unwrap_or((0, false));
    let mut pass_ctx = PassphraseCtx::for_path(archive_path, initial_password, event_tx, cancel);

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let reader = match open_reader_step(
            archive_path,
            raw_mode,
            known.is_none() && !raw_mode,
            &mut pass_ctx,
        )? {
            ReaderOpen::Ready(r) => r,
            ReaderOpen::RetryRaw => {
                raw_mode = true;
                continue;
            }
        };
        let mut entries_checked = 0usize;
        let mut bytes_done = 0u64;
        let mut first = true;
        let mut needs_raw_retry = false;
        let mut progress = ProgressThrottle::default();

        unsafe {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ArchiveError::Cancelled);
                }
                let entry = match read_next_header_step(
                    reader.as_ptr(),
                    archive_path,
                    &pass_ctx,
                    &mut first,
                    known.is_none() && !raw_mode,
                )? {
                    HeaderStep::Eof => break,
                    HeaderStep::RetryRaw => {
                        needs_raw_retry = true;
                        break;
                    }
                    HeaderStep::Entry(entry) => entry,
                };
                first = false;

                let raw_name = entry_pathname_string(entry)?;
                let Some(sanitized) = sanitize_entry_path_str(&raw_name)? else {
                    continue;
                };
                let label = resolve_raw_entry_path(raw_mode, sanitized, archive_path);

                let filetype = ffi::raw::archive_entry_filetype(entry);
                let is_dir = (filetype & ffi::raw::AE_IFMT) == ffi::raw::AE_IFDIR;
                if (filetype & ffi::raw::AE_IFMT) == ffi::raw::AE_IFLNK
                    && let Some(sym) = entry_symlink_string(entry)
                {
                    validate_symlink_target(Path::new(&label), &sym)?;
                }

                progress.send(event_tx, &label, bytes_done, bytes_total);
                if !is_dir {
                    pump_archive_data(
                        reader.as_ptr(),
                        DataSink::Discard,
                        &pass_ctx,
                        &mut bytes_done,
                        bytes_total,
                        &label,
                        event_tx,
                        &mut progress,
                        cancel,
                    )?;
                }
                entries_checked += 1;
            }
        }

        if needs_raw_retry {
            raw_mode = true;
            continue;
        }
        return Ok((entries_checked, bytes_done, pass_ctx.last_password()));
    }
}

#[cfg(test)]
pub fn extract_archive(
    archive_path: &Path,
    dest_dir: &Path,
    selected_prefixes: Option<&[String]>,
    strip_prefix: Option<&str>,
    initial_password: Option<&str>,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<(usize, Option<String>), ArchiveError> {
    let mut session = ExtractSession::new(initial_password.map(ToOwned::to_owned), 0);
    let count = extract_archive_with_session(
        archive_path,
        dest_dir,
        ExtractOptions {
            selected_prefixes,
            strip_prefix,
            known_raw: None,
        },
        &mut session,
        event_tx,
        cancel,
    )?;
    Ok((count, session.password))
}

/// Single-pass extraction reusing shared conflict choices, password, and cumulative progress.
pub fn extract_archive_with_session(
    archive_path: &Path,
    dest_dir: &Path,
    options: ExtractOptions<'_>,
    session: &mut ExtractSession,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<usize, ArchiveError> {
    let mut raw_mode = options.known_raw.unwrap_or(false);
    fs::create_dir_all(dest_dir)?;
    let dest_dir = fs::canonicalize(dest_dir)?;
    let mut pass_ctx =
        PassphraseCtx::for_path(archive_path, session.password.as_deref(), event_tx, cancel);
    let selection_filter = options.selected_prefixes.map(SelectionFilter::new);
    let mut files_extracted: usize = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let reader = match open_reader_step(
            archive_path,
            raw_mode,
            options.known_raw.is_none() && !raw_mode,
            &mut pass_ctx,
        )? {
            ReaderOpen::Ready(r) => r,
            ReaderOpen::RetryRaw => {
                raw_mode = true;
                continue;
            }
        };
        let disk_writer = WriteDiskArchive::new()?;
        let mut first = true;
        let mut needs_raw_retry = false;
        let mut progress = ProgressThrottle::default();

        unsafe {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ArchiveError::Cancelled);
                }
                let entry = match read_next_header_step(
                    reader.as_ptr(),
                    archive_path,
                    &pass_ctx,
                    &mut first,
                    options.known_raw.is_none() && !raw_mode,
                )? {
                    HeaderStep::Eof => break,
                    HeaderStep::RetryRaw => {
                        needs_raw_retry = true;
                        break;
                    }
                    HeaderStep::Entry(entry) => entry,
                };
                first = false;

                let raw_name = entry_pathname_string(entry)?;
                let Some(sanitized) = sanitize_entry_path_str(&raw_name)? else {
                    continue;
                };
                let archive_rel_str = resolve_raw_entry_path(raw_mode, sanitized, archive_path);

                if let Some(ref filter) = selection_filter
                    && !filter.matches(&archive_rel_str)
                {
                    let _ = ffi::raw::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                let Some(archive_rel_str) =
                    apply_strip_prefix(&archive_rel_str, options.strip_prefix)
                else {
                    continue;
                };

                let Some(mut rel_out) = sanitize_entry_path(archive_rel_str)? else {
                    continue;
                };

                let filetype = ffi::raw::archive_entry_filetype(entry);
                let is_dir = (filetype & ffi::raw::AE_IFMT) == ffi::raw::AE_IFDIR;
                let is_symlink = (filetype & ffi::raw::AE_IFMT) == ffi::raw::AE_IFLNK;

                if is_symlink
                    && let Some(sym) = entry_symlink_string(entry)
                {
                    validate_symlink_target(&rel_out, &sym)?;
                }

                let mut target_path = dest_dir.join(&rel_out);
                let mut skip_entry = false;

                while !is_dir && target_path.symlink_metadata().is_ok() {
                    match resolve_conflict(
                        &target_path,
                        &mut session.conflict_policy,
                        event_tx,
                        cancel,
                    )? {
                        ConflictAction::Write => break,
                        ConflictAction::Skip => {
                            skip_entry = true;
                            break;
                        }
                        ConflictAction::Rename(new_name) => {
                            let safe_name = sanitize_single_filename(&new_name)?;
                            rel_out.set_file_name(safe_name);
                            target_path = dest_dir.join(&rel_out);
                        }
                    }
                }
                if skip_entry {
                    let _ = ffi::raw::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                progress.send(
                    event_tx,
                    archive_rel_str,
                    session.bytes_done,
                    session.bytes_total,
                );

                let target_cstr = path_to_cstring(&target_path)?;
                ffi::raw::archive_entry_set_pathname(entry, target_cstr.as_ptr());

                if let Some(hl_str) = entry_hardlink_string(entry) {
                    let Some(hl_rel) = sanitize_entry_path_str(&hl_str)? else {
                        return Err(ArchiveError::UnsafePath(hl_str));
                    };
                    let stripped_hl = apply_strip_prefix(&hl_rel, options.strip_prefix)
                        .and_then(|s| sanitize_entry_path(s).ok().flatten())
                        .unwrap_or_else(|| PathBuf::from(hl_rel));
                    let hl_dest = dest_dir.join(stripped_hl);
                    let hl_cstr = path_to_cstring(&hl_dest)?;
                    ffi::raw::archive_entry_set_hardlink(entry, hl_cstr.as_ptr());
                }

                let hdr_rc = ffi::raw::archive_write_header(disk_writer.as_ptr(), entry);
                if hdr_rc != ffi::raw::ARCHIVE_OK && hdr_rc != ffi::raw::ARCHIVE_WARN {
                    return Err(ArchiveError::LibArchive(archive_err_string(
                        disk_writer.as_ptr(),
                    )));
                }

                if !is_dir {
                    pump_archive_data(
                        reader.as_ptr(),
                        DataSink::Disk(disk_writer.as_ptr()),
                        &pass_ctx,
                        &mut session.bytes_done,
                        session.bytes_total,
                        archive_rel_str,
                        event_tx,
                        &mut progress,
                        cancel,
                    )?;
                }

                let fin_rc = ffi::raw::archive_write_finish_entry(disk_writer.as_ptr());
                if fin_rc != ffi::raw::ARCHIVE_OK && fin_rc != ffi::raw::ARCHIVE_WARN {
                    return Err(ArchiveError::LibArchive(archive_err_string(
                        disk_writer.as_ptr(),
                    )));
                }

                if !is_dir {
                    files_extracted += 1;
                }
            }
        }

        if needs_raw_retry {
            raw_mode = true;
            continue;
        }
        if let Some(pw) = pass_ctx.last_password() {
            session.password = Some(pw);
        }
        return Ok(files_extracted);
    }
}

fn resolve_conflict(
    target_path: &Path,
    policy: &mut Option<ConflictPolicy>,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<ConflictAction, ArchiveError> {
    match policy {
        Some(ConflictPolicy::OverwriteAll) => return Ok(ConflictAction::Write),
        Some(ConflictPolicy::SkipAll) => return Ok(ConflictAction::Skip),
        None => {}
    }

    let Some(tx) = event_tx else {
        return Ok(ConflictAction::Write);
    };

    let (reply_tx, reply_rx) = async_channel::bounded(1);
    tx.send_blocking(WorkerEvent::FileConflict {
        target_path: target_path.to_path_buf(),
        reply: reply_tx,
    })
    .map_err(|_| ArchiveError::Cancelled)?;

    match reply_rx.recv_blocking() {
        Ok(ConflictResolution::Overwrite) => Ok(ConflictAction::Write),
        Ok(ConflictResolution::OverwriteAll) => {
            *policy = Some(ConflictPolicy::OverwriteAll);
            Ok(ConflictAction::Write)
        }
        Ok(ConflictResolution::Skip) => Ok(ConflictAction::Skip),
        Ok(ConflictResolution::SkipAll) => {
            *policy = Some(ConflictPolicy::SkipAll);
            Ok(ConflictAction::Skip)
        }
        Ok(ConflictResolution::Rename(new_name)) => Ok(ConflictAction::Rename(new_name)),
        Err(_) => {
            cancel.store(true, Ordering::Relaxed);
            Err(ArchiveError::Cancelled)
        }
    }
}
