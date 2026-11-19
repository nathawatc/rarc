use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::archive::{
    extract_archive_with_session, spawn_worker, ArchiveEntry, EntryKind, ExtractOptions,
    ExtractSession, OperationSummary, SelectionFilter, WorkerEvent,
};
use crate::ui::dialogs::{prompt_password_async, ArchivePropertiesInfo};
use crate::ui::session_temp_dir;

pub(super) mod imp {
    use super::*;
    use std::cell::{Cell, OnceCell, RefCell};

    #[derive(Default)]
    pub struct ArchiveRowObject {
        pub entry: OnceCell<Rc<ArchiveEntry>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArchiveRowObject {
        const NAME: &'static str = "RarcArchiveRow";
        type Type = super::ArchiveRowObject;
    }

    impl ObjectImpl for ArchiveRowObject {}

    #[derive(Default)]
    pub struct FolderTreeData {
        pub name: String,
        pub full_path: String,
    }

    #[derive(Default)]
    pub struct FolderTreeNode {
        pub data: OnceCell<FolderTreeData>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FolderTreeNode {
        const NAME: &'static str = "RarcFolderTreeNode";
        type Type = super::FolderTreeNode;
    }

    impl ObjectImpl for FolderTreeNode {}

    #[derive(Default)]
    pub struct ArchiveDragProvider {
        pub archive_path: RefCell<PathBuf>,
        pub selected_paths: RefCell<Vec<String>>,
        pub strip_prefix: RefCell<Option<String>>,
        pub password: RefCell<Option<String>>,
        pub is_raw: Cell<bool>,
        pub total_bytes: Cell<u64>,
        pub(in crate::ui::viewer) state: RefCell<std::rc::Weak<RefCell<ViewerState>>>,
        pub window: glib::WeakRef<gtk::Window>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArchiveDragProvider {
        const NAME: &'static str = "RarcDragContentProvider";
        type Type = super::ArchiveDragProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for ArchiveDragProvider {}

    impl gdk::subclass::content_provider::ContentProviderImpl for ArchiveDragProvider {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormats::builder()
                .add_mime_type("text/uri-list")
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), glib::Error>> + 'static>> {
            let mime = mime_type.to_string();
            let stream = stream.clone();
            let archive_path = self.archive_path.borrow().clone();
            let selected = self.selected_paths.borrow().clone();
            let strip_prefix = self.strip_prefix.borrow().clone();
            let password = self.password.borrow().clone();
            let is_raw = self.is_raw.get();
            let total_bytes = self.total_bytes.get();
            let state_weak = self.state.borrow().clone();
            let window = self.window.upgrade();
            let seq = state_weak
                .upgrade()
                .map(|st_rc| {
                    let mut st = st_rc.borrow_mut();
                    st.stage_seq += 1;
                    st.stage_seq
                })
                .unwrap_or(0);

            Box::pin(async move {
                if mime != "text/uri-list" || selected.is_empty() {
                    return Err(glib::Error::new(
                        gio::IOErrorEnum::NotSupported,
                        "Unsupported drag mime type or empty selection",
                    ));
                }

                let stage_root = session_temp_dir().map_err(|e| {
                    glib::Error::new(gio::IOErrorEnum::Failed, &format!("Temp dir error: {e}"))
                })?;
                let stage_dir = stage_root.join(format!("dnd-{seq}"));
                let stage_worker = stage_dir.clone();
                let selected_worker = selected.clone();
                let cancel = Arc::new(AtomicBool::new(false));
                let cancel_worker = cancel.clone();

                let archive_for_state = archive_path.clone();
                let strip_worker = strip_prefix.clone();
                let rx = spawn_worker(move |tx| {
                    let mut session = ExtractSession::new(password, total_bytes);
                    extract_archive_with_session(
                        &archive_path,
                        &stage_worker,
                        ExtractOptions {
                            selected_prefixes: Some(&selected_worker),
                            strip_prefix: strip_worker.as_deref(),
                            known_raw: Some(is_raw),
                        },
                        &mut session,
                        Some(tx),
                        &cancel_worker,
                    )?;
                    Ok(OperationSummary::Extracted {
                        password: session.password,
                    })
                });

                while let Ok(ev) = rx.recv().await {
                    match ev {
                        WorkerEvent::NeedPassword {
                            archive_name,
                            retry,
                            reply,
                        } => {
                            let answer = if let Some(ref win) = window {
                                prompt_password_async(win, &archive_name, retry).await
                            } else {
                                None
                            };
                            let _ = reply.send(answer).await;
                        }
                        WorkerEvent::FileConflict { reply, .. } => {
                            let _ = reply
                                .send(crate::archive::ConflictResolution::OverwriteAll)
                                .await;
                        }
                        WorkerEvent::Finished(Ok(OperationSummary::Extracted {
                            password: Some(pw),
                        })) => {
                            if let Some(state_rc) = state_weak.upgrade() {
                                let mut st = state_rc.borrow_mut();
                                if st.current_archive.as_ref() == Some(&archive_for_state) {
                                    st.cached_password = Some(pw);
                                }
                            }
                            break;
                        }
                        WorkerEvent::Finished(Ok(_)) => break,
                        WorkerEvent::Finished(Err(err)) => {
                            return Err(glib::Error::new(
                                gio::IOErrorEnum::Failed,
                                &err.to_string(),
                            ));
                        }
                        WorkerEvent::Progress { .. } => {}
                    }
                }

                let extracted_files =
                    collect_staged_files(&stage_dir, &selected, strip_prefix.as_deref());
                let mut uri_list = String::new();
                for f in extracted_files {
                    uri_list.push_str(f.uri().as_str());
                    uri_list.push_str("\r\n");
                }

                stream
                    .write_all_future(uri_list.into_bytes(), io_priority)
                    .await
                    .map(|_| ())
                    .map_err(|(_, e)| e)
            })
        }
    }
}

glib::wrapper! {
    pub struct ArchiveRowObject(ObjectSubclass<imp::ArchiveRowObject>);
}

impl ArchiveRowObject {
    pub(super) fn new(entry: Rc<ArchiveEntry>) -> Self {
        let obj: Self = glib::Object::new();
        let _ = obj.imp().entry.set(entry);
        obj
    }

