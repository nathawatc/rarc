# Repository Guide (`rarc`)

`rarc` is a pure-GTK4 (`gtk4 = "0.11.5"`, feature `v4_12`) Linux archive manager
with direct `libarchive` C FFI bindings via `pkg-config` ([build.rs](build.rs)).
There is no `libadwaita` and no high-level Rust `libarchive` wrapper crate
(`compress-tools` was intentionally excluded).

## Commands

Run before committing any change:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Notes:

- Keep zero compiler and Clippy warnings (`--all-targets -- -D warnings`).
- Non-GTK unit tests run fast and headless (`cargo test`). Live GTK window tests
  require a graphical session (`WAYLAND_DISPLAY` or `DISPLAY`).

## Module Map & Dependency Graph

Dependencies flow strictly downward with no cycles:

```text
src/main.rs          CLI parsing (lexopt), GTK startup, --tap orchestration
  ├── src/ui/        GTK4 dialogs, viewer, CSS, staging (depends on archive)
  └── src/archive/   libarchive FFI, path checks, read/write (zero UI deps)
```

- [src/main.rs](src/main.rs): Parses `[ARCHIVE]` or
  `--tap <extract-here|extract-to|create> <PATHS...>` via `parse_args_from`.
  Launches `gio::ApplicationFlags::NON_UNIQUE` so Thunar actions never collide
  with a running viewer instance, and calls `ui::cleanup_session_temp_dir()`
  when `app.run_with_args` exits.
- [src/archive/](src/archive/mod.rs): Pure Rust domain types, security checks,
  and `libarchive` FFI operations. Deep guide:
  [src/archive/AGENTS.md](src/archive/AGENTS.md).
- [src/ui/](src/ui/mod.rs): GTK4 presentation, modal worker prompts, and
  split-pane viewer. Deep guide: [src/ui/AGENTS.md](src/ui/AGENTS.md).
- [data/](data/rarc.desktop): Desktop entry (`rarc.desktop`) and Thunar plugin
  wrapper (`rarc.tap`).
- [resources/style.css](resources/style.css): Application CSS loaded at
  activation via `include_str!`. (Note: `@accent_bg_color` / `@theme_bg_color` /
  `@borders` are valid GTK4 CSS syntax even if external generic CSS linters warn
  on them.)

## Cross-Cutting Invariants

1. **Single source of truth for entry data**: `archive::ArchiveEntry` is carried
   directly inside `ArchiveRowData` and passed straight to
   `dialogs::show_properties_dialog`. Do not recreate parallel property/row
   entry DTOs.
2. **Single source of truth for archive formats**: `src/archive/formats.rs` owns
   `ArchiveFormat`, `WRITABLE_EXT_SPECS`, `COMPOUND_TAR_EXTS`,
   `READONLY_ARCHIVE_EXTS`, `archive_stem`, and `is_supported_archive_path`.
   Keep `data/rarc.desktop` MIME/extension coverage aligned with it.
3. **Single-pass streaming**: Do not add `list_archive` pre-scans inside
   `test_archive` or `extract_archive_with_session`; both support single-pass
   raw-stream fallback and receive precomputed `(bytes_total, is_raw)` hints
   from callers that already listed the archive.
4. **Encryption & `libarchive` limits**:
   - Write encryption is supported only for `.zip` (AES-256). UI password input
     is disabled for all other formats, and `add_to_archive` must never strip
     encryption from existing entries.
   - `libarchive` does not expose per-entry compressed sizes for container
     formats like `.zip`/`.7z`; keep `ArchiveEntry::compressed_size` as `None`
     (rendered as `—` in Properties) rather than implementing custom
     central-directory parsers.
5. **Cleanup & module discipline**: Keep modules split by responsibility
   (`archive::{ffi,formats,read,write,tests}`, `ui::dialogs::{create,properties}`,
   `ui::viewer::{models,columns,tree,actions}`). Prefer deleting or consolidating
   duplicated helpers over adding new abstraction layers.
