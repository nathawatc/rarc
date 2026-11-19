use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
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
    add_to_archive, extract_archive_with_session, is_supported_archive_path, list_archive,
    matches_selection, spawn_worker, test_archive, ArchiveEntry, ArchiveError, ConflictResolution,
    EntryKind, ExtractSession, OperationSummary, WorkerEvent,
};
use crate::ui::dialogs::{
    prompt_password_async, run_worker_with_progress, show_create_dialog, show_properties_dialog,
    ArchivePropertiesInfo, PropertyEntry,
};
use crate::ui::{format_mtime, format_size, session_temp_dir, show_error_dialog, show_info_dialog};

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Clone, Default)]
    pub struct ArchiveRowData {
        pub name: String,
        pub full_path: String,
        pub parent_dir: String,
        pub size: u64,
        pub compressed_size: Option<u64>,
        pub type_label: String,
        pub mtime: Option<i64>,
        pub mode: u32,
        pub symlink_target: Option<String>,
        pub is_dir: bool,
        pub is_encrypted: bool,
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

    pub struct FolderTreeData {
        pub name: String,
        pub full_path: String,
        pub children: gio::ListStore,
    }

    impl Default for FolderTreeData {
        fn default() -> Self {
            Self {
                name: String::new(),
                full_path: String::new(),
                children: gio::ListStore::new::<super::FolderTreeNode>(),
            }
        }
    }

    #[derive(Default)]
    pub struct FolderTreeNode {
        pub data: RefCell<FolderTreeData>,
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
        pub(super) state: RefCell<std::rc::Weak<RefCell<ViewerState>>>,
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
                            let _ = reply.send(ConflictResolution::OverwriteAll).await;
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

                let extracted_files = collect_staged_files(&stage_dir, &selected, strip_prefix.as_deref());
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
    fn new(data: imp::ArchiveRowData) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().data.borrow_mut() = data;
        obj
    }

    fn with_data<R>(&self, f: impl FnOnce(&imp::ArchiveRowData) -> R) -> R {
        f(&self.imp().data.borrow())
    }

    fn to_property_entry(&self) -> PropertyEntry {
        self.with_data(|d| PropertyEntry {
            name: d.name.clone(),
            full_path: d.full_path.clone(),
            type_label: d.type_label.clone(),
            size: d.size,
            compressed_size: d.compressed_size,
            mtime: d.mtime,
            mode: d.mode,
            symlink_target: d.symlink_target.clone(),
            is_dir: d.is_dir,
            is_encrypted: d.is_encrypted,
        })
    }
}

glib::wrapper! {
    pub struct FolderTreeNode(ObjectSubclass<imp::FolderTreeNode>);
}

impl FolderTreeNode {
    fn new(name: String, full_path: String) -> Self {
        let obj: Self = glib::Object::new();
        {
            let mut d = obj.imp().data.borrow_mut();
            d.name = name;
            d.full_path = full_path;
        }
        obj
    }

    fn name(&self) -> String {
        self.imp().data.borrow().name.clone()
    }

    fn full_path(&self) -> String {
        self.imp().data.borrow().full_path.clone()
    }

    fn children(&self) -> gio::ListStore {
        self.imp().data.borrow().children.clone()
    }
}

glib::wrapper! {
    pub struct ArchiveDragProvider(ObjectSubclass<imp::ArchiveDragProvider>)
        @extends gdk::ContentProvider;
}