    pub(super) fn entry_ref(&self) -> &ArchiveEntry {
        self.imp()
            .entry
            .get()
            .expect("ArchiveRowObject initialized")
    }

    pub(super) fn is_dir(&self) -> bool {
        self.entry_ref().kind == EntryKind::Directory
    }

    pub(super) fn name(&self) -> &str {
        path_basename(&self.entry_ref().path)
    }

    pub(super) fn parent_dir(&self) -> &str {
        path_parent(&self.entry_ref().path)
    }

    pub(super) fn entry(&self) -> ArchiveEntry {
        self.entry_ref().clone()
    }
}

glib::wrapper! {
    pub struct FolderTreeNode(ObjectSubclass<imp::FolderTreeNode>);
}

impl FolderTreeNode {
    pub(super) fn new(name: String, full_path: String) -> Self {
        let obj: Self = glib::Object::new();
        let _ = obj
            .imp()
            .data
            .set(imp::FolderTreeData { name, full_path });
        obj
    }

    pub(super) fn name(&self) -> &str {
        self.imp()
            .data
            .get()
            .map(|d| d.name.as_str())
            .unwrap_or("")
    }

    pub(super) fn full_path(&self) -> &str {
        self.imp()
            .data
            .get()
            .map(|d| d.full_path.as_str())
            .unwrap_or("")
    }
}

glib::wrapper! {
    pub struct ArchiveDragProvider(ObjectSubclass<imp::ArchiveDragProvider>)
        @extends gdk::ContentProvider;
}

impl ArchiveDragProvider {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        archive_path: PathBuf,
        selected_paths: Vec<String>,
        strip_prefix: Option<String>,
        password: Option<String>,
        is_raw: bool,
        total_bytes: u64,
        state: std::rc::Weak<RefCell<ViewerState>>,
        window: &gtk::Window,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        *imp.archive_path.borrow_mut() = archive_path;
        *imp.selected_paths.borrow_mut() = selected_paths;
        *imp.strip_prefix.borrow_mut() = strip_prefix;
        *imp.password.borrow_mut() = password;
        imp.is_raw.set(is_raw);
        imp.total_bytes.set(total_bytes);
        *imp.state.borrow_mut() = state;
        imp.window.set(Some(window));
        obj
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NavMode {
    Push,
    History,
    Reset,
}

#[derive(Default)]
pub(super) struct ViewerState {
    pub current_archive: Option<PathBuf>,
    pub cached_password: Option<String>,
    pub format_name: String,
    pub is_raw: bool,
    pub current_dir: String,
    pub search_query: String,
    pub stage_seq: u64,
    pub entry_count: usize,
    pub total_uncompressed: u64,
    pub encrypted_count: usize,
    pub syncing_tree: bool,
    pub history_back: Vec<String>,
    pub history_forward: Vec<String>,
    pub entries: Vec<Rc<ArchiveEntry>>,
    pub dir_entries: HashMap<String, Vec<usize>>,
    pub dir_children: HashMap<String, Vec<String>>,
}

impl ViewerState {
    pub fn active_strip_prefix(&self) -> Option<String> {
        if !self.search_query.is_empty() || self.current_dir.is_empty() {
            None
        } else {
            Some(self.current_dir.clone())
        }
    }

