//! Cards (Appendix V): a contact, a Wi-Fi network, a place, an event or a
//! secret sent as a message. Drawing one in a bubble, its actions, and
//! the dialogs that make one.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use threnody_ffi::{CardInfo, CardItemInfo, SecretFieldInfo};

use crate::ui;

/// A secret's field names as people read them.
pub fn field_label(name: &str) -> String {
    match name {
        "username" => "Username".into(),
        "password" => "Password".into(),
        "url" => "Website".into(),
        "otp" => "One-time code".into(),
        "number" => "Card number".into(),
        "holder" => "Name on card".into(),
        "expiry" => "Expires".into(),
        "cvc" => "Security code".into(),
        "pin" => "PIN".into(),
        "text" => "Note".into(),
        other => {
            let mut c = other.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    }
}

/// What kind of card it is, as a heading.
pub fn kind_label(c: &CardInfo) -> &'static str {
    match &c.item {
        CardItemInfo::Contact { .. } => "👤 Contact",
        CardItemInfo::Wifi { .. } => "📶 Wi-Fi network",
        CardItemInfo::Location { .. } => "📍 Location",
        CardItemInfo::Event { .. } => "📅 Event",
        CardItemInfo::Secret { kind, .. } => match kind.as_str() {
            "login" => "🔑 Login",
            "card" => "💳 Payment card",
            "note" => "🔒 Secure note",
            _ => "🔒 Secret",
        },
    }
}

fn degrees(e7: i32) -> String {
    format!("{:.6}", f64::from(e7) / 1e7)
}

/// When an event is, in local time.
pub fn when_text(start_ms: u64, end_ms: u64, all_day: bool) -> String {
    let local = |ms: u64| {
        glib::DateTime::from_unix_local(i64::try_from(ms / 1000).unwrap_or(0))
            .or_else(|_| glib::DateTime::now_local())
            .ok()
    };
    let date = |ms: u64| {
        local(ms)
            .and_then(|d| d.format("%a %e %b %Y").ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    let time = |ms: u64| {
        local(ms)
            .and_then(|d| d.format("%H:%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    if all_day {
        // An all-day event is its date, at midnight UTC.
        let utc = |ms: u64| {
            glib::DateTime::from_unix_utc(i64::try_from(ms / 1000).unwrap_or(0))
                .ok()
                .and_then(|d| d.format("%a %e %b %Y").ok())
                .map(|s| s.trim().to_owned())
                .unwrap_or_default()
        };
        return if end_ms > start_ms {
            format!("{} – {}", utc(start_ms), utc(end_ms))
        } else {
            utc(start_ms)
        };
    }
    let start = format!("{}, {}", date(start_ms), time(start_ms));
    if end_ms <= start_ms {
        start
    } else if date(start_ms) == date(end_ms) {
        format!("{start} – {}", time(end_ms))
    } else {
        format!("{start} – {}, {}", date(end_ms), time(end_ms))
    }
}

/// A covered value: dots, but a card's number keeps its last four digits.
fn covered(f: &SecretFieldInfo) -> String {
    match f.name.as_str() {
        "number" => {
            let digits: String = f.value.chars().filter(char::is_ascii_digit).collect();
            format!("•••• {}", &digits[digits.len().saturating_sub(4)..])
        }
        "cvc" | "pin" => "•".repeat(f.value.chars().count().clamp(3, 6)),
        _ => "••••••••".into(),
    }
}

fn field<'a>(fields: &'a [SecretFieldInfo], name: &str) -> &'a str {
    fields
        .iter()
        .find(|f| f.name == name)
        .map_or("", |f| f.value.as_str())
}

/// A payment card drawn as one: the ID-1 shape, its network's colours, a
/// chip, the number in groups, the holder and the expiry.
#[derive(Clone)]
pub struct PaymentCard {
    pub root: gtk::Box,
    title: gtk::Label,
    brand: gtk::Label,
    number: gtk::Label,
    holder: gtk::Label,
    expiry: gtk::Label,
}

impl PaymentCard {
    pub fn new(width: i32) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("payment-card");
        root.set_size_request(width, (width as f32 / 1.586) as i32);
        root.set_halign(gtk::Align::Start);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let title = ui::label("", &["heading"]);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let brand = ui::label("", &["card-brand"]);
        top.append(&title);
        top.append(&brand);
        root.append(&top);
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        root.append(&spacer);
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        chip.add_css_class("card-chip");
        chip.set_halign(gtk::Align::Start);
        chip.set_margin_bottom(8);
        root.append(&chip);
        let number = ui::label("", &["card-number"]);
        root.append(&number);
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        root.append(&spacer);
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let col = |caption: &str, end: bool| {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let c = ui::label(caption, &["card-caption"]);
            let v = ui::label("", &["heading"]);
            if end {
                c.set_xalign(1.0);
                v.set_xalign(1.0);
            }
            b.append(&c);
            b.append(&v);
            (b, v)
        };
        let (hb, holder) = col("CARDHOLDER", false);
        hb.set_hexpand(true);
        let (eb, expiry) = col("EXPIRES", true);
        bottom.append(&hb);
        bottom.append(&eb);
        root.append(&bottom);
        let card = Self {
            root,
            title,
            brand,
            number,
            holder,
            expiry,
        };
        card.show("", "", "", "", "");
        card
    }

    /// Draws it: `digits` decide its network, `shown` is what the number
    /// line says.
    pub fn show(&self, title: &str, digits: &str, shown: &str, holder: &str, expiry: &str) {
        let brand = threnody_ffi::card_brand(digits.to_owned());
        for b in BRANDS {
            self.root.remove_css_class(b.0);
        }
        if !brand.is_empty() {
            self.root.add_css_class(&brand);
        }
        self.title.set_text(title);
        self.brand
            .set_text(BRANDS.iter().find(|b| b.0 == brand).map_or("", |b| b.1));
        self.number.set_text(if shown.is_empty() {
            "•••• •••• •••• ••••"
        } else {
            shown
        });
        self.holder.set_text(&if holder.is_empty() {
            "—".into()
        } else {
            holder.to_uppercase()
        });
        self.expiry
            .set_text(if expiry.is_empty() { "MM/YY" } else { expiry });
    }

    /// "•••• •••• •••• 1111", or the number in groups when `revealed`.
    pub fn shown_number(digits: &str, revealed: bool) -> String {
        let d: String = digits.chars().filter(char::is_ascii_digit).collect();
        if revealed || d.is_empty() {
            threnody_ffi::format_card_number(d)
        } else {
            format!("•••• •••• •••• {}", &d[d.len().saturating_sub(4)..])
        }
    }

    fn from_secret(width: i32, title: &str, fields: &[SecretFieldInfo], revealed: bool) -> Self {
        let card = Self::new(width);
        let digits = field(fields, "number");
        card.show(
            title,
            digits,
            &Self::shown_number(digits, revealed),
            field(fields, "holder"),
            field(fields, "expiry"),
        );
        card
    }
}

const BRANDS: [(&str, &str); 7] = [
    ("visa", "VISA"),
    ("mastercard", "mastercard"),
    ("amex", "AMEX"),
    ("discover", "DISCOVER"),
    ("diners", "Diners Club"),
    ("jcb", "JCB"),
    ("unionpay", "UnionPay"),
];

/// Copies `text`; a `secret` one is marked for password managers'
/// clipboard tools to leave out of their history.
pub fn copy(w: &impl IsA<gtk::Widget>, text: &str, secret: bool) {
    let clip = w.as_ref().clipboard();
    if secret {
        let content = gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_value(&text.to_value()),
            gdk::ContentProvider::for_bytes(
                "x-kde-passwordManagerHint",
                &glib::Bytes::from_static(b"secret"),
            ),
        ]);
        if clip.set_content(Some(&content)).is_ok() {
            return;
        }
    }
    clip.set_text(text);
}

