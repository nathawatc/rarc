# rarc

Lightweight, modern GTK4 archive manager, powered by `libarchive` for Thunar integration.

## Features

- **Wide read support**: Inspects and extracts `.tar` (`.tar.gz`, `.tar.xz`,
  `.tar.zst`, `.tar.bz2`, `.tar.lz4`, `.tar.lzma`, etc.), `.zip`, `.7z`, `.rar`,
  `.iso`, `.cab`, `.cpio`, `.ar`, `.deb`, `.rpm`, `.xar`,
  `.cbz`/`.cbr`/`.cb7`/`.cbt`, `.epub`, `.jar`/`.apk`, and single-file
  compressed streams (`.gz`, `.xz`, `.zst`, `.bz2`, `.lz4`).
- **Archive creation & repacking**: Creates `.tar.zst`, `.zip` (with optional
  AES-256 password encryption), `.7z`, `.tar.xz`, and `.tar.gz`, and appends
  dropped files into existing `.zip`, `.7z`, and `.tar*` archives atomically.
- **Desktop workflow**:
  - Left folder tree sidebar with Left/Right arrow expansion/collapse and
    focus tracking
  - Clickable breadcrumb path bar and mouse thumb buttons (`Back` / `Forward`)
  - Live entry search filter and sortable columns (`Name`, `Uncompressed Size`,
    `Type`, `Modified Date`)
  - Context menu (`Open`, `Open With…`, `Extract Selected…`, `Copy` to
    clipboard, `Properties`)
  - HeaderBar integrity check (`Test`) and archive metadata (`Properties`)
  - Drag-and-drop in (add to open archive, inspect dropped archive, or create
    a new archive) and drag-and-drop out
- **Safety & prompts**: Zip-Slip and symlink-escape path validation,
  interactive conflict resolution (`Overwrite`, `Overwrite All`, `Skip`,
  `Skip All`, `Rename`), and modal passphrase prompts with retry and session
  caching.
- **Thunar integration**: Supports `--tap extract-here` (extracts directly for
  a single root folder/file, otherwise into `<archive_stem>/`),
  `--tap extract-to`, and `--tap create`.

## Requirements

- Rust toolchain (`edition = "2024"`)
- `pkg-config`
- `libarchive >= 3.2.0` development headers
- `gtk4 >= 4.12` development headers

On Arch Linux:

```bash
sudo pacman -S gtk4 libarchive pkgconf rust
```

On Debian / Ubuntu:

```bash
sudo apt install libgtk-4-dev libarchive-dev pkg-config cargo rustc
```

## Build & Install

```bash
make build      # cargo build --release
make test       # cargo test
make check      # cargo clippy --all-targets
sudo make install
```

`Makefile` installs:

- `target/release/rarc` -> `$(PREFIX)/bin/rarc`
- `data/rarc.desktop` -> `$(PREFIX)/share/applications/rarc.desktop`
- `data/rarc.tap` -> `$(PLUGINDIR)/rarc.tap` (auto-detects
  `lib/xfce4/thunar-archive-plugin` vs `libexec/thunar-archive-plugin`)

To remove installed files:

```bash
sudo make uninstall
```

## CLI Usage

```text
rarc [ARCHIVE]
rarc --tap <extract-here|extract-to|create> <PATHS...>
rarc --help | -h
rarc --version | -V
```

## License

Licensed under the GNU General Public License v3.0 — see [LICENSE](LICENSE).
