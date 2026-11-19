use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gio;
use gtk::prelude::*;

use crate::archive::{ArchiveEntry, EntryKind};
use crate::ui::{format_mtime, format_size};

use super::models::{cmp_ascii_ignore_case, path_basename, ArchiveRowObject};

pub(super) type CellRightClickFn = Rc<dyn Fn(&gtk::Widget, f64, f64, Option<u32>)>;

struct BoundRowPos(Rc<RefCell<Option<u32>>>);

pub(super) fn build_context_menu_model() -> gio::Menu {
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

fn attach_cell_right_click(
    widget: &impl IsA<gtk::Widget>,
    on_right_click: CellRightClickFn,
) {
    let pos_cell = Rc::new(RefCell::new(None));
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
        widget
            .as_ref()
            .set_data("rarc-pos-cell", BoundRowPos(pos_cell));
    }
}

fn set_cell_bound_pos(item: &gtk::ListItem, pos: Option<u32>) {
    if let Some(child) = item.child()
        && let Some(bound) = unsafe { child.data::<BoundRowPos>("rarc-pos-cell") }
    {
        *unsafe { bound.as_ref() }.0.borrow_mut() = pos;
    }
}

pub(super) fn append_columns(view: &gtk::ColumnView, on_right_click: &CellRightClickFn) {
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
        icon.set_icon_name(Some(if row.is_dir() {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        }));
        label.set_text(row.name());
        lock.set_visible(row.entry_ref().is_encrypted);
    });
    name_factory.connect_unbind(|_, obj| {
        set_cell_bound_pos(obj.downcast_ref::<gtk::ListItem>().unwrap(), None);
    });

    let name_sorter = gtk::CustomSorter::new(|a, b| {
        let ra = a.downcast_ref::<ArchiveRowObject>().unwrap();
        let rb = b.downcast_ref::<ArchiveRowObject>().unwrap();
        rb.is_dir()
            .cmp(&ra.is_dir())
            .then_with(|| cmp_ascii_ignore_case(ra.name(), rb.name()))
            .into()
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
        |e| {
            if e.kind == EntryKind::Directory {
                Cow::Borrowed("—")
            } else {
                Cow::Owned(format_size(e.size))
            }
        },
        |ea, eb| ea.size.cmp(&eb.size),
    );
    view.append_column(&size_col);

    // Type
    let type_col = make_text_column(
        "Type",
        0.0,
        false,
        120,
        on_right_click.clone(),
        |e| e.kind.label_cow(path_basename(&e.path)),
        |ea, eb| {
            ea.kind
                .label_cow(path_basename(&ea.path))
                .cmp(&eb.kind.label_cow(path_basename(&eb.path)))
        },
    );
    view.append_column(&type_col);

    // Modified Date
    let mtime_col = make_text_column(
        "Modified Date",
        0.0,
        true,
        150,
        on_right_click.clone(),
        |e| {
            if e.mtime.is_none() {
                Cow::Borrowed("—")
            } else {
                Cow::Owned(format_mtime(e.mtime))
            }
        },
        |ea, eb| ea.mtime.cmp(&eb.mtime),
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
    FText: Fn(&ArchiveEntry) -> Cow<'static, str> + 'static,
    FSorter: Fn(&ArchiveEntry, &ArchiveEntry) -> std::cmp::Ordering + 'static,
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
        label.set_text(&format_fn(row.entry_ref()));
    });
    factory.connect_unbind(|_, obj| {
        set_cell_bound_pos(obj.downcast_ref::<gtk::ListItem>().unwrap(), None);
    });

    let sorter = gtk::CustomSorter::new(move |a, b| {
        let ra = a.downcast_ref::<ArchiveRowObject>().unwrap();
        let rb = b.downcast_ref::<ArchiveRowObject>().unwrap();
        sort_fn(ra.entry_ref(), rb.entry_ref()).into()
    });

    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(&sorter)
        .fixed_width(fixed_width)
        .resizable(true)
        .build()
}