/// What a bubble's card can do, handed in by the chat.
pub struct Actions {
    /// Adds the person a contact card names (its invite link or handle);
    /// None where adding isn't offered.
    pub add: Option<Box<dyn Fn(String)>>,
    /// Shows it in full: a secret's fields, or a view-once card (which
    /// opens it for good).
    pub open: Box<dyn Fn()>,
}

/// The kind's icon, from the icon theme like the app's other icons.
fn kind_icon(c: &CardInfo) -> &'static str {
    match &c.item {
        CardItemInfo::Contact { .. } => "avatar-default-symbolic",
        CardItemInfo::Wifi { .. } => "network-wireless-symbolic",
        CardItemInfo::Location { .. } => "mark-location-symbolic",
        CardItemInfo::Event { .. } => "x-office-calendar-symbolic",
        CardItemInfo::Secret { kind, .. } => match kind.as_str() {
            "login" => "dialog-password-symbolic",
            "card" => "auth-smartcard-symbolic",
            _ => "changes-prevent-symbolic",
        },
    }
}

/// The card in a bubble: a header with its kind and what it is, its
/// details, and buttons for what can be done with it. A payment card is
/// drawn as one.
pub fn view(
    window: &adw::ApplicationWindow,
    c: &CardInfo,
    outgoing: bool,
    act: Actions,
) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
    b.add_css_class("card-box");
    let sealed = c.view_once && !c.opened;
    let title = match &c.item {
        CardItemInfo::Contact { name, .. } => name.clone(),
        CardItemInfo::Wifi { ssid, .. } => ssid.clone(),
        CardItemInfo::Location {
            label,
            latitude_e7,
            longitude_e7,
            ..
        } => {
            if label.is_empty() {
                format!("{}, {}", degrees(*latitude_e7), degrees(*longitude_e7))
            } else {
                label.clone()
            }
        }
        CardItemInfo::Event { title, .. } => title.clone(),
        CardItemInfo::Secret {
            kind,
            title,
            fields,
            ..
        } => {
            let digits: String = field(fields, "number")
                .chars()
                .filter(char::is_ascii_digit)
                .collect();
            if kind == "card" && !digits.is_empty() {
                format!("Ending {}", &digits[digits.len().saturating_sub(4)..])
            } else {
                title.clone()
            }
        }
    };
    let heading = if sealed {
        if outgoing {
            "Sent to view once".into()
        } else {
            "Open it to see it".into()
        }
    } else if c.opened && !matches!(c.item, CardItemInfo::Secret { .. }) {
        if outgoing {
            "Sent".into()
        } else {
            "Opened".into()
        }
    } else {
        title
    };
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = gtk::Image::from_icon_name(if sealed {
        "view-reveal-symbolic"
    } else {
        kind_icon(c)
    });
    icon.add_css_class("card-icon");
    icon.set_valign(gtk::Align::Center);
    header.append(&icon);
    let words = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let kind = kind_label(c).split_once(' ').map_or("", |(_, k)| k);
    words.append(&ui::label(
        &if sealed {
            format!("View once · {kind}")
        } else {
            kind.to_owned()
        },
        &["caption", "dim-label"],
    ));
    let h = ui::label(&heading, &["heading"]);
    h.set_wrap(true);
    h.set_max_width_chars(40);
    words.append(&h);
    header.append(&words);
    b.append(&header);
    let line = |text: &str, classes: &[&str]| {
        if !text.trim().is_empty() {
            let l = ui::label(text, classes);
            l.set_selectable(true);
            l.set_wrap(true);
            l.set_max_width_chars(50);
            b.append(&l);
        }
    };
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_margin_top(4);
    let act = Rc::new(act);
    let button = |label: &str, f: Box<dyn Fn()>| {
        let btn = gtk::Button::with_label(label);
        btn.add_css_class("flat");
        btn.add_css_class("card-action");
        btn.connect_clicked(move |_| f());
        buttons.append(&btn);
    };
    if sealed {
        if !outgoing {
            let a = act.clone();
            button("Open", Box::new(move || (a.open)()));
        }
    } else if c.opened {
        line(
            if outgoing {
                "What it held isn't kept on this device."
            } else {
                "It was viewed once and is gone."
            },
            &["caption", "dim-label"],
        );
    } else {
        match &c.item {
            CardItemInfo::Contact {
                invite,
                handle,
                phones,
                emails,
                address,
                note,
                ..
            } => {
                for p in phones {
                    line(&format!("📞  {p}"), &[]);
                }
                for e in emails {
                    line(&format!("✉️  {e}"), &[]);
                }
                line(address, &["dim-label"]);
                line(handle, &["dim-label"]);
                line(note, &["dim-label"]);
                let link = if invite.is_empty() { handle } else { invite };
                if !outgoing && !link.is_empty() && act.add.is_some() {
                    let (a, link) = (act.clone(), link.clone());
                    button(
                        "Add on Threnody",
                        Box::new(move || {
                            if let Some(add) = &a.add {
                                add(link.clone());
                            }
                        }),
                    );
                }
                let (w, card) = (window.clone(), c.item.clone());
                button("Save", Box::new(move || save_vcard(&w, &card)));
            }
            CardItemInfo::Wifi {
                ssid,
                password,
                security,
                hidden,
            } => {
                let mut what = vec![match security.as_str() {
                    "open" => "Open network",
                    "wep" => "WEP",
                    _ => "WPA/WPA2/WPA3",
                }];
                if *hidden {
                    what.push("hidden");
                }
                line(&what.join(" · "), &["caption", "dim-label"]);
                let (w, s) = (window.clone(), ssid.clone());
                button("Copy name", Box::new(move || copy(&w, &s, false)));
                if !password.is_empty() {
                    let (w, p) = (window.clone(), password.clone());
                    button("Copy password", Box::new(move || copy(&w, &p, true)));
                }
            }
            CardItemInfo::Location {
                latitude_e7,
                longitude_e7,
                label,
                accuracy_m,
            } => {
                if !label.is_empty() {
                    line(
                        &format!("{}, {}", degrees(*latitude_e7), degrees(*longitude_e7)),
                        &["caption", "dim-label", "monospace"],
                    );
                }
                if *accuracy_m > 0 {
                    line(&format!("Within {accuracy_m} m"), &["caption", "dim-label"]);
                }
                let (w, lat, lon) = (
                    window.clone(),
                    degrees(*latitude_e7),
                    degrees(*longitude_e7),
                );
                button(
                    "Open in maps",
                    Box::new(move || {
                        // A map site sees the place, so it is the user's to choose.
                        ui::confirm_link(
                            &w,
                            &format!(
                                "https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=17/{lat}/{lon}"
                            ),
                        );
                    }),
                );
            }
            CardItemInfo::Event {
                start_ms,
                end_ms,
                all_day,
                place,
                note,
                ..
            } => {
                line(
                    &format!("🕒  {}", when_text(*start_ms, *end_ms, *all_day)),
                    &[],
                );
                if !place.is_empty() {
                    line(&format!("📍  {place}"), &[]);
                }
                line(note, &["dim-label"]);
                let (w, card) = (window.clone(), c.item.clone());
                button("Add to calendar", Box::new(move || save_ics(&w, &card)));
            }
            CardItemInfo::Secret {
                kind,
                title,
                fields,
                ..
            } => {
                if kind == "card" {
                    b.append(&PaymentCard::from_secret(280, title, fields, false).root);
                } else {
                    for f in fields.iter().take(4) {
                        let v = if f.hidden {
                            covered(f)
                        } else {
                            f.value.clone()
                        };
                        line(&format!("{}  {v}", field_label(&f.name)), &[]);
                    }
                }
                let a = act.clone();
                button("Show", Box::new(move || (a.open)()));
            }
        }
    }
    if buttons.first_child().is_some() {
        b.append(&buttons);
    }
    b
}

