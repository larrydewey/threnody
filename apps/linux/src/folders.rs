//! The folder editor: a folder's name and emoji, its chats and rules,
//! when it notifies, its quiet hours, and who may break through them.

use adw::prelude::*;
use threnody_ffi::{FolderInfo, NotifyLevel, QuietSlotInfo};

pub const RULE_CONTACTS: u32 = 1;
pub const RULE_GROUPS: u32 = 2;
pub const RULE_VERIFIED: u32 = 4;
pub const RULE_UNREAD: u32 = 8;
pub const RULE_MENTIONS: u32 = 16;
const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const LEVELS: [(&str, Option<NotifyLevel>); 4] = [
    ("Each chat's own setting", None),
    ("All messages", Some(NotifyLevel::All)),
    (
        "Only mentions and replies to me",
        Some(NotifyLevel::Mentions),
    ),
    ("Off", Some(NotifyLevel::Off)),
];

fn heading(t: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(t));
    l.add_css_class("heading");
    l.set_xalign(0.0);
    l.set_margin_top(10);
    l
}

/// One quiet slot's row: day toggles, from and to (hours and minutes),
/// and a remove button.
struct SlotRow {
    root: gtk::Box,
    days: Vec<gtk::ToggleButton>,
    from: (gtk::SpinButton, gtk::SpinButton),
    to: (gtk::SpinButton, gtk::SpinButton),
}

impl SlotRow {
    fn new(s: &QuietSlotInfo) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        line.add_css_class("linked");
        let days: Vec<gtk::ToggleButton> = DAYS
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let b = gtk::ToggleButton::with_label(d);
                b.set_active(s.days & (1 << i) != 0);
                line.append(&b);
                b
            })
            .collect();
        root.append(&line);
        let time = |m: u16| {
            let h = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
            h.set_value(f64::from(m / 60 % 24));
            let mm = gtk::SpinButton::with_range(0.0, 59.0, 5.0);
            mm.set_value(f64::from(m % 60));
            (h, mm)
        };
        let from = time(s.start);
        let to = time(s.end);
        let times = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        times.append(&gtk::Label::new(Some("From")));
        times.append(&from.0);
        times.append(&gtk::Label::new(Some(":")));
        times.append(&from.1);
        times.append(&gtk::Label::new(Some("to")));
        times.append(&to.0);
        times.append(&gtk::Label::new(Some(":")));
        times.append(&to.1);
        root.append(&times);
        Self {
            root,
            days,
            from,
            to,
        }
    }

    fn value(&self) -> QuietSlotInfo {
        let days = self
            .days
            .iter()
            .enumerate()
            .filter(|(_, b)| b.is_active())
            .fold(0u8, |acc, (i, _)| acc | (1 << i));
        let m = |t: &(gtk::SpinButton, gtk::SpinButton)| {
            (t.0.value_as_int() * 60 + t.1.value_as_int()).clamp(0, 1439) as u16
        };
        QuietSlotInfo {
            days,
            start: m(&self.from),
            end: m(&self.to),
        }
    }
}

