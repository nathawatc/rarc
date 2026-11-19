use std::os::raw::c_int;
use std::path::Path;

use super::ffi::{self, check_read_write, to_cstring};
use super::{ArchiveError, ArchiveFormat};

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
        .ok_or_else(|| {
            ArchiveError::UnsupportedFormat(format!("cannot add files to {}", path.display()))
        })
}
