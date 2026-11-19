# UI Subsystem Guide (`src/ui/`)

Back to root guide: [../../AGENTS.md](../../AGENTS.md)

Pure `gtk4` (`v4_12`) desktop UI built without `libadwaita`. All long-running
archive work runs off the main thread via `archive::spawn_worker` and feeds UI
updates over `async_channel`.

## Module Layout

| Path | Responsibility |
|------|----------------|
| [mod.rs](mod.rs) | CSS loader (`resources/style.css`), process-scoped `session_temp_dir` / `cleanup_session_temp_dir`, `format_size`, `format_mtime`, `show_error_dialog`, `show_info_dialog`, shared `select_folder_dialog`. |
| [dialogs/mod.rs](dialogs/mod.rs) | `run_worker_with_progress` window + shared `run_modal_prompt` channel harness for `prompt_password_async` and `prompt_conflict_async`. |
| [dialogs/create.rs](dialogs/create.rs) | `show_create_dialog` (enables password entry only for `.zip`, confirms overwrite via `gtk::AlertDialog` before `create_archive`). |
| [dialogs/properties.rs](dialogs/properties.rs) | `show_properties_dialog` consuming `&[ArchiveEntry]` directly (single item, multi-selection summary, or archive metadata via `ArchivePropertiesInfo`). |
| [viewer/mod.rs](viewer/mod.rs) | Main window (`launch_standalone`), `HeaderBar`, `make_open_archive`, `install_header_actions`, and shared `run_viewer_worker`. |
| [viewer/models.rs](viewer/models.rs) | GObject subclasses (`ArchiveRowObject`, `FolderTreeNode`, `ArchiveDragProvider`), `ViewerState`, `NavFn`/`NavWeak`, row & tree builders + headless tests. |
| [viewer/columns.rs](viewer/columns.rs) | `ColumnView` factories & sorters (`Name`, `Uncompressed Size`, `Type`, `Modified Date`), context menu `gio::Menu`, typed `BoundRowPos` right-click binding. |
| [viewer/tree.rs](viewer/tree.rs) | Left folder tree `ListView`, `EventControllerKey` Left/Right handling, breadcrumb bar (`render_breadcrumbs`), monotonic `sync_tree_selection`, thumb mouse buttons `8`/`9`. |
| [viewer/actions.rs](viewer/actions.rs) | Context menu actions (`item.*`), Drag-and-Drop in/out wiring, `activate_row` preview launch (`FileLauncher`), `start_extract_dialog`, `copy_selected_to_clipboard`. |

## GTK4 Ownership & Lifetime Rules

1. **Cyclic navigation callback (`NavFn` / `NavWeak`)**: `navigate_to` is
   allocated via `Rc::new_cyclic` ([viewer/tree.rs](viewer/tree.rs)). Strong
   `Rc<NavCallback>` handles live on the window/widget controllers (`up_btn`,
   `tree_selection`, `thumb_click`, `column_view`, `open_archive`).
   `render_breadcrumbs` stores only `NavWeak` in dynamic breadcrumb button
   closures so old buttons drop cleanly on rebuild while `nav_weak.upgrade()`
   stays valid for the lifetime of the window.
2. **Popover parent cleanup**: `popover.set_parent(&column_view)` must stay
   paired with `column_view.connect_destroy(|_| pop.unparent())` so GTK tears
   down the popover cleanly without widget-parent warnings.
3. **Virtualized cell right-clicks**: `columns.rs` stores
   `BoundRowPos(Cell<u32>)` on each cell child via `"rarc-pos-cell"` in
   `connect_setup`, updating it in `connect_bind` and clearing it to `u32::MAX`
   in `connect_unbind`; a single right-click `GestureClick` on `column_view`
   resolves the row via `pick_row_pos` so recycled widgets never act on a stale
   index.
4. **Application lifetime holds (`--tap` mode)**: `--tap` windows have no main
   viewer window, so `run_worker_with_progress`, `TapAction::ExtractTo`, and
   `show_create_dialog`'s transition into progress hold `app.hold()` explicitly
   until the replacement window presents.

## Navigation, Focus & State Invariants

- **Tree Left/Right arrow focus**: In GTK4 `ListView`, calling only
  `SingleSelection::set_selected` leaves the internal focus cursor on the old
  row; collapsing an expanded folder then removes that row widget and resets
  focus to row `0`. Always update both selection and focus via
  `tree_view.scroll_to(pos, gtk::ListScrollFlags::FOCUS |
  gtk::ListScrollFlags::SELECT, None)` in both the key controller and
  `sync_tree_selection`.
- **Re-entrancy guard**: Wrap programmatic `sync_tree_selection` updates with
  `state.syncing_tree = true` / `false` so `tree_selection`'s
  `selected_item_notify` handler does not push duplicate navigation history.
- **Search `strip_prefix`**: `ViewerState::active_strip_prefix()` returns `None`
  when `search_query` is non-empty so Copy, Extract Selected, and DnD from search
  results never strip an unrelated `current_dir`.
- **Password writeback safety**: Both `run_viewer_worker` and
  `ArchiveDragProvider` check `st.current_archive.as_ref() == Some(&archive)`
  before updating `st.cached_password`, preventing stale background tasks from
  overwriting a newly opened archive's password.
- **Portal cancellation & symlinks**: `is_user_dismissed_error` suppresses
  false-positive error modals when the user dismisses `gtk::FileLauncher`
  (`gtk::DialogError::{Dismissed, Cancelled}` / `gio::IOErrorEnum::Cancelled`).
  `activate_row` guards `EntryKind::Symlink` with an info dialog since isolated
  symlink extraction cannot resolve sibling archive targets.

## Testing Guidelines

Keep pure data/path transformation helpers (`build_row_specs`,
`build_folder_children_map` in `viewer/models.rs`; `build_output_path`,
`summarize_inputs` in `dialogs/create.rs`) decoupled from
`gtk::Window`/`Display` creation so `cargo test` exercises them headlessly
without requiring a Wayland/X11 server.