impl ArchiveDragProvider {
    #[allow(clippy::too_many_arguments)]
    fn new(
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
enum NavMode {
    Push,
    History,
    Reset,
}

#[derive(Default)]
struct ViewerState {
    current_archive: Option<PathBuf>,
    cached_password: Option<String>,
    format_name: String,
    is_raw: bool,
    current_dir: String,
    search_query: String,
    stage_seq: u64,
    entry_count: usize,
    total_uncompressed: u64,
    encrypted_count: usize,
    syncing_tree: bool,
    history_back: Vec<String>,
    history_forward: Vec<String>,
}

impl ViewerState {
    fn active_strip_prefix(&self) -> Option<String> {
        if !self.search_query.is_empty() || self.current_dir.is_empty() {
            None
        } else {
            Some(self.current_dir.clone())
        }
    }
}

type CellRightClickFn = Rc<dyn Fn(&gtk::Widget, f64, f64, Option<u32>)>;
type NavCallback = Box<dyn Fn(String, bool, NavMode)>;
type NavFn = Rc<NavCallback>;
type NavWeak = std::rc::Weak<NavCallback>;

/// Launches the standalone archive viewer/manager window.
pub fn launch_standalone(app: &gtk::Application, initial_archive: Option<PathBuf>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("rarc")
        .default_width(980)
        .default_height(620)
        .build();

    let state = Rc::new(RefCell::new(ViewerState::default()));

    // HeaderBar controls
    let header = gtk::HeaderBar::new();
    let open_btn = gtk::Button::builder()
        .label("Open")
        .icon_name("document-open-symbolic")
        .tooltip_text("Open an archive")
        .build();
    let new_btn = gtk::Button::builder()
        .label("Create")
        .icon_name("archive-insert-symbolic")
        .tooltip_text("Create a new archive")
        .build();
    let extract_btn = gtk::Button::builder()
        .label("Extract")
        .icon_name("archive-extract-symbolic")
        .tooltip_text("Extract selected entries or entire archive")
        .sensitive(false)
        .build();
    let test_btn = gtk::Button::builder()
        .label("Test")
        .icon_name("security-high-symbolic")
        .tooltip_text("Test archive integrity")
        .sensitive(false)
        .build();
    let props_btn = gtk::Button::builder()
        .label("Properties")
        .icon_name("document-properties-symbolic")
        .tooltip_text("Archive properties")
        .sensitive(false)
        .build();

    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Filter entries…")
        .width_chars(22)
        .build();

    header.pack_start(&open_btn);
    header.pack_start(&new_btn);
    header.pack_start(&extract_btn);
    header.pack_start(&test_btn);
    header.pack_start(&props_btn);
    header.pack_end(&search_entry);
    // Keep title out of HeaderBar so it does not duplicate the window title bar.
    header.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.set_titlebar(Some(&header));

    // Breadcrumb / navigation bar
    let breadcrumb_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    breadcrumb_bar.add_css_class("rarc-breadcrumb-bar");
    let up_btn = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Up one folder")
        .sensitive(false)
        .build();
    up_btn.add_css_class("flat");
    let crumbs_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    crumbs_box.set_hexpand(true);
    let crumbs_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .child(&crumbs_box)
        .build();
    breadcrumb_bar.append(&up_btn);
    breadcrumb_bar.append(&crumbs_scroll);

    // Left folder tree sidebar
    let tree_root_store = gio::ListStore::new::<FolderTreeNode>();
    let tree_model = gtk::TreeListModel::new(tree_root_store.clone(), false, false, |obj| {
        let node = obj.downcast_ref::<FolderTreeNode>()?;
        let children = node.children();
        if children.n_items() > 0 {
            Some(children.upcast())
        } else {
            None
        }
    });
    let tree_selection = gtk::SingleSelection::new(Some(tree_model.clone()));
    tree_selection.set_autoselect(false);

    let tree_factory = gtk::SignalListItemFactory::new();
    tree_factory.connect_setup(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let expander = gtk::TreeExpander::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = gtk::Image::from_icon_name("folder-symbolic");
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        hbox.append(&icon);
        hbox.append(&label);
        expander.set_child(Some(&hbox));
        item.set_child(Some(&expander));
    });
    tree_factory.connect_bind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let list_row = item.item().and_downcast::<gtk::TreeListRow>().unwrap();
        let node = list_row.item().and_downcast::<FolderTreeNode>().unwrap();
        let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
        expander.set_list_row(Some(&list_row));
        let hbox = expander.child().and_downcast::<gtk::Box>().unwrap();
        let icon = hbox.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
        let full_path = node.full_path();
        icon.set_icon_name(Some(if full_path.is_empty() {
            "package-x-generic"
        } else {
            "folder-symbolic"
        }));
        label.set_text(&node.name());
    });
    tree_factory.connect_unbind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        if let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() {
            expander.set_list_row(None);
        }
    });

    let tree_view = gtk::ListView::new(Some(tree_selection.clone()), Some(tree_factory));
    {
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let sel_key = tree_selection.clone();
        let tv_key = tree_view.clone();
        key_ctrl.connect_key_pressed(move |_, key, _, _| {
            let Some(row) = sel_key.selected_item().and_downcast::<gtk::TreeListRow>() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Right | gdk::Key::KP_Right => {
                    let pos = row.position();
                    if row.is_expandable() && !row.is_expanded() {
                        row.set_expanded(true);
                        tv_key.scroll_to(
                            pos,
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                    if row.is_expanded() && row.children().is_some_and(|c| c.n_items() > 0) {
                        tv_key.scroll_to(
                            pos + 1,
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                }
                gdk::Key::Left | gdk::Key::KP_Left => {
                    if row.is_expanded() {
                        let pos = row.position();
                        row.set_expanded(false);
                        tv_key.scroll_to(
                            pos,
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                    if let Some(parent) = row.parent() {
                        tv_key.scroll_to(
                            parent.position(),
                            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                            None,
                        );
                        return glib::Propagation::Stop;
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        tree_view.add_controller(key_ctrl);
    }
    let tree_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_width(180)
        .child(&tree_view)
        .build();
    tree_scroll.add_css_class("rarc-tree-sidebar");

    // ColumnView data pipeline
    let store = gio::ListStore::new::<ArchiveRowObject>();
    let filter = {
        let state_ref = state.clone();
        gtk::CustomFilter::new(move |obj| {
            let row = obj.downcast_ref::<ArchiveRowObject>().unwrap();
            let st = state_ref.borrow();
            row.with_data(|d| {
                if st.search_query.is_empty() {
                    d.parent_dir == st.current_dir
                } else {
                    let q = st.search_query.as_str();
                    d.name.to_ascii_lowercase().contains(q)
                        || d.full_path.to_ascii_lowercase().contains(q)
                }
            })
        })
    };
    let filter_model = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));

    let column_view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
    column_view.set_show_row_separators(false);
    column_view.set_show_column_separators(true);
    column_view.set_vexpand(true);
    column_view.set_hexpand(true);

    // Context menu popover & actions ("item.*")
    let item_actions = gio::SimpleActionGroup::new();
    let act_open = gio::SimpleAction::new("open", None);
    let act_open_with = gio::SimpleAction::new("open-with", None);
    let act_extract = gio::SimpleAction::new("extract", None);
    let act_copy = gio::SimpleAction::new("copy", None);
    let act_props = gio::SimpleAction::new("properties", None);
    item_actions.add_action(&act_open);
    item_actions.add_action(&act_open_with);
    item_actions.add_action(&act_extract);
    item_actions.add_action(&act_copy);
    item_actions.add_action(&act_props);
    window.insert_action_group("item", Some(&item_actions));

    let menu_model = build_context_menu_model();
    let popover = gtk::PopoverMenu::from_model(Some(&menu_model));
    popover.set_parent(&column_view);
    popover.set_has_arrow(false);
    {
        let pop_cleanup = popover.clone();
        column_view.connect_destroy(move |_| pop_cleanup.unparent());
    }

    let sort_model = gtk::SortListModel::new(Some(filter_model), column_view.sorter());
    let selection = gtk::MultiSelection::new(Some(sort_model));

    // Shared right-click trigger for both cells and empty whitespace
    let show_context_menu: CellRightClickFn = {
        let column_ref = column_view.clone();
        let selection_ref = selection.clone();
        let popover_ref = popover.clone();
        let state_ref = state.clone();
        let act_open = act_open.clone();
        let act_open_with = act_open_with.clone();
        let act_extract = act_extract.clone();
        let act_copy = act_copy.clone();
        let act_props = act_props.clone();

        Rc::new(move |src_widget, x, y, pos_opt| {
            if state_ref.borrow().current_archive.is_none() {
                return;
            }
            if let Some(pos) = pos_opt
                && !selection_ref.is_selected(pos)
            {
                selection_ref.select_item(pos, true);
            }

            let selected_rows = collect_selected_rows(&selection_ref);
            let count = selected_rows.len();
            let single_file = count == 1
                && !selected_rows[0].with_data(|d| d.is_dir || d.symlink_target.is_some());
            act_open.set_enabled(count == 1);
            act_open_with.set_enabled(single_file);
            act_extract.set_enabled(count > 0);
            act_copy.set_enabled(count > 0);
            act_props.set_enabled(count > 0);

            let rect = if let Some(pt) =
                src_widget.compute_point(&column_ref, &gtk::graphene::Point::new(x as f32, y as f32))
            {
                gdk::Rectangle::new(pt.x().round() as i32, pt.y().round() as i32, 1, 1)
            } else {
                gdk::Rectangle::new(x.round() as i32, y.round() as i32, 1, 1)
            };
            popover_ref.set_pointing_to(Some(&rect));
            popover_ref.popup();
        })
    };

    append_columns(&column_view, &show_context_menu);
    column_view.set_model(Some(&selection));

    // Fallback right-click on empty whitespace below rows
    {
        let bg_click = gtk::GestureClick::builder().button(3).build();
        let col_ref = column_view.clone();
        let show_menu = show_context_menu.clone();
        bg_click.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_menu(col_ref.upcast_ref(), x, y, None);
        });
        column_view.add_controller(bg_click);
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&column_view)
        .build();

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&tree_scroll)
        .end_child(&scrolled)
        .position(230)
        .resize_start_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .vexpand(true)
        .hexpand(true)
        .build();

    // Empty state placeholder
    let empty_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    empty_box.set_valign(gtk::Align::Center);
    empty_box.set_halign(gtk::Align::Center);
    empty_box.add_css_class("rarc-empty-state");
    let empty_icon = gtk::Image::from_icon_name("package-x-generic");
    empty_icon.set_pixel_size(64);
    let empty_title = gtk::Label::new(Some("Open an archive or drop files here"));
    empty_title.add_css_class("title-3");
    let empty_sub = gtk::Label::new(Some(
        "Drop an archive to inspect it, or drop files/folders to create a new archive.",
    ));
    empty_sub.add_css_class("rarc-hint");
    empty_box.append(&empty_icon);
    empty_box.append(&empty_title);
    empty_box.append(&empty_sub);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_hexpand(true);
    stack.add_named(&empty_box, Some("empty"));
    stack.add_named(&paned, Some("list"));
    stack.set_visible_child_name("empty");

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.append(&breadcrumb_bar);
    root_box.append(&stack);
    window.set_child(Some(&root_box));

    // Shared directory navigation helper keeping ColumnView, breadcrumbs, history, and left tree in sync
    let navigate_to: NavFn = {
        let state_ref = state.clone();
        let crumbs_box = crumbs_box.clone();
        let up_btn = up_btn.clone();
        let filter_ref = filter.clone();
        let search_ref = search_entry.clone();
        let selection_ref = selection.clone();
        let tree_view_ref = tree_view.clone();
        let tree_model_ref = tree_model.clone();
        let tree_sel_ref = tree_selection.clone();

        Rc::new_cyclic(|nav_weak| {
            let nav_weak = nav_weak.clone();
            Box::new(move |target_dir: String, sync_tree: bool, mode: NavMode| {
                {
                    let mut st = state_ref.borrow_mut();
                    match mode {
                        NavMode::Push => {
                            if st.current_dir != target_dir {
                                let prev = st.current_dir.clone();
                                st.history_back.push(prev);
                                st.history_forward.clear();
                            }
                        }
                        NavMode::History => {}
                        NavMode::Reset => {
                            st.history_back.clear();
                            st.history_forward.clear();
                        }
                    }
                    st.current_dir = target_dir.clone();
                    st.search_query.clear();
                }
                if !search_ref.text().is_empty() {
                    search_ref.set_text("");
                }
                selection_ref.unselect_all();
                filter_ref.changed(gtk::FilterChange::Different);

                if sync_tree {
                    state_ref.borrow_mut().syncing_tree = true;
                    sync_tree_selection(&tree_view_ref, &tree_model_ref, &tree_sel_ref, &target_dir);
                    state_ref.borrow_mut().syncing_tree = false;
                }

                let archive_label = state_ref
                    .borrow()
                    .current_archive
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("Archive")
                    .to_string();
                render_breadcrumbs(&crumbs_box, &up_btn, &archive_label, &target_dir, &nav_weak);
            })
        })
    };

    // Thumb mouse buttons: Back (button 8) and Forward (button 9)
    {
        let thumb_click = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let state_ref = state.clone();
        let nav = navigate_to.clone();
        thumb_click.connect_pressed(move |gesture, _, _, _| {
            let btn = gesture.current_button();
            if btn != 8 && btn != 9 {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let target = {
                let mut st = state_ref.borrow_mut();
                if st.current_archive.is_none() {
                    return;
                }
                if btn == 8 {
                    st.history_back.pop().inspect(|_| {
                        let cur = st.current_dir.clone();
                        st.history_forward.push(cur);
                    })
                } else {
                    st.history_forward.pop().inspect(|_| {
                        let cur = st.current_dir.clone();
                        st.history_back.push(cur);
                    })
                }
            };
            if let Some(dir) = target {
                nav(dir, true, NavMode::History);
            }
        });
        window.add_controller(thumb_click);
    }

    // Left tree selection listener
    {
        let state_ref = state.clone();
        let nav = navigate_to.clone();
        tree_selection.connect_selected_item_notify(move |sel| {
            if state_ref.borrow().syncing_tree {
                return;
            }
            if let Some(item) = sel.selected_item()
                && let Ok(list_row) = item.downcast::<gtk::TreeListRow>()
                && let Some(node) = list_row.item().and_downcast::<FolderTreeNode>()
            {
                let target = node.full_path();
                if state_ref.borrow().current_dir != target {
                    nav(target, false, NavMode::Push);
                }
            }
        });
    }

    // Up button action
    {
        let state_ref = state.clone();
        let nav = navigate_to.clone();
        up_btn.connect_clicked(move |_| {
            let next = {
                let st = state_ref.borrow();
                if let Some((parent, _)) = st.current_dir.rsplit_once('/') {
                    parent.to_string()
                } else {
                    String::new()
                }
            };
            nav(next, true, NavMode::Push);
        });
    }

    // Search entry filtering
    {
        let state_ref = state.clone();
        let filter_ref = filter.clone();
        search_entry.connect_search_changed(move |entry| {
            state_ref.borrow_mut().search_query = entry.text().trim().to_ascii_lowercase();
            filter_ref.changed(gtk::FilterChange::Different);
        });
    }

    // Archive loader closure
    let open_archive: Rc<dyn Fn(PathBuf)> = {
        let window_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let tree_root_ref = tree_root_store.clone();
        let tree_model_ref = tree_model.clone();
        let stack_ref = stack.clone();
        let extract_ref = extract_btn.clone();
        let test_ref = test_btn.clone();
        let props_ref = props_btn.clone();
        let nav = navigate_to.clone();

        Rc::new(move |archive_path: PathBuf| {
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = cancel.clone();
            let path_worker = archive_path.clone();
            let initial_pw = {
                let st = state_ref.borrow();
                if st.current_archive.as_ref() == Some(&archive_path) {
                    st.cached_password.clone()
                } else {
                    None
                }
            };

            let rx = spawn_worker(move |tx| {
                let listing = list_archive(
                    &path_worker,
                    initial_pw.as_deref(),
                    Some(tx),
                    &cancel_worker,
                )?;
                Ok(OperationSummary::Listed(listing))
            });

            let win_done = window_ref.clone();
            let st_done = state_ref.clone();
            let store_done = store_ref.clone();
            let tree_root_done = tree_root_ref.clone();
            let tree_model_done = tree_model_ref.clone();
            let stack_done = stack_ref.clone();
            let extract_done = extract_ref.clone();
            let test_done = test_ref.clone();
            let props_done = props_ref.clone();
            let nav_done = nav.clone();
            let path_done = archive_path.clone();

            run_worker_with_progress(
                None,
                Some(window_ref.upcast_ref()),
                &format!(
                    "Opening {}",
                    archive_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("archive")
                ),
                rx,
                cancel,
                move |res| match res {
                    Ok(OperationSummary::Listed(listing)) => {
                        let rows = build_row_objects(&listing.entries);
                        let archive_name = path_done
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("Archive");
                        let root_node = build_folder_tree(archive_name, &listing.entries);
                        let entry_count = listing.entries.len();
                        let total_uncompressed: u64 =
                            listing.entries.iter().map(|e| e.size).sum();
                        let encrypted_count =
                            listing.entries.iter().filter(|e| e.is_encrypted).count();

                        store_done.remove_all();
                        store_done.extend_from_slice(&rows);
                        tree_root_done.remove_all();
                        tree_root_done.append(&root_node);
                        if let Some(first_row) = tree_model_done.row(0) {
                            first_row.set_expanded(true);
                        }

                        let (target_dir, nav_mode) = {
                            let mut st = st_done.borrow_mut();
                            let same_archive = st.current_archive.as_ref() == Some(&path_done);
                            st.current_archive = Some(path_done.clone());
                            st.cached_password = listing.password;
                            st.format_name = listing.format_name;
                            st.is_raw = listing.is_raw;
                            st.entry_count = entry_count;
                            st.total_uncompressed = total_uncompressed;
                            st.encrypted_count = encrypted_count;
                            if !same_archive {
                                st.current_dir.clear();
                            }
                            let mode = if same_archive {
                                NavMode::History
                            } else {
                                NavMode::Reset
                            };
                            (st.current_dir.clone(), mode)
                        };

                        win_done.set_title(Some(&format!("{archive_name} - rarc")));
                        extract_done.set_sensitive(true);
                        test_done.set_sensitive(true);
                        props_done.set_sensitive(true);
                        stack_done.set_visible_child_name("list");
                        nav_done(target_dir, true, nav_mode);
                    }
                    Ok(_) | Err(ArchiveError::Cancelled) => {}
                    Err(err) => {
                        show_error_dialog(
                            Some(&win_done),
                            "Unable to Open Archive",
                            &err.to_string(),
                        );
                    }
                },
            );
        })
    };

    // HeaderBar: Open button
    {
        let win_ref = window.clone();
        let open_fn = open_archive.clone();
        open_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Open Archive").build();
            let open_fn = open_fn.clone();
            dialog.open(Some(&win_ref), gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res
                    && let Some(path) = file.path()
                {
                    open_fn(path);
                }
            });
        });
    }

    // HeaderBar: Create button
    {
        let win_ref = window.clone();
        let open_fn = open_archive.clone();
        new_btn.connect_clicked(move |_| {
            let open_fn = open_fn.clone();
            show_create_dialog(
                None,
                Some(win_ref.upcast_ref()),
                Vec::new(),
                None,
                move |created| {
                    if let Some(path) = created {
                        open_fn(path);
                    }
                },
            );
        });
    }

    // HeaderBar: Extract button
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        extract_btn.connect_clicked(move |_| {
            start_extract_dialog(&win_ref, &state_ref, &store_ref, &selection_ref, false);
        });
    }

    // HeaderBar: Test Integrity button
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        test_btn.connect_clicked(move |_| {
            let (Some(archive_path), cached_pw, total_uncompressed, is_raw) = ({
                let st = state_ref.borrow();
                (
                    st.current_archive.clone(),
                    st.cached_password.clone(),
                    st.total_uncompressed,
                    st.is_raw,
                )
            }) else {
                return;
            };
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = cancel.clone();
            let arch_worker = archive_path.clone();

            let rx = spawn_worker(move |tx| {
                let (entries_checked, bytes_checked, pw) = test_archive(
                    &arch_worker,
                    cached_pw.as_deref(),
                    Some((total_uncompressed, is_raw)),
                    Some(tx),
                    &cancel_worker,
                )?;
                Ok(OperationSummary::Tested {
                    entries_checked,
                    bytes_checked,
                    password: pw,
                })
            });

            let win_done = win_ref.clone();
            let state_done = state_ref.clone();
            run_worker_with_progress(
                None,
                Some(win_ref.upcast_ref()),
                "Testing Archive Integrity",
                rx,
                cancel,
                move |res| match res {
                    Ok(OperationSummary::Tested {
                        entries_checked,
                        bytes_checked,
                        password,
                    }) => {
                        if let Some(pw) = password {
                            state_done.borrow_mut().cached_password = Some(pw);
                        }
                        show_info_dialog(
                            Some(&win_done),
                            "Integrity Check Passed",
                            &format!(
                                "The archive is intact.\nVerified {entries_checked} entries ({}) with 0 errors.",
                                format_size(bytes_checked)
                            ),
                        );
                    }
                    Ok(_) | Err(ArchiveError::Cancelled) => {}
                    Err(err) => {
                        show_error_dialog(
                            Some(&win_done),
                            "Integrity Check Failed",
                            &err.to_string(),
                        );
                    }
                },
            );
        });
    }

    // HeaderBar: Archive Properties button
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        props_btn.connect_clicked(move |_| {
            if let Some(info) = archive_properties_info(&state_ref.borrow()) {
                show_properties_dialog(win_ref.upcast_ref(), &[], &info);
            }
        });
    }

    // Context menu action handlers
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let nav = navigate_to.clone();
        act_open.connect_activate(move |_, _| {
            let rows = collect_selected_rows(&selection_ref);
            let Some(row) = rows.first() else {
                return;
            };
            activate_row(&win_ref, &state_ref, row, false, &nav);
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let nav = navigate_to.clone();
        act_open_with.connect_activate(move |_, _| {
            let rows = collect_selected_rows(&selection_ref);
            let Some(row) = rows.first() else {
                return;
            };
            if !row.with_data(|d| d.is_dir) {
                activate_row(&win_ref, &state_ref, row, true, &nav);
            }
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        act_extract.connect_activate(move |_, _| {
            start_extract_dialog(&win_ref, &state_ref, &store_ref, &selection_ref, true);
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        act_copy.connect_activate(move |_, _| {
            copy_selected_to_clipboard(&win_ref, &state_ref, &store_ref, &selection_ref);
        });
    }
    {
        let win_ref = window.clone();
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        act_props.connect_activate(move |_, _| {
            let Some(info) = archive_properties_info(&state_ref.borrow()) else {
                return;
            };
            let selected: Vec<PropertyEntry> = collect_selected_rows(&selection_ref)
                .into_iter()
                .map(|r| r.to_property_entry())
                .collect();
            if !selected.is_empty() {
                show_properties_dialog(win_ref.upcast_ref(), &selected, &info);
            }
        });
    }

    // Double-click activation: enter directory or extract & launch file
    {
        let state_ref = state.clone();
        let selection_ref = selection.clone();
        let nav = navigate_to.clone();
        let win_ref = window.clone();

        column_view.connect_activate(move |_, pos| {
            let Some(item) = selection_ref.item(pos) else {
                return;
            };
            let Ok(row) = item.downcast::<ArchiveRowObject>() else {
                return;
            };
            activate_row(&win_ref, &state_ref, &row, false, &nav);
        });
    }

    // Drag-and-Drop IN: drop files from Thunar into the window
    {
        let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let stack_enter = stack.clone();
        drop_target.connect_enter(move |_, _, _| {
            stack_enter.add_css_class("rarc-drop-active");
            gdk::DragAction::COPY
        });
        let stack_leave = stack.clone();
        drop_target.connect_leave(move |_| {
            stack_leave.remove_css_class("rarc-drop-active");
        });

        let stack_drop = stack.clone();
        let state_ref = state.clone();
        let win_ref = window.clone();
        let open_fn = open_archive.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            stack_drop.remove_css_class("rarc-drop-active");
            let Ok(file_list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<PathBuf> = file_list
                .files()
                .into_iter()
                .filter_map(|f| f.path())
                .collect();
            if paths.is_empty() {
                return false;
            }

            let open_archive_path = state_ref.borrow().current_archive.clone();
            if let Some(arch_path) = open_archive_path {
                let (dest_subdir, cached_pw) = {
                    let st = state_ref.borrow();
                    (st.current_dir.clone(), st.cached_password.clone())
                };
                let cancel = Arc::new(AtomicBool::new(false));
                let cancel_worker = cancel.clone();
                let arch_worker = arch_path.clone();
                let rx = spawn_worker(move |tx| {
                    let (_, password) = add_to_archive(
                        &arch_worker,
                        &paths,
                        &dest_subdir,
                        cached_pw.as_deref(),
                        Some(tx),
                        &cancel_worker,
                    )?;
                    Ok(OperationSummary::Added {
                        path: arch_worker,
                        password,
                    })
                });
                let win_err = win_ref.clone();
                let st_added = state_ref.clone();
                let reload = open_fn.clone();
                run_worker_with_progress(
                    None,
                    Some(win_ref.upcast_ref()),
                    "Adding Files to Archive",
                    rx,
                    cancel,
                    move |res| match res {
                        Ok(OperationSummary::Added { path, password }) => {
                            if let Some(pw) = password {
                                st_added.borrow_mut().cached_password = Some(pw);
                            }
                            reload(path);
                        }
                        Ok(_) | Err(ArchiveError::Cancelled) => {}
                        Err(err) => {
                            show_error_dialog(
                                Some(&win_err),
                                "Failed to Add Files",
                                &err.to_string(),
                            );
                        }
                    },
                );
                true
            } else if paths.len() == 1
                && paths[0].is_file()
                && is_supported_archive_path(&paths[0])
            {
                open_fn(paths[0].clone());
                true
            } else {
                let open_after = open_fn.clone();
                show_create_dialog(
                    None,
                    Some(win_ref.upcast_ref()),
                    paths,
                    None,
                    move |created| {
                        if let Some(path) = created {
                            open_after(path);
                        }
                    },
                );
                true
            }
        });
        stack.add_controller(drop_target);
    }

    // Drag-and-Drop OUT: drag selected entries to Thunar / folders
    {
        let drag_source = gtk::DragSource::builder()
            .actions(gdk::DragAction::COPY)
            .build();
        let state_ref = state.clone();
        let store_ref = store.clone();
        let selection_ref = selection.clone();
        let win_ref = window.clone();
        drag_source.connect_prepare(move |_, _, _| {
            let st = state_ref.borrow();
            let arch = st.current_archive.as_ref()?;
            let selected = collect_selected_paths(&selection_ref);
            if selected.is_empty() {
                return None;
            }
            let total_bytes = compute_selected_bytes(&store_ref, Some(&selected));
            let strip = st.active_strip_prefix();
            let provider = ArchiveDragProvider::new(
                arch.clone(),
                selected,
                strip,
                st.cached_password.clone(),
                st.is_raw,
                total_bytes,
                Rc::downgrade(&state_ref),
                win_ref.upcast_ref(),
            );
            Some(provider.upcast())
        });
        column_view.add_controller(drag_source);
    }

    window.present();
    if let Some(path) = initial_archive {
        open_archive(path);
    }
}

fn archive_properties_info(st: &ViewerState) -> Option<ArchivePropertiesInfo> {
    st.current_archive.as_ref().map(|p| ArchivePropertiesInfo {
        archive_path: p.clone(),
        format_name: st.format_name.clone(),
        entry_count: st.entry_count,
        total_uncompressed: st.total_uncompressed,
        encrypted_count: st.encrypted_count,
    })
}

fn build_context_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();

    let s1 = gio::Menu::new();
    s1.append(Some("Open"), Some("item.open"));
    s1.append(Some("Open With…"), Some("item.open-with"));
    menu.append_section(None, &s1);

    let s2 = gio::Menu::new();
    s2.append(Some("Extract Selected…"), Some("item.extract"));
    s2.append(Some("Copy"), Some("item.copy"));
    menu.append_section(None, &s2);

    let s3 = gio::Menu::new();
    s3.append(Some("Properties"), Some("item.properties"));
    menu.append_section(None, &s3);

    menu
}

fn activate_row(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    row: &ArchiveRowObject,
    always_ask: bool,
    navigate_to: &NavFn,
) {
    let (is_dir, is_symlink, full_path, name, parent_dir, entry_size) = row.with_data(|d| {
        (
            d.is_dir,
            d.symlink_target.is_some() || d.type_label == "Symbolic Link",
            d.full_path.clone(),
            d.name.clone(),
            d.parent_dir.clone(),
            d.size,
        )
    });

    if is_dir {
        navigate_to(full_path, true, NavMode::Push);
        return;
    }
    if is_symlink {
        show_info_dialog(
            Some(window),
            "Cannot Open Symbolic Link",
            "Symbolic links inside archives cannot be opened directly.",
        );
        return;
    }

    let (archive_path, seq, cached_pw, is_raw) = {
        let mut st = state.borrow_mut();
        let Some(arch) = st.current_archive.clone() else {
            return;
        };
        st.stage_seq += 1;
        (arch, st.stage_seq, st.cached_password.clone(), st.is_raw)
    };
    let Ok(temp_root) = session_temp_dir() else {
        return;
    };
    let preview_dir = temp_root.join(format!("preview-{seq}"));
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();
    let target_file = preview_dir.join(&name);
    let strip = if parent_dir.is_empty() {
        None
    } else {
        Some(parent_dir)
    };

    let rx = spawn_worker(move |tx| {
        let mut session = ExtractSession::new(cached_pw, entry_size);
        extract_archive_with_session(
            &archive_path,
            &preview_dir,
            Some(&[full_path]),
            strip.as_deref(),
            Some(is_raw),
            &mut session,
            Some(tx),
            &cancel_worker,
        )?;
        Ok(OperationSummary::Extracted {
            password: session.password,
        })
    });

    let win_launch = window.clone();
    let state_pw = state.clone();
    run_worker_with_progress(
        None,
        Some(window.upcast_ref()),
        &format!("Opening {name}"),
        rx,
        cancel,
        move |res| match res {
            Ok(OperationSummary::Extracted { password }) => {
                if let Some(pw) = password {
                    state_pw.borrow_mut().cached_password = Some(pw);
                }
                if target_file.exists() {
                    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&target_file)));
                    launcher.set_always_ask(always_ask);
                    let win_err = win_launch.clone();
                    launcher.launch(Some(&win_launch), gio::Cancellable::NONE, move |lres| {
                        if let Err(e) = lres
                            && !is_user_dismissed_error(&e)
                        {
                            show_error_dialog(
                                Some(&win_err),
                                "Unable to Launch File",
                                e.message(),
                            );
                        }
                    });
                }
            }
            Ok(_) | Err(ArchiveError::Cancelled) => {}
            Err(err) => {
                show_error_dialog(Some(&win_launch), "Preview Failed", &err.to_string());
            }
        },
    );
}

