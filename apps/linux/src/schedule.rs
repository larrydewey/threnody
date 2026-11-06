//! Send later: the time picker, the list of a chat's scheduled messages,
//! and their texts. The node sends them when due (while this app runs).

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use threnody_ffi::{ScheduleFailure, ScheduledInfo};

use crate::ui;

/// What the picker and the list say about who sends it.
pub const CAVEAT: &str = "Sends from this device. Threnody needs to be running here \
     (closing the window keeps it running).";

/// The quick choices, as of `now` (local time): in an hour, this evening
/// at 20:00 (offered before 19:00), tomorrow at 08:00.
pub fn presets(now: &glib::DateTime) -> Vec<(String, glib::DateTime)> {
    let mut out = Vec::new();
    if let Ok(t) = now.add_hours(1) {
        out.push((format!("In 1 hour ({})", clock(&t)), t));
    }
    let tz = now.timezone();
    if now.hour() < 19
        && let Ok(t) =
            glib::DateTime::new(&tz, now.year(), now.month(), now.day_of_month(), 20, 0, 0.0)
    {
        out.push(("This evening, 20:00".into(), t));
    }
    if let Ok(d) = now.add_days(1)
        && let Ok(t) = glib::DateTime::new(&tz, d.year(), d.month(), d.day_of_month(), 8, 0, 0.0)
    {
        let day = t.format("%a").map(|s| s.to_string()).unwrap_or_default();
        out.push((format!("Tomorrow morning, 08:00 ({day})"), t));
    }
    out
}

fn clock(t: &glib::DateTime) -> String {
    t.format("%H:%M").map(|s| s.to_string()).unwrap_or_default()
}

fn ms_of(t: &glib::DateTime) -> u64 {
    u64::try_from(t.to_unix()).unwrap_or(0) * 1000
}

fn now_ms() -> u64 {
    glib::DateTime::now_utc().map_or(0, |t| ms_of(&t))
}

/// When a scheduled message goes, in local time: "14:05", "tomorrow
/// 08:00", "Tue 08:00" within the week, else "12 Mar 08:00".
pub fn when_label(at_ms: u64) -> String {
    let (Ok(t), Ok(now)) = (
        glib::DateTime::from_unix_local(i64::try_from(at_ms / 1000).unwrap_or(0)),
        glib::DateTime::now_local(),
    ) else {
        return String::new();
    };
    when_label_at(&t, &now)
}

fn when_label_at(t: &glib::DateTime, now: &glib::DateTime) -> String {
    let day = |d: &glib::DateTime| {
        glib::DateTime::new(
            &d.timezone(),
            d.year(),
            d.month(),
            d.day_of_month(),
            12,
            0,
            0.0,
        )
    };
    let days = match (day(t), day(now)) {
        (Ok(a), Ok(b)) => (a.difference(&b).as_seconds() as f64 / 86_400.0).round() as i64,
        _ => 99,
    };
    let fmt = match days {
        0 => "%H:%M",
        1 => "tomorrow %H:%M",
        2..=6 => "%a %H:%M",
        _ if t.year() == now.year() => "%-d %b %H:%M",
        _ => "%-d %b %Y %H:%M",
    };
    t.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

/// Why a scheduled message wasn't sent, for its row and the notification.
pub fn failure_text(r: ScheduleFailure) -> &'static str {
    match r {
        ScheduleFailure::Missed => "missed its time",
        ScheduleFailure::Contact => "the contact was blocked or removed",
        ScheduleFailure::Group => "you're no longer in the group",
        ScheduleFailure::Error => "couldn't send",
        ScheduleFailure::Removed => "this device was removed from your account",
    }
}