/// A card in full, in a dialog: a secret's fields with *Reveal* and
/// *Copy*, or a view-once card's contents.
pub fn show(window: &adw::ApplicationWindow, c: &CardInfo, add: Option<Box<dyn Fn(String)>>) {
    let CardItemInfo::Secret { title, fields, .. } = &c.item else {
        let d = ui::alert(kind_label(c), "", &[("done", "Done")]);
        let plain = CardInfo {
            view_once: false,
            ..c.clone()
        };
        d.set_extra_child(Some(&view(
            window,
            &plain,
            false,
            Actions {
                add,
                open: Box::new(|| {}),
            },
        )));
        d.present(Some(window));
        return;
    };
    let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let is_card = matches!(&c.item, CardItemInfo::Secret { kind, .. } if kind == "card");
    if is_card {
        // The card itself; Reveal shows the whole number on it.
        let card = PaymentCard::from_secret(320, title, fields, false);
        card.root.set_halign(gtk::Align::Center);
        card.root.set_margin_bottom(8);
        body.append(&card.root);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_halign(gtk::Align::Center);
        let reveal = gtk::Button::with_label("Reveal number");
        reveal.add_css_class("flat");
        let digits = field(fields, "number").to_owned();
        let (holder, expiry, t) = (
            field(fields, "holder").to_owned(),
            field(fields, "expiry").to_owned(),
            title.clone(),
        );
        let shown = std::cell::Cell::new(false);
        let (c2, d2) = (card.clone(), digits.clone());
        reveal.connect_clicked(move |btn| {
            shown.set(!shown.get());
            c2.show(
                &t,
                &d2,
                &PaymentCard::shown_number(&d2, shown.get()),
                &holder,
                &expiry,
            );
            btn.set_label(if shown.get() {
                "Hide number"
            } else {
                "Reveal number"
            });
        });
        let copy_number = gtk::Button::with_label("Copy number");
        copy_number.add_css_class("flat");
        copy_number.connect_clicked(move |btn| {
            copy(
                btn,
                &digits
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>(),
                true,
            );
            btn.set_label("Copied");
        });
        row.append(&reveal);
        row.append(&copy_number);
        body.append(&row);
    }
    for f in fields
        .iter()
        .filter(|f| !is_card || !matches!(f.name.as_str(), "number" | "holder" | "expiry"))
    {
        body.append(&ui::label(&field_label(&f.name), &["caption", "dim-label"]));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let value = ui::label(
            &if f.hidden {
                covered(f)
            } else {
                f.value.clone()
            },
            &[],
        );
        value.set_hexpand(true);
        value.set_wrap(true);
        if matches!(
            f.name.as_str(),
            "number" | "password" | "cvc" | "pin" | "otp"
        ) {
            value.add_css_class("monospace");
        }
        row.append(&value);
        if f.hidden {
            let reveal = gtk::Button::with_label("Reveal");
            reveal.add_css_class("flat");
            let (v, f2) = (value.clone(), f.clone());
            reveal.connect_clicked(move |btn| {
                let shown = v.text() == f2.value;
                v.set_text(&if shown {
                    covered(&f2)
                } else {
                    f2.value.clone()
                });
                btn.set_label(if shown { "Reveal" } else { "Hide" });
            });
            row.append(&reveal);
        }
        let copy_btn = gtk::Button::with_label("Copy");
        copy_btn.add_css_class("flat");
        let (val, secret) = (f.value.clone(), f.hidden || f.name != "url");
        copy_btn.connect_clicked(move |btn| {
            copy(btn, &val, secret);
            btn.set_label("Copied");
        });
        row.append(&copy_btn);
        body.append(&row);
    }
    let note = if c.view_once {
        "Viewed once: it's gone from this chat when you close this."
    } else {
        ""
    };
    let d = ui::alert(title, note, &[("done", "Done")]);
    d.set_extra_child(Some(&body));
    d.present(Some(window));
}