    pub fn properties_info(&self) -> Option<ArchivePropertiesInfo> {
        self.current_archive.as_ref().map(|p| ArchivePropertiesInfo {
            archive_path: p.clone(),
            format_name: self.format_name.clone(),
            entry_count: self.entry_count,
            total_uncompressed: self.total_uncompressed,
            encrypted_count: self.encrypted_count,
        })
    }
}

pub(super) type NavCallback = Box<dyn Fn(String, bool, NavMode)>;
pub(super) type NavFn = Rc<NavCallback>;
pub(super) type NavWeak = std::rc::Weak<NavCallback>;

pub(super) fn collect_staged_files(
    stage_dir: &Path,
    selected: &[String],
    strip_prefix: Option<&str>,
) -> Vec<gio::File> {
    let prefix_slash = strip_prefix
        .filter(|s| !s.is_empty())
        .map(|s| format!("{}/", s.trim_end_matches('/')));

    let mut files = Vec::new();
    for sel in selected {
        let rel = if let Some(ref pre) = prefix_slash {
            sel.strip_prefix(pre).unwrap_or(sel.as_str())
        } else {
            sel.as_str()
        };
        let extracted = stage_dir.join(rel);
        if extracted.exists() {
            files.push(gio::File::for_path(extracted));
        }
    }
    files
}

pub(super) fn collect_selected_rows(selection: &gtk::MultiSelection) -> Vec<ArchiveRowObject> {
    let bitset = selection.selection();
    let mut rows = Vec::new();
    if let Some((iter, first)) = gtk::BitsetIter::init_first(&bitset) {
        if let Some(obj) = selection.item(first)
            && let Ok(row) = obj.downcast::<ArchiveRowObject>()
        {
            rows.push(row);
        }
        for pos in iter {
            if let Some(obj) = selection.item(pos)
                && let Ok(row) = obj.downcast::<ArchiveRowObject>()
            {
                rows.push(row);
            }
        }
    }
    rows
}

pub(super) fn collect_selected_paths(selection: &gtk::MultiSelection) -> Vec<String> {
    collect_selected_rows(selection)
        .into_iter()
        .map(|row| row.entry_ref().path.clone())
        .collect()
}

pub(super) fn compute_selected_bytes(
    entries: &[Rc<ArchiveEntry>],
    selected_prefixes: Option<&[String]>,
) -> u64 {
    let filter = selected_prefixes.map(SelectionFilter::new);
    entries
        .iter()
        .filter(|e| {
            e.kind != EntryKind::Directory
                && filter.as_ref().is_none_or(|f| f.matches(&e.path))
        })
        .map(|e| e.size)
        .sum()
}

pub(super) fn path_basename(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((_, name)) => name,
        None => path,
    }
}

pub(super) fn path_parent(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

pub(super) fn cmp_ascii_ignore_case(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

pub(super) fn contains_ascii_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    let n = needle_lower.as_bytes();
    if n.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len())
        .any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

pub(super) struct ArchiveIndex {
    pub entries: Vec<Rc<ArchiveEntry>>,
    pub dir_entries: HashMap<String, Vec<usize>>,
    pub dir_children: HashMap<String, Vec<String>>,
}

/// Consumes raw archive entries in a single pass, synthesizing missing parent folders
/// and building `O(1)` directory-to-entry and folder-tree indices.
pub(super) fn index_archive_entries(raw_entries: Vec<ArchiveEntry>) -> ArchiveIndex {
    let mut entries: Vec<Rc<ArchiveEntry>> = Vec::with_capacity(raw_entries.len());
    let mut dir_entries: HashMap<String, Vec<usize>> = HashMap::new();
    let mut dir_children: HashMap<String, Vec<String>> = HashMap::new();
    // Tracks every directory path seen (explicit or implicit ancestor);
    // value is `true` when an explicit Directory entry already exists in `entries`.
    let mut seen_dirs: HashMap<String, bool> = HashMap::new();
    let mut missing_lens: Vec<usize> = Vec::new();

    for entry in raw_entries {
        let idx = entries.len();
        let is_dir = entry.kind == EntryKind::Directory;
        let parent_dir = path_parent(&entry.path);

        if let Some(list) = dir_entries.get_mut(parent_dir) {
            list.push(idx);
        } else {
            dir_entries.insert(parent_dir.to_string(), vec![idx]);
        }

        if is_dir
            && let Some(has_explicit) = seen_dirs.get_mut(entry.path.as_str())
        {
            *has_explicit = true;
        }

        let ancestor_dir = if is_dir {
            Some(entry.path.as_str())
        } else if !parent_dir.is_empty() {
            Some(parent_dir)
        } else {
            None
        };

        if let Some(full_dir) = ancestor_dir
            && !seen_dirs.contains_key(full_dir)
        {
            missing_lens.clear();
            let mut cur = full_dir;
            while !cur.is_empty() && !seen_dirs.contains_key(cur) {
                missing_lens.push(cur.len());
                cur = path_parent(cur);
            }
            for &len in missing_lens.iter().rev() {
                let dir = &full_dir[..len];
                let p = path_parent(dir);
                let dir_owned = dir.to_string();
                if let Some(children) = dir_children.get_mut(p) {
                    children.push(dir_owned.clone());
                } else {
                    dir_children.insert(p.to_string(), vec![dir_owned.clone()]);
                }
                let explicit = is_dir && dir == entry.path;
                seen_dirs.insert(dir_owned, explicit);
            }
        }

        entries.push(Rc::new(entry));
    }

    for (dir_path, has_explicit) in seen_dirs {
        if !has_explicit {
            let idx = entries.len();
            let parent_dir = path_parent(&dir_path);
            if let Some(list) = dir_entries.get_mut(parent_dir) {
                list.push(idx);
            } else {
                dir_entries.insert(parent_dir.to_string(), vec![idx]);
            }
            entries.push(Rc::new(ArchiveEntry {
                path: dir_path,
                size: 0,
                compressed_size: None,
                kind: EntryKind::Directory,
                mtime: None,
                mode: 0o755,
                symlink_target: None,
                is_encrypted: false,
            }));
        }
    }

    for list in dir_children.values_mut() {
        list.sort_by(|a, b| cmp_ascii_ignore_case(path_basename(a), path_basename(b)));
    }

    ArchiveIndex {
        entries,
        dir_entries,
        dir_children,
    }
}

pub(super) fn refresh_visible_store(store: &gio::ListStore, state: &ViewerState) {
    let rows: Vec<ArchiveRowObject> = if state.search_query.is_empty() {
        state
            .dir_entries
            .get(&state.current_dir)
            .map(|indices| {
                indices
                    .iter()
                    .map(|&idx| ArchiveRowObject::new(Rc::clone(&state.entries[idx])))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        let q = state.search_query.as_str();
        state
            .entries
            .iter()
            .filter(|e| contains_ascii_ignore_case(&e.path, q))
            .map(|e| ArchiveRowObject::new(Rc::clone(e)))
            .collect()
    };
    store.splice(0, store.n_items(), &rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_entry(path: &str, size: u64) -> ArchiveEntry {
        ArchiveEntry {
            path: path.to_string(),
            size,
            kind: EntryKind::File,
            ..Default::default()
        }
    }

    fn dir_entry(path: &str, mtime: Option<i64>) -> ArchiveEntry {
        ArchiveEntry {
            path: path.to_string(),
            kind: EntryKind::Directory,
            mtime,
            ..Default::default()
        }
    }

    #[test]
    fn index_synthesizes_unique_missing_parent_dirs_and_maps_children() {
        let entries = vec![
            file_entry("a/b/one.txt", 10),
            file_entry("a/b/two.txt", 20),
            dir_entry("a/b", Some(1234)),
        ];
        let index = index_archive_entries(entries);
        let dir_a: Vec<_> = index.entries.iter().filter(|e| e.path == "a").collect();
        let dir_ab: Vec<_> = index.entries.iter().filter(|e| e.path == "a/b").collect();

        assert_eq!(dir_a.len(), 1);
        assert_eq!(dir_a[0].kind, EntryKind::Directory);
        assert_eq!(path_parent(&dir_a[0].path), "");
        assert_eq!(dir_ab.len(), 1);
        assert_eq!(dir_ab[0].mtime, Some(1234));
        assert_eq!(index.dir_entries[""].len(), 1);
        assert_eq!(index.dir_entries["a"].len(), 1);
        assert_eq!(index.dir_entries["a/b"].len(), 2);
    }

    #[test]
    fn folder_tree_map_sorts_alphabetically_and_dedups() {
        let entries = vec![
            file_entry("zeta/sub/x.txt", 1),
            file_entry("Alpha/sub/y.txt", 2),
            dir_entry("zeta", None),
        ];
        let index = index_archive_entries(entries);
        let top: Vec<&str> = index.dir_children[""]
            .iter()
            .map(|p| path_basename(p))
            .collect();
        assert_eq!(top, vec!["Alpha", "zeta"]);
        assert_eq!(index.dir_children["zeta"], vec!["zeta/sub".to_string()]);
    }

    #[test]
    fn ascii_case_insensitive_helpers() {
        assert_eq!(cmp_ascii_ignore_case("Alpha", "alpha"), Ordering::Equal);
        assert_eq!(cmp_ascii_ignore_case("Alpha", "beta"), Ordering::Less);
        assert!(contains_ascii_ignore_case("src/Main.RS", "main.rs"));
        assert!(!contains_ascii_ignore_case("src/main.rs", "cargo"));
        assert!(contains_ascii_ignore_case("anything", ""));
    }
}
