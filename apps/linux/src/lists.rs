//! The Lists window: every list on the left, the open one on the right,
//! where items are ticked, added, edited, moved, nested and shared.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use threnody_ffi::{ListInfo, ListItemInfo};

use crate::core::Node;
use crate::ui::{self, bg};
use crate::window::App;

/// A change to one item, run on the worker thread.
type ItemChange = Box<dyn Fn(&Node) -> Result<(), threnody_ffi::ThrenodyError> + Send + Sync>;
/// Something the list menu does.
type MenuAction = Box<dyn Fn(&Rc<ListsWindow>)>;

pub struct ListsWindow {
    app: Weak<App>,
    window: adw::Window,
    sidebar: gtk::ListBox,
    title: adw::WindowTitle,
    items: gtk::Box,
    entry: gtk::Entry,
    menu: gtk::MenuButton,
    lists: RefCell<Vec<ListInfo>>,
    open: RefCell<Option<String>>,
    weak_self: Weak<ListsWindow>,
}

/// Inline markdown as Pango markup: **bold**, *italic* or _italic_,
/// ~~struck~~, `code`.
fn markup(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    let marks: [(&str, &str, &str); 5] = [
        ("**", "<b>", "</b>"),
        ("~~", "<s>", "</s>"),
        ("`", "<tt>", "</tt>"),
        ("*", "<i>", "</i>"),
        ("_", "<i>", "</i>"),
    ];
    'outer: while !rest.is_empty() {
        for (m, open, close) in marks {
            if let Some(after) = rest.strip_prefix(m)
                && let Some(end) = after.find(m)
                && end > 0
            {
                out.push_str(open);
                out.push_str(&glib::markup_escape_text(&after[..end]));
                out.push_str(close);
                rest = &after[end + m.len()..];
                continue 'outer;
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        out.push_str(&glib::markup_escape_text(&c.to_string()));
        rest = &rest[c.len_utf8()..];
    }
    out
}

impl ListsWindow {
    pub fn new(app: &Rc<App>) -> Rc<Self> {
        let window = adw::Window::builder()
            .title("Lists")
            .default_width(820)
            .default_height(620)
            .transient_for(&app.window)
            .build();
        let sidebar = gtk::ListBox::new();
        sidebar.add_css_class("navigation-sidebar");
        let side_header = adw::HeaderBar::new();
        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("New list"));
        side_header.pack_start(&add);
        let side = adw::ToolbarView::new();
        side.add_top_bar(&side_header);
        side.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .child(&sidebar)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        ));

        let title = adw::WindowTitle::new("Lists", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));
        let menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("List options")
            .build();
        header.pack_end(&menu);
        let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
        items.set_margin_start(12);
        items.set_margin_end(12);
        items.set_margin_top(6);
        let entry = gtk::Entry::builder()
            .placeholder_text("Add an item (# for a heading)")
            .hexpand(true)
            .build();
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bottom.set_margin_start(12);
        bottom.set_margin_end(12);
        bottom.set_margin_top(6);
        bottom.set_margin_bottom(12);
        bottom.append(&entry);
        let main = adw::ToolbarView::new();
        main.add_top_bar(&header);
        main.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .child(&items)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        ));
        main.add_bottom_bar(&bottom);

        let split = adw::NavigationSplitView::new();
        split.set_sidebar(Some(&adw::NavigationPage::new(&side, "Lists")));
        split.set_content(Some(&adw::NavigationPage::new(&main, "List")));
        window.set_content(Some(&split));

        let this = Rc::new_cyclic(|weak_self| Self {
            app: Rc::downgrade(app),
            window,
            sidebar,
            title,
            items,
            entry,
            menu,
            lists: RefCell::new(Vec::new()),
            open: RefCell::new(None),
            weak_self: weak_self.clone(),
        });
        let weak = Rc::downgrade(&this);
        add.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.new_list();
            }
        });
        let weak = Rc::downgrade(&this);
        this.sidebar.connect_row_activated(move |_, row| {
            let Some(t) = weak.upgrade() else { return };
            let id = t
                .lists
                .borrow()
                .get(row.index() as usize)
                .map(|l| l.id.clone());
            if let Some(id) = id {
                t.show(&id);
            }
        });
        let weak = Rc::downgrade(&this);
        this.entry.connect_activate(move |e| {
            if let Some(t) = weak.upgrade() {
                t.add_item(e.text().trim());
                e.set_text("");
            }
        });
        this.reload();
        this
    }

    pub fn present(&self) {
        self.window.present();
    }

    fn node(&self) -> Option<Node> {
        self.app.upgrade().and_then(|a| a.core.node(None).ok())
    }

    /// Reloads every list (after a change, here or from elsewhere).
    pub fn reload(self: &Rc<Self>) {
        let Some(node) = self.node() else { return };
        let weak = self.weak_self.clone();
        bg(
            move || node.lists(),
            move |lists| {
                let Some(t) = weak.upgrade() else { return };
                while let Some(r) = t.sidebar.row_at_index(0) {
                    t.sidebar.remove(&r);
                }
                for l in &lists {
                    let todo: Vec<&ListItemInfo> = l.items.iter().filter(|i| !i.heading).collect();
                    let done = todo.iter().filter(|i| i.done).count();
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&l.title).as_str())
                        .subtitle(if todo.is_empty() {
                            "Empty".to_owned()
                        } else {
                            format!(
                                "{done} of {} done{}",
                                todo.len(),
                                if l.shared_with.is_empty() {
                                    ""
                                } else {
                                    " · shared"
                                }
                            )
                        })
                        .activatable(true)
                        .build();
                    t.sidebar.append(&row);
                }
                let open = t.open.borrow().clone();
                *t.lists.borrow_mut() = lists;
                match open.or_else(|| t.lists.borrow().first().map(|l| l.id.clone())) {
                    Some(id) => t.show(&id),
                    None => t.show_none(),
                }
            },
        );
    }

    fn show_none(&self) {
        *self.open.borrow_mut() = None;
        self.title.set_title("Lists");
        self.title.set_subtitle("");
        while let Some(c) = self.items.first_child() {
            self.items.remove(&c);
        }
        let page = adw::StatusPage::builder()
            .icon_name("view-list-bullet-symbolic")
            .title("No lists yet")
            .description("Shopping, gift ideas, packing… Lists stay on all your devices, and you can share one with a contact or a group so everyone can tick things off.")
            .vexpand(true)
            .build();
        self.items.append(&page);
        self.entry.set_sensitive(false);
        self.menu.set_sensitive(false);
    }

    /// Shows list `id`, opening it if it was in a chat.
    pub fn show(self: &Rc<Self>, id: &str) {
        let Some(l) = self.lists.borrow().iter().find(|l| l.id == id).cloned() else {
            return self.show_none();
        };
        *self.open.borrow_mut() = Some(l.id.clone());
        if let Some(i) = self.lists.borrow().iter().position(|x| x.id == id) {
            self.sidebar
                .select_row(self.sidebar.row_at_index(i as i32).as_ref());
        }
        self.entry.set_sensitive(true);
        self.menu.set_sensitive(true);
        self.title.set_title(&l.title);
        let todo: Vec<&ListItemInfo> = l.items.iter().filter(|i| !i.heading).collect();
        self.title.set_subtitle(&if todo.is_empty() {
            String::new()
        } else {
            format!(
                "{} of {} done",
                todo.iter().filter(|i| i.done).count(),
                todo.len()
            )
        });
        self.build_menu(&l);
        while let Some(c) = self.items.first_child() {
            self.items.remove(&c);
        }
        for (k, item) in l.items.iter().enumerate() {
            self.items.append(&self.row(&l, k, item));
        }
    }

    fn row(self: &Rc<Self>, l: &ListInfo, at: usize, item: &ListItemInfo) -> gtk::Widget {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        b.set_margin_start(24 * i32::from(item.indent));
        b.set_margin_top(if item.heading { 14 } else { 2 });
        if !item.heading {
            let check = gtk::CheckButton::new();
            check.set_active(item.done);
            let (weak, lid, iid) = (self.weak_self.clone(), l.id.clone(), item.id);
            check.connect_toggled(move |c| {
                let on = c.is_active();
                if let Some(t) = weak.upgrade() {
                    let lid = lid.clone();
                    t.change(move |n| n.set_list_item_done(lid, iid, on));
                }
            });
            b.append(&check);
        }
        let label = gtk::Label::new(None);
        label.set_markup(&if item.done {
            format!("<s>{}</s>", markup(&item.text))
        } else {
            markup(&item.text)
        });
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_wrap(true);
        if item.heading {
            label.add_css_class("heading");
        } else if item.done {
            label.add_css_class("dim-label");
        }
        // Click the text to edit it.
        let click = gtk::GestureClick::new();
        let (weak, lid, iid, text) = (
            self.weak_self.clone(),
            l.id.clone(),
            item.id,
            item.text.clone(),
        );
        click.connect_released(move |_, _, _, _| {
            let Some(t) = weak.upgrade() else { return };
            let Some(app) = t.app.upgrade() else { return };
            let (weak, lid) = (Rc::downgrade(&t), lid.clone());
            ui::ask_text(&t.window, "Edit", "", "Text", &text, "Save", move |s| {
                if let (Some(t), false) = (weak.upgrade(), s.is_empty()) {
                    let lid = lid.clone();
                    t.change(move |n| n.edit_list_item(lid, iid, s));
                }
            });
            let _ = app;
        });
        label.add_controller(click);
        b.append(&label);

        // Move, nest, delete.
        let more = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Item options")
            .build();
        more.add_css_class("flat");
        let pop = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let popover = gtk::Popover::new();
        let ids: Vec<u64> = l.items.iter().map(|i| i.id).collect();
        let add = |label: &str, f: ItemChange| {
            let btn = gtk::Button::with_label(label);
            btn.add_css_class("flat");
            let (weak, p) = (self.weak_self.clone(), popover.clone());
            let f = std::sync::Arc::new(f);
            btn.connect_clicked(move |_| {
                p.popdown();
                if let Some(t) = weak.upgrade() {
                    let f = f.clone();
                    t.change(move |n| f(n));
                }
            });
            pop.append(&btn);
        };
        let (lid, iid, indent) = (l.id.clone(), item.id, item.indent);
        if at > 0 {
            let (lid, to) = (lid.clone(), if at >= 2 { Some(ids[at - 2]) } else { None });
            add(
                "Move up",
                Box::new(move |n| n.move_list_item(lid.clone(), iid, to)),
            );
        }
        if at + 1 < ids.len() {
            let (lid, to) = (lid.clone(), ids[at + 1]);
            add(
                "Move down",
                Box::new(move |n| n.move_list_item(lid.clone(), iid, Some(to))),
            );
        }
        if !item.heading && indent < 3 {
            let lid = lid.clone();
            add(
                "Indent",
                Box::new(move |n| n.indent_list_item(lid.clone(), iid, indent + 1)),
            );
        }
        if indent > 0 {
            let lid = lid.clone();
            add(
                "Outdent",
                Box::new(move |n| n.indent_list_item(lid.clone(), iid, indent - 1)),
            );
        }
        add(
            "Delete",
            Box::new(move |n| n.delete_list_item(lid.clone(), iid)),
        );
        popover.set_child(Some(&pop));
        more.set_popover(Some(&popover));
        b.append(&more);
        b.upcast()
    }

    /// Runs a change on the worker thread, then reloads.
    fn change(
        self: &Rc<Self>,
        f: impl FnOnce(&Node) -> Result<(), threnody_ffi::ThrenodyError> + Send + 'static,
    ) {
        let Some(node) = self.node() else { return };
        let weak = self.weak_self.clone();
        bg(
            move || f(&node).map_err(|e| e.to_string()),
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                if let Err(e) = r
                    && let Some(app) = t.app.upgrade()
                {
                    app.toast(&format!("Couldn't change the list: {e}"));
                }
                t.reload();
            },
        );
    }

    fn add_item(self: &Rc<Self>, text: &str) {
        let Some(id) = self.open.borrow().clone() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let last = self
            .lists
            .borrow()
            .iter()
            .find(|l| l.id == id)
            .and_then(|l| l.items.last().map(|i| i.id));
        let heading = text.starts_with('#');
        let text = text.trim_start_matches('#').trim().to_owned();
        self.change(move |n| n.add_list_item(id, last, text, heading).map(|_| ()));
    }

    fn new_list(self: &Rc<Self>) {
        let weak = self.weak_self.clone();
        ui::ask_text(
            &self.window,
            "New list",
            "Groceries, Christmas ideas…",
            "Name",
            "",
            "Create",
            move |title| {
                let Some(t) = weak.upgrade() else { return };
                if title.is_empty() {
                    return;
                }
                let Some(node) = t.node() else { return };
                let weak = Rc::downgrade(&t);
                bg(
                    move || node.create_list(title),
                    move |r| {
                        let Some(t) = weak.upgrade() else { return };
                        if let Ok(id) = r {
                            *t.open.borrow_mut() = Some(id);
                        }
                        t.reload();
                    },
                );
            },
        );
    }

    fn build_menu(self: &Rc<Self>, l: &ListInfo) {
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let item = |label: &str, f: MenuAction| {
            let btn = gtk::Button::with_label(label);
            btn.add_css_class("flat");
            let (weak, p) = (self.weak_self.clone(), pop.clone());
            btn.connect_clicked(move |_| {
                p.popdown();
                if let Some(t) = weak.upgrade() {
                    f(&t);
                }
            });
            b.append(&btn);
        };
        let l2 = l.clone();
        item(
            "Rename…",
            Box::new(move |t| {
                let (weak, id) = (Rc::downgrade(t), l2.id.clone());
                ui::ask_text(
                    &t.window,
                    "Rename",
                    "",
                    "Name",
                    &l2.title,
                    "Save",
                    move |s| {
                        if let (Some(t), false) = (weak.upgrade(), s.is_empty()) {
                            let id = id.clone();
                            t.change(move |n| n.rename_list(id, s));
                        }
                    },
                );
            }),
        );
        let l2 = l.clone();
        item("Share…", Box::new(move |t| t.share(&l2)));
        let id = l.id.clone();
        item(
            "Paste a markdown list…",
            Box::new(move |t| {
                let (weak, id) = (Rc::downgrade(t), id.clone());
                t.window
                    .clipboard()
                    .read_text_async(gtk::gio::Cancellable::NONE, move |r| {
                        let text = r.ok().flatten().map(|s| s.to_string()).unwrap_or_default();
                        let Some(t) = weak.upgrade() else { return };
                        if text.trim().is_empty() {
                            if let Some(app) = t.app.upgrade() {
                                app.toast("Copy a markdown list first (- [ ] item, # heading).");
                            }
                            return;
                        }
                        let id = id.clone();
                        t.change(move |n| n.add_list_markdown(id, text).map(|_| ()));
                    });
            }),
        );
        let id = l.id.clone();
        item(
            "Copy as markdown",
            Box::new(move |t| {
                let Some(node) = t.node() else { return };
                if let Ok(md) = node.list_markdown(id.clone()) {
                    t.window.clipboard().set_text(&md);
                    if let Some(app) = t.app.upgrade() {
                        app.toast("Copied as markdown");
                    }
                }
            }),
        );
        let id = l.id.clone();
        item(
            "Clear ticked items",
            Box::new(move |t| {
                let id = id.clone();
                t.change(move |n| n.clear_done_items(id).map(|_| ()));
            }),
        );
        let (id, title) = (l.id.clone(), l.title.clone());
        item(
            "Delete list…",
            Box::new(move |t| {
                let (weak, id) = (Rc::downgrade(t), id.clone());
                ui::confirm(
                    &t.window,
                    &format!("Delete \"{title}\"?"),
                    "It goes from all your devices. Anyone you shared it with keeps their copy.",
                    "Delete",
                    true,
                    move || {
                        if let Some(t) = weak.upgrade() {
                            *t.open.borrow_mut() = None;
                            let id = id.clone();
                            t.change(move |n| n.delete_list(id));
                        }
                    },
                );
            }),
        );
        pop.set_child(Some(&b));
        self.menu.set_popover(Some(&pop));
    }

    fn share(self: &Rc<Self>, l: &ListInfo) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let chats: Vec<(String, String)> = app
            .core
            .conversations()
            .into_iter()
            .filter(|c| c.chat.persona.is_none() && !c.key.is_empty() && c.key != "notes")
            .filter(|c| !l.shared_with.contains(&c.key))
            .map(|c| (c.key, c.title))
            .collect();
        if chats.is_empty() {
            app.toast("It's shared with every chat already.");
            return;
        }
        let names: Vec<String> = chats.iter().map(|(_, t)| t.clone()).collect();
        let (weak, id) = (self.weak_self.clone(), l.id.clone());
        ui::choose(
            &self.window,
            &format!("Share \"{}\"", l.title),
            "Everyone in the chat can change it.",
            &names,
            None,
            move |i| {
                let Some(t) = weak.upgrade() else { return };
                let (id, key) = (id.clone(), chats[i].0.clone());
                t.change(move |n| n.share_list(id, key));
            },
        );
    }
}

impl Drop for ListsWindow {
    fn drop(&mut self) {
        self.window.destroy();
    }
}
