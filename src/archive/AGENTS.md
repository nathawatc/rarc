# Archive Subsystem Guide (`src/archive/`)

Back to root guide: [../../AGENTS.md](../../AGENTS.md)

This module wraps C `libarchive` (`>= 3.2.0`) directly and has **zero GTK/UI
dependencies**. Communication with the UI runs over
`async_channel::Sender<WorkerEvent>` on a background thread (`spawn_worker`).

## Module Layout

| File | Responsibility |
|------|-----------------|
| [mod.rs](mod.rs) | Public domain types (`ArchiveEntry`, `ArchiveListing`, `ArchiveError`, `ExtractSession`, `WorkerEvent`), path sanitization, `resolve_extract_here_dir`, `spawn_worker`. |
| [ffi.rs](ffi.rs) | Raw C declarations (`mod raw`), RAII handle wrappers (`ReadArchive`, `WriteDiskArchive`, `WriteArchive`, `OwnedEntry`), `PassphraseCtx`, header-step and raw-open helpers. |
| [formats.rs](formats.rs) | Single source of truth for `COMPOUND_TAR_EXTS`, `WRITABLE_EXT_SPECS`, `READONLY_ARCHIVE_EXTS`, `WritableSpec`, `archive_stem`, `is_supported_archive_path`. |
| [read.rs](read.rs) | `list_archive`, `test_archive`, `extract_archive_with_session`, shared `pump_archive_data` (`DataSink::{Discard, Disk, Stream}`), `resolve_raw_entry_path`, `resolve_conflict`. |
| [write.rs](write.rs) | `create_archive`, `add_to_archive`, `probe_encrypted_password`, disk tree collection (`collect_disk_items`), `write_disk_item`, atomic temp sibling helpers. |
| [tests.rs](tests.rs) | Unit & end-to-end `libarchive` tests (`TestDir` temp RAII guard, Zip-Slip, symlink escapes, hardlinks, raw `.gz`, encrypted add, batch sessions). |

## FFI & Memory Safety Rules

1. **RAII owns every pointer**: Never call `archive_read_free` /
   `archive_write_free` / `archive_entry_free` manually outside `ffi.rs` `Drop`
   impls. Note `WriteArchive::close(self)` sets `closed = true` and nulls `ptr`
   so `Drop` will not double-close; call `writer.fail()` before `drop(writer)`
   on aborted writes so truncated output is discarded cleanly.
2. **`PassphraseCtx` pointer stability**: `open_reader_step` registers
   `(pass_ctx as *mut PassphraseCtx) as *mut c_void` with
   `archive_read_set_passphrase_callback`.
   - `pass_ctx` must remain pinned on the caller's stack frame until the
     `ReadArchive` is dropped.
   - `PassphraseCtx.stored` is a `Vec<CString>`: pushing new entries may
     reallocate the `Vec` backing array, but each `CString`'s heap byte buffer
     remains at a stable address for `libarchive` until `PassphraseCtx` drops.
   - `passphrase_callback` is wrapped in `std::panic::catch_unwind` and checks
     `cancelled` / `user_declined` before blocking on `NeedPassword`.
3. **UTF-8 C string reads**: Use `entry_pathname_string`, `entry_symlink_string`,
   and `entry_hardlink_string` (which prefer `*_utf8` and fall back to local C
   strings via `utf8_or_local_cstr`). Check return codes via `check_read_write`
   (`ARCHIVE_OK | ARCHIVE_WARN`) and map read errors through `reader_error` so
   user cancellations always return `ArchiveError::Cancelled`.

## Raw Stream Fallback (`raw_mode`)

Single-file compressed streams (e.g. `.gz`, `.xz`, `.zst` without a `.tar`
container) must work without double-reading archives:

- `open_reader_step` handles the case where `archive_read_open_filename` itself
  detects a compression filter (`archive_filter_code(reader, 0) !=
  ARCHIVE_FILTER_NONE`) and fails container format bidding before any header is
  read.
- `read_next_header_step` handles the case where plain-text compressed payloads
  falsely bid as `ARCHIVE_FORMAT_MTREE` (when extension is not `.mtree`) or fail
  on header #1 with a filter active.
- `resolve_raw_entry_path` maps libarchive's synthetic `"data"` pathname to
  `archive_stem(archive_path)` uniformly across listing, testing, and
  extraction.

## Path Security & Hardlinks

- Every entry path passes through `sanitize_entry_path` (strips `.` and
  leading/repeated `/` or `\`, rejects NUL bytes and any `..` segment).
- Symlinks pass `validate_symlink_target`, ensuring relative depth never drops
  below extraction root `dest_dir`, reinforced by
  `ARCHIVE_EXTRACT_SECURE_SYMLINKS | ARCHIVE_EXTRACT_SECURE_NODOTDOT` on
  `WriteDiskArchive`.
- Hardlink targets (`entry_hardlink_string`) run through `sanitize_entry_path`
  **and** `apply_strip_prefix` consistently with `entry`'s own pathname before
  `archive_entry_set_hardlink`.

## Atomic Writes & Encryption Preservation

- `create_archive` and `add_to_archive` write to
  `.{filename}.rarc-tmp-{pid}-{i}` (`temp_sibling_path`) and atomically
  `fs::rename` over the target only after `writer.close()?` succeeds.
- `collect_disk_items(..., exclude_path)` excludes canonicalized `output_path` /
  `archive_path` so dropping an archive or its parent directory into itself
  never embeds the archive recursively.
- **Encrypted ZIP repack guard**: ZIP file headers are unencrypted, so
  `list_archive` succeeds with `password: None` even when entries have
  `is_encrypted == true`. `add_to_archive` calls `probe_encrypted_password` up
  front when `has_encrypted && effective_pw.is_none()` to validate/collect the
  passphrase (and reject encrypted non-ZIP repacks) before configuring
  `WriteArchive`, preventing plaintext downgrade.

## Batch Session (`ExtractSession`)

Multi-archive operations (`--tap extract-here` / `--tap extract-to`) reuse a
single `ExtractSession` across archives so entered passwords,
`OverwriteAll`/`SkipAll` choices, and cumulative `bytes_done / bytes_total`
persist across files.