/// Writes `text` to a new file called `name` in the cache and opens it
/// with the desktop's app for it (contacts, calendar).
fn open_file(window: &adw::ApplicationWindow, name: &str, text: &str) {
    let dir = glib::user_cache_dir().join("threnody-cards");
    let path = dir.join(name);
    let written = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, text));
    if written.is_err() {
        return;
    }
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
    let p = path.clone();
    launcher.launch(Some(window), gio::Cancellable::NONE, move |_| {
        // The app read it (or there is none): don't leave it lying around.
        glib::timeout_add_seconds_local_once(60, move || {
            let _ = std::fs::remove_file(&p);
        });
    });
}

/// vCard and iCalendar text values escape these.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

fn save_vcard(window: &adw::ApplicationWindow, item: &CardItemInfo) {
    let CardItemInfo::Contact {
        name,
        phones,
        emails,
        address,
        note,
        ..
    } = item
    else {
        return;
    };
    let mut v = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{}\r\nN:{};;;;\r\n",
        escape(name),
        escape(name)
    );
    for p in phones {
        v += &format!("TEL:{}\r\n", escape(p));
    }
    for e in emails {
        v += &format!("EMAIL:{}\r\n", escape(e));
    }
    if !address.is_empty() {
        v += &format!("ADR:;;{};;;;\r\n", escape(address));
    }
    if !note.is_empty() {
        v += &format!("NOTE:{}\r\n", escape(note));
    }
    v += "END:VCARD\r\n";
    open_file(window, "contact.vcf", &v);
}