fn is_user_dismissed_error(err: &glib::Error) -> bool {
    err.matches(gtk::DialogError::Dismissed)
        || err.matches(gtk::DialogError::Cancelled)
        || err.matches(gio::IOErrorEnum::Cancelled)
        || err.message().to_ascii_lowercase().contains("dismissed")
}

fn start_extract_dialog(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
    require_selection: bool,
) {
    let Some(archive_path) = state.borrow().current_archive.clone() else {
        return;
    };
    let selected = collect_selected_paths(selection);
    if require_selection && selected.is_empty() {
        return;
    }
    let selected_opt = if selected.is_empty() {
        None
    } else {
        Some(selected)
    };
    let (strip_prefix, cached_pw, is_raw, total_bytes) = {
        let st = state.borrow();
        let strip = if selected_opt.is_some() {
            st.active_strip_prefix()
        } else {
            None
        };
        let bytes = if selected_opt.is_some() {
            compute_selected_bytes(store, selected_opt.as_deref())
        } else {
            st.total_uncompressed
        };
        (strip, st.cached_password.clone(), st.is_raw, bytes)
    };

    let initial_folder = archive_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(gio::File::for_path);
    let mut builder = gtk::FileDialog::builder().title("Extract To Folder");
    if let Some(ref f) = initial_folder {
        builder = builder.initial_folder(f);
    }
    let dialog = builder.build();

    let win_extract = window.clone();
    let state_done = state.clone();
    dialog.select_folder(Some(window), gio::Cancellable::NONE, move |res| {
        let Ok(folder) = res else { return };
        let Some(dest_dir) = folder.path() else { return };

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = cancel.clone();
        let arch_worker = archive_path.clone();
        let dest_worker = dest_dir.clone();
        let rx = spawn_worker(move |tx| {
            let mut session = ExtractSession::new(cached_pw, total_bytes);
            extract_archive_with_session(
                &arch_worker,
                &dest_worker,
                selected_opt.as_deref(),
                strip_prefix.as_deref(),
                Some(is_raw),
                &mut session,
                Some(tx),
                &cancel_worker,
            )?;
            Ok(OperationSummary::Extracted {
                password: session.password,
            })
        });

        let win_err = win_extract.clone();
        run_worker_with_progress(
            None,
            Some(win_extract.upcast_ref()),
            "Extracting Archive",
            rx,
            cancel,
            move |res| match res {
                Ok(OperationSummary::Extracted { password }) => {
                    if let Some(pw) = password {
                        state_done.borrow_mut().cached_password = Some(pw);
                    }
                }
                Ok(_) | Err(ArchiveError::Cancelled) => {}
                Err(err) => {
                    show_error_dialog(Some(&win_err), "Extraction Failed", &err.to_string());
                }
            },
        );
    });
}