/// Shows the editor; `save` gets the folder (id 0 if new). `chats` is
/// every chat that can go in: key and title.
pub fn edit(
    parent: &impl IsA<gtk::Widget>,
    folder: Option<FolderInfo>,
    chats: Vec<(String, String)>,
    save: impl Fn(FolderInfo) + 'static,
) {
    let d = adw::AlertDialog::new(
        Some(if folder.is_some() {
            "Edit folder"
        } else {
            "New folder"
        }),
        None,
    );
    d.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    d.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let emoji = gtk::Entry::builder()
        .placeholder_text("🙂")
        .text(folder.as_ref().map_or("", |f| f.emoji.as_str()))
        .max_width_chars(3)
        .width_chars(3)
        .build();
    let name = gtk::Entry::builder()
        .placeholder_text("Name (Family, Work…)")
        .text(folder.as_ref().map_or("", |f| f.name.as_str()))
        .hexpand(true)
        .build();
    top.append(&emoji);
    top.append(&name);
    form.append(&top);

    form.append(&heading("Chats"));
    let picked = folder.as_ref().map(|f| f.chats.clone()).unwrap_or_default();
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let checks: Vec<(String, gtk::CheckButton)> = chats
        .iter()
        .map(|(k, t)| {
            let c = gtk::CheckButton::with_label(t);
            c.set_active(picked.contains(k));
            list.append(&c);
            (k.clone(), c)
        })
        .collect();
    form.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .max_content_height(180)
            .propagate_natural_height(true)
            .build(),
    );

    let rules = folder.as_ref().map_or(0, |f| f.rules);
    let rule = |label: &str, bit: u32| {
        let c = gtk::CheckButton::with_label(label);
        c.set_active(rules & bit != 0);
        (bit, c)
    };
    form.append(&heading("Also include"));
    let rule_checks = vec![
        rule("All contacts", RULE_CONTACTS),
        rule("All groups", RULE_GROUPS),
        rule("Verified contacts", RULE_VERIFIED),
    ];
    for (_, c) in &rule_checks {
        form.append(c);
    }
    form.append(&heading("Only show"));
    let keep_checks = vec![
        rule("Chats with unread messages", RULE_UNREAD),
        rule("Chats that mention me", RULE_MENTIONS),
    ];
    for (_, c) in &keep_checks {
        form.append(c);
    }

    form.append(&heading("Notifications"));
    let notify = gtk::DropDown::from_strings(&LEVELS.map(|(l, _)| l));
    let current = folder.as_ref().and_then(|f| f.notify);
    notify.set_selected(LEVELS.iter().position(|(_, n)| *n == current).unwrap_or(0) as u32);
    form.append(&notify);

    form.append(&heading("Quiet hours"));
    let note = gtk::Label::new(Some("Messages still arrive; they just don't notify."));
    note.add_css_class("dim-label");
    note.set_xalign(0.0);
    form.append(&note);
    let slots_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let slots: std::rc::Rc<std::cell::RefCell<Vec<SlotRow>>> = Default::default();
    let add_slot = {
        let (slots, slots_box) = (slots.clone(), slots_box.clone());
        move |s: &QuietSlotInfo| {
            let row = SlotRow::new(s);
            let remove = gtk::Button::with_label("Remove");
            remove.add_css_class("flat");
            remove.set_halign(gtk::Align::Start);
            let (slots2, box2, root) = (slots.clone(), slots_box.clone(), row.root.clone());
            remove.connect_clicked(move |_| {
                box2.remove(&root);
                slots2.borrow_mut().retain(|r| r.root != root);
            });
            row.root.append(&remove);
            slots_box.append(&row.root);
            slots.borrow_mut().push(row);
        }
    };
    for s in folder.as_ref().map(|f| f.quiet.clone()).unwrap_or_default() {
        add_slot(&s);
    }
    form.append(&slots_box);
    let more = gtk::Button::with_label("Add quiet hours");
    more.set_halign(gtk::Align::Start);
    more.connect_clicked(move |_| {
        add_slot(&QuietSlotInfo {
            days: 0x7F,
            start: 22 * 60,
            end: 7 * 60,
        })
    });
    form.append(&more);

    form.append(&heading("Let messages through"));
    let through =
        gtk::CheckButton::with_label("When someone writes to me directly again and again");
    let count = gtk::SpinButton::with_range(1.0, 20.0, 1.0);
    let window = gtk::SpinButton::with_range(1.0, 1440.0, 1.0);
    match &folder {
        Some(f) if f.breakthrough_count > 0 => {
            through.set_active(true);
            count.set_value(f64::from(f.breakthrough_count));
            window.set_value(f64::from(f.breakthrough_window_min));
        }
        _ => {
            count.set_value(2.0);
            window.set_value(10.0);
        }
    }
    form.append(&through);
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    line.append(&count);
    line.append(&gtk::Label::new(Some("messages within")));
    line.append(&window);
    line.append(&gtk::Label::new(Some("minutes")));
    form.append(&line);
    let hint = gtk::Label::new(Some(
        "Directly: in a chat with you, or mentioning or replying to you in a group. A chat you turned off stays off.",
    ));
    hint.add_css_class("dim-label");
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    form.append(&hint);

    d.set_extra_child(Some(
        &gtk::ScrolledWindow::builder()
            .child(&form)
            .min_content_height(420)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build(),
    ));
    let id = folder.as_ref().map_or(0, |f| f.id);
    d.connect_response(None, move |_, r| {
        if r != "save" {
            return;
        }
        let rules = rule_checks
            .iter()
            .chain(&keep_checks)
            .filter(|(_, c)| c.is_active())
            .fold(0, |acc, (b, _)| acc | b);
        save(FolderInfo {
            id,
            name: name.text().trim().to_owned(),
            emoji: emoji.text().trim().to_owned(),
            chats: checks
                .iter()
                .filter(|(_, c)| c.is_active())
                .map(|(k, _)| k.clone())
                .collect(),
            rules,
            notify: LEVELS[notify.selected() as usize].1,
            quiet: slots
                .borrow()
                .iter()
                .map(SlotRow::value)
                .filter(|s| s.days != 0)
                .collect(),
            breakthrough_count: if through.is_active() {
                count.value_as_int() as u8
            } else {
                0
            },
            breakthrough_window_min: window.value_as_int() as u16,
        });
    });
    d.present(Some(parent));
}