fn save_ics(window: &adw::ApplicationWindow, item: &CardItemInfo) {
    let CardItemInfo::Event {
        title,
        start_ms,
        end_ms,
        all_day,
        place,
        note,
    } = item
    else {
        return;
    };
    let utc = |ms: u64, fmt: &str| {
        glib::DateTime::from_unix_utc(i64::try_from(ms / 1000).unwrap_or(0))
            .ok()
            .and_then(|d| d.format(fmt).ok())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    let (start, end) = if *all_day {
        let end = if *end_ms > *start_ms {
            *end_ms
        } else {
            *start_ms
        } + 86_400_000;
        (
            format!("DTSTART;VALUE=DATE:{}", utc(*start_ms, "%Y%m%d")),
            format!("DTEND;VALUE=DATE:{}", utc(end, "%Y%m%d")),
        )
    } else {
        let end = if *end_ms > *start_ms {
            *end_ms
        } else {
            *start_ms + 3_600_000
        };
        (
            format!("DTSTART:{}", utc(*start_ms, "%Y%m%dT%H%M%SZ")),
            format!("DTEND:{}", utc(end, "%Y%m%dT%H%M%SZ")),
        )
    };
    let uid = format!("{start_ms}-{}@threnody", glib::random_int());
    let mut v = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Threnody//Cards//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{}\r\n{start}\r\n{end}\r\nSUMMARY:{}\r\n",
        utc(glib::real_time() as u64 / 1000, "%Y%m%dT%H%M%SZ"),
        escape(title)
    );
    if !place.is_empty() {
        v += &format!("LOCATION:{}\r\n", escape(place));
    }
    if !note.is_empty() {
        v += &format!("DESCRIPTION:{}\r\n", escape(note));
    }
    v += "END:VEVENT\r\nEND:VCALENDAR\r\n";
    open_file(window, "event.ics", &v);
}

// ---- Making cards ----

fn entry(hint: &str, text: &str) -> gtk::Entry {
    let e = gtk::Entry::builder().placeholder_text(hint).build();
    e.set_text(text);
    e
}

fn ask(
    window: &adw::ApplicationWindow,
    title: &str,
    body: &str,
    form: &gtk::Box,
    make: impl Fn() -> Option<CardInfo> + 'static,
    send: impl Fn(CardInfo) + 'static,
) {
    let d = ui::alert(title, body, &[("cancel", "Cancel"), ("send", "Send")]);
    d.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    d.set_extra_child(Some(form));
    let w = window.clone();
    d.connect_response(Some("send"), move |_, _| match make() {
        Some(c) => send(c),
        None => {
            let e = ui::alert("Not sent", "Fill in what it needs first.", &[("ok", "OK")]);
            e.present(Some(&w));
        }
    });
    d.present(Some(window));
}

fn card(item: CardItemInfo, view_once: bool, expires_in_s: u32) -> CardInfo {
    CardInfo {
        item,
        view_once,
        no_forward: false,
        opened: false,
        expires_in_s,
    }
}

/// A contact: ourselves (`invite` our link, `handle` ours, if any) or
/// someone typed in.
pub fn compose_contact(
    window: &adw::ApplicationWindow,
    name: &str,
    invite: String,
    handle: String,
    send: impl Fn(CardInfo) + 'static,
) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let (n, p, e, a, note) = (
        entry("Name", name),
        entry("Phone", ""),
        entry("Email", ""),
        entry("Address", ""),
        entry("Note", ""),
    );
    for w in [&n, &p, &e, &a, &note] {
        form.append(w);
    }
    let body = if handle.is_empty() && invite.is_empty() {
        String::new()
    } else if handle.is_empty() {
        "With your Threnody invite link, so they can add you.".into()
    } else {
        format!("With your Threnody handle {handle}, so they can add you.")
    };
    ask(
        window,
        "Share a contact",
        &body,
        &form,
        move || {
            let one = |w: &gtk::Entry| {
                let t = w.text().trim().to_owned();
                if t.is_empty() { vec![] } else { vec![t] }
            };
            let item = CardItemInfo::Contact {
                name: n.text().trim().to_owned(),
                invite: invite.clone(),
                handle: handle.clone(),
                phones: one(&p),
                emails: one(&e),
                address: a.text().trim().to_owned(),
                note: note.text().trim().to_owned(),
            };
            let c = card(item, false, 0);
            (!threnody_ffi::card_text(c.clone()).is_empty()).then_some(c)
        },
        send,
    );
}

