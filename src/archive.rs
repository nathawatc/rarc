use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Read;
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::path::{Component, Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_channel::Sender;

/// Archive creation formats offered by the UI and `--tap create`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarZst,
    Zip,
    SevenZ,
    TarXz,
    TarGz,
}

impl ArchiveFormat {
    pub const ALL: [ArchiveFormat; 5] = [
        ArchiveFormat::TarZst,
        ArchiveFormat::Zip,
        ArchiveFormat::SevenZ,
        ArchiveFormat::TarXz,
        ArchiveFormat::TarGz,
    ];

    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::TarZst => ".tar.zst",
            ArchiveFormat::Zip => ".zip",
            ArchiveFormat::SevenZ => ".7z",
            ArchiveFormat::TarXz => ".tar.xz",
            ArchiveFormat::TarGz => ".tar.gz",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ArchiveFormat::TarZst => "Tar + Zstandard (.tar.zst)",
            ArchiveFormat::Zip => "ZIP Archive (.zip)",
            ArchiveFormat::SevenZ => "7-Zip Archive (.7z)",
            ArchiveFormat::TarXz => "Tar + XZ (.tar.xz)",
            ArchiveFormat::TarGz => "Tar + Gzip (.tar.gz)",
        }
    }

    pub fn supports_write_encryption(self) -> bool {
        matches!(self, ArchiveFormat::Zip)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl EntryKind {
    pub fn label(self, name: &str) -> String {
        match self {
            EntryKind::Directory => "Folder".to_string(),
            EntryKind::Symlink => "Symbolic Link".to_string(),
            EntryKind::Other => "Special".to_string(),
            EntryKind::File => match Path::new(name).extension().and_then(|e| e.to_str()) {
                Some(ext) if !ext.is_empty() => format!("{} File", ext.to_ascii_uppercase()),
                _ => "File".to_string(),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    /// Normalized forward-slash relative path inside the archive (never empty, no trailing slash).
    pub path: String,
    pub size: u64,
    // TODO: populate per-entry compressed size for ZIP/7z if needed later.
    pub compressed_size: Option<u64>,
    pub kind: EntryKind,
    pub mtime: Option<i64>,
    pub mode: u32,
    pub symlink_target: Option<String>,
    pub is_encrypted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictResolution {
    Overwrite,
    OverwriteAll,
    Skip,
    SkipAll,
    Rename(String),
}

#[derive(Clone, Debug)]
pub struct ArchiveListing {
    pub entries: Vec<ArchiveEntry>,
    pub password: Option<String>,
    pub is_raw: bool,
    pub format_name: String,
}

#[derive(Clone, Debug)]
pub enum OperationSummary {
    Listed(ArchiveListing),
    Extracted {
        password: Option<String>,
    },
    Tested {
        entries_checked: usize,
        bytes_checked: u64,
        password: Option<String>,
    },
    Created(PathBuf),
    Added {
        path: PathBuf,
        password: Option<String>,
    },
}

#[derive(Debug)]
pub enum WorkerEvent {
    Progress {
        current_file: String,
        bytes_done: u64,
        bytes_total: u64,
    },
    NeedPassword {
        archive_name: String,
        retry: bool,
        reply: Sender<Option<String>>,
    },
    FileConflict {
        target_path: PathBuf,
        reply: Sender<ConflictResolution>,
    },
    Finished(Result<OperationSummary, ArchiveError>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    Cancelled,
    UnsafePath(String),
    PasswordRequired,
    UnsupportedFormat(String),
    Io(String),
    LibArchive(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::Cancelled => write!(f, "Operation cancelled"),
            ArchiveError::UnsafePath(p) => write!(f, "Unsafe archive entry path rejected: {p}"),
            ArchiveError::PasswordRequired => write!(f, "Passphrase required"),
            ArchiveError::UnsupportedFormat(msg) => write!(f, "Unsupported format: {msg}"),
            ArchiveError::Io(msg) => write!(f, "I/O error: {msg}"),
            ArchiveError::LibArchive(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<std::io::Error> for ArchiveError {
    fn from(err: std::io::Error) -> Self {
        ArchiveError::Io(err.to_string())
    }
}

/// Validates and normalizes an archive entry path to prevent Zip Slip traversal.
///
/// Returns `Ok(None)` when the entry refers strictly to the archive root itself (`.`, `./`, `/`),
/// `Ok(Some(relative))` for a safe non-empty relative path, or `Err(ArchiveError::UnsafePath)`
/// if the path contains NUL bytes, empty/invalid syntax, or any `..` traversal component.
pub fn sanitize_entry_path(raw: &str) -> Result<Option<PathBuf>, ArchiveError> {
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.contains('\0') {
        return Err(ArchiveError::UnsafePath(raw.to_string()));
    }

    // Reject Windows-style backslash traversal as well as POSIX `..` segments.
    let mut normalized = PathBuf::new();
    for seg in raw.split(['/', '\\']) {
        match seg {
            "" | "." => continue,
            ".." => return Err(ArchiveError::UnsafePath(raw.to_string())),
            part => normalized.push(part),
        }
    }

    if normalized.as_os_str().is_empty() {
        return Ok(None);
    }

    for comp in normalized.components() {
        match comp {
            Component::Normal(_) => {}
            _ => return Err(ArchiveError::UnsafePath(raw.to_string())),
        }
    }

    Ok(Some(normalized))
}

/// Validates that a symlink target stored at `entry_rel` stays within the extraction root.
fn validate_symlink_target(entry_rel: &Path, target: &str) -> Result<(), ArchiveError> {
    if target.is_empty() || target.contains('\0') {
        return Err(ArchiveError::UnsafePath(target.to_string()));
    }
    let target_path = Path::new(target);
    if target_path.is_absolute() {
        return Err(ArchiveError::UnsafePath(target.to_string()));
    }

    let mut depth: isize = entry_rel
        .parent()
        .map(|p| p.components().count() as isize)
        .unwrap_or(0);
    for seg in target.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err(ArchiveError::UnsafePath(target.to_string()));
                }
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

fn path_to_slash_string(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

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
            filter: Some(ffi::ARCHIVE_FILTER_ZSTD),
        },
    ),
    (
        ".tzst",
        WritableSpec::Tar {
            filter: Some(ffi::ARCHIVE_FILTER_ZSTD),
        },
    ),
    (
        ".tar.xz",
        WritableSpec::Tar {
            filter: Some(ffi::ARCHIVE_FILTER_XZ),
        },
    ),
    (
        ".txz",
        WritableSpec::Tar {
            filter: Some(ffi::ARCHIVE_FILTER_XZ),
        },
    ),
    (
        ".tar.gz",
        WritableSpec::Tar {
            filter: Some(ffi::ARCHIVE_FILTER_GZIP),
        },
    ),
    (
        ".tgz",
        WritableSpec::Tar {
            filter: Some(ffi::ARCHIVE_FILTER_GZIP),
        },
    ),
    (
        ".tar.bz2",
        WritableSpec::Tar {
            filter: Some(ffi::ARCHIVE_FILTER_BZIP2),
        },
    ),
    (
        ".tbz2",
        WritableSpec::Tar {
            filter: Some(ffi::ARCHIVE_FILTER_BZIP2),
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

/// Smart root-folder rule for `--tap extract-here`:
/// - Extract directly into `parent` if there is a single top-level directory (or single file).
/// - Otherwise create `<parent>/<archive_stem>/` (with numeric suffix if it already exists).
pub fn resolve_extract_here_dir(entries: &[ArchiveEntry], archive_path: &Path) -> PathBuf {
    let parent = archive_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut roots: HashSet<&str> = HashSet::new();
    let mut single_root_is_dir = false;
    for entry in entries {
        if let Some((first, rest)) = entry.path.split_once('/') {
            roots.insert(first);
            if !rest.is_empty() {
                single_root_is_dir = true;
            }
        } else {
            roots.insert(entry.path.as_str());
            if entry.kind == EntryKind::Directory {
                single_root_is_dir = true;
            }
        }
        if roots.len() > 1 {
            break;
        }
    }

    let extract_directly =
        (roots.len() == 1 && single_root_is_dir) || (roots.len() <= 1 && entries.len() <= 1);
    if extract_directly {
        return parent.to_path_buf();
    }

    let stem = archive_stem(archive_path);
    let base = parent.join(&stem);
    if !base.exists() {
        return base;
    }
    for i in 1..=1000 {
        let candidate = parent.join(format!("{stem} ({i})"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

#[derive(Clone, Copy, Debug)]
enum WritableSpec {
    Tar { filter: Option<c_int> },
    Zip,
    SevenZ,
}

impl WritableSpec {
    fn from_format(fmt: ArchiveFormat) -> Self {
        match fmt {
            ArchiveFormat::TarZst => WritableSpec::Tar {
                filter: Some(ffi::ARCHIVE_FILTER_ZSTD),
            },
            ArchiveFormat::Zip => WritableSpec::Zip,
            ArchiveFormat::SevenZ => WritableSpec::SevenZ,
            ArchiveFormat::TarXz => WritableSpec::Tar {
                filter: Some(ffi::ARCHIVE_FILTER_XZ),
            },
            ArchiveFormat::TarGz => WritableSpec::Tar {
                filter: Some(ffi::ARCHIVE_FILTER_GZIP),
            },
        }
    }

    fn from_detected(format_code: c_int, filter_code: c_int) -> Result<Self, ArchiveError> {
        let base = format_code & ffi::ARCHIVE_FORMAT_BASE_MASK;
        match base {
            ffi::ARCHIVE_FORMAT_ZIP => Ok(WritableSpec::Zip),
            ffi::ARCHIVE_FORMAT_7ZIP => Ok(WritableSpec::SevenZ),
            ffi::ARCHIVE_FORMAT_TAR => {
                let filter = match filter_code {
                    ffi::ARCHIVE_FILTER_NONE => None,
                    ffi::ARCHIVE_FILTER_GZIP
                    | ffi::ARCHIVE_FILTER_BZIP2
                    | ffi::ARCHIVE_FILTER_XZ
                    | ffi::ARCHIVE_FILTER_ZSTD => Some(filter_code),
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

    fn apply(self, writer: *mut ffi::archive, password: Option<&str>) -> Result<(), ArchiveError> {
        unsafe {
            match self {
                WritableSpec::Tar { filter } => {
                    check_read_write(
                        ffi::archive_write_set_format_pax_restricted(writer),
                        writer,
                    )?;
                    if let Some(code) = filter {
                        check_read_write(ffi::archive_write_add_filter(writer, code), writer)?;
                    }
                    if password.is_some_and(|p| !p.is_empty()) {
                        return Err(ArchiveError::UnsupportedFormat(
                            "TAR archives do not support password encryption".to_string(),
                        ));
                    }
                }
                WritableSpec::Zip => {
                    check_read_write(ffi::archive_write_set_format_zip(writer), writer)?;
                    if let Some(pw) = password.filter(|p| !p.is_empty()) {
                        let c_zip = c"zip";
                        let c_enc = c"encryption";
                        let c_aes = c"aes256";
                        check_read_write(
                            ffi::archive_write_set_format_option(
                                writer,
                                c_zip.as_ptr(),
                                c_enc.as_ptr(),
                                c_aes.as_ptr(),
                            ),
                            writer,
                        )?;
                        let c_pw = to_cstring(pw)?;
                        check_read_write(
                            ffi::archive_write_set_passphrase(writer, c_pw.as_ptr()),
                            writer,
                        )?;
                    }
                }
                WritableSpec::SevenZ => {
                    check_read_write(ffi::archive_write_set_format_7zip(writer), writer)?;
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

/// Lists all entries in `archive_path`, prompting over `event_tx` if encrypted.
pub fn list_archive(
    archive_path: &Path,
    initial_password: Option<&str>,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<ArchiveListing, ArchiveError> {
    let mut raw_mode = false;
    let file_size = fs::metadata(archive_path).ok().map(|m| m.len());
    let mut pass_ctx = PassphraseCtx::new(
        archive_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive")
            .to_string(),
        initial_password,
        event_tx.cloned(),
        cancel.clone(),
    );

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }

        let reader = open_reader(archive_path, raw_mode, &mut pass_ctx)?;
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
                let sanitized = sanitize_entry_path(&raw_name)?;
                let Some(rel_path) = sanitized else {
                    continue;
                };

                let path_str = if raw_mode && path_to_slash_string(&rel_path) == "data" {
                    archive_stem(archive_path)
                } else {
                    path_to_slash_string(&rel_path)
                };

                let filetype = ffi::archive_entry_filetype(entry);
                let kind = filetype_to_kind(filetype);
                let size = if ffi::archive_entry_size_is_set(entry) != 0 {
                    ffi::archive_entry_size(entry).max(0) as u64
                } else {
                    0
                };
                let compressed_size = if raw_mode { file_size } else { None };
                let mtime = if ffi::archive_entry_mtime_is_set(entry) != 0 {
                    Some(ffi::archive_entry_mtime(entry) as i64)
                } else {
                    None
                };
                let is_encrypted = ffi::archive_entry_is_encrypted(entry) != 0;
                let mode = ffi::archive_entry_perm(entry);
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
    let mut pass_ctx = PassphraseCtx::new(
        archive_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive")
            .to_string(),
        initial_password,
        event_tx.cloned(),
        cancel.clone(),
    );

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let reader = open_reader(archive_path, raw_mode, &mut pass_ctx)?;
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
                let label = if raw_mode && path_to_slash_string(&sanitized) == "data" {
                    archive_stem(archive_path)
                } else {
                    path_to_slash_string(&sanitized)
                };

                let filetype = ffi::archive_entry_filetype(entry);
                let is_dir = (filetype & ffi::AE_IFMT) == ffi::AE_IFDIR;
                if (filetype & ffi::AE_IFMT) == ffi::AE_IFLNK
                    && let Some(sym) = entry_symlink_string(entry)
                {
                    validate_symlink_target(&sanitized, &sym)?;
                }

                if let Some(tx) = event_tx {
                    let _ = tx.try_send(WorkerEvent::Progress {
                        current_file: label.clone(),
                        bytes_done,
                        bytes_total,
                    });
                }

                if !is_dir {
                    let mut buffer: *const c_void = ptr::null();
                    let mut size: usize = 0;
                    let mut offset: i64 = 0;
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(ArchiveError::Cancelled);
                        }
                        let drc = ffi::archive_read_data_block(
                            reader.as_ptr(),
                            &mut buffer,
                            &mut size,
                            &mut offset,
                        );
                        if drc == ffi::ARCHIVE_EOF {
                            break;
                        }
                        if drc != ffi::ARCHIVE_OK && drc != ffi::ARCHIVE_WARN {
                            return Err(reader_error(reader.as_ptr(), &pass_ctx));
                        }
                        bytes_done += size as u64;
                        if let Some(tx) = event_tx {
                            let _ = tx.try_send(WorkerEvent::Progress {
                                current_file: label.clone(),
                                bytes_done,
                                bytes_total,
                            });
                        }
                    }
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

/// Shared state preserved across multi-archive extraction runs.
#[derive(Default)]
pub struct ExtractSession {
    conflict_policy: Option<ConflictPolicy>,
    pub password: Option<String>,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl ExtractSession {
    pub fn new(password: Option<String>, bytes_total: u64) -> Self {
        Self {
            conflict_policy: None,
            password,
            bytes_done: 0,
            bytes_total,
        }
    }
}

enum HeaderStep {
    Eof,
    Entry(*mut ffi::archive_entry),
    RetryRaw,
}

unsafe fn read_next_header_step(
    reader: *mut ffi::archive,
    archive_path: &Path,
    pass_ctx: &PassphraseCtx,
    first: &mut bool,
    allow_raw_retry: bool,
) -> Result<HeaderStep, ArchiveError> {
    let mut entry: *mut ffi::archive_entry = ptr::null_mut();
    let rc = unsafe { ffi::archive_read_next_header(reader, &mut entry) };
    if rc == ffi::ARCHIVE_EOF {
        return Ok(HeaderStep::Eof);
    }
    if rc != ffi::ARCHIVE_OK && rc != ffi::ARCHIVE_WARN {
        if *first
            && allow_raw_retry
            && unsafe { ffi::archive_filter_code(reader, 0) } != ffi::ARCHIVE_FILTER_NONE
        {
            return Ok(HeaderStep::RetryRaw);
        }
        return Err(reader_error(reader, pass_ctx));
    }
    if *first && allow_raw_retry {
        let fmt = unsafe { ffi::archive_format(reader) } & ffi::ARCHIVE_FORMAT_BASE_MASK;
        let is_mtree_ext = archive_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mtree"));
        if fmt == ffi::ARCHIVE_FORMAT_MTREE && !is_mtree_ext {
            return Ok(HeaderStep::RetryRaw);
        }
    }
    Ok(HeaderStep::Entry(entry))
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
    let mut pass_ctx = PassphraseCtx::new(
        archive_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive")
            .to_string(),
        session.password.as_deref(),
        event_tx.cloned(),
        cancel.clone(),
    );

    let mut files_extracted: usize = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ArchiveError::Cancelled);
        }
        let reader = open_reader(archive_path, raw_mode, &mut pass_ctx)?;
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
                let archive_rel_str = if raw_mode && path_to_slash_string(&sanitized) == "data" {
                    archive_stem(archive_path)
                } else {
                    path_to_slash_string(&sanitized)
                };

                if !matches_selection(&archive_rel_str, selected_prefixes) {
                    let _ = ffi::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                let Some(archive_rel_str) = apply_strip_prefix(&archive_rel_str, strip_prefix)
                else {
                    continue;
                };

                let Some(mut rel_out) = sanitize_entry_path(&archive_rel_str)? else {
                    continue;
                };

                let filetype = ffi::archive_entry_filetype(entry);
                let is_dir = (filetype & ffi::AE_IFMT) == ffi::AE_IFDIR;
                let is_symlink = (filetype & ffi::AE_IFMT) == ffi::AE_IFLNK;

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
                    let _ = ffi::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                if let Some(tx) = event_tx {
                    let _ = tx.try_send(WorkerEvent::Progress {
                        current_file: archive_rel_str.clone(),
                        bytes_done: session.bytes_done,
                        bytes_total: session.bytes_total,
                    });
                }

                let target_cstr = path_to_cstring(&target_path)?;
                ffi::archive_entry_set_pathname(entry, target_cstr.as_ptr());

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
                    ffi::archive_entry_set_hardlink(entry, hl_cstr.as_ptr());
                }

                let hdr_rc = ffi::archive_write_header(disk_writer.as_ptr(), entry);
                if hdr_rc != ffi::ARCHIVE_OK && hdr_rc != ffi::ARCHIVE_WARN {
                    return Err(ArchiveError::LibArchive(archive_err_string(
                        disk_writer.as_ptr(),
                    )));
                }

                if !is_dir {
                    copy_archive_data(
                        reader.as_ptr(),
                        disk_writer.as_ptr(),
                        &pass_ctx,
                        &mut session.bytes_done,
                        session.bytes_total,
                        &archive_rel_str,
                        event_tx,
                        cancel,
                    )?;
                }

                let fin_rc = ffi::archive_write_finish_entry(disk_writer.as_ptr());
                if fin_rc != ffi::ARCHIVE_OK && fin_rc != ffi::ARCHIVE_WARN {
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
            ffi::archive_write_open_filename(writer.as_ptr(), tmp_cstr.as_ptr()),
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
        let mut probe_ctx = PassphraseCtx::new(
            archive_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("archive")
                .to_string(),
            None,
            event_tx.cloned(),
            cancel.clone(),
        );
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

    let mut pass_ctx = PassphraseCtx::new(
        archive_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive")
            .to_string(),
        effective_pw.as_deref(),
        event_tx.cloned(),
        cancel.clone(),
    );

    let reader = open_reader(archive_path, false, &mut pass_ctx)?;
    let tmp_path = temp_sibling_path(archive_path);
    let writer = WriteArchive::new()?;
    let mut writer_opened = false;
    let mut bytes_done = 0u64;

    let res = (|| -> Result<(), ArchiveError> {
        unsafe {
            let mut entry: *mut ffi::archive_entry = ptr::null_mut();
            let mut first = true;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ArchiveError::Cancelled);
                }
                let rc = ffi::archive_read_next_header(reader.as_ptr(), &mut entry);
                if rc == ffi::ARCHIVE_EOF {
                    if first {
                        let spec = infer_writable_from_extension(archive_path)?;
                        spec.apply(writer.as_ptr(), effective_pw.as_deref())?;
                        let tmp_cstr = path_to_cstring(&tmp_path)?;
                        check_read_write(
                            ffi::archive_write_open_filename(writer.as_ptr(), tmp_cstr.as_ptr()),
                            writer.as_ptr(),
                        )?;
                        writer_opened = true;
                    }
                    break;
                }
                if rc != ffi::ARCHIVE_OK && rc != ffi::ARCHIVE_WARN {
                    return Err(reader_error(reader.as_ptr(), &pass_ctx));
                }

                if first {
                    first = false;
                    let fmt = ffi::archive_format(reader.as_ptr());
                    let flt = ffi::archive_filter_code(reader.as_ptr(), 0);
                    let spec = WritableSpec::from_detected(fmt, flt)?;
                    spec.apply(writer.as_ptr(), effective_pw.as_deref())?;
                    let tmp_cstr = path_to_cstring(&tmp_path)?;
                    check_read_write(
                        ffi::archive_write_open_filename(writer.as_ptr(), tmp_cstr.as_ptr()),
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
                    let _ = ffi::archive_read_data_skip(reader.as_ptr());
                    continue;
                }

                if let Some(tx) = event_tx {
                    let _ = tx.try_send(WorkerEvent::Progress {
                        current_file: rel.clone(),
                        bytes_done,
                        bytes_total,
                    });
                }

                check_read_write(
                    ffi::archive_write_header(writer.as_ptr(), entry),
                    writer.as_ptr(),
                )?;
                let filetype = ffi::archive_entry_filetype(entry);
                if (filetype & ffi::AE_IFMT) != ffi::AE_IFDIR {
                    copy_archive_stream_data(
                        reader.as_ptr(),
                        writer.as_ptr(),
                        &pass_ctx,
                        &mut bytes_done,
                        bytes_total,
                        &rel,
                        event_tx,
                        cancel,
                    )?;
                }
                check_read_write(
                    ffi::archive_write_finish_entry(writer.as_ptr()),
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
        let mut entry: *mut ffi::archive_entry = ptr::null_mut();
        let mut first = true;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            let rc = ffi::archive_read_next_header(probe.as_ptr(), &mut entry);
            if rc == ffi::ARCHIVE_EOF {
                break;
            }
            if rc != ffi::ARCHIVE_OK && rc != ffi::ARCHIVE_WARN {
                return Err(reader_error(probe.as_ptr(), pass_ctx));
            }
            if first {
                first = false;
                let fmt = ffi::archive_format(probe.as_ptr());
                let flt = ffi::archive_filter_code(probe.as_ptr(), 0);
                let spec = WritableSpec::from_detected(fmt, flt)?;
                if !matches!(spec, WritableSpec::Zip) {
                    return Err(ArchiveError::UnsupportedFormat(
                        "modifying encrypted non-ZIP archives is not supported".to_string(),
                    ));
                }
            }
            let is_dir = (ffi::archive_entry_filetype(entry) & ffi::AE_IFMT) == ffi::AE_IFDIR;
            if !is_dir && ffi::archive_entry_is_encrypted(entry) != 0 {
                let mut buf: *const c_void = ptr::null();
                let mut size: usize = 0;
                let mut offset: i64 = 0;
                let drc =
                    ffi::archive_read_data_block(probe.as_ptr(), &mut buf, &mut size, &mut offset);
                if drc != ffi::ARCHIVE_OK && drc != ffi::ARCHIVE_WARN && drc != ffi::ARCHIVE_EOF {
                    return Err(reader_error(probe.as_ptr(), pass_ctx));
                }
                break;
            }
            let _ = ffi::archive_read_data_skip(probe.as_ptr());
        }
    }
    Ok(pass_ctx.last_password())
}

fn infer_writable_from_extension(path: &Path) -> Result<WritableSpec, ArchiveError> {
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

pub fn matches_selection(entry_path: &str, selected_prefixes: Option<&[String]>) -> bool {
    let Some(prefixes) = selected_prefixes else {
        return true;
    };
    prefixes.iter().any(|sel| {
        let sel_clean = sel.trim_end_matches('/');
        entry_path == sel_clean
            || entry_path
                .strip_prefix(sel_clean)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn sanitize_single_filename(name: &str) -> Result<String, ArchiveError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains(['/', '\\', '\0'])
    {
        return Err(ArchiveError::UnsafePath(name.to_string()));
    }
    Ok(trimmed.to_string())
}

#[derive(Clone, Copy)]
enum ConflictPolicy {
    OverwriteAll,
    SkipAll,
}

enum ConflictAction {
    Write,
    Skip,
    Rename(String),
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
    writer: *mut ffi::archive,
    item: &DiskItem,
    bytes_done: &mut u64,
    bytes_total: u64,
    event_tx: Option<&Sender<WorkerEvent>>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), ArchiveError> {
    if let Some(tx) = event_tx {
        let _ = tx.try_send(WorkerEvent::Progress {
            current_file: item.archive_path.clone(),
            bytes_done: *bytes_done,
            bytes_total,
        });
    }

    let entry = OwnedEntry::new()?;
    let c_path = to_cstring(&item.archive_path)?;
    unsafe {
        ffi::archive_entry_set_pathname(entry.as_ptr(), c_path.as_ptr());
        ffi::archive_entry_set_perm(entry.as_ptr(), item.mode);
        ffi::archive_entry_set_mtime(entry.as_ptr(), item.mtime as c_long, 0);

        match item.kind {
            EntryKind::Directory => {
                ffi::archive_entry_set_filetype(entry.as_ptr(), ffi::AE_IFDIR);
                ffi::archive_entry_set_size(entry.as_ptr(), 0);
                check_read_write(
                    ffi::archive_write_header(writer, entry.as_ptr()),
                    writer,
                )?;
            }
            EntryKind::Symlink => {
                ffi::archive_entry_set_filetype(entry.as_ptr(), ffi::AE_IFLNK);
                ffi::archive_entry_set_size(entry.as_ptr(), 0);
                if let Some(ref target) = item.symlink_target {
                    let c_target = path_to_cstring(target)?;
                    ffi::archive_entry_set_symlink(entry.as_ptr(), c_target.as_ptr());
                }
                check_read_write(
                    ffi::archive_write_header(writer, entry.as_ptr()),
                    writer,
                )?;
            }
            EntryKind::File | EntryKind::Other => {
                ffi::archive_entry_set_filetype(entry.as_ptr(), ffi::AE_IFREG);
                ffi::archive_entry_set_size(entry.as_ptr(), item.size as i64);
                check_read_write(
                    ffi::archive_write_header(writer, entry.as_ptr()),
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
                    let written = ffi::archive_write_data(writer, buf.as_ptr() as *const c_void, n);
                    if written < 0 {
                        return Err(ArchiveError::LibArchive(archive_err_string(writer)));
                    }
                    *bytes_done += n as u64;
                    if let Some(tx) = event_tx {
                        let _ = tx.try_send(WorkerEvent::Progress {
                            current_file: item.archive_path.clone(),
                            bytes_done: *bytes_done,
                            bytes_total,
                        });
                    }
                }
            }
        }

        check_read_write(ffi::archive_write_finish_entry(writer), writer)?;
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

#[allow(clippy::too_many_arguments)]
unsafe fn copy_archive_data(
    reader: *mut ffi::archive,
    disk_writer: *mut ffi::archive,
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
        let rc = unsafe { ffi::archive_read_data_block(reader, &mut buffer, &mut size, &mut offset) };
        if rc == ffi::ARCHIVE_EOF {
            return Ok(());
        }
        if rc != ffi::ARCHIVE_OK && rc != ffi::ARCHIVE_WARN {
            return Err(reader_error(reader, pass_ctx));
        }

        let wrc = unsafe { ffi::archive_write_data_block(disk_writer, buffer, size, offset) };
        if wrc < 0 {
            return Err(ArchiveError::LibArchive(unsafe {
                archive_err_string(disk_writer)
            }));
        }

        *bytes_done += size as u64;
        if let Some(tx) = event_tx {
            let _ = tx.try_send(WorkerEvent::Progress {
                current_file: label.to_string(),
                bytes_done: *bytes_done,
                bytes_total,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn copy_archive_stream_data(
    reader: *mut ffi::archive,
    writer: *mut ffi::archive,
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
        let rc = unsafe { ffi::archive_read_data_block(reader, &mut buffer, &mut size, &mut offset) };
        if rc == ffi::ARCHIVE_EOF {
            return Ok(());
        }
        if rc != ffi::ARCHIVE_OK && rc != ffi::ARCHIVE_WARN {
            return Err(reader_error(reader, pass_ctx));
        }

        if size > 0 {
            let wrc = unsafe { ffi::archive_write_data(writer, buffer, size) };
            if wrc < 0 {
                return Err(ArchiveError::LibArchive(unsafe {
                    archive_err_string(writer)
                }));
            }
            *bytes_done += size as u64;
            if let Some(tx) = event_tx {
                let _ = tx.try_send(WorkerEvent::Progress {
                    current_file: label.to_string(),
                    bytes_done: *bytes_done,
                    bytes_total,
                });
            }
        }
    }
}

struct PassphraseCtx {
    archive_name: String,
    initial: Option<CString>,
    stored: Vec<CString>,
    event_tx: Option<Sender<WorkerEvent>>,
    cancelled: Arc<AtomicBool>,
    user_declined: bool,
}

impl PassphraseCtx {
    fn new(
        archive_name: String,
        initial_password: Option<&str>,
        event_tx: Option<Sender<WorkerEvent>>,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        let initial = initial_password
            .filter(|p| !p.is_empty())
            .and_then(|p| CString::new(p).ok());
        Self {
            archive_name,
            initial,
            stored: Vec::new(),
            event_tx,
            cancelled,
            user_declined: false,
        }
    }

    fn last_password(&self) -> Option<String> {
        self.stored
            .last()
            .or(self.initial.as_ref())
            .and_then(|c| c.to_str().ok())
            .map(ToOwned::to_owned)
    }
}

unsafe extern "C" fn passphrase_callback(
    _archive: *mut ffi::archive,
    client_data: *mut c_void,
) -> *const c_char {
    if client_data.is_null() {
        return ptr::null();
    }
    let res = std::panic::catch_unwind(|| {
        let ctx = unsafe { &mut *(client_data as *mut PassphraseCtx) };
        if ctx.cancelled.load(Ordering::Relaxed) || ctx.user_declined {
            return ptr::null();
        }
        let Some(tx) = &ctx.event_tx else {
            return ptr::null();
        };

        let (reply_tx, reply_rx) = async_channel::bounded(1);
        let retry = !ctx.stored.is_empty() || ctx.initial.is_some();
        if tx
            .send_blocking(WorkerEvent::NeedPassword {
                archive_name: ctx.archive_name.clone(),
                retry,
                reply: reply_tx,
            })
            .is_err()
        {
            ctx.user_declined = true;
            return ptr::null();
        }

        match reply_rx.recv_blocking() {
            Ok(Some(pw)) => {
                if let Ok(c_pw) = CString::new(pw) {
                    ctx.stored.push(c_pw);
                    ctx.stored.last().unwrap().as_ptr()
                } else {
                    ptr::null()
                }
            }
            Ok(None) | Err(_) => {
                ctx.user_declined = true;
                ctx.cancelled.store(true, Ordering::Relaxed);
                ptr::null()
            }
        }
    });
    res.unwrap_or(ptr::null())
}

fn open_reader(
    archive_path: &Path,
    raw_mode: bool,
    pass_ctx: &mut PassphraseCtx,
) -> Result<ReadArchive, ArchiveError> {
    let reader = ReadArchive::new()?;
    let c_path = path_to_cstring(archive_path)?;
    unsafe {
        check_read_write(
            ffi::archive_read_support_filter_all(reader.as_ptr()),
            reader.as_ptr(),
        )?;
        if raw_mode {
            check_read_write(
                ffi::archive_read_support_format_raw(reader.as_ptr()),
                reader.as_ptr(),
            )?;
        } else {
            check_read_write(
                ffi::archive_read_support_format_all(reader.as_ptr()),
                reader.as_ptr(),
            )?;
        }
        if let Some(ref initial) = pass_ctx.initial {
            check_read_write(
                ffi::archive_read_add_passphrase(reader.as_ptr(), initial.as_ptr()),
                reader.as_ptr(),
            )?;
        }
        check_read_write(
            ffi::archive_read_set_passphrase_callback(
                reader.as_ptr(),
                (pass_ctx as *mut PassphraseCtx) as *mut c_void,
                Some(passphrase_callback),
            ),
            reader.as_ptr(),
        )?;
        check_read_write(
            ffi::archive_read_open_filename(reader.as_ptr(), c_path.as_ptr(), 16384),
            reader.as_ptr(),
        )?;
    }
    Ok(reader)
}

fn reader_error(reader: *mut ffi::archive, pass_ctx: &PassphraseCtx) -> ArchiveError {
    if pass_ctx.cancelled.load(Ordering::Relaxed) || pass_ctx.user_declined {
        return ArchiveError::Cancelled;
    }
    let msg = unsafe { archive_err_string(reader) };
    if msg.to_ascii_lowercase().contains("passphrase") {
        ArchiveError::PasswordRequired
    } else {
        ArchiveError::LibArchive(msg)
    }
}

unsafe fn describe_reader_format(reader: *mut ffi::archive) -> String {
    let fmt_ptr = unsafe { ffi::archive_format_name(reader) };
    let fmt = if fmt_ptr.is_null() {
        "Unknown".to_string()
    } else {
        unsafe { CStr::from_ptr(fmt_ptr) }
            .to_string_lossy()
            .into_owned()
    };
    let flt_code = unsafe { ffi::archive_filter_code(reader, 0) };
    let flt_ptr = unsafe { ffi::archive_filter_name(reader, 0) };
    if flt_code != ffi::ARCHIVE_FILTER_NONE && !flt_ptr.is_null() {
        let flt = unsafe { CStr::from_ptr(flt_ptr) }.to_string_lossy();
        format!("{fmt} ({flt})")
    } else {
        fmt
    }
}

fn apply_strip_prefix(rel_path: &str, strip_prefix: Option<&str>) -> Option<String> {
    if let Some(prefix) = strip_prefix.map(|p| p.trim_end_matches('/')).filter(|p| !p.is_empty()) {
        let prefix_slash = format!("{prefix}/");
        if let Some(stripped) = rel_path.strip_prefix(&prefix_slash) {
            return Some(stripped.to_string());
        } else if rel_path == prefix {
            return None;
        }
    }
    Some(rel_path.to_string())
}

unsafe fn utf8_or_local_cstr(utf8: *const c_char, fallback: *const c_char) -> Option<String> {
    let ptr = if !utf8.is_null() { utf8 } else { fallback };
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
    }
}

unsafe fn entry_pathname_string(entry: *mut ffi::archive_entry) -> Result<String, ArchiveError> {
    unsafe {
        utf8_or_local_cstr(
            ffi::archive_entry_pathname_utf8(entry),
            ffi::archive_entry_pathname(entry),
        )
    }
    .ok_or_else(|| ArchiveError::LibArchive("Archive entry has unreadable filename".to_string()))
}

unsafe fn entry_symlink_string(entry: *mut ffi::archive_entry) -> Option<String> {
    unsafe {
        utf8_or_local_cstr(
            ffi::archive_entry_symlink_utf8(entry),
            ffi::archive_entry_symlink(entry),
        )
    }
}

unsafe fn entry_hardlink_string(entry: *mut ffi::archive_entry) -> Option<String> {
    unsafe {
        utf8_or_local_cstr(
            ffi::archive_entry_hardlink_utf8(entry),
            ffi::archive_entry_hardlink(entry),
        )
    }
}

fn filetype_to_kind(filetype: u32) -> EntryKind {
    match filetype & ffi::AE_IFMT {
        ffi::AE_IFDIR => EntryKind::Directory,
        ffi::AE_IFREG => EntryKind::File,
        ffi::AE_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

fn path_to_cstring(path: &Path) -> Result<CString, ArchiveError> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ArchiveError::UnsafePath(path.display().to_string()))
}

fn to_cstring(s: &str) -> Result<CString, ArchiveError> {
    CString::new(s).map_err(|_| ArchiveError::UnsafePath(s.to_string()))
}

unsafe fn check_read_write(rc: c_int, archive: *mut ffi::archive) -> Result<(), ArchiveError> {
    match rc {
        ffi::ARCHIVE_OK | ffi::ARCHIVE_WARN => Ok(()),
        _ => Err(ArchiveError::LibArchive(unsafe { archive_err_string(archive) })),
    }
}

unsafe fn archive_err_string(archive: *mut ffi::archive) -> String {
    let ptr = unsafe { ffi::archive_error_string(archive) };
    if ptr.is_null() {
        let code = unsafe { ffi::archive_errno(archive) };
        if code > 0 {
            std::io::Error::from_raw_os_error(code).to_string()
        } else {
            "libarchive operation failed".to_string()
        }
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

struct ReadArchive(*mut ffi::archive);

impl ReadArchive {
    fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { ffi::archive_read_new() };
        if ptr.is_null() {
            Err(ArchiveError::LibArchive(
                "Failed to allocate archive reader".to_string(),
            ))
        } else {
            Ok(Self(ptr))
        }
    }

    fn as_ptr(&self) -> *mut ffi::archive {
        self.0
    }
}

impl Drop for ReadArchive {
    fn drop(&mut self) {
        unsafe {
            ffi::archive_read_close(self.0);
            ffi::archive_read_free(self.0);
        }
    }
}

struct WriteDiskArchive(*mut ffi::archive);

impl WriteDiskArchive {
    fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { ffi::archive_write_disk_new() };
        if ptr.is_null() {
            return Err(ArchiveError::LibArchive(
                "Failed to allocate archive disk writer".to_string(),
            ));
        }
        let writer = Self(ptr);
        let flags = ffi::ARCHIVE_EXTRACT_TIME
            | ffi::ARCHIVE_EXTRACT_PERM
            | ffi::ARCHIVE_EXTRACT_SECURE_SYMLINKS
            | ffi::ARCHIVE_EXTRACT_SECURE_NODOTDOT;
        unsafe {
            check_read_write(ffi::archive_write_disk_set_options(ptr, flags), ptr)?;
            check_read_write(ffi::archive_write_disk_set_standard_lookup(ptr), ptr)?;
        }
        Ok(writer)
    }

    fn as_ptr(&self) -> *mut ffi::archive {
        self.0
    }
}

impl Drop for WriteDiskArchive {
    fn drop(&mut self) {
        unsafe {
            ffi::archive_write_close(self.0);
            ffi::archive_write_free(self.0);
        }
    }
}

struct WriteArchive {
    ptr: *mut ffi::archive,
    closed: bool,
}

impl WriteArchive {
    fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { ffi::archive_write_new() };
        if ptr.is_null() {
            Err(ArchiveError::LibArchive(
                "Failed to allocate archive writer".to_string(),
            ))
        } else {
            Ok(Self { ptr, closed: false })
        }
    }

    fn as_ptr(&self) -> *mut ffi::archive {
        self.ptr
    }

    fn fail(&self) {
        unsafe {
            ffi::archive_write_fail(self.ptr);
        }
    }

    fn close(mut self) -> Result<(), ArchiveError> {
        self.closed = true;
        unsafe {
            let rc = ffi::archive_write_close(self.ptr);
            let res = check_read_write(rc, self.ptr);
            ffi::archive_write_free(self.ptr);
            self.ptr = ptr::null_mut();
            res
        }
    }
}

impl Drop for WriteArchive {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                if !self.closed {
                    ffi::archive_write_close(self.ptr);
                }
                ffi::archive_write_free(self.ptr);
            }
        }
    }
}

struct OwnedEntry(*mut ffi::archive_entry);

impl OwnedEntry {
    fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { ffi::archive_entry_new() };
        if ptr.is_null() {
            Err(ArchiveError::LibArchive(
                "Failed to allocate archive entry".to_string(),
            ))
        } else {
            Ok(Self(ptr))
        }
    }

    fn as_ptr(&self) -> *mut ffi::archive_entry {
        self.0
    }
}

impl Drop for OwnedEntry {
    fn drop(&mut self) {
        unsafe {
            ffi::archive_entry_free(self.0);
        }
    }
}

/// Helper to spawn an operation on a dedicated worker thread reporting to `async-channel`.
pub fn spawn_worker<F>(f: F) -> async_channel::Receiver<WorkerEvent>
where
    F: FnOnce(&Sender<WorkerEvent>) -> Result<OperationSummary, ArchiveError> + Send + 'static,
{
    let (tx, rx) = async_channel::unbounded();
    std::thread::spawn(move || {
        let result = f(&tx);
        let _ = tx.send_blocking(WorkerEvent::Finished(result));
    });
    rx
}

mod ffi {
    use std::os::raw::{c_char, c_int, c_long, c_uint, c_void};

    pub const ARCHIVE_EOF: c_int = 1;
    pub const ARCHIVE_OK: c_int = 0;
    pub const ARCHIVE_WARN: c_int = -20;

    pub const ARCHIVE_FORMAT_BASE_MASK: c_int = 0xff0000;
    pub const ARCHIVE_FORMAT_TAR: c_int = 0x30000;
    pub const ARCHIVE_FORMAT_ZIP: c_int = 0x50000;
    pub const ARCHIVE_FORMAT_MTREE: c_int = 0x80000;
    pub const ARCHIVE_FORMAT_7ZIP: c_int = 0xE0000;

    pub const ARCHIVE_FILTER_NONE: c_int = 0;
    pub const ARCHIVE_FILTER_GZIP: c_int = 1;
    pub const ARCHIVE_FILTER_BZIP2: c_int = 2;
    pub const ARCHIVE_FILTER_XZ: c_int = 6;
    pub const ARCHIVE_FILTER_ZSTD: c_int = 14;

    pub const ARCHIVE_EXTRACT_PERM: c_int = 0x0002;
    pub const ARCHIVE_EXTRACT_TIME: c_int = 0x0004;
    pub const ARCHIVE_EXTRACT_SECURE_SYMLINKS: c_int = 0x0100;
    pub const ARCHIVE_EXTRACT_SECURE_NODOTDOT: c_int = 0x0200;

    pub const AE_IFMT: u32 = 0o170000;
    pub const AE_IFREG: u32 = 0o100000;
    pub const AE_IFLNK: u32 = 0o120000;
    pub const AE_IFDIR: u32 = 0o040000;

    #[repr(C)]
    pub struct archive {
        _unused: [u8; 0],
    }

    #[repr(C)]
    pub struct archive_entry {
        _unused: [u8; 0],
    }

    pub type ArchivePassphraseCallback =
        Option<unsafe extern "C" fn(*mut archive, *mut c_void) -> *const c_char>;

    unsafe extern "C" {
        pub fn archive_read_new() -> *mut archive;
        pub fn archive_read_support_filter_all(a: *mut archive) -> c_int;
        pub fn archive_read_support_format_all(a: *mut archive) -> c_int;
        pub fn archive_read_support_format_raw(a: *mut archive) -> c_int;
        pub fn archive_read_add_passphrase(a: *mut archive, passphrase: *const c_char) -> c_int;
        pub fn archive_read_set_passphrase_callback(
            a: *mut archive,
            client_data: *mut c_void,
            cb: ArchivePassphraseCallback,
        ) -> c_int;
        pub fn archive_read_open_filename(
            a: *mut archive,
            filename: *const c_char,
            block_size: usize,
        ) -> c_int;
        pub fn archive_read_next_header(
            a: *mut archive,
            entry: *mut *mut archive_entry,
        ) -> c_int;
        pub fn archive_read_data_block(
            a: *mut archive,
            buff: *mut *const c_void,
            size: *mut usize,
            offset: *mut i64,
        ) -> c_int;
        pub fn archive_read_data_skip(a: *mut archive) -> c_int;
        pub fn archive_read_close(a: *mut archive) -> c_int;
        pub fn archive_read_free(a: *mut archive) -> c_int;

        pub fn archive_write_disk_new() -> *mut archive;
        pub fn archive_write_disk_set_options(a: *mut archive, flags: c_int) -> c_int;
        pub fn archive_write_disk_set_standard_lookup(a: *mut archive) -> c_int;
        pub fn archive_write_header(a: *mut archive, entry: *mut archive_entry) -> c_int;
        pub fn archive_write_data_block(
            a: *mut archive,
            buffer: *const c_void,
            size: usize,
            offset: i64,
        ) -> isize;
        pub fn archive_write_finish_entry(a: *mut archive) -> c_int;
        pub fn archive_write_close(a: *mut archive) -> c_int;
        pub fn archive_write_free(a: *mut archive) -> c_int;

        pub fn archive_write_new() -> *mut archive;
        pub fn archive_write_set_format_pax_restricted(a: *mut archive) -> c_int;
        pub fn archive_write_set_format_zip(a: *mut archive) -> c_int;
        pub fn archive_write_set_format_7zip(a: *mut archive) -> c_int;
        pub fn archive_write_add_filter(a: *mut archive, filter_code: c_int) -> c_int;
        pub fn archive_write_set_format_option(
            a: *mut archive,
            module: *const c_char,
            option: *const c_char,
            value: *const c_char,
        ) -> c_int;
        pub fn archive_write_set_passphrase(a: *mut archive, passphrase: *const c_char) -> c_int;
        pub fn archive_write_open_filename(a: *mut archive, filename: *const c_char) -> c_int;
        pub fn archive_write_data(a: *mut archive, data: *const c_void, size: usize) -> isize;
        pub fn archive_write_fail(a: *mut archive) -> c_int;

        pub fn archive_format(a: *mut archive) -> c_int;
        pub fn archive_format_name(a: *mut archive) -> *const c_char;
        pub fn archive_filter_code(a: *mut archive, index: c_int) -> c_int;
        pub fn archive_filter_name(a: *mut archive, index: c_int) -> *const c_char;
        pub fn archive_errno(a: *mut archive) -> c_int;
        pub fn archive_error_string(a: *mut archive) -> *const c_char;

        pub fn archive_entry_new() -> *mut archive_entry;
        pub fn archive_entry_free(entry: *mut archive_entry);
        pub fn archive_entry_pathname(entry: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_pathname_utf8(entry: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_set_pathname(entry: *mut archive_entry, name: *const c_char);
        pub fn archive_entry_hardlink(entry: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_hardlink_utf8(entry: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_set_hardlink(entry: *mut archive_entry, link: *const c_char);
        pub fn archive_entry_symlink(entry: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_symlink_utf8(entry: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_set_symlink(entry: *mut archive_entry, link: *const c_char);
        pub fn archive_entry_filetype(entry: *mut archive_entry) -> c_uint;
        pub fn archive_entry_perm(entry: *mut archive_entry) -> c_uint;
        pub fn archive_entry_set_filetype(entry: *mut archive_entry, filetype: c_uint);
        pub fn archive_entry_set_perm(entry: *mut archive_entry, perm: c_uint);
        pub fn archive_entry_size(entry: *mut archive_entry) -> i64;
        pub fn archive_entry_size_is_set(entry: *mut archive_entry) -> c_int;
        pub fn archive_entry_set_size(entry: *mut archive_entry, size: i64);
        pub fn archive_entry_mtime(entry: *mut archive_entry) -> c_long;
        pub fn archive_entry_mtime_is_set(entry: *mut archive_entry) -> c_int;
        pub fn archive_entry_set_mtime(entry: *mut archive_entry, sec: c_long, nsec: c_long);
        pub fn archive_entry_is_encrypted(entry: *mut archive_entry) -> c_int;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "rarc-test-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn zip_slip_sanitization() {
        assert!(matches!(
            sanitize_entry_path("../etc/passwd"),
            Err(ArchiveError::UnsafePath(_))
        ));
        assert!(matches!(
            sanitize_entry_path("a/b/../../../evil"),
            Err(ArchiveError::UnsafePath(_))
        ));
        assert!(matches!(
            sanitize_entry_path("..\\..\\Windows\\System32"),
            Err(ArchiveError::UnsafePath(_))
        ));
        assert!(matches!(
            sanitize_entry_path("bad\0name"),
            Err(ArchiveError::UnsafePath(_))
        ));
        assert_eq!(sanitize_entry_path("./").unwrap(), None);
        assert_eq!(sanitize_entry_path("/").unwrap(), None);
        assert_eq!(
            sanitize_entry_path("/var/tmp/./hello.txt").unwrap(),
            Some(PathBuf::from("var/tmp/hello.txt"))
        );
    }

    #[test]
    fn symlink_escape_rejected() {
        assert!(validate_symlink_target(Path::new("a/b/link"), "../sibling").is_ok());
        assert!(validate_symlink_target(Path::new("a/link"), "../../escape").is_err());
        assert!(validate_symlink_target(Path::new("link"), "/etc/passwd").is_err());
    }

    fn dummy_entry(path: &str, kind: EntryKind) -> ArchiveEntry {
        ArchiveEntry {
            path: path.into(),
            size: 10,
            compressed_size: None,
            kind,
            mtime: None,
            mode: 0o644,
            symlink_target: None,
            is_encrypted: false,
        }
    }

    #[test]
    fn smart_root_folder_detection() {
        let single_dir_entries = vec![
            dummy_entry("proj", EntryKind::Directory),
            dummy_entry("proj/main.rs", EntryKind::File),
        ];
        assert_eq!(
            resolve_extract_here_dir(&single_dir_entries, Path::new("/tmp/proj.tar.zst")),
            PathBuf::from("/tmp")
        );

        let multi_root = vec![
            dummy_entry("a.txt", EntryKind::File),
            dummy_entry("b.txt", EntryKind::File),
        ];
        assert_eq!(
            resolve_extract_here_dir(&multi_root, Path::new("/tmp/bundle.tar.xz")),
            PathBuf::from("/tmp/bundle")
        );
    }

    #[test]
    fn roundtrip_all_formats_and_add() {
        let tmp = TestDir::new("roundtrip");
        let src_dir = tmp.0.join("sample");
        fs::create_dir_all(src_dir.join("sub")).unwrap();
        fs::write(src_dir.join("hello.txt"), b"hello rarc").unwrap();
        fs::write(src_dir.join("sub/data.bin"), b"0123456789").unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        for fmt in ArchiveFormat::ALL {
            let archive_path = tmp.0.join(format!("test{}", fmt.extension()));
            let written = create_archive(
                &archive_path,
                fmt,
                std::slice::from_ref(&src_dir),
                None,
                None,
                &cancel,
            )
            .expect("create_archive");
            assert!(written >= 4);

            let listing = list_archive(&archive_path, None, None, &cancel).unwrap();
            assert!(listing.entries.iter().any(|e| e.path == "sample/hello.txt"));
            assert!(listing.entries.iter().any(|e| e.path == "sample/sub/data.bin"));

            let out_dir = tmp.0.join(format!("out-{}", fmt.extension().trim_start_matches('.')));
            extract_archive(&archive_path, &out_dir, None, None, None, None, &cancel).unwrap();
            assert_eq!(
                fs::read(out_dir.join("sample/hello.txt")).unwrap(),
                b"hello rarc"
            );
            assert_eq!(
                fs::read(out_dir.join("sample/sub/data.bin")).unwrap(),
                b"0123456789"
            );
        }

        // Test adding an extra file into .zip via add_to_archive
        let extra = tmp.0.join("extra.txt");
        fs::write(&extra, b"dropped file").unwrap();
        let zip_path = tmp.0.join("test.zip");
        add_to_archive(&zip_path, &[extra], "sample/sub", None, None, &cancel).unwrap();
        let listing = list_archive(&zip_path, None, None, &cancel).unwrap();
        assert!(listing.entries.iter().any(|e| e.path == "sample/sub/extra.txt"));
    }

    #[test]
    fn encrypted_zip_prompts_and_extracts() {
        let tmp = TestDir::new("enc");
        let secret_file = tmp.0.join("secret.txt");
        fs::write(&secret_file, b"top secret payload").unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let zip_path = tmp.0.join("locked.zip");
        create_archive(
            &zip_path,
            ArchiveFormat::Zip,
            &[secret_file],
            Some("hunter2"),
            None,
            &cancel,
        )
        .unwrap();

        let (tx, rx) = async_channel::unbounded();
        let out_dir = tmp.0.join("out");
        let zip_clone = zip_path.clone();
        let out_clone = out_dir.clone();
        let cancel_clone = cancel.clone();

        let handle = std::thread::spawn(move || {
            let res = extract_archive(
                &zip_clone,
                &out_clone,
                None,
                None,
                None,
                Some(&tx),
                &cancel_clone,
            );
            let _ = tx.send_blocking(WorkerEvent::Finished(
                res.map(|(_, pw)| OperationSummary::Extracted { password: pw }),
            ));
        });

        let mut got_prompt = false;
        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                WorkerEvent::NeedPassword { reply, .. } => {
                    got_prompt = true;
                    let _ = reply.send_blocking(Some("hunter2".to_string()));
                }
                WorkerEvent::Finished(res) => {
                    res.unwrap();
                    break;
                }
                _ => {}
            }
        }
        handle.join().unwrap();
        assert!(got_prompt);
        assert_eq!(fs::read(out_dir.join("secret.txt")).unwrap(), b"top secret payload");
    }

    #[test]
    fn conflict_skip_and_rename() {
        let tmp = TestDir::new("conflict");
        let input = tmp.0.join("doc.txt");
        fs::write(&input, b"new content").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let zip_path = tmp.0.join("doc.zip");
        create_archive(&zip_path, ArchiveFormat::Zip, &[input], None, None, &cancel).unwrap();

        let out_dir = tmp.0.join("dest");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("doc.txt"), b"existing content").unwrap();

        let (tx, rx) = async_channel::unbounded();
        let z = zip_path.clone();
        let o = out_dir.clone();
        let c = cancel.clone();
        let handle = std::thread::spawn(move || {
            let res = extract_archive(&z, &o, None, None, None, Some(&tx), &c);
            let _ = tx.send_blocking(WorkerEvent::Finished(
                res.map(|(_, pw)| OperationSummary::Extracted { password: pw }),
            ));
        });

        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                WorkerEvent::FileConflict { reply, .. } => {
                    let _ = reply.send_blocking(ConflictResolution::Rename("doc_2.txt".into()));
                }
                WorkerEvent::Finished(res) => {
                    res.unwrap();
                    break;
                }
                _ => {}
            }
        }
        handle.join().unwrap();
        assert_eq!(fs::read(out_dir.join("doc.txt")).unwrap(), b"existing content");
        assert_eq!(fs::read(out_dir.join("doc_2.txt")).unwrap(), b"new content");
    }

    #[test]
    fn cancellation_aborts_cleanly() {
        let tmp = TestDir::new("cancel");
        let f = tmp.0.join("file.txt");
        fs::write(&f, b"abc").unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let zip_path = tmp.0.join("cancel.zip");
        let res = create_archive(&zip_path, ArchiveFormat::Zip, &[f], None, None, &cancel);
        assert!(matches!(res, Err(ArchiveError::Cancelled)));
        assert!(!zip_path.exists());
    }

    #[test]
    fn malicious_zip_slip_archive_rejected() {
        let tmp = TestDir::new("zipslip-e2e");
        let evil_zip = tmp.0.join("evil.zip");
        let writer = WriteArchive::new().unwrap();
        WritableSpec::Zip.apply(writer.as_ptr(), None).unwrap();
        let c_zip = path_to_cstring(&evil_zip).unwrap();
        unsafe {
            check_read_write(
                ffi::archive_write_open_filename(writer.as_ptr(), c_zip.as_ptr()),
                writer.as_ptr(),
            )
            .unwrap();
            let entry = OwnedEntry::new().unwrap();
            let c_name = c"../../escaped.txt";
            ffi::archive_entry_set_pathname(entry.as_ptr(), c_name.as_ptr());
            ffi::archive_entry_set_filetype(entry.as_ptr(), ffi::AE_IFREG);
            ffi::archive_entry_set_perm(entry.as_ptr(), 0o644);
            ffi::archive_entry_set_size(entry.as_ptr(), 4);
            check_read_write(
                ffi::archive_write_header(writer.as_ptr(), entry.as_ptr()),
                writer.as_ptr(),
            )
            .unwrap();
            assert_eq!(
                ffi::archive_write_data(writer.as_ptr(), b"evil".as_ptr() as *const c_void, 4),
                4
            );
            check_read_write(ffi::archive_write_finish_entry(writer.as_ptr()), writer.as_ptr())
                .unwrap();
        }
        writer.close().unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let dest = tmp.0.join("nested/out");
        let res = extract_archive(&evil_zip, &dest, None, None, None, None, &cancel);
        assert!(matches!(res, Err(ArchiveError::UnsafePath(_))));
        assert!(!tmp.0.join("escaped.txt").exists());
    }

    #[test]
    fn test_integrity_passes_and_detects_corruption() {
        let tmp = TestDir::new("integrity");
        let input = tmp.0.join("payload.txt");
        let content: Vec<u8> = (0..4096).map(|i| ((i * 73 + 19) % 251) as u8).collect();
        fs::write(&input, &content).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let zip_path = tmp.0.join("valid.zip");
        create_archive(&zip_path, ArchiveFormat::Zip, &[input], None, None, &cancel).unwrap();

        let (entries, bytes, _) = test_archive(&zip_path, None, None, None, &cancel).unwrap();
        assert_eq!(entries, 1);
        assert_eq!(bytes, 4096);

        // Flip bytes inside the compressed stream and verify test_archive detects CRC/stream error.
        let mut raw = fs::read(&zip_path).unwrap();
        let mid = raw.len() / 2;
        raw[mid] ^= 0xff;
        raw[mid + 1] ^= 0xff;
        let corrupt_zip = tmp.0.join("corrupt.zip");
        fs::write(&corrupt_zip, raw).unwrap();
        assert!(test_archive(&corrupt_zip, None, None, None, &cancel).is_err());
    }

    #[test]
    fn encrypted_zip_add_preserves_encryption() {
        let tmp = TestDir::new("enc-add");
        let secret = tmp.0.join("secret.txt");
        let extra = tmp.0.join("extra.txt");
        fs::write(&secret, b"confidential").unwrap();
        fs::write(&extra, b"added later").unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let zip_path = tmp.0.join("locked.zip");
        create_archive(
            &zip_path,
            ArchiveFormat::Zip,
            &[secret],
            Some("hunter2"),
            None,
            &cancel,
        )
        .unwrap();

        // Without passphrase or prompt channel, add_to_archive must fail rather than downgrade.
        let err = add_to_archive(
            &zip_path,
            std::slice::from_ref(&extra),
            "",
            None,
            None,
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, ArchiveError::PasswordRequired));

        // With interactive prompt (simulating Drag-and-Drop before password is cached),
        // add_to_archive re-encrypts both existing and newly added entries.
        let (tx, rx) = async_channel::unbounded();
        let z = zip_path.clone();
        let e = extra.clone();
        let c = cancel.clone();
        let handle = std::thread::spawn(move || {
            let res = add_to_archive(&z, &[e], "", None, Some(&tx), &c);
            let _ = tx.send_blocking(WorkerEvent::Finished(
                res.map(|(_, pw)| OperationSummary::Added {
                    path: z,
                    password: pw,
                }),
            ));
        });
        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                WorkerEvent::NeedPassword { reply, .. } => {
                    let _ = reply.send_blocking(Some("hunter2".to_string()));
                }
                WorkerEvent::Finished(Ok(OperationSummary::Added { password, .. })) => {
                    assert_eq!(password.as_deref(), Some("hunter2"));
                    break;
                }
                WorkerEvent::Finished(res) => panic!("unexpected result: {res:?}"),
                _ => {}
            }
        }
        handle.join().unwrap();

        let listing = list_archive(&zip_path, None, None, &cancel).unwrap();
        assert!(listing.entries.iter().all(|ent| ent.is_encrypted));

        let out_dir = tmp.0.join("out");
        assert!(extract_archive(&zip_path, &out_dir, None, None, None, None, &cancel).is_err());
        extract_archive(
            &zip_path,
            &out_dir,
            None,
            None,
            Some("hunter2"),
            None,
            &cancel,
        )
        .unwrap();
        assert_eq!(fs::read(out_dir.join("secret.txt")).unwrap(), b"confidential");
        assert_eq!(fs::read(out_dir.join("extra.txt")).unwrap(), b"added later");
    }

    #[test]
    fn add_to_archive_ignores_archive_itself() {
        let tmp = TestDir::new("self-add");
        let work = tmp.0.join("work");
        fs::create_dir_all(&work).unwrap();
        let note = work.join("note.txt");
        fs::write(&note, b"hello").unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let zip_path = work.join("bundle.zip");
        create_archive(&zip_path, ArchiveFormat::Zip, &[note], None, None, &cancel).unwrap();

        let (count, _) = add_to_archive(
            &zip_path,
            std::slice::from_ref(&zip_path),
            "",
            None,
            None,
            &cancel,
        )
        .unwrap();
        assert_eq!(count, 0);

        add_to_archive(&zip_path, &[work], "", None, None, &cancel).unwrap();
        let listing = list_archive(&zip_path, None, None, &cancel).unwrap();
        assert!(listing.entries.iter().any(|e| e.path == "work/note.txt"));
        assert!(!listing.entries.iter().any(|e| e.path.ends_with("bundle.zip")));
    }

    #[test]
    fn stripped_hardlink_extraction_succeeds() {
        let tmp = TestDir::new("hardlink-strip");
        let tar_path = tmp.0.join("links.tar");
        let writer = WriteArchive::new().unwrap();
        WritableSpec::Tar { filter: None }
            .apply(writer.as_ptr(), None)
            .unwrap();
        let c_tar = path_to_cstring(&tar_path).unwrap();
        unsafe {
            check_read_write(
                ffi::archive_write_open_filename(writer.as_ptr(), c_tar.as_ptr()),
                writer.as_ptr(),
            )
            .unwrap();

            let e1 = OwnedEntry::new().unwrap();
            ffi::archive_entry_set_pathname(e1.as_ptr(), c"sub/orig.txt".as_ptr());
            ffi::archive_entry_set_filetype(e1.as_ptr(), ffi::AE_IFREG);
            ffi::archive_entry_set_perm(e1.as_ptr(), 0o644);
            ffi::archive_entry_set_size(e1.as_ptr(), 5);
            check_read_write(
                ffi::archive_write_header(writer.as_ptr(), e1.as_ptr()),
                writer.as_ptr(),
            )
            .unwrap();
            assert_eq!(
                ffi::archive_write_data(writer.as_ptr(), b"links".as_ptr() as *const c_void, 5),
                5
            );
            check_read_write(ffi::archive_write_finish_entry(writer.as_ptr()), writer.as_ptr())
                .unwrap();

            let e2 = OwnedEntry::new().unwrap();
            ffi::archive_entry_set_pathname(e2.as_ptr(), c"sub/alias.txt".as_ptr());
            ffi::archive_entry_set_hardlink(e2.as_ptr(), c"sub/orig.txt".as_ptr());
            ffi::archive_entry_set_filetype(e2.as_ptr(), ffi::AE_IFREG);
            ffi::archive_entry_set_perm(e2.as_ptr(), 0o644);
            check_read_write(
                ffi::archive_write_header(writer.as_ptr(), e2.as_ptr()),
                writer.as_ptr(),
            )
            .unwrap();
            check_read_write(ffi::archive_write_finish_entry(writer.as_ptr()), writer.as_ptr())
                .unwrap();
        }
        writer.close().unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let dest = tmp.0.join("out");
        let sel = vec!["sub/orig.txt".to_string(), "sub/alias.txt".to_string()];
        extract_archive(
            &tar_path,
            &dest,
            Some(&sel),
            Some("sub"),
            None,
            None,
            &cancel,
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("orig.txt")).unwrap(), b"links");
        assert_eq!(fs::read(dest.join("alias.txt")).unwrap(), b"links");
    }

    #[test]
    fn extract_session_reuses_password_and_conflict_policy() {
        let tmp = TestDir::new("batch-session");
        let f1 = tmp.0.join("shared.txt");
        let f2 = tmp.0.join("other.txt");
        fs::write(&f1, b"first").unwrap();
        let z1 = tmp.0.join("one.zip");
        let cancel = Arc::new(AtomicBool::new(false));
        create_archive(&z1, ArchiveFormat::Zip, &[f1], Some("pw"), None, &cancel).unwrap();

        fs::write(tmp.0.join("shared.txt"), b"second").unwrap();
        let f1_again = tmp.0.join("shared.txt");
        fs::write(&f2, b"tail").unwrap();
        let z2 = tmp.0.join("two.zip");
        create_archive(
            &z2,
            ArchiveFormat::Zip,
            &[f1_again, f2],
            Some("pw"),
            None,
            &cancel,
        )
        .unwrap();

        let out_dir = tmp.0.join("dest");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("shared.txt"), b"existing").unwrap();

        let (tx, rx) = async_channel::unbounded();
        let c = cancel.clone();
        let o = out_dir.clone();
        let handle = std::thread::spawn(move || {
            let mut session = ExtractSession::new(None, 0);
            extract_archive_with_session(&z1, &o, None, None, None, &mut session, Some(&tx), &c)
                .unwrap();
            extract_archive_with_session(&z2, &o, None, None, None, &mut session, Some(&tx), &c)
                .unwrap();
            let _ = tx.send_blocking(WorkerEvent::Finished(Ok(OperationSummary::Extracted {
                password: session.password,
            })));
            session.bytes_done
        });

        let mut pw_prompts = 0usize;
        let mut conflict_prompts = 0usize;
        while let Ok(ev) = rx.recv_blocking() {
            match ev {
                WorkerEvent::NeedPassword { reply, .. } => {
                    pw_prompts += 1;
                    let _ = reply.send_blocking(Some("pw".to_string()));
                }
                WorkerEvent::FileConflict { reply, .. } => {
                    conflict_prompts += 1;
                    let _ = reply.send_blocking(ConflictResolution::OverwriteAll);
                }
                WorkerEvent::Finished(res) => {
                    res.unwrap();
                    break;
                }
                _ => {}
            }
        }
        let total_bytes = handle.join().unwrap();
        assert_eq!(pw_prompts, 1);
        assert_eq!(conflict_prompts, 1);
        assert_eq!(total_bytes, 15);
        assert_eq!(fs::read(out_dir.join("shared.txt")).unwrap(), b"second");
        assert_eq!(fs::read(out_dir.join("other.txt")).unwrap(), b"tail");
    }

    #[test]
    fn extension_recognition_and_stem_consistent() {
        assert!(is_supported_archive_path(Path::new("comic.cbz")));
        assert!(is_supported_archive_path(Path::new("book.EPUB")));
        assert!(is_supported_archive_path(Path::new("old.tar.lzma")));
        assert!(!is_supported_archive_path(Path::new("notes.txt")));

        assert_eq!(archive_stem(Path::new("/tmp/proj.tar.zst")), "proj");
        assert_eq!(archive_stem(Path::new("/tmp/archive.tgz")), "archive");
        assert!(matches!(
            infer_writable_from_extension(Path::new("out.txz")),
            Ok(WritableSpec::Tar { filter: Some(_) })
        ));
        assert!(matches!(
            infer_writable_from_extension(Path::new("readonly.rar")),
            Err(ArchiveError::UnsupportedFormat(_))
        ));
    }
}
