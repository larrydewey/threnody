//! One open conversation: header, trust banner, messages and composer.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use threnody_ffi::{FileOptions, HistoryEntry};

use crate::core::{self, ChatRef, Conversation, Core, Node, Target};
use crate::settings;
use crate::ui::{self, bg};
use crate::window::App;

/// How many messages a chat loads.
const HISTORY: u32 = 1000;
/// A banner or menu action on the open chat.
type Action = fn(&Rc<ChatView>);
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

pub struct ChatView {
    app: Weak<App>,
    conv: RefCell<Conversation>,
    root: adw::ToolbarView,
    title: adw::WindowTitle,
    banner: gtk::Box,
    messages: gtk::Box,
    scroll: gtk::ScrolledWindow,
    stack: gtk::Stack,
    composer: gtk::Box,
    input: gtk::TextView,
    menu: gtk::MenuButton,
    entries: RefCell<Vec<HistoryEntry>>,
    loading: Cell<bool>,
    again: Cell<bool>,
    /// The history was shown at least once.
    loaded: Cell<bool>,
    /// Whether the main identity already has the contact they revealed.
    revealed_known: Cell<bool>,
    /// Sensitive photos the user uncovered, by time and sender.
    shown: RefCell<Vec<(u64, String)>>,
    /// Typing indicator row (shown when peer is typing in 1:1 chats).
    typing_row: gtk::Box,
    /// Whether the peer is currently typing.
    peer_typing: Cell<bool>,
    /// Set of remote message ids we've already sent read receipts for.
    sent_read: RefCell<HashSet<u64>>,
    /// Timer source id for typing timeout.
    typing_timeout: RefCell<Option<glib::SourceId>>,
    /// Whether we've sent "typing: true" for the current non-empty compose.
    sending_typing: Cell<bool>,
    /// Weak reference to self for creating weak pointers in callbacks.
    weak_self: Weak<ChatView>,
}

impl ChatView {
    pub fn new(app: &Rc<App>, conv: Conversation) -> Rc<Self> {
        let title = adw::WindowTitle::new(&conv.title, "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));
        let menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Conversation options")
            .build();
        header.pack_end(&menu);

        let banner = gtk::Box::new(gtk::Orientation::Vertical, 6);
        banner.add_css_class("chat-banner");
        banner.set_visible(false);

        let messages = gtk::Box::new(gtk::Orientation::Vertical, 2);
        messages.set_margin_start(12);
        messages.set_margin_end(12);
        messages.set_margin_top(12);
        messages.set_margin_bottom(12);
        messages.set_valign(gtk::Align::End);
        let clamp = adw::Clamp::builder()
            .maximum_size(860)
            .child(&messages)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("mail-unread-symbolic")
            .title("No messages yet")
            .build();
        empty.add_css_class("compact");
        let stack = gtk::Stack::new();
        stack.add_named(&scroll, Some("messages"));
        stack.add_named(&empty, Some("empty"));
        stack.set_vexpand(true);
        empty.set_description(Some(if matches!(conv.chat.target, Target::Group(_)) {
            "Group messages are end-to-end encrypted with MLS."
        } else {
            "Messages are end-to-end encrypted and stored encrypted on this device."
        }));

        let input = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(7)
            .bottom_margin(7)
            .left_margin(10)
            .right_margin(10)
            .hexpand(true)
            .build();
        input.add_css_class("composer-input");
        let input_scroll = gtk::ScrolledWindow::builder()
            .child(&input)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(160)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        input_scroll.add_css_class("composer-frame");
        let attach = gtk::Button::from_icon_name("mail-attachment-symbolic");
        attach.set_tooltip_text(Some("Send photos or files"));
        attach.add_css_class("flat");
        attach.add_css_class("circular");
        attach.set_valign(gtk::Align::End);
        let send = gtk::Button::from_icon_name("go-up-symbolic");
        send.set_tooltip_text(Some("Send (Enter)"));
        send.add_css_class("suggested-action");
        send.add_css_class("circular");
        send.set_valign(gtk::Align::End);
        let composer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        composer.add_css_class("composer");
        composer.append(&attach);
        composer.append(&input_scroll);
        composer.append(&send);
        let composer_clamp = adw::Clamp::builder()
            .maximum_size(860)
            .child(&composer)
            .build();

        // Build typing indicator row (shown when peer is typing in 1:1 chats).
        let typing_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        typing_row.add_css_class("chat-typing");
        typing_row.add_css_class("bubble");
        typing_row.add_css_class("incoming");
        typing_row.set_halign(gtk::Align::Start);
        typing_row.set_margin_top(8);
        let dots = gtk::Label::new(Some("…"));
        dots.add_css_class("typing-dots");
        typing_row.append(&dots);
        typing_row.set_visible(false);

        // Composer with "(Message)" placeholder overlay.
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&input_scroll));
        let placeholder = gtk::Label::new(Some("(Message)"));
        placeholder.add_css_class("composer-placeholder");
        placeholder.set_halign(gtk::Align::Start);
        placeholder.set_valign(gtk::Align::Center);
        placeholder.set_margin_start(12);
        placeholder.set_can_target(false);
        overlay.add_overlay(&placeholder);