/// Asks when to send: a quick choice (counted from when it is clicked),
/// or a date and time up to a year ahead. `done` gets the time (ms since
/// the epoch), which may be past if the user picked one.
pub fn pick_time(
    parent: &impl IsA<gtk::Widget>,
    current: Option<u64>,
    done: impl Fn(u64) + 'static,
) {
    let done = Rc::new(done);
    let d = ui::alert(
        if current.is_some() {
            "Change time"
        } else {
            "Send later"
        },
        "",
        &[("cancel", "Cancel"), ("schedule", "Schedule")],
    );
    d.set_response_appearance("schedule", adw::ResponseAppearance::Suggested);
    let b = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let Ok(now) = glib::DateTime::now_local() else {
        return;
    };
    let quick = presets(&now);
    for (text, _) in &quick {
        list.append(
            &adw::ActionRow::builder()
                .title(glib::markup_escape_text(text))
                .activatable(true)
                .build(),
        );
    }
    // A date and time of the user's own.
    let start = current
        .and_then(|ms| glib::DateTime::from_unix_local(i64::try_from(ms / 1000).ok()?).ok())
        .or_else(|| now.add_hours(1).ok())
        .unwrap_or_else(|| now.clone());
    let calendar = gtk::Calendar::new();
    calendar.select_day(&start);
    let hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    hour.set_value(f64::from(start.hour()));
    hour.set_wrap(true);
    let minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    minute.set_value(f64::from(start.minute()));
    minute.set_wrap(true);
    for s in [&hour, &minute] {
        s.set_numeric(true);
        s.set_width_chars(2);
        // "08", not "8".
        s.connect_output(|s| {
            s.set_text(&format!("{:02}", s.value() as i32));
            glib::Propagation::Stop
        });
    }
    let time = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    time.set_halign(gtk::Align::Center);
    time.set_margin_top(6);
    time.set_margin_bottom(6);
    time.append(&hour);
    time.append(&gtk::Label::new(Some(":")));
    time.append(&minute);
    let custom = gtk::Box::new(gtk::Orientation::Vertical, 6);
    custom.set_margin_start(6);
    custom.set_margin_end(6);
    custom.append(&calendar);
    custom.append(&time);
    let expander = adw::ExpanderRow::builder()
        .title("Pick date and time")
        .subtitle("Up to a year ahead, then Schedule")
        .expanded(current.is_some())
        .build();
    let custom_row = gtk::ListBoxRow::builder()
        .child(&custom)
        .activatable(false)
        .build();
    expander.add_row(&custom_row);
    list.append(&expander);
    b.append(&list);
    b.append(&ui::label(CAVEAT, &["caption", "dim-label"]));
    d.set_extra_child(Some(&b));

    let (dd, picked) = (d.clone(), done.clone());
    list.connect_row_activated(move |_, row| {
        let i = usize::try_from(row.index()).unwrap_or(usize::MAX);
        if i >= quick.len() {
            return;
        }
        // Counted from now, not from when the picker opened.
        let Some((_, t)) = glib::DateTime::now_local()
            .ok()
            .and_then(|n| presets(&n).into_iter().nth(i))
        else {
            return;
        };
        dd.close();
        picked(ms_of(&t));
    });
    d.connect_response(Some("schedule"), move |_, _| {
        let date = calendar.date();
        let Ok(t) = glib::DateTime::new(
            &glib::TimeZone::local(),
            date.year(),
            date.month(),
            date.day_of_month(),
            hour.value() as i32,
            minute.value() as i32,
            0.0,
        ) else {
            return;
        };
        done(ms_of(&t));
    });
    d.present(Some(parent));
}

/// What can be done with a scheduled message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    SendNow,
    Edit,
    Move,
    Delete,
}

