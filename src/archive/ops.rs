use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::raw::{c_int, c_long, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_channel::Sender;

use super::ffi::{
    self, check_read_write, describe_reader_format, entry_hardlink_string, entry_pathname_string,
    entry_symlink_string, filetype_to_kind, open_reader, open_reader_step, path_to_cstring,
    read_next_header_step, reader_error, to_cstring, HeaderStep, OwnedEntry, PassphraseCtx,
    ReaderOpen, WriteArchive, WriteDiskArchive, archive_err_string,
};
use super::{
    apply_strip_prefix, matches_selection, path_to_slash_string, sanitize_entry_path,
    sanitize_single_filename, validate_symlink_target, ArchiveEntry, ArchiveError, ArchiveFormat,
    ArchiveListing, ConflictAction, ConflictPolicy, ConflictResolution, EntryKind, ExtractSession,
    WorkerEvent,
};

const COMPOUND_TAR_EXTS: &[&str] = &[
    ".tar.zst",
    ".tar.xz",
    ".tar.gz",
    ".tar.bz2",
    ".tar.lz4",
    ".tar.lzma",
    ".tar.lzo",
    ".tar.lz",
    ".tar.z",
];

const WRITABLE_EXT_SPECS: &[(&str, WritableSpec)] = &[
    (".zip", WritableSpec::Zip),
    (".7z", WritableSpec::SevenZ),
    (
        ".tar.zst",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_ZSTD),
        },
    ),
    (
        ".tzst",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_ZSTD),
        },
    ),
    (
        ".tar.xz",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_XZ),
        },
    ),
    (
        ".txz",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_XZ),
        },
    ),
    (
        ".tar.gz",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_GZIP),
        },
    ),
    (
        ".tgz",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_GZIP),
        },
    ),
    (
        ".tar.bz2",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_BZIP2),
        },
    ),
    (
        ".tbz2",
        WritableSpec::Tar {
            filter: Some(ffi::raw::ARCHIVE_FILTER_BZIP2),
        },
    ),
    (".tar", WritableSpec::Tar { filter: None }),
];

const READONLY_ARCHIVE_EXTS: &[&str] = &[
    ".rar", ".cbr", ".cbz", ".cb7", ".cbt", ".epub", ".jar", ".apk", ".xpi", ".iso", ".cab",
    ".cpio", ".ar", ".deb", ".rpm", ".xar", ".lha", ".lzh", ".mtree", ".tzo", ".gz", ".xz",
    ".zst", ".bz2", ".lz4", ".lzma", ".lzo", ".lz", ".lrzip",
];

/// Returns `true` when `path` ends with a known archive or compressed-stream extension.
pub fn is_supported_archive_path(path: &Path) -> bool {
    let lower = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    COMPOUND_TAR_EXTS.iter().any(|ext| lower.ends_with(ext))
        || WRITABLE_EXT_SPECS
            .iter()
            .any(|(ext, _)| lower.ends_with(ext))
        || READONLY_ARCHIVE_EXTS.iter().any(|ext| lower.ends_with(ext))
}