pub fn compose_wifi(window: &adw::ApplicationWindow, send: impl Fn(CardInfo) + 'static) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let ssid = entry("Network name", "");
    let pw = gtk::PasswordEntry::builder()
        .placeholder_text("Password")
        .show_peek_icon(true)
        .build();
    let security = gtk::DropDown::from_strings(&["WPA/WPA2/WPA3", "Open", "WEP"]);
    let hidden = gtk::CheckButton::with_label("Hidden network");
    form.append(&ssid);
    form.append(&pw);
    form.append(&security);
    form.append(&hidden);
    ask(
        window,
        "Share a Wi-Fi network",
        "",
        &form,
        move || {
            let sec = match security.selected() {
                1 => "open",
                2 => "wep",
                _ => "wpa",
            };
            let name = ssid.text().trim().to_owned();
            (!name.is_empty()).then(|| {
                card(
                    CardItemInfo::Wifi {
                        ssid: name,
                        password: if sec == "open" {
                            String::new()
                        } else {
                            pw.text().to_string()
                        },
                        security: sec.into(),
                        hidden: hidden.is_active(),
                    },
                    false,
                    0,
                )
            })
        },
        send,
    );
}

/// Degrees × 10⁷ from "lat, lon", a `geo:` URI, or a map link with
/// `mlat=`/`mlon=`, `@lat,lon` or `q=lat,lon` in it.
pub fn parse_place(s: &str) -> Option<(i32, i32)> {
    let s = s.trim();
    let num = |t: &str| t.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    let pair = |t: &str| -> Option<(f64, f64)> {
        let mut it = t.split(',');
        let lat = num(it.next()?)?;
        let lon = num(it.next()?.split(['?', ';', '&', '/', 'z']).next()?)?;
        Some((lat, lon))
    };
    let found = if let Some(rest) = s.strip_prefix("geo:") {
        pair(rest)
    } else if let (Some(a), Some(b)) = (s.find("mlat="), s.find("mlon=")) {
        let val = |i: usize| num(s[i + 5..].split(['&', '#']).next().unwrap_or(""));
        val(a).zip(val(b))
    } else if let Some(i) = s.find('@') {
        pair(&s[i + 1..])
    } else if let Some(i) = s.find("q=") {
        pair(s[i + 2..].split('&').next().unwrap_or(""))
    } else {
        pair(s)
    };
    let (lat, lon) = found?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then(|| ((lat * 1e7).round() as i32, (lon * 1e7).round() as i32))
}

pub fn compose_location(window: &adw::ApplicationWindow, send: impl Fn(CardInfo) + 'static) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let place = entry("Coordinates or a map link", "");
    let label = entry("Label (optional)", "");
    form.append(&place);
    form.append(&label);
    ask(
        window,
        "Share a location",
        "Paste coordinates (51.5007, -0.1246) or a link from a map site.",
        &form,
        move || {
            let (lat, lon) = parse_place(&place.text())?;
            Some(card(
                CardItemInfo::Location {
                    latitude_e7: lat,
                    longitude_e7: lon,
                    label: label.text().trim().to_owned(),
                    accuracy_m: 0,
                },
                false,
                0,
            ))
        },
        send,
    );
}

pub fn compose_event(window: &adw::ApplicationWindow, send: impl Fn(CardInfo) + 'static) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let title = entry("What", "");
    let cal = gtk::Calendar::new();
    let now = glib::DateTime::now_local().ok();
    let hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let minute = gtk::SpinButton::with_range(0.0, 55.0, 5.0);
    hour.set_value(f64::from(now.as_ref().map_or(12, |n| (n.hour() + 1) % 24)));
    let length = gtk::DropDown::from_strings(&["30 minutes", "1 hour", "2 hours", "3 hours"]);
    length.set_selected(1);
    let all_day = gtk::CheckButton::with_label("All day");
    let time_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    time_row.append(&ui::label("At", &[]));
    time_row.append(&hour);
    time_row.append(&ui::label(":", &[]));
    time_row.append(&minute);
    time_row.append(&length);
    let tr = time_row.clone();
    all_day.connect_toggled(move |c| tr.set_sensitive(!c.is_active()));
    let place = entry("Where", "");
    let note = entry("Note", "");
    for w in [
        title.upcast_ref::<gtk::Widget>(),
        cal.upcast_ref(),
        all_day.upcast_ref(),
        time_row.upcast_ref(),
        place.upcast_ref(),
        note.upcast_ref(),
    ] {
        form.append(w);
    }
    ask(
        window,
        "Share an event",
        "",
        &form,
        move || {
            let t = title.text().trim().to_owned();
            if t.is_empty() {
                return None;
            }
            let d = cal.date();
            let (start, end) = if all_day.is_active() {
                let s = glib::DateTime::from_utc(d.year(), d.month(), d.day_of_month(), 0, 0, 0.0)
                    .ok()?;
                (s.to_unix() as u64 * 1000, 0)
            } else {
                let s = glib::DateTime::from_local(
                    d.year(),
                    d.month(),
                    d.day_of_month(),
                    hour.value() as i32,
                    minute.value() as i32,
                    0.0,
                )
                .ok()?;
                let mins: u64 = match length.selected() {
                    0 => 30,
                    2 => 120,
                    3 => 180,
                    _ => 60,
                };
                let s = s.to_unix() as u64 * 1000;
                (s, s + mins * 60_000)
            };
            Some(card(
                CardItemInfo::Event {
                    title: t,
                    start_ms: start,
                    end_ms: end,
                    all_day: all_day.is_active(),
                    place: place.text().trim().to_owned(),
                    note: note.text().trim().to_owned(),
                },
                false,
                0,
            ))
        },
        send,
    );
}