/// A chat's scheduled messages, soonest first, each with a menu of what
/// to do with it; `act` does it.
pub fn list_dialog(
    parent: &impl IsA<gtk::Widget>,
    items: &[ScheduledInfo],
    timer: bool,
    act: impl Fn(Act, ScheduledInfo) + 'static,
) {
    let act = Rc::new(act);
    let body = if timer {
        "Disappearing messages apply when it's sent."
    } else {
        ""
    };
    let d = ui::alert("Scheduled messages", body, &[("close", "Close")]);
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for item in items {
        let mut title = when_label(item.due_ms);
        if item.quote.is_some() {
            title.push_str(" · ↩ reply");
        }
        let snippet: String = item
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let mut subtitle = glib::markup_escape_text(&snippet).to_string();
        if let Some(f) = item.failed {
            subtitle.push_str(&format!("\n⚠ Not sent: {}", failure_text(f)));
        }
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&title))
            .subtitle(subtitle)
            .build();
        if item.failed.is_some() {
            row.add_css_class("warning");
        }
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let pop = gtk::Popover::builder().child(&menu).build();
        for (label, a) in [
            ("Send now", Act::SendNow),
            ("Edit text…", Act::Edit),
            ("Change time…", Act::Move),
            ("Delete…", Act::Delete),
        ] {
            let b = gtk::Button::with_label(label);
            b.add_css_class("flat");
            if a == Act::Delete {
                b.add_css_class("destructive-action");
            }
            let (act, item, pop, dd) = (act.clone(), item.clone(), pop.clone(), d.clone());
            b.connect_clicked(move |_| {
                pop.popdown();
                dd.close();
                act(a, item.clone());
            });
            menu.append(&b);
        }
        let button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Send now, edit, change time or delete")
            .valign(gtk::Align::Center)
            .popover(&pop)
            .build();
        button.add_css_class("flat");
        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .build();
    d.set_extra_child(Some(&scroller));
    d.present(Some(parent));
}

/// Whether `due_ms` is still ahead.
pub fn in_future(due_ms: u64) -> bool {
    due_ms > now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(deprecated)] // `from_identifier` needs GLib 2.68 bindings
    fn berlin() -> glib::TimeZone {
        glib::TimeZone::new(Some("Europe/Berlin"))
    }

    #[test]
    fn presets_skip_evening_after_seven_and_are_in_the_future() {
        let tz = berlin();
        let morning = glib::DateTime::new(&tz, 2026, 6, 10, 10, 30, 0.0).unwrap();
        let p = presets(&morning);
        assert_eq!(p.len(), 3);
        assert!(p.iter().all(|(_, t)| t.to_unix() > morning.to_unix()));
        assert_eq!(p[0].1.to_unix() - morning.to_unix(), 3600);
        assert_eq!((p[1].1.hour(), p[1].1.minute()), (20, 0));
        assert_eq!((p[2].1.day_of_month(), p[2].1.hour()), (11, 8));

        let evening = glib::DateTime::new(&tz, 2026, 6, 10, 19, 30, 0.0).unwrap();
        let p = presets(&evening);
        assert_eq!(p.len(), 2, "no 'this evening' after 19:00");
        assert!(p.iter().all(|(_, t)| t.to_unix() > evening.to_unix()));

        // The night clocks go forward: 22 hours on the clock, 21 in fact.
        let before = glib::DateTime::new(&tz, 2026, 3, 28, 10, 0, 0.0).unwrap();
        let p = presets(&before);
        let tomorrow = &p.last().unwrap().1;
        assert_eq!((tomorrow.day_of_month(), tomorrow.hour()), (29, 8));
        assert_eq!(tomorrow.to_unix() - before.to_unix(), 21 * 3600);
    }

    #[test]
    fn failure_texts_cover_every_reason() {
        // A match with no catch-all: a new reason needs a text to compile.
        for r in [
            ScheduleFailure::Missed,
            ScheduleFailure::Contact,
            ScheduleFailure::Group,
            ScheduleFailure::Error,
            ScheduleFailure::Removed,
        ] {
            assert!(!failure_text(r).is_empty());
        }
    }

    #[test]
    fn future_times_read_as_days_ahead() {
        let tz = berlin();
        let now = glib::DateTime::new(&tz, 2026, 6, 10, 23, 0, 0.0).unwrap();
        let at = |d, h| glib::DateTime::new(&tz, 2026, 6, d, h, 5, 0.0).unwrap();
        assert_eq!(when_label_at(&at(10, 23), &now), "23:05");
        assert_eq!(when_label_at(&at(11, 8), &now), "tomorrow 08:05");
        assert!(when_label_at(&at(14, 8), &now).ends_with(" 08:05"));
        assert!(when_label_at(&at(30, 8), &now).starts_with("30 "));
    }
}