/// Strips common archive extensions (including compound `.tar.*`) to derive a clean folder stem.
pub fn archive_stem(archive_path: &Path) -> String {
    let file_name = archive_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted");

    let lower = file_name.to_ascii_lowercase();
    for compound in COMPOUND_TAR_EXTS {
        if lower.ends_with(compound) && file_name.len() > compound.len() {
            return file_name[..file_name.len() - compound.len()].to_string();
        }
    }

    match Path::new(file_name).file_stem().and_then(|s| s.to_str()) {
        Some(stem) if !stem.is_empty() => stem.to_string(),
        _ => "extracted".to_string(),
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum WritableSpec {
    Tar { filter: Option<c_int> },
    Zip,
    SevenZ,
}

impl WritableSpec {
    pub(super) fn from_format(fmt: ArchiveFormat) -> Self {
        match fmt {
            ArchiveFormat::TarZst => WritableSpec::Tar {
                filter: Some(ffi::raw::ARCHIVE_FILTER_ZSTD),
            },
            ArchiveFormat::Zip => WritableSpec::Zip,
            ArchiveFormat::SevenZ => WritableSpec::SevenZ,
            ArchiveFormat::TarXz => WritableSpec::Tar {
                filter: Some(ffi::raw::ARCHIVE_FILTER_XZ),
            },
            ArchiveFormat::TarGz => WritableSpec::Tar {
                filter: Some(ffi::raw::ARCHIVE_FILTER_GZIP),
            },
        }
    }

    pub(super) fn from_detected(format_code: c_int, filter_code: c_int) -> Result<Self, ArchiveError> {
        let base = format_code & ffi::raw::ARCHIVE_FORMAT_BASE_MASK;
        match base {
            ffi::raw::ARCHIVE_FORMAT_ZIP => Ok(WritableSpec::Zip),
            ffi::raw::ARCHIVE_FORMAT_7ZIP => Ok(WritableSpec::SevenZ),
            ffi::raw::ARCHIVE_FORMAT_TAR => {
                let filter = match filter_code {
                    ffi::raw::ARCHIVE_FILTER_NONE => None,
                    ffi::raw::ARCHIVE_FILTER_GZIP
                    | ffi::raw::ARCHIVE_FILTER_BZIP2
                    | ffi::raw::ARCHIVE_FILTER_XZ
                    | ffi::raw::ARCHIVE_FILTER_ZSTD => Some(filter_code),
                    other => {
                        return Err(ArchiveError::UnsupportedFormat(format!(
                            "cannot modify tar with filter code {other}"
                        )));
                    }
                };
                Ok(WritableSpec::Tar { filter })
            }
            _ => Err(ArchiveError::UnsupportedFormat(
                "modifying this archive format is not supported by libarchive".to_string(),
            )),
        }
    }

    pub(super) fn apply(
        self,
        writer: *mut ffi::raw::archive,
        password: Option<&str>,
    ) -> Result<(), ArchiveError> {
        unsafe {
            match self {
                WritableSpec::Tar { filter } => {
                    check_read_write(
                        ffi::raw::archive_write_set_format_pax_restricted(writer),
                        writer,
                    )?;
                    if let Some(code) = filter {
                        check_read_write(ffi::raw::archive_write_add_filter(writer, code), writer)?;
                    }
                    if password.is_some_and(|p| !p.is_empty()) {
                        return Err(ArchiveError::UnsupportedFormat(
                            "TAR archives do not support password encryption".to_string(),
                        ));
                    }
                }
                WritableSpec::Zip => {
                    check_read_write(ffi::raw::archive_write_set_format_zip(writer), writer)?;
                    if let Some(pw) = password.filter(|p| !p.is_empty()) {
                        check_read_write(
                            ffi::raw::archive_write_set_format_option(
                                writer,
                                c"zip".as_ptr(),
                                c"encryption".as_ptr(),
                                c"aes256".as_ptr(),
                            ),
                            writer,
                        )?;
                        let c_pw = to_cstring(pw)?;
                        check_read_write(
                            ffi::raw::archive_write_set_passphrase(writer, c_pw.as_ptr()),
                            writer,
                        )?;
                    }
                }
                WritableSpec::SevenZ => {
                    check_read_write(ffi::raw::archive_write_set_format_7zip(writer), writer)?;
                    if password.is_some_and(|p| !p.is_empty()) {
                        return Err(ArchiveError::UnsupportedFormat(
                            "libarchive does not support creating encrypted 7z archives".to_string(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

pub(super) fn infer_writable_from_extension(path: &Path) -> Result<WritableSpec, ArchiveError> {
    let lower = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    WRITABLE_EXT_SPECS
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map(|(_, spec)| *spec)
        .ok_or_else(|| ArchiveError::UnsupportedFormat(format!("cannot add files to {}", path.display())))
}

fn resolve_raw_entry_path(raw_mode: bool, sanitized: &Path, archive_path: &Path) -> String {
    let rel = path_to_slash_string(sanitized);
    if raw_mode && rel == "data" {
        archive_stem(archive_path)
    } else {
        rel
    }
}

fn send_progress(
    event_tx: Option<&Sender<WorkerEvent>>,
    label: &str,
    bytes_done: u64,
    bytes_total: u64,
) {
    if let Some(tx) = event_tx {
        let _ = tx.try_send(WorkerEvent::Progress {
            current_file: label.to_string(),
            bytes_done,
            bytes_total,
        });
    }
}

enum DataSink {
    Discard,
    Disk(*mut ffi::raw::archive),
    Stream(*mut ffi::raw::archive),
}

#[allow(clippy::too_many_arguments)]
unsafe fn pump_archive_data(
    reader: *mut ffi::raw::archive,
    sink: DataSink,
    pass_ctx: &PassphraseCtx,
    bytes_done: &mut u64,
    bytes_total: u64,
    label: &str,
    event_tx: Option<&Sender<WorkerEvent>>,
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
        send_progress(event_tx, label, *bytes_done, bytes_total);
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
                let Some(rel_path) = sanitize_entry_path(&raw_name)? else {
                    continue;
                };
                let path_str = resolve_raw_entry_path(raw_mode, &rel_path, archive_path);

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
                let Some(sanitized) = sanitize_entry_path(&raw_name)? else {
                    continue;
                };
                let label = resolve_raw_entry_path(raw_mode, &sanitized, archive_path);

                let filetype = ffi::raw::archive_entry_filetype(entry);
                let is_dir = (filetype & ffi::raw::AE_IFMT) == ffi::raw::AE_IFDIR;
                if (filetype & ffi::raw::AE_IFMT) == ffi::raw::AE_IFLNK
                    && let Some(sym) = entry_symlink_string(entry)
                {
                    validate_symlink_target(&sanitized, &sym)?;
                }

                send_progress(event_tx, &label, bytes_done, bytes_total);
                if !is_dir {
                    pump_archive_data(
                        reader.as_ptr(),
                        DataSink::Discard,
                        &pass_ctx,
                        &mut bytes_done,
                        bytes_total,
                        &label,
                        event_tx,
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
        selected_prefixes,
        strip_prefix,
        None,
        &mut session,
        event_tx,
        cancel,
    )?;
    Ok((count, session.password))
}

/// Single-pass extraction reusing shared conflict choices, password, and cumulative progress.
#[allow(clippy::too_many_arguments)]
pub fn extract_archive_with_session(
    archive_path: &Path,
    dest_dir: &Path,
    selected_prefixes: Option<&[String]>,
    strip_prefix: Option<&str>,
    known_raw: Option<bool>,
    session: &mut ExtractSession,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<usize, ArchiveError> {
    let mut raw_mode = known_raw.unwrap_or(false);
    fs::create_dir_all(dest_dir)?;
    let dest_dir = fs::canonicalize(dest_dir)?;
    let mut pass_ctx =
        PassphraseCtx::for_path(archive_path, session.password.as_deref(), event_tx, cancel);
    let mut files_extracted: usize = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let reader = match open_reader_step(
            archive_path,
            raw_mode,
            known_raw.is_none() && !raw_mode,
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
                    known_raw.is_none() && !raw_mode,
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
                let Some(sanitized) = sanitize_entry_path(&raw_name)? else {
                    continue;
                };
                let archive_rel_str = resolve_raw_entry_path(raw_mode, &sanitized, archive_path);

                if !matches_selection(&archive_rel_str, selected_prefixes) {
                    let _ = ffi::raw::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                let Some(archive_rel_str) = apply_strip_prefix(&archive_rel_str, strip_prefix)
                else {
                    continue;
                };

                let Some(mut rel_out) = sanitize_entry_path(&archive_rel_str)? else {
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

                send_progress(
                    event_tx,
                    &archive_rel_str,
                    session.bytes_done,
                    session.bytes_total,
                );

                let target_cstr = path_to_cstring(&target_path)?;
                ffi::raw::archive_entry_set_pathname(entry, target_cstr.as_ptr());

                if let Some(hl_str) = entry_hardlink_string(entry) {
                    let Some(safe_hl) = sanitize_entry_path(&hl_str)? else {
                        return Err(ArchiveError::UnsafePath(hl_str));
                    };
                    let hl_rel = path_to_slash_string(&safe_hl);
                    let stripped_hl = apply_strip_prefix(&hl_rel, strip_prefix)
                        .and_then(|s| sanitize_entry_path(&s).ok().flatten())
                        .unwrap_or(safe_hl);
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
                        &archive_rel_str,
                        event_tx,
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
    let res = (|| -> Result<(), ArchiveError> {
        for item in &disk_items {
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            write_disk_item(
                writer.as_ptr(),
                item,
                &mut bytes_done,
                bytes_total,
                event_tx,
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
                let Some(sanitized) = sanitize_entry_path(&raw_name)? else {
                    continue;
                };
                let rel = path_to_slash_string(&sanitized);
                if replace_set.contains(&rel) {
                    let _ = ffi::raw::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                send_progress(event_tx, &rel, bytes_done, bytes_total);
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
                        cancel,
                    )?;
                }
                check_read_write(
                    ffi::raw::archive_write_finish_entry(writer.as_ptr()),
                    writer.as_ptr(),
                )?;
            }
        }

        for item in &disk_items {
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            write_disk_item(
                writer.as_ptr(),
                item,
                &mut bytes_done,
                bytes_total,
                event_tx,
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

fn write_disk_item(
    writer: *mut ffi::raw::archive,
    item: &DiskItem,
    bytes_done: &mut u64,
    bytes_total: u64,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), ArchiveError> {
    send_progress(event_tx, &item.archive_path, *bytes_done, bytes_total);

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
                let mut buf = [0u8; 65536];
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(ArchiveError::Cancelled);
                    }
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    let written =
                        ffi::raw::archive_write_data(writer, buf.as_ptr() as *const c_void, n);
                    if written < 0 {
                        return Err(ArchiveError::LibArchive(archive_err_string(writer)));
                    }
                    *bytes_done += n as u64;
                    send_progress(event_tx, &item.archive_path, *bytes_done, bytes_total);
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