fn copy_selected_to_clipboard(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    store: &gio::ListStore,
    selection: &gtk::MultiSelection,
) {
    let Some(archive_path) = state.borrow().current_archive.clone() else {
        return;
    };
    let selected = collect_selected_paths(selection);
    if selected.is_empty() {
        return;
    }

    let (seq, strip_prefix, cached_pw, is_raw, total_bytes) = {
        let mut st = state.borrow_mut();
        st.stage_seq += 1;
        let strip = st.active_strip_prefix();
        let bytes = compute_selected_bytes(store, Some(&selected));
        (
            st.stage_seq,
            strip,
            st.cached_password.clone(),
            st.is_raw,
            bytes,
        )
    };
    let Ok(temp_root) = session_temp_dir() else {
        return;
    };
    let stage_dir = temp_root.join(format!("copy-{seq}"));
    let stage_worker = stage_dir.clone();
    let selected_worker = selected.clone();
    let strip_worker = strip_prefix.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = cancel.clone();

    let rx = spawn_worker(move |tx| {
        let mut session = ExtractSession::new(cached_pw, total_bytes);
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

    let win_done = window.clone();
    let state_done = state.clone();
    run_worker_with_progress(
        None,
        Some(window.upcast_ref()),
        "Copying to Clipboard",
        rx,
        cancel,
        move |res| match res {
            Ok(OperationSummary::Extracted { password }) => {
                if let Some(pw) = password {
                    state_done.borrow_mut().cached_password = Some(pw);
                }
                let gio_files = collect_staged_files(&stage_dir, &selected, strip_prefix.as_deref());
                if gio_files.is_empty() {
                    return;
                }
                let uri_lines: Vec<String> =
                    gio_files.iter().map(|f| f.uri().to_string()).collect();
                let uri_list = format!("{}\r\n", uri_lines.join("\r\n"));
                let gnome_copied = format!("copy\n{}", uri_lines.join("\n"));

                let file_list = gdk::FileList::from_array(&gio_files);
                let provider = gdk::ContentProvider::new_union(&[
                    gdk::ContentProvider::for_value(&file_list.to_value()),
                    gdk::ContentProvider::for_bytes(
                        "x-special/gnome-copied-files",
                        &glib::Bytes::from_owned(gnome_copied.into_bytes()),
                    ),
                    gdk::ContentProvider::for_bytes(
                        "text/uri-list",
                        &glib::Bytes::from_owned(uri_list.into_bytes()),
                    ),
                ]);

                if let Some(display) = gdk::Display::default() {
                    let _ = display.clipboard().set_content(Some(&provider));
                }
            }
            Ok(_) | Err(ArchiveError::Cancelled) => {}
            Err(err) => {
                show_error_dialog(Some(&win_done), "Copy Failed", &err.to_string());
            }
        },
    );
}

fn collect_staged_files(
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

fn render_breadcrumbs(
    crumbs_box: &gtk::Box,
    up_btn: &gtk::Button,
    archive_label: &str,
    current_dir: &str,
    nav_weak: &NavWeak,
) {
    while let Some(child) = crumbs_box.first_child() {
        crumbs_box.remove(&child);
    }
    up_btn.set_sensitive(!current_dir.is_empty());

    let mut segments: Vec<(String, String)> = vec![(archive_label.to_string(), String::new())];
    if !current_dir.is_empty() {
        let mut acc = String::new();
        for part in current_dir.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            segments.push((part.to_string(), acc.clone()));
        }
    }

    let last_idx = segments.len().saturating_sub(1);
    for (idx, (label, target_dir)) in segments.into_iter().enumerate() {
        if idx > 0 {
            let sep = gtk::Label::new(Some("/"));
            sep.add_css_class("rarc-muted");
            crumbs_box.append(&sep);
        }
        let btn = gtk::Button::with_label(&label);
        btn.add_css_class("flat");
        btn.add_css_class("rarc-crumb-btn");
        if idx == last_idx {
            btn.add_css_class("current");
        }
        let nav_weak = nav_weak.clone();
        btn.connect_clicked(move |_| {
            if let Some(nav) = nav_weak.upgrade() {
                nav(target_dir.clone(), true, NavMode::Push);
            }
        });
        crumbs_box.append(&btn);
    }
}

fn sync_tree_selection(
    tree_view: &gtk::ListView,
    tree_model: &gtk::TreeListModel,
    tree_selection: &gtk::SingleSelection,
    target_dir: &str,
) {
    // Expand ancestor rows top-down while advancing scan offset monotonically.
    if let Some(root_row) = tree_model.row(0) {
        root_row.set_expanded(true);
    }

    let mut target_idx = if target_dir.is_empty() { Some(0) } else { None };
    if !target_dir.is_empty() {
        let mut start = 1u32;
        let mut acc = String::new();
        for part in target_dir.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            let Some(idx) = find_tree_row_index_from(tree_model, start, &acc) else {
                break;
            };
            if acc == target_dir {
                target_idx = Some(idx);
                break;
            }
            if let Some(row) = tree_model.row(idx) {
                row.set_expanded(true);
            }
            start = idx + 1;
        }
    }

    if let Some(idx) = target_idx {
        tree_selection.set_selected(idx);
        tree_view.scroll_to(
            idx,
            gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
            None,
        );
    }
}

fn find_tree_row_index_from(
    tree_model: &gtk::TreeListModel,
    start: u32,
    target_dir: &str,
) -> Option<u32> {
    for idx in start..tree_model.n_items() {
        if let Some(row) = tree_model.row(idx)
            && let Some(node) = row.item().and_downcast::<FolderTreeNode>()
            && node.full_path() == target_dir
        {
            return Some(idx);
        }
    }
    None
}

fn build_folder_tree(archive_name: &str, entries: &[ArchiveEntry]) -> FolderTreeNode {
    let mut dir_children: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
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
                dir_children
                    .entry(parent)
                    .or_default()
                    .push((part.to_string(), acc.clone()));
            }
        }
    }

    fn populate_node(
        name: String,
        full_path: String,
        dir_children: &BTreeMap<String, Vec<(String, String)>>,
    ) -> FolderTreeNode {
        let node = FolderTreeNode::new(name, full_path.clone());
        if let Some(children) = dir_children.get(&full_path) {
            let mut sorted = children.clone();
            sorted.sort_by_key(|a| a.0.to_ascii_lowercase());
            let store = node.children();
            for (child_name, child_path) in sorted {
                store.append(&populate_node(child_name, child_path, dir_children));
            }
        }
        node
    }

    populate_node(archive_name.to_string(), String::new(), &dir_children)
}

