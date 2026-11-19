use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_channel::Sender;

use super::{ArchiveError, EntryKind, WorkerEvent};

pub(super) struct PassphraseCtx {
    pub(super) archive_name: String,
    pub(super) initial: Option<CString>,
    pub(super) stored: Vec<CString>,
    pub(super) event_tx: Option<Sender<WorkerEvent>>,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) user_declined: bool,
}

impl PassphraseCtx {
    pub(super) fn for_path(
        archive_path: &Path,
        initial_password: Option<&str>,
        event_tx: Option<&Sender<WorkerEvent>>,
        cancelled: &Arc<AtomicBool>,
    ) -> Self {
        let archive_name = archive_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive")
            .to_string();
        let initial = initial_password
            .filter(|p| !p.is_empty())
            .and_then(|p| CString::new(p).ok());
        Self {
            archive_name,
            initial,
            stored: Vec::new(),
            event_tx: event_tx.cloned(),
            cancelled: cancelled.clone(),
            user_declined: false,
        }
    }

    pub(super) fn last_password(&self) -> Option<String> {
        self.stored
            .last()
            .or(self.initial.as_ref())
            .and_then(|c| c.to_str().ok())
            .map(ToOwned::to_owned)
    }
}

unsafe extern "C" fn passphrase_callback(
    _archive: *mut raw::archive,
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

pub(super) enum HeaderStep {
    Eof,
    Entry(*mut raw::archive_entry),
    RetryRaw,
}

pub(super) unsafe fn read_next_header_step(
    reader: *mut raw::archive,
    archive_path: &Path,
    pass_ctx: &PassphraseCtx,
    first: &mut bool,
    allow_raw_retry: bool,
) -> Result<HeaderStep, ArchiveError> {
    let mut entry: *mut raw::archive_entry = ptr::null_mut();
    let rc = unsafe { raw::archive_read_next_header(reader, &mut entry) };
    if rc == raw::ARCHIVE_EOF {
        return Ok(HeaderStep::Eof);
    }
    if rc != raw::ARCHIVE_OK && rc != raw::ARCHIVE_WARN {
        if *first
            && allow_raw_retry
            && unsafe { raw::archive_filter_code(reader, 0) } != raw::ARCHIVE_FILTER_NONE
        {
            return Ok(HeaderStep::RetryRaw);
        }
        return Err(reader_error(reader, pass_ctx));
    }
    if *first && allow_raw_retry {
        let fmt = unsafe { raw::archive_format(reader) } & raw::ARCHIVE_FORMAT_BASE_MASK;
        let is_mtree_ext = archive_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mtree"));
        if fmt == raw::ARCHIVE_FORMAT_MTREE && !is_mtree_ext {
            return Ok(HeaderStep::RetryRaw);
        }
    }
    Ok(HeaderStep::Entry(entry))
}

pub(super) enum ReaderOpen {
    Ready(ReadArchive),
    RetryRaw,
}

pub(super) fn open_reader_step(
    archive_path: &Path,
    raw_mode: bool,
    allow_raw_retry: bool,
    pass_ctx: &mut PassphraseCtx,
) -> Result<ReaderOpen, ArchiveError> {
    let reader = ReadArchive::new()?;
    let c_path = path_to_cstring(archive_path)?;
    unsafe {
        check_read_write(
            raw::archive_read_support_filter_all(reader.as_ptr()),
            reader.as_ptr(),
        )?;
        if raw_mode {
            check_read_write(
                raw::archive_read_support_format_raw(reader.as_ptr()),
                reader.as_ptr(),
            )?;
        } else {
            check_read_write(
                raw::archive_read_support_format_all(reader.as_ptr()),
                reader.as_ptr(),
            )?;
        }
        if let Some(ref initial) = pass_ctx.initial {
            check_read_write(
                raw::archive_read_add_passphrase(reader.as_ptr(), initial.as_ptr()),
                reader.as_ptr(),
            )?;
        }
        check_read_write(
            raw::archive_read_set_passphrase_callback(
                reader.as_ptr(),
                (pass_ctx as *mut PassphraseCtx) as *mut c_void,
                Some(passphrase_callback),
            ),
            reader.as_ptr(),
        )?;
        let open_rc = raw::archive_read_open_filename(reader.as_ptr(), c_path.as_ptr(), 65536);
        if open_rc != raw::ARCHIVE_OK && open_rc != raw::ARCHIVE_WARN {
            let err = reader_error(reader.as_ptr(), pass_ctx);
            if allow_raw_retry
                && !raw_mode
                && !matches!(err, ArchiveError::Cancelled | ArchiveError::PasswordRequired)
                && raw::archive_filter_code(reader.as_ptr(), 0) != raw::ARCHIVE_FILTER_NONE
            {
                return Ok(ReaderOpen::RetryRaw);
            }
            return Err(err);
        }
    }
    Ok(ReaderOpen::Ready(reader))
}

pub(super) fn open_reader(
    archive_path: &Path,
    raw_mode: bool,
    pass_ctx: &mut PassphraseCtx,
) -> Result<ReadArchive, ArchiveError> {
    match open_reader_step(archive_path, raw_mode, false, pass_ctx)? {
        ReaderOpen::Ready(r) => Ok(r),
        ReaderOpen::RetryRaw => unreachable!(),
    }
}

pub(super) fn reader_error(reader: *mut raw::archive, pass_ctx: &PassphraseCtx) -> ArchiveError {
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

pub(super) unsafe fn describe_reader_format(reader: *mut raw::archive) -> String {
    let fmt_ptr = unsafe { raw::archive_format_name(reader) };
    let fmt = if fmt_ptr.is_null() {
        "Unknown".to_string()
    } else {
        unsafe { CStr::from_ptr(fmt_ptr) }
            .to_string_lossy()
            .into_owned()
    };
    let flt_code = unsafe { raw::archive_filter_code(reader, 0) };
    let flt_ptr = unsafe { raw::archive_filter_name(reader, 0) };
    if flt_code != raw::ARCHIVE_FILTER_NONE && !flt_ptr.is_null() {
        let flt = unsafe { CStr::from_ptr(flt_ptr) }.to_string_lossy();
        format!("{fmt} ({flt})")
    } else {
        fmt
    }
}

unsafe fn utf8_or_local_cstr(utf8: *const c_char, fallback: *const c_char) -> Option<String> {
    let ptr = if !utf8.is_null() { utf8 } else { fallback };
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
    }
}

pub(super) unsafe fn entry_pathname_string(
    entry: *mut raw::archive_entry,
) -> Result<String, ArchiveError> {
    unsafe {
        utf8_or_local_cstr(
            raw::archive_entry_pathname_utf8(entry),
            raw::archive_entry_pathname(entry),
        )
    }
    .ok_or_else(|| ArchiveError::LibArchive("Archive entry has unreadable filename".to_string()))
}

pub(super) unsafe fn entry_symlink_string(entry: *mut raw::archive_entry) -> Option<String> {
    unsafe {
        utf8_or_local_cstr(
            raw::archive_entry_symlink_utf8(entry),
            raw::archive_entry_symlink(entry),
        )
    }
}

pub(super) unsafe fn entry_hardlink_string(entry: *mut raw::archive_entry) -> Option<String> {
    unsafe {
        utf8_or_local_cstr(
            raw::archive_entry_hardlink_utf8(entry),
            raw::archive_entry_hardlink(entry),
        )
    }
}

pub(super) fn filetype_to_kind(filetype: u32) -> EntryKind {
    match filetype & raw::AE_IFMT {
        raw::AE_IFDIR => EntryKind::Directory,
        raw::AE_IFREG => EntryKind::File,
        raw::AE_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

pub(super) fn path_to_cstring(path: &Path) -> Result<CString, ArchiveError> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ArchiveError::UnsafePath(path.display().to_string()))
}

pub(super) fn to_cstring(s: &str) -> Result<CString, ArchiveError> {
    CString::new(s).map_err(|_| ArchiveError::UnsafePath(s.to_string()))
}

pub(super) unsafe fn check_read_write(
    rc: c_int,
    archive: *mut raw::archive,
) -> Result<(), ArchiveError> {
    match rc {
        raw::ARCHIVE_OK | raw::ARCHIVE_WARN => Ok(()),
        _ => Err(ArchiveError::LibArchive(unsafe { archive_err_string(archive) })),
    }
}

pub(super) unsafe fn archive_err_string(archive: *mut raw::archive) -> String {
    let ptr = unsafe { raw::archive_error_string(archive) };
    if ptr.is_null() {
        let code = unsafe { raw::archive_errno(archive) };
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

pub(super) struct ReadArchive(*mut raw::archive);

impl ReadArchive {
    pub(super) fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { raw::archive_read_new() };
        if ptr.is_null() {
            Err(ArchiveError::LibArchive(
                "Failed to allocate archive reader".to_string(),
            ))
        } else {
            Ok(Self(ptr))
        }
    }

    pub(super) fn as_ptr(&self) -> *mut raw::archive {
        self.0
    }
}

impl Drop for ReadArchive {
    fn drop(&mut self) {
        unsafe {
            raw::archive_read_close(self.0);
            raw::archive_read_free(self.0);
        }
    }
}

pub(super) struct WriteDiskArchive(*mut raw::archive);

impl WriteDiskArchive {
    pub(super) fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { raw::archive_write_disk_new() };
        if ptr.is_null() {
            return Err(ArchiveError::LibArchive(
                "Failed to allocate archive disk writer".to_string(),
            ));
        }
        let writer = Self(ptr);
        let flags = raw::ARCHIVE_EXTRACT_TIME
            | raw::ARCHIVE_EXTRACT_PERM
            | raw::ARCHIVE_EXTRACT_SECURE_SYMLINKS
            | raw::ARCHIVE_EXTRACT_SECURE_NODOTDOT;
        unsafe {
            check_read_write(raw::archive_write_disk_set_options(ptr, flags), ptr)?;
            check_read_write(raw::archive_write_disk_set_standard_lookup(ptr), ptr)?;
        }
        Ok(writer)
    }

    pub(super) fn as_ptr(&self) -> *mut raw::archive {
        self.0
    }
}

impl Drop for WriteDiskArchive {
    fn drop(&mut self) {
        unsafe {
            raw::archive_write_close(self.0);
            raw::archive_write_free(self.0);
        }
    }
}

pub(super) struct WriteArchive {
    ptr: *mut raw::archive,
    closed: bool,
}

impl WriteArchive {
    pub(super) fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { raw::archive_write_new() };
        if ptr.is_null() {
            Err(ArchiveError::LibArchive(
                "Failed to allocate archive writer".to_string(),
            ))
        } else {
            Ok(Self { ptr, closed: false })
        }
    }

    pub(super) fn as_ptr(&self) -> *mut raw::archive {
        self.ptr
    }

    pub(super) fn fail(&self) {
        unsafe {
            raw::archive_write_fail(self.ptr);
        }
    }

    pub(super) fn close(mut self) -> Result<(), ArchiveError> {
        self.closed = true;
        unsafe {
            let rc = raw::archive_write_close(self.ptr);
            let res = check_read_write(rc, self.ptr);
            raw::archive_write_free(self.ptr);
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
                    raw::archive_write_close(self.ptr);
                }
                raw::archive_write_free(self.ptr);
            }
        }
    }
}

pub(super) struct OwnedEntry(*mut raw::archive_entry);

impl OwnedEntry {
    pub(super) fn new() -> Result<Self, ArchiveError> {
        let ptr = unsafe { raw::archive_entry_new() };
        if ptr.is_null() {
            Err(ArchiveError::LibArchive(
                "Failed to allocate archive entry".to_string(),
            ))
        } else {
            Ok(Self(ptr))
        }
    }

    pub(super) fn as_ptr(&self) -> *mut raw::archive_entry {
        self.0
    }
}

impl Drop for OwnedEntry {
    fn drop(&mut self) {
        unsafe {
            raw::archive_entry_free(self.0);
        }
    }
}

pub(super) mod raw {
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
        #[cfg(test)]
        pub fn archive_write_set_format_raw(a: *mut archive) -> c_int;
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
