mod ffi;
mod ops;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use async_channel::Sender;

pub use ops::{
    add_to_archive, archive_stem, create_archive, extract_archive_with_session,
    is_supported_archive_path, list_archive, test_archive,
};

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

fn apply_strip_prefix(rel_path: &str, strip_prefix: Option<&str>) -> Option<String> {
    if let Some(prefix) = strip_prefix
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| !p.is_empty())
    {
        let prefix_slash = format!("{prefix}/");
        if let Some(stripped) = rel_path.strip_prefix(&prefix_slash) {
            return Some(stripped.to_string());
        } else if rel_path == prefix {
            return None;
        }
    }
    Some(rel_path.to_string())
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
