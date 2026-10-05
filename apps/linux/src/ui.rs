//! Small helpers shared by the window and the chat: running node calls off
//! the main thread, dialogs, QR codes and time formatting.

use adw::prelude::*;
use gtk::{gdk, glib};

/// Runs `work` (blocking node calls) on a worker thread, then `done` with
/// its result on the main thread.
pub fn bg<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    glib::spawn_future_local(async move {
        if let Ok(v) = gtk::gio::spawn_blocking(work).await {
            done(v);
        }
    });
}

/// Runs `work` in the background and ignores what it returns.
pub fn bg_quiet(work: impl FnOnce() + Send + 'static) {
    bg(work, |()| {});
}

pub fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .build();
    for c in classes {
        l.add_css_class(c);
    }
    l
}

/// A dialog with a heading, body and responses (`(id, label)`); the first
/// response is the cancelling one, the last the default.
pub fn alert(heading: &str, body: &str, responses: &[(&str, &str)]) -> adw::AlertDialog {
    let d = adw::AlertDialog::new(Some(heading), Some(body));
    for (id, text) in responses {
        d.add_response(id, text);
    }
    if let Some((first, _)) = responses.first() {
        d.set_close_response(first);
    }
    if let Some((last, _)) = responses.last() {
        d.set_default_response(Some(last));
    }
    d
}

/// Asks a yes-or-no question; `run` happens on `confirm`. `destructive`
/// colours the button red.
pub fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    confirm: &str,
    destructive: bool,
    run: impl Fn() + 'static,
) {
    let d = alert(heading, body, &[("cancel", "Cancel"), ("ok", confirm)]);
    d.set_response_appearance(
        "ok",
        if destructive {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    d.connect_response(None, move |_, r| {
        if r == "ok" {
            run();
        }
    });
    d.present(Some(parent));
}

/// Asks for one line of text; `run` gets it (trimmed) when confirmed.
pub fn ask_text(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    placeholder: &str,
    initial: &str,
    confirm: &str,
    run: impl Fn(String) + 'static,
) {
    let d = alert(heading, body, &[("cancel", "Cancel"), ("ok", confirm)]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder)
        .text(initial)
        .activates_default(true)
        .build();
    d.set_extra_child(Some(&entry));
    d.connect_response(None, move |_, r| {
        if r == "ok" {
            run(entry.text().trim().to_owned());
        }
    });
    d.present(Some(parent));
    // Grab after presenting; the dialog focuses its default button otherwise.
    let e = d.extra_child();
    glib::idle_add_local_once(move || {
        if let Some(e) = e {
            e.grab_focus();
        }
    });
}

/// Offers a list of choices; `run` gets the chosen index.
pub fn choose(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    options: &[String],
    current: Option<usize>,
    run: impl Fn(usize) + 'static,
) {
    let d = alert(heading, body, &[("cancel", "Cancel")]);
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for (i, o) in options.iter().enumerate() {
        let row = adw::ActionRow::builder().title(o).activatable(true).build();
        if current == Some(i) {
            row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
        }
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .build();
    d.set_extra_child(Some(&scroller));
    let dd = d.clone();
    list.connect_row_activated(move |_, row| {
        let i = usize::try_from(row.index()).unwrap_or(0);
        dd.close();
        run(i);
    });
    d.present(Some(parent));
}

/// A QR code as a texture, `scale` pixels per module, with a quiet zone.
pub fn qr_texture(text: &str, scale: usize) -> Option<gdk::Texture> {
    let qr = threnody_ffi::qr_matrix(text.to_owned()).ok()?;
    let n = qr.size as usize;
    let quiet = 4;
    let side = (n + 2 * quiet) * scale;
    let mut px = vec![255u8; side * side * 3];
    for y in 0..n {
        for x in 0..n {
            if !qr.dark[y * n + x] {
                continue;
            }
            for dy in 0..scale {
                let row = ((y + quiet) * scale + dy) * side;
                for dx in 0..scale {
                    let i = (row + (x + quiet) * scale + dx) * 3;
                    px[i..i + 3].fill(0);
                }
            }
        }
    }
    let side_i = i32::try_from(side).ok()?;
    Some(
        gdk::MemoryTexture::new(
            side_i,
            side_i,
            gdk::MemoryFormat::R8g8b8,
            &glib::Bytes::from_owned(px),
            side * 3,
        )
        .upcast(),
    )
}

/// Shows a QR code and the text it holds, with a copy button.
pub fn show_code(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    text: &str,
    caption: &str,
) {
    let d = alert(heading, body, &[("close", "Done")]);
    let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
    if let Some(t) = qr_texture(text, 6) {
        let pic = gtk::Picture::for_paintable(&t);
        pic.set_can_shrink(false);
        pic.set_halign(gtk::Align::Center);
        b.append(&pic);
    }
    let link = label(text, &["monospace", "caption"]);
    link.set_selectable(true);
    link.set_xalign(0.5);
    link.set_justify(gtk::Justification::Center);
    b.append(&link);
    let copy = gtk::Button::with_label("Copy");
    copy.set_halign(gtk::Align::Center);
    copy.add_css_class("pill");
    let t = text.to_owned();
    copy.connect_clicked(move |btn| {
        btn.clipboard().set_text(&t);
        btn.set_label("Copied");
    });
    b.append(&copy);
    if !caption.is_empty() {
        let c = label(caption, &["dim-label", "caption"]);
        c.set_xalign(0.5);
        c.set_justify(gtk::Justification::Center);
        b.append(&c);
    }
    d.set_extra_child(Some(&b));
    d.present(Some(parent));
    // Not the selectable link: focusing it would select all of it.
    glib::idle_add_local_once(move || {
        copy.grab_focus();
    });
}

/// Formats a safety number as twelve groups of five digits.
pub fn safety_groups(n: &str) -> String {
    let digits: Vec<char> = n.chars().filter(char::is_ascii_digit).collect();
    digits
        .chunks(5)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|row| row.join("  "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A time for a message: "14:05" today, else "Mon 14:05" or "3 Oct".
pub fn time_label(at_ms: u64) -> String {
    let Ok(t) = glib::DateTime::from_unix_local(i64::try_from(at_ms / 1000).unwrap_or(0)) else {
        return String::new();
    };
    let now = glib::DateTime::now_local().ok();
    let fmt = match now {
        Some(n) if n.ymd() == t.ymd() => "%H:%M",
        Some(n) if n.difference(&t).as_seconds() < 6 * 86_400 => "%a %H:%M",
        Some(n) if n.year() == t.year() => "%-d %b %H:%M",
        _ => "%-d %b %Y",
    };
    t.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

/// A short time for the conversation list.
pub fn list_time(at_ms: u64) -> String {
    let Ok(t) = glib::DateTime::from_unix_local(i64::try_from(at_ms / 1000).unwrap_or(0)) else {
        return String::new();
    };
    let now = glib::DateTime::now_local().ok();
    let fmt = match now {
        Some(n) if n.ymd() == t.ymd() => "%H:%M",
        Some(n) if n.difference(&t).as_seconds() < 6 * 86_400 => "%a",
        _ => "%-d %b",
    };
    t.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "kB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// A row with a title, a subtitle and a trailing button.
pub fn link_button_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(glib::markup_escape_text(title))
        .subtitle(glib::markup_escape_text(subtitle))
        .activatable(true)
        .build()
}