fn collect_selected_rows(selection: &gtk::MultiSelection) -> Vec<ArchiveRowObject> {
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

fn collect_selected_paths(selection: &gtk::MultiSelection) -> Vec<String> {
    collect_selected_rows(selection)
        .into_iter()
        .map(|row| row.with_data(|d| d.full_path.clone()))
        .collect()
}

fn compute_selected_bytes(store: &gio::ListStore, selected_prefixes: Option<&[String]>) -> u64 {
    let mut total = 0u64;
    for idx in 0..store.n_items() {
        if let Some(obj) = store.item(idx)
            && let Some(row) = obj.downcast_ref::<ArchiveRowObject>()
        {
            row.with_data(|d| {
                if !d.is_dir && matches_selection(&d.full_path, selected_prefixes) {
                    total += d.size;
                }
            });
        }
    }
    total
}

fn build_row_objects(entries: &[ArchiveEntry]) -> Vec<ArchiveRowObject> {
    let mut rows = Vec::with_capacity(entries.len());
    let mut seen_paths: HashSet<String> = HashSet::with_capacity(entries.len());
    let mut queued_dirs: HashSet<String> = HashSet::new();

    for entry in entries {
        let is_dir = entry.kind == EntryKind::Directory;
        let (parent_dir, name) = split_parent_and_name(&entry.path);
        seen_paths.insert(entry.path.clone());

        // Ensure all ancestor virtual folders exist for folder navigation.
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

        rows.push(ArchiveRowObject::new(imp::ArchiveRowData {
            type_label: entry.kind.label(&name),
            name,
            full_path: entry.path.clone(),
            parent_dir,
            size: entry.size,
            compressed_size: entry.compressed_size,
            mtime: entry.mtime,
            mode: entry.mode,
            symlink_target: entry.symlink_target.clone(),
            is_dir,
            is_encrypted: entry.is_encrypted,
        }));
    }

    for dir_path in queued_dirs {
        if seen_paths.insert(dir_path.clone()) {
            let (parent_dir, name) = split_parent_and_name(&dir_path);
            rows.push(ArchiveRowObject::new(imp::ArchiveRowData {
                name,
                full_path: dir_path,
                parent_dir,
                size: 0,
                compressed_size: None,
                type_label: "Folder".to_string(),
                mtime: None,
                mode: 0o755,
                symlink_target: None,
                is_dir: true,
                is_encrypted: false,
            }));
        }
    }

    rows
}

fn split_parent_and_name(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

fn attach_cell_right_click(
    widget: &impl IsA<gtk::Widget>,
    on_right_click: CellRightClickFn,
) {
    let pos_cell: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
    let gesture = gtk::GestureClick::builder().button(3).build();
    let w_ref = widget.as_ref().clone();
    let pos_click = pos_cell.clone();
    gesture.connect_pressed(move |g, _, x, y| {
        if let Some(pos) = *pos_click.borrow() {
            g.set_state(gtk::EventSequenceState::Claimed);
            on_right_click(&w_ref, x, y, Some(pos));
        }
    });
    widget.as_ref().add_controller(gesture);
    unsafe {
        widget.as_ref().set_data("rarc-pos-cell", pos_cell);
    }
}

fn set_cell_bound_pos(item: &gtk::ListItem, pos: Option<u32>) {
    if let Some(child) = item.child()
        && let Some(pos_cell) = unsafe { child.data::<Rc<RefCell<Option<u32>>>>("rarc-pos-cell") }
    {
        *unsafe { pos_cell.as_ref() }.borrow_mut() = pos;
    }
}

fn append_columns(view: &gtk::ColumnView, on_right_click: &CellRightClickFn) {
    // Name column (icon + lock badge + label)
    let name_factory = gtk::SignalListItemFactory::new();
    {
        let on_rc = on_right_click.clone();
        name_factory.connect_setup(move |_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let icon = gtk::Image::new();
            let lock = gtk::Image::from_icon_name("channel-secure-symbolic");
            lock.set_visible(false);
            lock.set_tooltip_text(Some("Encrypted"));
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_hexpand(true);
            hbox.append(&icon);
            hbox.append(&label);
            hbox.append(&lock);

            attach_cell_right_click(&hbox, on_rc.clone());
            item.set_child(Some(&hbox));
        });
    }
    name_factory.connect_bind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let row = item.item().and_downcast::<ArchiveRowObject>().unwrap();
        let hbox = item.child().and_downcast::<gtk::Box>().unwrap();
        let icon = hbox.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
        let lock = label.next_sibling().and_downcast::<gtk::Image>().unwrap();
        set_cell_bound_pos(item, Some(item.position()));
        row.with_data(|d| {
            icon.set_icon_name(Some(if d.is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            label.set_text(&d.name);
            lock.set_visible(d.is_encrypted);
        });
    });
    name_factory.connect_unbind(|_, obj| {
        set_cell_bound_pos(obj.downcast_ref::<gtk::ListItem>().unwrap(), None);
    });

    let name_sorter = gtk::CustomSorter::new(|a, b| {
        let ra = a.downcast_ref::<ArchiveRowObject>().unwrap();
        let rb = b.downcast_ref::<ArchiveRowObject>().unwrap();
        ra.with_data(|da| {
            rb.with_data(|db| {
                db.is_dir
                    .cmp(&da.is_dir)
                    .then_with(|| da.name.to_ascii_lowercase().cmp(&db.name.to_ascii_lowercase()))
                    .into()
            })
        })
    });
    let name_col = gtk::ColumnViewColumn::builder()
        .title("Name")
        .factory(&name_factory)
        .sorter(&name_sorter)
        .expand(true)
        .resizable(true)
        .build();
    view.append_column(&name_col);
    view.sort_by_column(Some(&name_col), gtk::SortType::Ascending);

    // Uncompressed Size
    let size_col = make_text_column(
        "Uncompressed Size",
        1.0,
        true,
        130,
        on_right_click.clone(),
        |d| {
            if d.is_dir {
                "—".to_string()
            } else {
                format_size(d.size)
            }
        },
        |da, db| da.size.cmp(&db.size),
    );
    view.append_column(&size_col);

    // Type
    let type_col = make_text_column(
        "Type",
        0.0,
        false,
        120,
        on_right_click.clone(),
        |d| d.type_label.clone(),
        |da, db| da.type_label.cmp(&db.type_label),
    );
    view.append_column(&type_col);

    // Modified Date
    let mtime_col = make_text_column(
        "Modified Date",
        0.0,
        true,
        150,
        on_right_click.clone(),
        |d| format_mtime(d.mtime),
        |da, db| da.mtime.cmp(&db.mtime),
    );
    view.append_column(&mtime_col);
}

fn make_text_column<FText, FSorter>(
    title: &str,
    xalign: f32,
    numeric: bool,
    fixed_width: i32,
    on_right_click: CellRightClickFn,
    format_fn: FText,
    sort_fn: FSorter,
) -> gtk::ColumnViewColumn
where
    FText: Fn(&imp::ArchiveRowData) -> String + 'static,
    FSorter: Fn(&imp::ArchiveRowData, &imp::ArchiveRowData) -> std::cmp::Ordering + 'static,
{
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.set_xalign(xalign);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if numeric {
            label.add_css_class("rarc-numeric-cell");
        }
        attach_cell_right_click(&label, on_right_click.clone());
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let row = item.item().and_downcast::<ArchiveRowObject>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        set_cell_bound_pos(item, Some(item.position()));
        row.with_data(|d| label.set_text(&format_fn(d)));
    });
    factory.connect_unbind(|_, obj| {
        set_cell_bound_pos(obj.downcast_ref::<gtk::ListItem>().unwrap(), None);
    });

    let sorter = gtk::CustomSorter::new(move |a, b| {
        let ra = a.downcast_ref::<ArchiveRowObject>().unwrap();
        let rb = b.downcast_ref::<ArchiveRowObject>().unwrap();
        ra.with_data(|da| rb.with_data(|db| sort_fn(da, db).into()))
    });

    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(&sorter)
        .fixed_width(fixed_width)
        .resizable(true)
        .build()
}