/// A login, a payment card or a secure note, typed in: viewed once and
/// gone after a day unless the user says otherwise.
pub fn compose_secret(
    window: &adw::ApplicationWindow,
    kind: &'static str,
    in_notes: bool,
    send: impl Fn(CardInfo) + 'static,
) {
    if kind == "card" {
        return compose_payment_card(window, in_notes, send);
    }
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let title = entry(
        match kind {
            "login" => "Name (the site, say)",
            "card" => "Name (Joint Visa, say)",
            _ => "Title",
        },
        "",
    );
    form.append(&title);
    let secret = |hint: &str| {
        gtk::PasswordEntry::builder()
            .placeholder_text(hint)
            .show_peek_icon(true)
            .build()
            .upcast::<gtk::Editable>()
    };
    let plain = |hint: &str| entry(hint, "").upcast::<gtk::Editable>();
    let fields: Vec<(&'static str, gtk::Editable, bool)> = match kind {
        "login" => vec![
            ("username", plain("Username"), false),
            ("password", secret("Password"), true),
            ("url", plain("Website"), false),
        ],
        "card" => vec![
            ("number", secret("Card number"), true),
            ("holder", plain("Name on card"), false),
            ("expiry", plain("Expires (MM/YY)"), false),
            ("cvc", secret("Security code"), true),
        ],
        _ => vec![("text", secret("Note"), true)],
    };
    for (_, w, _) in &fields {
        form.append(w);
    }
    let once = gtk::CheckButton::with_label("They can view it once");
    // In our own notes there's nobody to view it once.
    once.set_active(!in_notes);
    once.set_visible(!in_notes);
    let expiry = gtk::DropDown::from_strings(&[
        "Gone after 1 hour",
        "Gone after 1 day",
        "Gone after 1 week",
    ]);
    expiry.set_selected(1);
    form.append(&once);
    form.append(&expiry);
    ask(
        window,
        match kind {
            "login" => "Share a login",
            "card" => "Share a payment card",
            _ => "Share a secure note",
        },
        "It isn't kept on this computer once sent, and isn't backed up.",
        &form,
        move || {
            let t = title.text().trim().to_owned();
            let fs: Vec<SecretFieldInfo> = fields
                .iter()
                .map(|(name, w, hidden)| SecretFieldInfo {
                    name: (*name).into(),
                    value: w.text().trim().to_owned(),
                    hidden: *hidden,
                })
                .filter(|f| !f.value.is_empty())
                .collect();
            if t.is_empty() || fs.is_empty() {
                return None;
            }
            let secs = match expiry.selected() {
                0 => 3600,
                2 => 7 * 86_400,
                _ => 86_400,
            };
            Some(card(
                CardItemInfo::Secret {
                    kind: kind.into(),
                    title: t,
                    fields: fs,
                    source: String::new(),
                },
                once.is_active(),
                secs,
            ))
        },
        send,
    );
}