        let composer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        composer.add_css_class("composer");
        composer.append(&attach);
        composer.append(&overlay);
        composer.append(&send);
        let composer_clamp = adw::Clamp::builder()
            .maximum_size(860)
            .child(&composer)
            .build();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&banner);
        body.append(&stack);
        let root = adw::ToolbarView::new();
        root.add_top_bar(&header);
        root.set_content(Some(&body));
        root.add_bottom_bar(&composer_clamp);

        let mut this = Rc::new(Self {
            app: Rc::downgrade(app),
            conv: RefCell::new(conv.clone()),
            root,
            title,
            banner,
            messages,
            scroll,
            stack,
            composer,
            input,
            menu,
            entries: RefCell::new(Vec::new()),
            loading: Cell::new(false),
            again: Cell::new(false),
            loaded: Cell::new(false),
            revealed_known: Cell::new(true),
            shown: RefCell::new(Vec::new()),
            typing_row,
            peer_typing: Cell::new(false),
            sent_read: RefCell::new(HashSet::new()),
            typing_timeout: RefCell::new(None),
            sending_typing: Cell::new(false),
            weak_self: Weak::new(),
        });

        let weak = Rc::downgrade(&this);
        // Initialize weak_self after creation
        Rc::get_mut(&mut this).unwrap().weak_self = weak.clone();
        send.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.send();
            }
        });
        let weak = Rc::downgrade(&this);
        attach.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.pick_files();
            }
        });
        // Enter sends; Shift+Enter starts a new line.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
            if enter && !mods.contains(gdk::ModifierType::SHIFT_MASK) {
                if let Some(t) = weak.upgrade() {
                    t.send();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        this.input.add_controller(keys);

        // Dropped files are sent like attached ones.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(&this);
        drop.connect_drop(move |_, value, _, _| {
            let (Some(t), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>()) else {
                return false;
            };
            let paths: Vec<std::path::PathBuf> =
                list.files().iter().filter_map(gio::File::path).collect();
            if paths.is_empty() {
                return false;
            }
            t.confirm_files(paths);
            true
        });
        this.root.add_controller(drop);

        // Connect text buffer changed signal for typing notifications.
        {
            let weak = Rc::downgrade(&this);
            this.input.buffer().connect_changed(move |buf| {
                let Some(t) = weak.upgrade() else { return };
                if t.group().is_some() {
                    return; // No typing indicator for groups yet.
                }
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
                let is_empty = text.trim().is_empty();
                if is_empty {
                    if t.sending_typing.replace(false) {
                        t.notify_typing(false);
                    }
                    // Cancel any pending timeout.
                    if let Some(id) = t.typing_timeout.borrow_mut().take() {
                        id.remove();
                    }
                } else if !t.sending_typing.replace(true) {
                    t.notify_typing(true);
                }
                // Reset the "stop typing" timeout.
                if let Some(id) = t.typing_timeout.borrow_mut().take() {
                    id.remove();
                }
                let weak = Rc::downgrade(&t);
                *t.typing_timeout.borrow_mut() = Some(glib::timeout_add_local_once(
                    std::time::Duration::from_secs(3),
                    move || {
                        if let Some(t) = weak.upgrade() {
                            if t.sending_typing.replace(false) {
                                t.notify_typing(false);
                            }
                        }
                    },
                ));
            });
        }

        this.update(conv);
        this.reload();
        this.input.grab_focus();
        this
    }

    pub fn widget(&self) -> &adw::ToolbarView {
        &self.root
    }

    pub fn chat(&self) -> ChatRef {
        self.conv.borrow().chat.clone()
    }

    fn persona(&self) -> Option<String> {
        self.conv.borrow().chat.persona.clone()
    }

    fn group(&self) -> Option<String> {
        match &self.conv.borrow().chat.target {
            Target::Group(g) => Some(g.clone()),
            Target::Contact(_) => None,
        }
    }

    fn device(&self) -> String {
        self.conv.borrow().device.clone()
    }

    fn node(&self) -> Option<Node> {
        self.app
            .upgrade()?
            .core
            .node(self.persona().as_deref())
            .ok()
    }

    fn toast(&self, text: &str) {
        if let Some(a) = self.app.upgrade() {
            a.toast(text);
        }
    }

    /// Sends a typing notification to the peer.
    fn notify_typing(&self, active: bool) {
        let Some(node) = self.node() else { return };
        let device = self.device();
        bg(move || { let _ = node.set_typing(device, active); }, |_| {});
    }

    /// Shows or hides the typing indicator for this conversation.
    pub fn set_typing(&self, active: bool) {
        if self.group().is_some() {
            return; // No typing indicator for groups yet.
        }
        if self.peer_typing.replace(active) != active {
            self.typing_row.set_visible(active);
            if active {
                // Re-render to append the typing row at the end.
                let entries = self.entries.borrow().clone();
                self.display_entries(entries);
            }
        }
    }

    // ----- Header, banner and menu -----

    /// Shows a newer state of the conversation (connected, approved, …).
    pub fn update(self: &Rc<Self>, conv: Conversation) {
        *self.conv.borrow_mut() = conv.clone();
        if let (Some(rev), Some(app)) = (&conv.revealed, self.app.upgrade()) {
            let core = app.core.clone();
            let rev = rev.clone();
            let weak = self.weak_self.clone();
            bg(
                move || core.main.contacts().iter().any(|c| c.fingerprint == rev),
                move |known| {
                    if let Some(t) = weak.upgrade()
                        && t.revealed_known.replace(known) != known
                    {
                        t.header();
                    }
                },
            );
            self.revealed_known.set(true);
        }
        self.header();
    }

    fn header(self: &Rc<Self>) {
        let c = self.conv.borrow().clone();
        let app = self.app.upgrade();
        let persona_label = c
            .chat
            .persona
            .as_deref()
            .and_then(|p| app.as_ref()?.core.persona_label(p));
        self.title.set_title(&c.title);
        while let Some(child) = self.banner.first_child() {
            self.banner.remove(&child);
        }
        self.banner.set_visible(false);
        self.banner.remove_css_class("warning");
        let mut can_send = true;

        if let Some(invite) = &c.invite {
            self.title.set_subtitle("invitation");
            let from = self.node().map_or_else(
                || core::short(&invite.from),
                |n| Core::name_of(&n, &invite.from),
            );
            self.show_banner(
                &format!(
                    "{from} invites you to this group. Members see each other's messages and who else is in it."
                ),
                false,
                &[("Join", Self::join_group as Action), ("Decline", Self::decline_group)],
            );
            can_send = false;
        } else if let Some(g) = &c.group {
            let mut parts = vec![if g.members.len() == 1 {
                "1 member".to_owned()
            } else {
                format!("{} members", g.members.len())
            }];
            if g.owned {
                parts.push("you're the owner".into());
            }
            if let Some(l) = &persona_label {
                parts.insert(0, format!("🎭 as {l}"));
            }
            self.title.set_subtitle(&parts.join(" · "));
            if g.owned && g.members.len() == 1 {
                self.show_banner(
                    "Only you are in this group. Invite contacts to it.",
                    false,
                    &[("Invite", Self::invite_to_group as Action)],
                );
            }
        } else if matches!(c.chat.target, Target::Group(_)) {
            self.title.set_subtitle("not a member");
            self.show_banner(
                "You're no longer in this group. The owner can invite you again.",
                false,
                &[],
            );
            can_send = false;
        } else {
            let mut parts = Vec::new();
            if let Some(l) = &persona_label {
                parts.push(format!("🎭 as {l}"));
            }
            parts.push(
                if c.connected {
                    "connected"
                } else {
                    "not connected"
                }
                .to_owned(),
            );
            parts.push(
                if c.verified {
                    "verified"
                } else {
                    "⚠ not verified"
                }
                .to_owned(),
            );
            if c.devices.len() > 1 {
                parts.push(format!("{} devices", c.devices.len()));
            }
            self.title.set_subtitle(&parts.join(" · "));
            if !c.accepted {
                self.show_banner(
                    &format!(
                        "{} wants to message you. They can't tell whether you've read this. \
                         Accept to reply; block to stop them for good.",
                        c.title
                    ),
                    true,
                    &[
                        ("Accept", Self::accept as Action),
                        ("Block", Self::block),
                        ("Delete", Self::delete_request),
                    ],
                );
                can_send = false;
            } else if let (Some(rev), false) = (&c.revealed, self.revealed_known.get()) {
                let text = format!(
                    "{} proved they are {}. Add them to talk to them as themselves{}",
                    c.title,
                    core::short(rev),
                    if c.chat.persona.is_some() {
                        " (from your main identity)."
                    } else {
                        "."
                    }
                );
                if c.revealed_invite.is_some() {
                    self.show_banner(&text, false, &[("Add them", Self::add_revealed as Action)]);
                } else {
                    self.show_banner(&text, false, &[]);
                }
            } else if !c.verified && c.any_verified {
                self.show_banner(
                    &format!(
                        "⚠ {} added a device you haven't verified. Until you compare its safety number, \
                         someone else could be reading as them.",
                        c.title
                    ),
                    true,
                    &[("Compare", Self::safety as Action)],
                );
            } else if !c.approved {
                self.show_banner(
                    &format!(
                        "Approve {} once you trust them, and compare safety numbers to make sure no one is in \
                         the middle. Mutually approved contacts find each other nearby, relay for each other and \
                         hold each other's messages while offline.",
                        c.title
                    ),
                    false,
                    &[("Approve", Self::approve as Action), ("Compare", Self::safety)],
                );
            } else if !c.verified {
                self.show_banner(
                    &format!(
                        "⚠ Not verified. Compare safety numbers with {} to make sure no one is in the middle.",
                        c.title
                    ),
                    true,
                    &[("Compare", Self::safety as Action)],
                );
            }
        }
        if c.verified || c.group.is_some() {
            self.title.remove_css_class("warning");
        } else {
            self.title.add_css_class("warning");
        }
        self.composer.set_sensitive(can_send);
        self.composer.set_visible(can_send);
        self.build_menu();
    }

    fn show_banner(self: &Rc<Self>, text: &str, warning: bool, buttons: &[(&str, Action)]) {
        let l = ui::label(text, &[]);
        self.banner.append(&l);
        if !buttons.is_empty() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_halign(gtk::Align::End);
            for (i, (name, f)) in buttons.iter().enumerate() {
                let b = gtk::Button::with_label(name);
                if i == 0 {
                    b.add_css_class("suggested-action");
                }
                let (weak, f) = (self.weak_self.clone(), *f);
                b.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        f(&t);
                    }
                });
                row.append(&b);
            }
            self.banner.append(&row);
        }
        if warning {
            self.banner.add_css_class("warning");
        }
        self.banner.set_visible(true);
    }

    fn build_menu(self: &Rc<Self>) {
        let c = self.conv.borrow().clone();
        let group = gio::SimpleActionGroup::new();
        let menu = gio::Menu::new();
        let add = |label: &str, name: &str, f: Action| {
            let a = gio::SimpleAction::new(name, None);
            let weak = self.weak_self.clone();
            a.connect_activate(move |_, _| {
                if let Some(t) = weak.upgrade() {
                    f(&t);
                }
            });
            group.add_action(&a);
            menu.append(Some(label), Some(&format!("chat.{name}")));
        };
        if let Some(g) = &c.group {
            add("Members", "members", Self::members);
            if g.owned {
                add("Invite contacts", "invite", Self::invite_to_group);
            }
            add("Media, files and links", "media", Self::media);
            add("Disappearing messages", "timer", Self::disappearing);
            add("Clear chat…", "clear", Self::clear);
            add(
                if g.owned {
                    "Delete group…"
                } else {
                    "Leave group…"
                },
                "leave",
                Self::leave_group,
            );
        } else if c.invite.is_none() && matches!(c.chat.target, Target::Contact(_)) {
            add("Media, files and links", "media", Self::media);
            add("Rename", "rename", Self::rename);
            add("Safety number", "safety", Self::safety);
            if c.approved {
                add("Revoke approval", "revoke", Self::revoke);
            } else {
                add("Approve", "approve", Self::approve);
            }
            add("Disappearing messages", "timer", Self::disappearing);
            add("Share your profile…", "share", Self::share_profile);
            add(
                "Offer a credential…",
                "offer-credential",
                Self::offer_credential,
            );
            add(
                "Ask for a credential…",
                "ask-credential",
                Self::ask_credential,
            );
            if c.chat.persona.is_some() {
                add("Reveal who you are…", "reveal", Self::reveal);
            }
            add("Clear chat…", "clear", Self::clear);
            add("Delete contact…", "delete", Self::delete_contact);
        }
        self.root.insert_action_group("chat", Some(&group));
        self.menu.set_menu_model(Some(&menu));
        self.menu.set_visible(menu.n_items() > 0);
    }

    // ----- Messages -----

    /// Reloads the history (coalescing reloads that arrive while loading).
    pub fn reload(self: &Rc<Self>) {
        if self.loading.replace(true) {
            self.again.set(true);
            return;
        }
        let Some(node) = self.node() else {
            self.loading.set(false);
            return;
        };
        let (group, device) = (self.group(), self.device());
        let weak = self.weak_self.clone();
        bg(
            move || match group {
                Some(g) => node.group_history(g, HISTORY).unwrap_or_default(),
                None => node.history(device, HISTORY).unwrap_or_default(),
            },
            move |entries| {
                let Some(t) = weak.upgrade() else { return };
                t.loading.set(false);
                if !t.loaded.replace(true) || *t.entries.borrow() != entries {
                    t.display_entries(entries);
                }
                if t.again.replace(false) {
                    t.reload();
                }
            },
        );
    }

    fn display_entries(&self, entries: Vec<HistoryEntry>) {
        let adj = self.scroll.vadjustment();
        let at_bottom =
            adj.value() + adj.page_size() >= adj.upper() - 40.0 || self.entries.borrow().is_empty();
        while let Some(child) = self.messages.first_child() {
            self.messages.remove(&child);
        }
        let node = self.node();
        let group = self.group().is_some();
        let mut last_day = String::new();
        let mut prev: Option<&HistoryEntry> = None;
        for e in &entries {
            let day = glib::DateTime::from_unix_local(i64::try_from(e.at_ms / 1000).unwrap_or(0))
                .ok()
                .and_then(|d| d.format("%A %-d %B %Y").ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            if day != last_day {
                let l = gtk::Label::new(Some(&day));
                l.add_css_class("day-separator");
                l.add_css_class("caption");
                l.add_css_class("dim-label");
                l.set_margin_top(10);
                l.set_margin_bottom(6);
                self.messages.append(&l);
                last_day = day;
                prev = None;
            }
            // Consecutive messages from the same sender within a few minutes
            // don't repeat the name.
            let continued = prev.is_some_and(|p| {
                p.outgoing == e.outgoing
                    && p.device == e.device
                    && e.at_ms.saturating_sub(p.at_ms) < 300_000
            });
            let sender = (group && !e.outgoing && !continued).then(|| {
                node.as_ref()
                    .map_or_else(|| core::short(&e.device), |n| Core::name_of(n, &e.device))
            });
            self.messages.append(&self.bubble(e, sender));
            prev = Some(e);
        }
        self.stack.set_visible_child_name(if entries.is_empty() {
            "empty"
        } else {
            "messages"
        });
        *self.entries.borrow_mut() = entries;
        if at_bottom {
            self.scroll_to_end();
        }
    }

    fn scroll_to_end(&self) {
        let scroll = self.scroll.clone();
        // After layout, so the new height is known.
        glib::idle_add_local_once(move || {
            let adj = scroll.vadjustment();
            adj.set_value(adj.upper());
            let scroll = scroll.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                let adj = scroll.vadjustment();
                adj.set_value(adj.upper());
            });
        });
    }

    fn bubble(&self, e: &HistoryEntry, sender: Option<String>) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.set_halign(if e.outgoing {
            gtk::Align::End
        } else {
            gtk::Align::Start
        });
        row.set_margin_top(if sender.is_some() { 8 } else { 2 });

        let bubble = gtk::Box::new(gtk::Orientation::Vertical, 4);
        bubble.add_css_class("bubble");
        bubble.add_css_class(if e.outgoing { "outgoing" } else { "incoming" });
        if let Some(s) = sender {
            let l = ui::label(&s, &["caption-heading", "accent"]);
            bubble.append(&l);
        }
        if let Some(f) = &e.file {
            bubble.append(&self.file_view(e, f));
            if !e.text.is_empty() {
                let caption = ui::label(&e.text, &["message-text"]);
                caption.set_selectable(true);
                bubble.append(&caption);
            }
        } else {
            let text = ui::label(&e.text, &["message-text"]);
            text.set_selectable(true);
            text.set_max_width_chars(60);
            // Links are clickable.
            if let Some(markup) = linkify(&e.text) {
                text.set_markup(&markup);
                // Links ask before opening in the browser.
                let weak = self.app.clone();
                text.connect_activate_link(move |_, uri| {
                    if let Some(app) = weak.upgrade() {
                        let (u, w) = (uri.to_owned(), app.window.clone());
                        ui::confirm(
                            &app.window,
                            "Open this link?",
                            uri,
                            "Open",
                            false,
                            move || {
                                gtk::UriLauncher::new(&u).launch(
                                    Some(&w),
                                    gio::Cancellable::NONE,
                                    |_| {},
                                );
                            },
                        );
                    }
                    glib::Propagation::Stop
                });
            }
            bubble.append(&text);
        }

        let meta = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        meta.set_halign(gtk::Align::End);
        let mut bits = vec![ui::time_label(e.at_ms)];
        if e.edited {
            bits.insert(0, "edited".into());
        }
        if e.disappearing {
            bits.push("⏱".into());
        }
        if e.outgoing {
            bits.push(if e.recipients > 0 && !e.delivered {
                format!("✓ {}/{}", e.delivered_to, e.recipients)
            } else if e.delivered {
                "✓✓".into()
            } else {
                "✓".into()
            });
        }
        let m = gtk::Label::new(Some(&bits.join(" ")));
        m.add_css_class("caption");
        m.add_css_class("bubble-meta");
        if e.outgoing && e.delivered && e.read {
            m.add_css_class("read");
        }
        meta.append(&m);
        bubble.append(&meta);
        row.append(&bubble);

        if !e.reactions.is_empty() {
            let reactions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            reactions.set_halign(if e.outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            });
            for r in &e.reactions {
                let b = gtk::ToggleButton::with_label(&format!("{} {}", r.emoji, r.count));
                b.set_active(r.mine);
                b.add_css_class("reaction");
                b.set_tooltip_text(Some(if r.mine {
                    "Take yours back"
                } else {
                    "Add yours"
                }));
                let (weak, entry, emoji, mine) =
                    (self.weak_self.clone(), e.clone(), r.emoji.clone(), r.mine);
                b.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.react(&entry, emoji.clone(), !mine);
                    }
                });
                reactions.append(&b);
            }
            row.append(&reactions);
        }

        // Right-click (or long-press) for reactions, copy, edit and delete.
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (weak, entry, anchor) = (self.weak_self.clone(), e.clone(), bubble.clone());
        click.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(t) = weak.upgrade() {
                t.message_menu(&anchor, &entry, x, y);
            }
        });
        bubble.add_controller(click);
        let press = gtk::GestureLongPress::new();
        press.set_touch_only(true);
        let (weak, entry, anchor) = (self.weak_self.clone(), e.clone(), bubble.clone());
        press.connect_pressed(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                t.message_menu(&anchor, &entry, x, y);
            }
        });
        bubble.add_controller(press);
        row
    }

    fn file_view(&self, e: &HistoryEntry, f: &threnody_ffi::FileInfo) -> gtk::Widget {
        let path = f
            .location
            .as_ref()
            .map(std::path::PathBuf::from)
            .filter(|p| p.exists());
        let key = (e.at_ms, e.device.clone());
        if core::is_image(&f.name)
            && let Some(p) = &path
        {
            if f.sensitive && !e.outgoing && !self.shown.borrow().contains(&key) {
                // Not even decoded until asked for.
                let b = gtk::Button::with_label("Sensitive photo · click to show");
                b.add_css_class("sensitive-cover");
                b.set_size_request(240, 160);
                let weak = self.weak_self.clone();
                b.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.shown.borrow_mut().push(key.clone());
                        let entries = t.entries.borrow().clone();
                        t.display_entries(entries);
                    }
                });
                return b.upcast();
            }
            let pic = gtk::Picture::for_filename(p);
            pic.set_can_shrink(true);
            pic.set_content_fit(gtk::ContentFit::Contain);
            pic.set_size_request(240, 180);
            pic.add_css_class("photo");
            let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
            frame.set_size_request(320, 240);
            pic.set_vexpand(true);
            frame.append(&pic);
            pic.set_cursor_from_name(Some("pointer"));
            pic.set_tooltip_text(Some(&f.name));
            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_PRIMARY);
            let (weak, p) = (self.weak_self.clone(), p.clone());
            click.connect_released(move |_, _, _, _| {
                if let Some(t) = weak.upgrade() {
                    t.open_file(&p);
                }
            });
            pic.add_controller(click);
            return frame.upcast();
        }
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("file-row");
        let icon = gtk::Image::from_icon_name(if core::is_image(&f.name) {
            "image-x-generic-symbolic"
        } else {
            "text-x-generic-symbolic"
        });
        icon.set_pixel_size(32);
        row.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let name = gtk::Label::builder()
            .label(&f.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(36)
            .build();
        name.add_css_class("heading");
        text.append(&name);
        let size = ui::label(
            &format!(
                "{}{}{}",
                ui::human_size(f.size),
                if f.sensitive { " · sensitive" } else { "" },
                if path.is_none() {
                    " · not on this device"
                } else {
                    ""
                }
            ),
            &["caption", "dim-label"],
        );
        text.append(&size);
        row.append(&text);
        if let Some(p) = path {
            // Clicking the file opens it.
            row.set_cursor_from_name(Some("pointer"));
            row.set_tooltip_text(Some("Open"));
            let open = gtk::GestureClick::new();
            open.set_button(gdk::BUTTON_PRIMARY);
            let (weak, p2) = (self.weak_self.clone(), p.clone());
            open.connect_released(move |_, _, _, _| {
                if let Some(t) = weak.upgrade() {
                    t.open_file(&p2);
                }
            });
            row.add_controller(open);
            let folder = gtk::Button::from_icon_name("folder-open-symbolic");
            folder.set_tooltip_text(Some("Show in folder"));
            folder.add_css_class("flat");
            folder.set_valign(gtk::Align::Center);
            let weak = self.weak_self.clone();
            folder.connect_clicked(move |_| {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                gtk::FileLauncher::new(Some(&gio::File::for_path(&p))).open_containing_folder(
                    Some(&app.window),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            });
            row.append(&folder);
        }
        row.upcast()
    }

    fn open_file(&self, path: &std::path::Path) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let weak = self.app.clone();
        gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
            Some(&app.window),
            gio::Cancellable::NONE,
            move |r| {
                if let (Err(_), Some(a)) = (r, weak.upgrade()) {
                    a.toast("No application can open this file");
                }
            },
        );
    }

    fn message_menu(self: &Rc<Self>, anchor: &gtk::Box, e: &HistoryEntry, x: f64, y: f64) {
        let pop = gtk::Popover::new();
        pop.set_parent(anchor);
        #[allow(clippy::cast_possible_truncation)]
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let mine_on = |emoji: &str| e.reactions.iter().any(|r| r.emoji == emoji && r.mine);
        if e.id != 0 {
            let emojis = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            for em in QUICK_REACTIONS {
                let btn = gtk::ToggleButton::with_label(em);
                btn.add_css_class("flat");
                btn.add_css_class("reaction-pick");
                let on = mine_on(em);
                btn.set_active(on);
                let (weak, entry, p) = (self.weak_self.clone(), e.clone(), pop.clone());
                btn.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.react(&entry, em.to_owned(), !on);
                    }
                    p.popdown();
                });
                emojis.append(&btn);
            }
            let more = gtk::MenuButton::builder()
                .icon_name("list-add-symbolic")
                .build();
            more.add_css_class("flat");
            more.set_tooltip_text(Some("More emoji"));
            let chooser = gtk::EmojiChooser::new();
            let (weak, entry, p) = (self.weak_self.clone(), e.clone(), pop.clone());
            chooser.connect_emoji_picked(move |_, em| {
                if let Some(t) = weak.upgrade() {
                    let on = entry.reactions.iter().any(|r| r.emoji == em && r.mine);
                    t.react(&entry, em.to_owned(), !on);
                }
                p.popdown();
            });
            more.set_popover(Some(&chooser));
            emojis.append(&more);
            b.append(&emojis);
            b.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        let item = |label: &str, f: Box<dyn Fn()>| {
            let btn = gtk::Button::with_label(label);
            btn.add_css_class("flat");
            if let Some(l) = btn.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
            let p = pop.clone();
            btn.connect_clicked(move |_| {
                p.popdown();
                f();
            });
            b.append(&btn);
        };
        if e.file.is_none() && !e.text.is_empty() {
            let (text, a) = (e.text.clone(), anchor.clone());
            item("Copy text", Box::new(move || a.clipboard().set_text(&text)));
        }
        let group = self.group().is_some();
        if e.outgoing && e.file.is_none() && e.id != 0 && !group {
            let (weak, entry) = (self.weak_self.clone(), e.clone());
            item(
                "Edit",
                Box::new(move || {
                    if let Some(t) = weak.upgrade() {
                        t.edit(&entry);
                    }
                }),
            );
        }
        let (weak, entry) = (self.weak_self.clone(), e.clone());
        item(
            "Delete for me",
            Box::new(move || {
                if let Some(t) = weak.upgrade() {
                    t.delete(&entry, false);
                }
            }),
        );
        if e.outgoing && e.id != 0 && !group {
            let (weak, entry) = (self.weak_self.clone(), e.clone());
            item(
                "Delete for everyone",
                Box::new(move || {
                    if let Some(t) = weak.upgrade() {
                        t.delete(&entry, true);
                    }
                }),
            );
        }
        pop.set_child(Some(&b));
        pop.popup();
    }

    fn react(self: &Rc<Self>, e: &HistoryEntry, emoji: String, add: bool) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let (id, group, device) = (e.id, self.group(), self.device());
        app.run(
            self.persona(),
            "react",
            move |n| {
                match group {
                    Some(g) => n.react_in_group(g, id, emoji, add),
                    None => n.react(device, id, emoji, add),
                }
                .map(|_| ())
            },
            None,
        );
    }

    fn edit(self: &Rc<Self>, e: &HistoryEntry) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let (weak, id) = (self.weak_self.clone(), e.id);
        ui::ask_text(
            &app.window,
            "Edit message",
            "They see it marked as edited, if they're running a current version.",
            "Message",
            &e.text,
            "Save",
            move |text| {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                if text.is_empty() {
                    return;
                }
                let device = t.device();
                app.run(
                    t.persona(),
                    "edit",
                    move |n| n.edit_message(device, id, text).map(|_| ()),
                    None,
                );
            },
        );
    }

    fn delete(self: &Rc<Self>, e: &HistoryEntry, everyone: bool) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let (group, device, entry) = (self.group(), self.device(), e.clone());
        let ok = everyone.then(|| {
            "Deleted. Their devices delete it too, if they're running a current version.".to_owned()
        });
        app.run(
            self.persona(),
            "delete",
            move |n| {
                match group {
                    Some(g) => n.delete_group_entry(g, entry.at_ms, entry.device),
                    None if entry.id != 0 => n.delete_messages(device, vec![entry.id], everyone),
                    None => n.delete_entry(device, entry.at_ms, entry.device),
                }
                .map(|_| ())
            },
            ok,
        );
    }

    // ----- Sending -----

    fn send(self: &Rc<Self>) {
        let buf = self.input.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .trim()
            .to_owned();
        if text.is_empty() {
            return;
        }
        let Some(node) = self.node() else { return };
        // Stop typing indicator when sending.
        if self.sending_typing.replace(false) {
            self.notify_typing(false);
        }
        if let Some(id) = self.typing_timeout.borrow_mut().take() {
            id.remove();
        }
        buf.set_text("");
        let (group, device) = (self.group(), self.device());
        let weak = self.weak_self.clone();
        let sent = text.clone();
        bg(
            move || match group {
                Some(g) => node
                    .send_group_text(g, sent)
                    .map(|()| None)
                    .map_err(|e| e.to_string()),
                // It reaches the contact directly, by relay, or sealed for mailboxes.
                None => match node.send_text(device, sent) {
                    Ok(0) => Ok(Some(
                        "Not delivered: they're offline and no mutual contact can hold it.",
                    )),
                    Ok(_) => Ok(None),
                    Err(e) => Err(e.to_string()),
                },
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                let error = match r {
                    Ok(None) => None,
                    Ok(Some(msg)) => Some(msg.to_owned()),
                    Err(e) => Some(format!("Couldn't send: {e}")),
                };
                if let Some(e) = error {
                    t.toast(&e);
                    let buf = t.input.buffer();
                    if buf.char_count() == 0 {
                        buf.set_text(&text);
                    }
                }
                t.reload();
                if let Some(a) = t.app.upgrade() {
                    a.refresh_soon();
                }
            },
        );
        self.scroll_to_end();
    }

    fn pick_files(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Send photos or files")
            .modal(true)
            .build();
        let weak = self.weak_self.clone();
        dialog.open_multiple(Some(&app.window), gio::Cancellable::NONE, move |r| {
            let (Some(t), Ok(files)) = (weak.upgrade(), r) else {
                return;
            };
            let paths: Vec<std::path::PathBuf> = (0..files.n_items())
                .filter_map(|i| files.item(i).and_downcast::<gio::File>()?.path())
                .collect();
            if !paths.is_empty() {
                t.confirm_files(paths);
            }
        });
    }

    /// Asks for a caption and whether the files are sensitive, then sends.
    fn confirm_files(self: &Rc<Self>, paths: Vec<std::path::PathBuf>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let images = paths
            .iter()
            .filter(|p| core::is_image(&p.file_name().unwrap_or_default().to_string_lossy()))
            .count();
        let what = match (paths.len(), images == paths.len()) {
            (1, true) => "a photo".to_owned(),
            (1, false) => "a file".to_owned(),
            (n, true) => format!("{n} photos"),
            (n, false) => format!("{n} files"),
        };
        let names = paths
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let d = ui::alert(
            &format!("Send {what}"),
            &names,
            &[("cancel", "Cancel"), ("send", "Send")],
        );
        d.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let caption = gtk::Entry::builder()
            .placeholder_text("Add a message")
            .activates_default(true)
            .build();
        let sensitive = gtk::CheckButton::with_label(if images > 0 {
            "Sensitive: they see it covered until they click it"
        } else {
            "Sensitive: shown covered until opened"
        });
        b.append(&caption);
        b.append(&sensitive);
        if images > 0 && app.core.settings().flag(settings::STRIP) {
            b.append(&ui::label(
                "Location and camera details are removed before sending.",
                &["caption", "dim-label"],
            ));
        }
        d.set_extra_child(Some(&b));
        let weak = self.weak_self.clone();
        d.connect_response(Some("send"), move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.send_files(
                    paths.clone(),
                    caption.text().trim().to_owned(),
                    sensitive.is_active(),
                );
            }
        });
        d.present(Some(&app.window));
    }

    fn send_files(
        self: &Rc<Self>,
        paths: Vec<std::path::PathBuf>,
        caption: String,
        sensitive: bool,
    ) {
        let Some(node) = self.node() else { return };
        let (group, device) = (self.group(), self.device());
        self.toast(if paths.len() == 1 {
            "Sending…"
        } else {
            "Sending files…"
        });
        let weak = self.weak_self.clone();
        bg(
            move || {
                let album = if paths.len() > 1 {
                    threnody_ffi::album_id()
                } else {
                    0
                };
                let max = node.max_file_size();
                let mut failed = Vec::new();
                for (i, p) in paths.iter().enumerate() {
                    let name = p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let data = match std::fs::read(p) {
                        Ok(d) if d.len() as u64 <= max => d,
                        Ok(_) => {
                            failed.push(format!("{name} is larger than {}", ui::human_size(max)));
                            continue;
                        }
                        Err(e) => {
                            failed.push(format!("{name}: {e}"));
                            continue;
                        }
                    };
                    let options = FileOptions {
                        sensitive,
                        // The album's caption is its first entry's text.
                        caption: if i == 0 {
                            caption.clone()
                        } else {
                            String::new()
                        },
                        album,
                    };
                    let location = Some(p.display().to_string());
                    let r = match &group {
                        Some(g) => {
                            node.send_group_file(g.clone(), name.clone(), data, location, options)
                        }
                        None => {
                            node.send_file(device.clone(), name.clone(), data, location, options)
                        }
                    };
                    if let Err(e) = r {
                        failed.push(format!("{name}: {e}"));
                    }
                }
                failed
            },
            move |failed| {
                let Some(t) = weak.upgrade() else { return };
                if !failed.is_empty() {
                    t.toast(&format!("Couldn't send: {}", failed.join("; ")));
                }
                t.reload();
                if let Some(a) = t.app.upgrade() {
                    a.refresh_soon();
                }
            },
        );
    }

    // ----- Conversation actions -----

    fn run(
        self: &Rc<Self>,
        what: &str,
        work: impl FnOnce(&Node) -> Result<(), threnody_ffi::ThrenodyError> + Send + 'static,
        ok: Option<String>,
    ) {
        if let Some(app) = self.app.upgrade() {
            app.run(self.persona(), what, work, ok);
        }
    }

    fn accept(self: &Rc<Self>) {
        let d = self.device();
        self.run("accept", move |n| n.accept_contact(d), None);
    }

    fn block(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let title = self.conv.borrow().title.clone();
        let weak = self.weak_self.clone();
        ui::confirm(
            &app.window,
            &format!("Block {title}?"),
            "Their sessions are refused and the conversation is deleted. Approving them again unblocks them.",
            "Block",
            true,
            move || {
                let Some(t) = weak.upgrade() else { return };
                let d = t.device();
                t.run("block", move |n| n.block_contact(d), None);
                if let Some(a) = t.app.upgrade() {
                    a.close_chat();
                }
            },
        );
    }

    fn delete_request(self: &Rc<Self>) {
        let d = self.device();
        self.run("delete", move |n| n.delete_request(d), None);
        if let Some(a) = self.app.upgrade() {
            a.close_chat();
        }
    }

    fn approve(self: &Rc<Self>) {
        let d = self.device();
        self.run("approve", move |n| n.set_approval(d, true), None);
    }

    fn revoke(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let title = self.conv.borrow().title.clone();
        let weak = self.weak_self.clone();
        ui::confirm(
            &app.window,
            "Revoke approval?",
            &format!(
                "{title} will no longer be able to find you nearby, relay for you or hold your messages."
            ),
            "Revoke",
            true,
            move || {
                if let Some(t) = weak.upgrade() {
                    let d = t.device();
                    t.run("revoke", move |n| n.set_approval(d, false), None);
                }
            },
        );
    }

    fn rename(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let title = self.conv.borrow().title.clone();
        let weak = self.weak_self.clone();
        ui::ask_text(
            &app.window,
            "Name this contact",
            "Only you see this name.",
            "Name",
            &title,
            "Save",
            move |name| {
                if let (Some(t), false) = (weak.upgrade(), name.is_empty()) {
                    let d = t.device();
                    t.run("rename", move |n| n.set_name(d, name), None);
                }
            },
        );
    }

    /// Compares the safety number of an unverified device (else the
    /// contact's), and offers the next one once it matches.
    fn safety(self: &Rc<Self>) {
        let c = self.conv.borrow().clone();
        let Some(node) = self.node() else { return };
        let target = c
            .unverified
            .first()
            .cloned()
            .unwrap_or_else(|| c.device.clone());
        let many = c.devices.len() > 1;
        let weak = self.weak_self.clone();
        let t2 = target.clone();
        bg(
            move || node.safety_number(t2).map_err(|e| e.to_string()),
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                let number = match r {
                    Ok(n) => n,
                    Err(e) => return t.toast(&e),
                };
                let heading = if many {
                    format!("Safety number: device {}", core::short(&target))
                } else {
                    "Safety number".into()
                };
                let body = format!(
                    "Compare this with the number on {}'s {}screen, in person or on a call. \
                     If they match, no one is intercepting your messages.",
                    c.title,
                    if many {
                        format!("device {} ", core::short(&target))
                    } else {
                        String::new()
                    }
                );
                let d = ui::alert(
                    &heading,
                    &body,
                    &[("later", "Not now"), ("match", "They match")],
                );
                d.set_response_appearance("match", adw::ResponseAppearance::Suggested);
                let digits = gtk::Label::new(Some(&ui::safety_groups(&number)));
                digits.add_css_class("safety-number");
                digits.set_selectable(true);
                digits.set_justify(gtk::Justification::Center);
                d.set_extra_child(Some(&digits));
                let weak = Rc::downgrade(&t);
                d.connect_response(Some("match"), move |_, _| {
                    let Some(t) = weak.upgrade() else { return };
                    let Some(node) = t.node() else { return };
                    let (target, weak) = (target.clone(), Rc::downgrade(&t));
                    let more_than_one = t.conv.borrow().unverified.len() > 1;
                    bg(
                        move || node.mark_verified(target).map_err(|e| e.to_string()),
                        move |r| {
                            let Some(t) = weak.upgrade() else { return };
                            match r {
                                Ok(()) => {
                                    t.toast("Verified");
                                    if let Some(a) = t.app.upgrade() {
                                        a.refresh();
                                    }
                                    // More devices to check? Offer the next one.
                                    if more_than_one {
                                        let mut c = t.conv.borrow().clone();
                                        c.unverified.remove(0);
                                        t.update(c);
                                        t.safety();
                                    }
                                }
                                Err(e) => t.toast(&format!("Couldn't verify: {e}")),
                            }
                        },
                    );
                });
                d.present(Some(&app.window));
            },
        );
    }

    fn disappearing(self: &Rc<Self>) {
        let Some(node) = self.node() else { return };
        let (group, device) = (self.group(), self.device());
        let weak = self.weak_self.clone();
        bg(
            {
                let (group, device) = (group.clone(), device.clone());
                move || match group {
                    Some(g) => node.group_disappearing(g).ok().flatten(),
                    None => node.disappearing(device).ok().flatten(),
                }
            },
            move |current| {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                let options: Vec<String> = settings::TIMERS
                    .iter()
                    .map(|(l, _)| (*l).to_owned())
                    .collect();
                let at = settings::TIMERS.iter().position(|(_, s)| *s == current);
                let weak = Rc::downgrade(&t);
                ui::choose(
                    &app.window,
                    "Disappearing messages",
                    "New messages are deleted on both sides this long after they're sent.",
                    &options,
                    at,
                    move |i| {
                        let Some(t) = weak.upgrade() else { return };
                        let secs = settings::TIMERS[i].1;
                        let (group, device) = (group.clone(), device.clone());
                        t.run(
                            "set the timer",
                            move |n| match group {
                                Some(g) => n.set_group_disappearing(g, secs),
                                None => n.set_disappearing(device, secs),
                            },
                            Some(format!(
                                "New messages: {}",
                                settings::timer_label(secs).to_lowercase()
                            )),
                        );
                    },
                );
            },
        );
    }

    fn share_profile(self: &Rc<Self>) {
        let Some(node) = self.node() else { return };
        let device = self.device();
        let title = self.conv.borrow().title.clone();
        let persona = self.persona();
        let weak = self.weak_self.clone();
        bg(
            {
                let device = device.clone();
                move || (node.profile(), node.shared_with(device).unwrap_or_default())
            },
            move |(attrs, shared)| {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                if attrs.is_empty() {
                    let d = ui::alert(
                        "Your profile is empty",
                        &format!(
                            "Add details such as your name first; then choose which ones {title} sees."
                        ),
                        &[("cancel", "Cancel"), ("add", "Add details")],
                    );
                    let weak = Rc::downgrade(&app);
                    d.connect_response(Some("add"), move |_, _| {
                        if let Some(a) = weak.upgrade() {
                            a.edit_profile(persona.clone());
                        }
                    });
                    d.present(Some(&app.window));
                    return;
                }
                let d = ui::alert(
                    &format!("What {title} sees"),
                    "Nothing is shared until you choose. They may keep what they've seen.",
                    &[("cancel", "Cancel"), ("save", "Save")],
                );
                d.set_response_appearance("save", adw::ResponseAppearance::Suggested);
                let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
                let checks: Vec<(String, gtk::CheckButton)> = attrs
                    .iter()
                    .map(|a| {
                        let c = gtk::CheckButton::with_label(&format!("{}: {}", a.key, a.value));
                        c.set_active(shared.contains(&a.key));
                        b.append(&c);
                        (a.key.clone(), c)
                    })
                    .collect();
                d.set_extra_child(Some(&b));
                let weak = Rc::downgrade(&t);
                d.connect_response(Some("save"), move |_, _| {
                    let Some(t) = weak.upgrade() else { return };
                    let keys: Vec<String> = checks
                        .iter()
                        .filter(|(_, c)| c.is_active())
                        .map(|(k, _)| k.clone())
                        .collect();
                    let ok = if keys.is_empty() {
                        format!("{title} sees none of your profile")
                    } else {
                        format!("{title} sees your {}", keys.join(", "))
                    };
                    let device = device.clone();
                    t.run("share", move |n| n.set_shared_with(device, keys), Some(ok));
                });
                d.present(Some(&app.window));
            },
        );
    }

    /// Vouches for attributes of this contact with a credential they keep.
    fn offer_credential(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let title = self.conv.borrow().title.clone();
        let d = ui::alert(
            &format!("Offer {title} a credential"),
            "You vouch for these attributes. They can later prove any of them to others, \
             without showing the rest.",
            &[("cancel", "Cancel"), ("offer", "Offer")],
        );
        d.set_response_appearance("offer", adw::ResponseAppearance::Suggested);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let schema = gtk::Entry::builder()
            .placeholder_text("Kind, e.g. hackspace/member")
            .build();
        let attrs = gtk::TextView::builder()
            .height_request(90)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        attrs.buffer().set_text("name=\nmember=yes");
        attrs.add_css_class("card");
        let days = gtk::SpinButton::with_range(1.0, 3650.0, 1.0);
        days.set_value(365.0);
        b.append(&schema);
        b.append(&ui::label(
            "One attribute per line, as key=value:",
            &["caption", "dim-label"],
        ));
        b.append(&attrs);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&ui::label("Valid for (days)", &[]));
        row.append(&days);
        b.append(&row);
        d.set_extra_child(Some(&b));
        let weak = self.weak_self.clone();
        d.connect_response(Some("offer"), move |_, _| {
            let Some(t) = weak.upgrade() else { return };
            let buf = attrs.buffer();
            let text = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .to_string();
            let list: Vec<threnody_ffi::ProfileAttr> = text
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim(), v.trim()))
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .map(|(k, v)| threnody_ffi::ProfileAttr {
                    key: k.into(),
                    value: v.into(),
                })
                .collect();
            let (schema, days, device) = (
                schema.text().trim().to_owned(),
                days.value() as u32,
                t.device(),
            );
            if schema.is_empty() || list.is_empty() {
                return t.toast("A kind and at least one attribute are needed");
            }
            t.run(
                "offer the credential",
                move |n| n.offer_credential(device, schema, list, days).map(|_| ()),
                Some("Offered".into()),
            );
        });
        d.present(Some(&app.window));
    }

    /// Asks this contact to prove attributes of a credential.
    fn ask_credential(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let title = self.conv.borrow().title.clone();
        let d = ui::alert(
            &format!("Ask {title} to prove something"),
            "They choose whether to answer, and with what. You learn only what they show.",
            &[("cancel", "Cancel"), ("ask", "Ask")],
        );
        d.set_response_appearance("ask", adw::ResponseAppearance::Suggested);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let schema = gtk::Entry::builder()
            .placeholder_text("Kind, e.g. hackspace/member")
            .build();
        let keys = gtk::Entry::builder()
            .placeholder_text("Attributes, comma-separated (optional)")
            .build();
        b.append(&schema);
        b.append(&keys);
        d.set_extra_child(Some(&b));
        let weak = self.weak_self.clone();
        d.connect_response(Some("ask"), move |_, _| {
            let Some(t) = weak.upgrade() else { return };
            let schema = schema.text().trim().to_owned();
            if schema.is_empty() {
                return;
            }
            let keys: Vec<String> = keys
                .text()
                .split(',')
                .map(|k| k.trim().to_owned())
                .filter(|k| !k.is_empty())
                .collect();
            let device = t.device();
            t.run(
                "ask",
                move |n| n.ask_credential(device, schema, keys).map(|_| ()),
                Some("Asked".into()),
            );
        });
        d.present(Some(&app.window));
    }

    /// Proves to them that this anonymous identity is us. Can't be undone.
    fn reveal(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(persona) = self.persona() else {
            return;
        };
        let title = self.conv.borrow().title.clone();
        let weak = self.weak_self.clone();
        ui::confirm(
            &app.window,
            "Reveal who you are?",
            &format!(
                "{title} will get proof, signed by your main identity, that this anonymous identity is you, \
                 and an invite to reach you. They can show that proof to anyone. This can't be taken back."
            ),
            "Reveal",
            true,
            move || {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                let (core, device, persona, title) =
                    (app.core.clone(), t.device(), persona.clone(), title.clone());
                let weak = Rc::downgrade(&t);
                bg(
                    move || {
                        let p = core.node(Some(&persona))?;
                        let invite = core.invite(None).ok();
                        core.main
                            .reveal_through(p, device, invite)
                            .map_err(|e| e.to_string())
                    },
                    move |r| {
                        if let Some(t) = weak.upgrade() {
                            t.toast(&match r {
                                Ok(()) => format!("{title} now knows who you are"),
                                Err(e) => format!("Couldn't reveal: {e}"),
                            });
                        }
                    },
                );
            },
        );
    }

    /// Adds the identity they revealed, as a contact of our main identity.
    fn add_revealed(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(invite) = self.conv.borrow().revealed_invite.clone() else {
            return;
        };
        app.connect(invite);
    }

    fn clear(self: &Rc<Self>) {
        if let Some(app) = self.app.upgrade() {
            let c = self.conv.borrow().clone();
            app.clear_chat(&c);
        }
    }

    fn delete_contact(self: &Rc<Self>) {
        if let Some(app) = self.app.upgrade() {
            let c = self.conv.borrow().clone();
            app.delete_contact(&c);
        }
    }

    // ----- Groups -----

    fn join_group(self: &Rc<Self>) {
        let Some(g) = self.group() else { return };
        self.run(
            "join",
            move |n| n.accept_group_invite(g),
            Some("Joined".into()),
        );
    }

    fn decline_group(self: &Rc<Self>) {
        let Some(g) = self.group() else { return };
        self.run("decline", move |n| n.decline_group_invite(g), None);
        if let Some(a) = self.app.upgrade() {
            a.close_chat();
        }
    }

    fn members(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(info) = self.conv.borrow().group.clone() else {
            return;
        };
        let Some(node) = self.node() else { return };
        let me = node.device_fingerprint();
        let names: Vec<String> = info
            .members
            .iter()
            .map(|m| {
                let mut n = Core::name_of(&node, m);
                if *m == info.owner {
                    n.push_str(" · owner");
                }
                n
            })
            .collect();
        let body = if info.owned {
            "Click a member to remove them."
        } else {
            "Only the owner can add and remove members."
        };
        let weak = self.weak_self.clone();
        let members = info.members.clone();
        let (heading, shown) = (info.name.clone(), names.clone());
        ui::choose(&app.window, &heading, body, &shown, None, move |i| {
            let Some(t) = weak.upgrade() else { return };
            let Some(app) = t.app.upgrade() else { return };
            let fp = members[i].clone();
            if !info.owned || fp == me {
                return;
            }
            let name = names[i].clone();
            let (weak, gname) = (Rc::downgrade(&t), info.name.clone());
            ui::confirm(
                &app.window,
                &format!("Remove {name}?"),
                &format!(
                    "They stop receiving new messages in {gname}. You can invite them again later."
                ),
                "Remove",
                true,
                move || {
                    if let (Some(t), Some(g)) =
                        (weak.upgrade(), weak.upgrade().and_then(|t| t.group()))
                    {
                        let fp = fp.clone();
                        t.run("remove", move |n| n.remove_from_group(g, fp), None);
                    }
                },
            );
        });
    }

    fn invite_to_group(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(info) = self.conv.borrow().group.clone() else {
            return;
        };
        let persona = self.persona();
        // Contacts of the same identity who aren't in the group yet.
        let candidates: Vec<Conversation> = app
            .core
            .conversations()
            .into_iter()
            .filter(|c| {
                c.chat.persona == persona
                    && c.group.is_none()
                    && c.invite.is_none()
                    && c.accepted
                    && !c.devices.iter().any(|d| info.members.contains(d))
            })
            .collect();
        if candidates.is_empty() {
            return self.toast("All your contacts are already in this group");
        }
        let names: Vec<String> = candidates.iter().map(|c| c.title.clone()).collect();
        let weak = self.weak_self.clone();
        ui::choose(
            &app.window,
            &format!("Invite to {}", info.name),
            "Contacts who haven't approved you are asked before they join.",
            &names,
            None,
            move |i| {
                let Some(t) = weak.upgrade() else { return };
                let (Some(g), device) = (t.group(), candidates[i].device.clone()) else {
                    return;
                };
                let ok = format!("Invited {}", candidates[i].title);
                t.run("invite", move |n| n.invite_to_group(g, device), Some(ok));
            },
        );
    }

    fn leave_group(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(info) = self.conv.borrow().group.clone() else {
            return;
        };
        let weak = self.weak_self.clone();
        ui::confirm(
            &app.window,
            &if info.owned {
                format!("Delete {}?", info.name)
            } else {
                format!("Leave {}?", info.name)
            },
            if info.owned {
                "Everyone is removed and the group ends. Its history stays on your devices."
            } else {
                "You stop receiving its messages. Its history stays on your devices; the owner can invite you again."
            },
            if info.owned { "Delete" } else { "Leave" },
            true,
            move || {
                let Some(t) = weak.upgrade() else { return };
                let Some(g) = t.group() else { return };
                t.run("leave", move |n| n.leave_group(g), None);
            },
        );
    }

    /// The conversation's photos, files and links, newest first.
    fn media(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let entries = self.entries.borrow().clone();
        let d = adw::Dialog::builder()
            .title("Media, files and links")
            .content_width(560)
            .content_height(600)
            .build();
        let page = adw::PreferencesPage::new();
        let photos = adw::PreferencesGroup::builder().title("Photos").build();
        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .min_children_per_line(3)
            .max_children_per_line(5)
            .row_spacing(6)
            .column_spacing(6)
            .build();
        let files = adw::PreferencesGroup::builder().title("Files").build();
        let links = adw::PreferencesGroup::builder().title("Links").build();
        let (mut n_photos, mut n_files, mut n_links) = (0, 0, 0);
        for e in entries.iter().rev() {
            if let Some(f) = &e.file {
                let path = f
                    .location
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.exists());
                if core::is_image(&f.name)
                    && let Some(p) = path
                {
                    n_photos += 1;
                    let w: gtk::Widget = if f.sensitive && !e.outgoing {
                        gtk::Button::with_label("Sensitive").upcast()
                    } else {
                        let pic = gtk::Picture::for_filename(&p);
                        pic.set_content_fit(gtk::ContentFit::Cover);
                        pic.set_size_request(96, 96);
                        let b = gtk::Button::new();
                        b.set_child(Some(&pic));
                        b.upcast()
                    };
                    w.set_size_request(96, 96);
                    if let Ok(b) = w.clone().downcast::<gtk::Button>() {
                        let weak = self.weak_self.clone();
                        b.add_css_class("flat");
                        b.connect_clicked(move |_| {
                            if let Some(t) = weak.upgrade() {
                                t.open_file(&p);
                            }
                        });
                    }
                    flow.append(&w);
                    continue;
                }
                n_files += 1;
                let row = ui::link_button_row(
                    &f.name,
                    &format!("{} · {}", ui::human_size(f.size), ui::time_label(e.at_ms)),
                );
                if let Some(p) = path {
                    let weak = self.weak_self.clone();
                    row.connect_activated(move |_| {
                        if let Some(t) = weak.upgrade() {
                            t.open_file(&p);
                        }
                    });
                } else {
                    row.set_activatable(false);
                }
                files.add(&row);
            }
            for url in urls(&e.text) {
                n_links += 1;
                let row = ui::link_button_row(&url, &ui::time_label(e.at_ms));
                let (w, u) = (app.window.clone(), url.clone());
                row.connect_activated(move |_| {
                    let u = u.clone();
                    let w2 = w.clone();
                    ui::confirm(
                        &w,
                        "Open this link?",
                        &u.clone(),
                        "Open",
                        false,
                        move || {
                            gtk::UriLauncher::new(&u).launch(
                                Some(&w2),
                                gio::Cancellable::NONE,
                                |_| {},
                            );
                        },
                    );
                });
                links.add(&row);
            }
        }
        if n_photos > 0 {
            photos.add(&flow);
            page.add(&photos);
        }
        if n_files > 0 {
            page.add(&files);
        }
        if n_links > 0 {
            page.add(&links);
        }
        if n_photos + n_files + n_links == 0 {
            let none = adw::PreferencesGroup::builder()
                .description("No photos, files or links in this conversation yet.")
                .build();
            page.add(&none);
        }
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        d.set_child(Some(&view));
        d.present(Some(&app.window));
    }
}

/// The http(s) links in `text`.
fn urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|w| w.starts_with("https://") || w.starts_with("http://"))
        .map(|w| {
            w.trim_end_matches(['.', ',', ')', '!', '?', ';', ':'])
                .to_owned()
        })
        .collect()
}

/// Pango markup for `text` with its links clickable, or None without links.
fn linkify(text: &str) -> Option<String> {
    let links = urls(text);
    if links.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut rest = text;
    for url in links {
        let Some(at) = rest.find(&url) else { continue };
        out.push_str(&glib::markup_escape_text(&rest[..at]));
        let esc = glib::markup_escape_text(&url);
        out.push_str(&format!("<a href=\"{esc}\">{esc}</a>"));
        rest = &rest[at + url.len()..];
    }
    out.push_str(&glib::markup_escape_text(rest));
    Some(out)
}
