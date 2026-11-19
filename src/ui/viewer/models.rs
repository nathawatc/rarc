use std::cell::{OnceCell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
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
    extract_archive_with_session, matches_selection, spawn_worker, ArchiveEntry, EntryKind,
    ExtractSession, OperationSummary, WorkerEvent,
};
use crate::ui::dialogs::{prompt_password_async, ArchivePropertiesInfo};
use crate::ui::session_temp_dir;

pub(super) mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Clone, Default)]
    pub struct ArchiveRowData {
        pub entry: ArchiveEntry,
        pub name: String,
        pub parent_dir: String,
        pub type_label: String,
    }

    impl ArchiveRowData {
        pub fn is_dir(&self) -> bool {
            self.entry.kind == EntryKind::Directory
        }
    }

    #[derive(Default)]
    pub struct ArchiveRowObject {
        pub data: RefCell<ArchiveRowData>,
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
        pub seq: Cell<u64>,
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
            let seq = self.seq.get().wrapping_add(1);
            self.seq.set(seq);

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
                        Some(&selected_worker),
                        strip_worker.as_deref(),
                        Some(is_raw),
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
    pub(super) fn new(data: imp::ArchiveRowData) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().data.borrow_mut() = data;
        obj
    }

    pub(super) fn with_data<R>(&self, f: impl FnOnce(&imp::ArchiveRowData) -> R) -> R {
        f(&self.imp().data.borrow())
    }

    pub(super) fn entry(&self) -> ArchiveEntry {
        self.imp().data.borrow().entry.clone()
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
        .map(|row| row.with_data(|d| d.entry.path.clone()))
        .collect()
}

pub(super) fn compute_selected_bytes(
    store: &gio::ListStore,
    selected_prefixes: Option<&[String]>,
) -> u64 {
    let mut total = 0u64;
    for idx in 0..store.n_items() {
        if let Some(obj) = store.item(idx)
            && let Some(row) = obj.downcast_ref::<ArchiveRowObject>()
        {
            row.with_data(|d| {
                if !d.is_dir() && matches_selection(&d.entry.path, selected_prefixes) {
                    total += d.entry.size;
                }
            });
        }
    }
    total
}

pub(super) fn path_basename(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((_, name)) => name,
        None => path,
    }
}

pub(super) fn cmp_ascii_ignore_case(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

pub(super) fn build_folder_children_map(entries: &[ArchiveEntry]) -> HashMap<String, Vec<String>> {
    let mut dir_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen_dirs: HashSet<String> = HashSet::new();

    for entry in entries {
        let dir_path = if entry.kind == EntryKind::Directory {
            Some(entry.path.as_str())
        } else {
            entry.path.rsplit_once('/').map(|(p, _)| p)
        };
        let Some(full_dir) = dir_path else { continue };
        let mut acc = String::new();
        for part in full_dir.split('/') {
            let parent = acc.clone();
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if seen_dirs.insert(acc.clone()) {
                dir_children.entry(parent).or_default().push(acc.clone());
            }
        }
    }
    for list in dir_children.values_mut() {
        list.sort_by(|a, b| cmp_ascii_ignore_case(path_basename(a), path_basename(b)));
    }
    dir_children
}

fn build_row_specs(entries: &[ArchiveEntry]) -> Vec<imp::ArchiveRowData> {
    let mut rows = Vec::with_capacity(entries.len());
    let mut seen_paths: HashSet<String> = HashSet::with_capacity(entries.len());
    let mut queued_dirs: HashSet<String> = HashSet::new();

    for entry in entries {
        let (parent_dir, name) = split_parent_and_name(&entry.path);
        seen_paths.insert(entry.path.clone());

        if !parent_dir.is_empty() {
            let mut acc = String::new();
            for part in parent_dir.split('/') {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                if !seen_paths.contains(&acc) && !queued_dirs.contains(&acc) {
                    queued_dirs.insert(acc.clone());
                }
            }
        }

        rows.push(imp::ArchiveRowData {
            type_label: entry.kind.label(&name),
            name,
            parent_dir,
            entry: entry.clone(),
        });
    }

    for dir_path in queued_dirs {
        if seen_paths.insert(dir_path.clone()) {
            let (parent_dir, name) = split_parent_and_name(&dir_path);
            let entry = ArchiveEntry {
                path: dir_path,
                size: 0,
                compressed_size: None,
                kind: EntryKind::Directory,
                mtime: None,
                mode: 0o755,
                symlink_target: None,
                is_encrypted: false,
            };
            rows.push(imp::ArchiveRowData {
                type_label: entry.kind.label(&name),
                name,
                parent_dir,
                entry,
            });
        }
    }

    rows
}

pub(super) fn build_row_objects(entries: &[ArchiveEntry]) -> Vec<ArchiveRowObject> {
    build_row_specs(entries)
        .into_iter()
        .map(ArchiveRowObject::new)
        .collect()
}

pub(super) fn split_parent_and_name(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    }
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
    fn row_specs_synthesize_unique_missing_parent_dirs() {
        let entries = vec![
            file_entry("a/b/one.txt", 10),
            file_entry("a/b/two.txt", 20),
            dir_entry("a/b", Some(1234)),
        ];
        let rows = build_row_specs(&entries);
        let dir_a: Vec<_> = rows.iter().filter(|r| r.entry.path == "a").collect();
        let dir_ab: Vec<_> = rows.iter().filter(|r| r.entry.path == "a/b").collect();

        assert_eq!(dir_a.len(), 1);
        assert!(dir_a[0].is_dir());
        assert_eq!(dir_a[0].parent_dir, "");
        assert_eq!(dir_ab.len(), 1);
        assert_eq!(dir_ab[0].entry.mtime, Some(1234));
    }

    #[test]
    fn folder_tree_map_sorts_alphabetically_and_dedups() {
        let entries = vec![
            file_entry("zeta/sub/x.txt", 1),
            file_entry("Alpha/sub/y.txt", 2),
            dir_entry("zeta", None),
        ];
        let tree = build_folder_children_map(&entries);
        let top: Vec<&str> = tree[""].iter().map(|p| path_basename(p)).collect();
        assert_eq!(top, vec!["Alpha", "zeta"]);
        assert_eq!(tree["zeta"], vec!["zeta/sub".to_string()]);
    }
}