/// A payment card typed onto a picture of it: the card updates as each
/// field is filled, the number falls into its groups, the expiry into
/// MM/YY, and a mistyped number is pointed out.
fn compose_payment_card(
    window: &adw::ApplicationWindow,
    in_notes: bool,
    send: impl Fn(CardInfo) + 'static,
) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let preview = PaymentCard::new(320);
    preview.root.set_halign(gtk::Align::Center);
    preview.root.set_margin_bottom(6);
    form.append(&preview.root);
    let title = entry("Name it (Joint Visa, say)", "");
    let number = entry("Card number", "");
    number.set_input_purpose(gtk::InputPurpose::Digits);
    number.set_max_length(23);
    let holder = entry("Name on card", "");
    let expiry = entry("MM/YY", "");
    expiry.set_max_length(5);
    expiry.set_input_purpose(gtk::InputPurpose::Digits);
    let cvc = gtk::PasswordEntry::builder()
        .placeholder_text("CVC")
        .show_peek_icon(true)
        .build();
    let warn = ui::label("", &["caption", "error"]);
    warn.set_visible(false);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_homogeneous(true);
    row.append(&expiry);
    row.append(&cvc);
    for w in [
        title.upcast_ref::<gtk::Widget>(),
        number.upcast_ref(),
        warn.upcast_ref(),
        holder.upcast_ref(),
        row.upcast_ref(),
    ] {
        form.append(w);
    }
    let redraw = {
        let (p, t, n, h, e, w) = (
            preview.clone(),
            title.clone(),
            number.clone(),
            holder.clone(),
            expiry.clone(),
            warn.clone(),
        );
        Rc::new(move || {
            let digits: String = n.text().chars().filter(char::is_ascii_digit).collect();
            p.show(
                &t.text(),
                &digits,
                &PaymentCard::shown_number(&digits, true),
                &h.text(),
                &e.text(),
            );
            let full = if threnody_ffi::card_brand(digits.clone()) == "amex" {
                15
            } else {
                16
            };
            let bad = digits.len() >= full && !threnody_ffi::card_number_valid(digits);
            w.set_text("Check the number: a digit looks wrong");
            w.set_visible(bad);
        })
    };
    // Shapes what is typed; deleting is left alone, so a space or slash can go.
    let shape = |e: &gtk::Entry, f: fn(String) -> String| {
        let last = Rc::new(std::cell::Cell::new(0usize));
        let busy = Rc::new(std::cell::Cell::new(false));
        let r = redraw.clone();
        e.connect_changed(move |e| {
            if busy.get() {
                return;
            }
            let t = e.text().to_string();
            let grew = t.chars().count() > last.get();
            if grew {
                let shaped = f(t.clone());
                if shaped != t {
                    busy.set(true);
                    e.set_text(&shaped);
                    e.set_position(-1);
                    busy.set(false);
                }
            }
            last.set(e.text().chars().count());
            r();
        });
    };
    shape(&number, threnody_ffi::format_card_number);
    shape(&expiry, threnody_ffi::format_card_expiry);
    for e in [&title, &holder] {
        let r = redraw.clone();
        e.connect_changed(move |_| r());
    }
    redraw();
    let once = gtk::CheckButton::with_label("They can view it once");
    // In our own notes there's nobody to view it once.
    once.set_active(!in_notes);
    once.set_visible(!in_notes);
    let keep = gtk::DropDown::from_strings(&[
        "Gone after 1 hour",
        "Gone after 1 day",
        "Gone after 1 week",
    ]);
    keep.set_selected(1);
    form.append(&once);
    form.append(&keep);
    ask(
        window,
        "Share a payment card",
        "It isn't kept on this computer once sent, and isn't backed up.",
        &form,
        move || {
            let digits: String = number.text().chars().filter(char::is_ascii_digit).collect();
            if digits.len() < 12 {
                return None;
            }
            let brand = threnody_ffi::card_brand(digits.clone());
            let mut t = title.text().trim().to_owned();
            if t.is_empty() {
                t = BRANDS
                    .iter()
                    .find(|b| b.0 == brand)
                    .map_or("Payment card".into(), |b| format!("{} card", b.1));
            }
            let fields: Vec<SecretFieldInfo> = [
                ("number", threnody_ffi::format_card_number(digits), true),
                ("holder", holder.text().trim().to_owned(), false),
                (
                    "expiry",
                    threnody_ffi::format_card_expiry(expiry.text().trim().to_owned())
                        .trim_end_matches('/')
                        .to_owned(),
                    false,
                ),
                ("cvc", cvc.text().trim().to_owned(), true),
            ]
            .into_iter()
            .filter(|f| !f.1.is_empty())
            .map(|(name, value, hidden)| SecretFieldInfo {
                name: name.into(),
                value,
                hidden,
            })
            .collect();
            let secs = match keep.selected() {
                0 => 3600,
                2 => 7 * 86_400,
                _ => 86_400,
            };
            Some(card(
                CardItemInfo::Secret {
                    kind: "card".into(),
                    title: t,
                    fields,
                    source: String::new(),
                },
                once.is_active(),
                secs,
            ))
        },
        send,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_come_from_coordinates_and_map_links() {
        let london = Some((515_007_000, -1_246_000));
        assert_eq!(parse_place("51.5007, -0.1246"), london);
        assert_eq!(parse_place("geo:51.5007,-0.1246?z=17"), london);
        assert_eq!(
            parse_place(
                "https://www.openstreetmap.org/?mlat=51.5007&mlon=-0.1246#map=17/51.5/-0.12"
            ),
            london
        );
        assert_eq!(
            parse_place("https://www.google.com/maps/@51.5007,-0.1246,17z"),
            london
        );
        assert_eq!(
            parse_place("https://maps.example/?q=51.5007,-0.1246"),
            london
        );
        assert_eq!(parse_place("91, 0"), None);
        assert_eq!(parse_place("somewhere"), None);
    }
}
