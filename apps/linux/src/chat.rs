//! One open conversation: header, trust banner, messages and composer.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use threnody_ffi::{Clip, FileOptions, HistoryEntry};

use crate::core::{self, ChatRef, Conversation, Core, Node, Target};
use crate::ui::{self, bg};
use crate::window::App;
use crate::{clip, gifs, settings};

/// How many messages a chat loads (more to reach an older search match).
const HISTORY: u32 = 1000;
/// The most matches a search in a chat finds.
const SEARCH_LIMIT: u32 = 500;
/// We're taken to have stopped typing after this long without a keystroke.
const TYPING_IDLE: std::time::Duration = std::time::Duration::from_secs(5);
/// While typing goes on, the contact is told again this often.
const TYPING_AGAIN: std::time::Duration = std::time::Duration::from_secs(4);
/// The contact's "…" goes after this long without word.
const PEER_TYPING: std::time::Duration = std::time::Duration::from_secs(8);
/// A banner or menu action on the open chat.
type Action = fn(&Rc<ChatView>);
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];
/// A message by its time and sending device, as search finds it.
pub type Key = (u64, String);

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
    /// How many messages are loaded; grows to take in an older message
    /// to jump to.
    limit: Cell<u32>,
    /// The bubbles on screen, for search to mark and scroll to.
    bubbles: RefCell<Vec<(Key, gtk::Box)>>,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    search_count: gtk::Label,
    search_nav: gtk::Box,
    /// The messages matching the search, newest first.
    hits: RefCell<Vec<Key>>,
    /// Which of them is selected.
    hit_at: Cell<usize>,
    /// The highlighted message: the selected match, or one jumped to.
    current: RefCell<Option<Key>>,
    /// A message to scroll to once it is loaded.
    jump: RefCell<Option<Key>>,
    /// The match to select when the search under way finishes.
    want: RefCell<Option<Key>>,
    /// Counts searches, so a slow one doesn't overwrite a newer one.
    search_gen: Cell<u32>,
    loading: Cell<bool>,
    again: Cell<bool>,
    /// The history was shown at least once.
    loaded: Cell<bool>,
    /// Whether the main identity already has the contact they revealed.
    revealed_known: Cell<bool>,
    /// Sensitive photos the user uncovered, by time and sender.
    shown: RefCell<Vec<(u64, String)>>,
    /// Voice and video messages by file, kept across redraws so one
    /// playing goes on playing when a message arrives.
    players: RefCell<HashMap<std::path::PathBuf, gtk::MediaFile>>,
    /// Typing indicator row (shown when peer is typing in 1:1 chats).
    typing_row: gtk::Box,
    /// Whether the peer is currently typing.
    peer_typing: Cell<bool>,
    /// Set of remote message ids we've already sent read receipts for.
    sent_read: RefCell<HashSet<u64>>,
    /// Fires when we've stopped typing for a while.
    typing_timeout: RefCell<Option<glib::SourceId>>,
    /// Whether the peer was last told we are typing.
    sending_typing: Cell<bool>,
    /// When it was last told so (it is told again while typing goes on).
    typing_sent_at: Cell<Option<std::time::Instant>>,
    /// Hides the peer's "…" if its "stopped" never comes.
    peer_quiet: RefCell<Option<glib::SourceId>>,
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

        // Search in this conversation (Ctrl+F).
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search this conversation")
            .hexpand(true)
            .build();
        let search_count = gtk::Label::new(None);
        search_count.add_css_class("dim-label");
        search_count.add_css_class("numeric");
        let older = gtk::Button::from_icon_name("go-up-symbolic");
        older.set_tooltip_text(Some("Older match (Enter)"));
        older.add_css_class("flat");
        let newer = gtk::Button::from_icon_name("go-down-symbolic");
        newer.set_tooltip_text(Some("Newer match (Shift+Enter)"));
        newer.add_css_class("flat");
        let search_nav = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        search_nav.append(&older);
        search_nav.append(&newer);
        search_nav.set_sensitive(false);
        let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        search_box.append(&search_entry);
        search_box.append(&search_count);
        search_box.append(&search_nav);
        let search_bar = gtk::SearchBar::builder()
            .child(
                &adw::Clamp::builder()
                    .maximum_size(600)
                    .child(&search_box)
                    .build(),
            )
            .build();
        search_bar.connect_entry(&search_entry);
        let find = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search (Ctrl+F)")
            .build();
        find.bind_property("active", &search_bar, "search-mode-enabled")
            .bidirectional()
            .sync_create()
            .build();
        header.pack_end(&find);
        // Calls are with contacts (not groups, yet): voice, or video.
        if conv.group.is_none() && conv.invite.is_none() {
            for (icon, tip, video) in [
                ("camera-video-symbolic", "Video call", true),
                ("call-start-symbolic", "Call", false),
            ] {
                let call = gtk::Button::from_icon_name(icon);
                call.set_tooltip_text(Some(tip));
                let weak_app = Rc::downgrade(app);
                let c = conv.clone();
                call.connect_clicked(move |_| {
                    if let Some(app) = weak_app.upgrade() {
                        app.start_call(&c, video);
                    }
                });
                header.pack_end(&call);
            }
        }

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
        // Emoji go in at the cursor; the chooser stays for more.
        let emoji = gtk::MenuButton::builder()
            .icon_name("face-smile-symbolic")
            .tooltip_text("Emoji")
            .valign(gtk::Align::End)
            .build();
        emoji.add_css_class("flat");
        emoji.add_css_class("circular");
        let chooser = gtk::EmojiChooser::new();
        let buffer = input.buffer();
        chooser.connect_emoji_picked(move |_, em| {
            buffer.delete_selection(true, true);
            buffer.insert_at_cursor(em);
        });
        emoji.set_popover(Some(&chooser));
        let gif = gtk::Button::builder()
            .label("GIF")
            .tooltip_text("Send a GIF")
            .valign(gtk::Align::End)
            .build();
        gif.add_css_class("flat");
        gif.add_css_class("gif-button");
        let voice = gtk::Button::from_icon_name("audio-input-microphone-symbolic");
        voice.set_tooltip_text(Some("Record a voice message"));
        voice.add_css_class("flat");
        voice.add_css_class("circular");
        voice.set_valign(gtk::Align::End);
        let video = gtk::Button::from_icon_name("camera-web-symbolic");
        video.set_tooltip_text(Some("Record a video message"));
        video.add_css_class("flat");
        video.add_css_class("circular");
        video.set_valign(gtk::Align::End);
        let send = gtk::Button::from_icon_name("go-up-symbolic");
        send.set_tooltip_text(Some("Send (Enter)"));
        send.add_css_class("suggested-action");
        send.add_css_class("circular");
        send.set_valign(gtk::Align::End);
        // Typing indicator row (shown when peer is typing in 1:1 chats).
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
        composer.append(&emoji);
        composer.append(&gif);
        composer.append(&overlay);
        composer.append(&voice);
        composer.append(&video);
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
        root.add_top_bar(&search_bar);
        root.set_content(Some(&body));
        root.add_bottom_bar(&composer_clamp);

        let this = Rc::new_cyclic(|weak_self| Self {
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
            limit: Cell::new(HISTORY),
            bubbles: RefCell::new(Vec::new()),
            search_bar,
            search_entry,
            search_count,
            search_nav,
            hits: RefCell::new(Vec::new()),
            hit_at: Cell::new(0),
            current: RefCell::new(None),
            jump: RefCell::new(None),
            want: RefCell::new(None),
            search_gen: Cell::new(0),
            loading: Cell::new(false),
            again: Cell::new(false),
            loaded: Cell::new(false),
            revealed_known: Cell::new(true),
            shown: RefCell::new(Vec::new()),
            players: RefCell::new(HashMap::new()),
            typing_row,
            peer_typing: Cell::new(false),
            sent_read: RefCell::new(HashSet::new()),
            typing_timeout: RefCell::new(None),
            sending_typing: Cell::new(false),
            weak_self: weak_self.clone(),
            peer_quiet: RefCell::new(None),
            typing_sent_at: Cell::new(None),
        });

        let weak = Rc::downgrade(&this);
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
        let weak = Rc::downgrade(&this);
        gif.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.gifs();
            }
        });
        for (button, is_video) in [(&voice, false), (&video, true)] {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.record(is_video);
                }
            });
        }
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
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
                placeholder.set_visible(text.is_empty());
                if let Some(id) = t.typing_timeout.borrow_mut().take() {
                    id.remove();
                }
                if text.trim().is_empty() {
                    t.notify_typing(false);
                    return;
                }
                t.notify_typing(true);
                let weak = Rc::downgrade(&t);
                *t.typing_timeout.borrow_mut() =
                    Some(glib::timeout_add_local_once(TYPING_IDLE, move || {
                        if let Some(t) = weak.upgrade() {
                            t.typing_timeout.borrow_mut().take();
                            t.notify_typing(false);
                        }
                    }));
            });
        }

        // Searching: as you type; Enter and Up go to older matches,
        // Shift+Enter and Down to newer ones; Escape closes.
        let weak = Rc::downgrade(&this);
        this.search_entry.connect_search_changed(move |_| {
            if let Some(t) = weak.upgrade() {
                t.search();
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let older = match key {
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    !mods.contains(gdk::ModifierType::SHIFT_MASK)
                }
                gdk::Key::Up => true,
                gdk::Key::Down => false,
                _ => return glib::Propagation::Proceed,
            };
            if let Some(t) = weak.upgrade() {
                t.step(older);
            }
            glib::Propagation::Stop
        });
        this.search_entry.add_controller(keys);
        for (button, older) in [(&older, true), (&newer, false)] {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.step(older);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        this.search_entry.connect_next_match(move |_| {
            if let Some(t) = weak.upgrade() {
                t.step(true);
            }
        });
        let weak = Rc::downgrade(&this);
        this.search_entry.connect_previous_match(move |_| {
            if let Some(t) = weak.upgrade() {
                t.step(false);
            }
        });
        let weak = Rc::downgrade(&this);
        this.search_bar
            .connect_search_mode_enabled_notify(move |bar| {
                let Some(t) = weak.upgrade() else { return };
                if !bar.is_search_mode() {
                    t.close_search();
                }
            });

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

    /// Tells the contact (not a group) we started or stopped typing: on a
    /// change, and again now and then while typing goes on.
    fn notify_typing(&self, active: bool) {
        if self.group().is_some() {
            return;
        }
        let again = active
            && self
                .typing_sent_at
                .get()
                .is_some_and(|t| t.elapsed() > TYPING_AGAIN);
        if self.sending_typing.get() == active && !again {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if active && !app.core.settings().flag(settings::SEND_TYPING) {
            return;
        }
        let Some(node) = self.node() else { return };
        self.sending_typing.set(active);
        self.typing_sent_at.set(Some(std::time::Instant::now()));
        let device = self.device();
        bg(
            move || {
                let _ = node.set_typing(device, active);
            },
            |()| {},
        );
    }

    /// Shows or hides "…" for the contact typing.
    pub fn set_typing(&self, active: bool) {
        if self.group().is_some() {
            return;
        }
        if let Some(id) = self.peer_quiet.borrow_mut().take() {
            id.remove();
        }
        if active {
            let weak = self.weak_self.clone();
            *self.peer_quiet.borrow_mut() =
                Some(glib::timeout_add_local_once(PEER_TYPING, move || {
                    if let Some(t) = weak.upgrade() {
                        t.peer_quiet.borrow_mut().take();
                        t.set_typing(false);
                    }
                }));
        }
        if self.peer_typing.replace(active) != active {
            self.typing_row.set_visible(active);
            if active {
                self.scroll_to_end();
            }
        }
    }

    /// The window came to the front: what's on screen is now read.
    pub fn shown(&self) {
        let entries = self.entries.borrow().clone();
        self.report_read(&entries);
    }

    /// Tells the contact which of its messages are now on screen (1:1
    /// chats, with read receipts on); ids it was told of aren't sent again.
    fn report_read(&self, entries: &[HistoryEntry]) {
        if self.group().is_some() {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if !app.core.settings().flag(settings::SEND_READ) || !app.window.is_active() {
            return;
        }
        let ids: Vec<u64> = {
            let sent = self.sent_read.borrow();
            entries
                .iter()
                .filter(|e| !e.outgoing && e.id != 0 && !sent.contains(&e.id))
                .map(|e| e.id)
                .collect()
        };
        let Some(node) = self.node() else { return };
        if ids.is_empty() {
            return;
        }
        let device = self.device();
        let weak = self.weak_self.clone();
        bg(
            {
                let ids = ids.clone();
                move || node.report_read(device, ids).is_ok()
            },
            // Without a session now, they're reported on a later render.
            move |ok| {
                if let Some(t) = weak.upgrade()
                    && ok
                {
                    t.sent_read.borrow_mut().extend(ids);
                }
            },
        );
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
        let (limit, until) = (self.limit.get(), self.jump.borrow().as_ref().map(|k| k.0));
        let weak = self.weak_self.clone();
        bg(
            move || {
                let fetch = |n| match &group {
                    Some(g) => node.group_history(g.clone(), n).unwrap_or_default(),
                    None => node.history(device.clone(), n).unwrap_or_default(),
                };
                let mut n = limit;
                let mut entries = fetch(n);
                // Further back, until the message to jump to is in.
                while until.is_some_and(|at| older_than_loaded(&entries, n, at)) && n < u32::MAX {
                    n = n.saturating_mul(4);
                    entries = fetch(n);
                }
                (n, entries)
            },
            move |(n, entries)| {
                let Some(t) = weak.upgrade() else { return };
                t.loading.set(false);
                t.limit.set(n);
                if !t.loaded.replace(true) || *t.entries.borrow() != entries {
                    t.display_entries(entries);
                }
                t.finish_jump();
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
        self.bubbles.borrow_mut().clear();
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
        self.messages.append(&self.typing_row);
        self.report_read(&entries);
        self.stack.set_visible_child_name(if entries.is_empty() {
            "empty"
        } else {
            "messages"
        });
        *self.entries.borrow_mut() = entries;
        self.mark_matches();
        // A message to jump to decides where to scroll instead.
        if at_bottom && self.jump.borrow().is_none() {
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
        self.bubbles
            .borrow_mut()
            .push(((e.at_ms, e.device.clone()), bubble.clone()));

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
        if let (Some(c), Some(p)) = (f.clip, &path) {
            if f.sensitive && !e.outgoing && !self.shown.borrow().contains(&key) {
                let b = gtk::Button::with_label(if c.video {
                    "Sensitive video message · click to show"
                } else {
                    "Sensitive voice message · click to show"
                });
                b.add_css_class("sensitive-cover");
                b.set_size_request(240, if c.video { 160 } else { -1 });
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
            return self.clip_view(c, p);
        }
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
            animate(&pic, p);
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
        let icon = gtk::Image::from_icon_name(match f.clip {
            Some(c) if c.video => "camera-web-symbolic",
            Some(_) => "audio-input-microphone-symbolic",
            None if core::is_image(&f.name) => "image-x-generic-symbolic",
            None => "text-x-generic-symbolic",
        });
        icon.set_pixel_size(32);
        row.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let label = match f.clip {
            Some(c) => core::file_label(&f.name, false, "", Some(c))
                .split_once(' ')
                .map_or_else(String::new, |(_, l)| l.to_owned()),
            None => f.name.clone(),
        };
        let name = gtk::Label::builder()
            .label(&label)
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

    /// A voice or video message that plays in the chat.
    fn clip_view(&self, c: Clip, path: &std::path::Path) -> gtk::Widget {
        let media = self
            .players
            .borrow_mut()
            .entry(path.to_owned())
            .or_insert_with(|| gtk::MediaFile::for_filename(path))
            .clone();
        if c.video {
            let v = gtk::Video::new();
            v.set_media_stream(Some(&media));
            v.set_autoplay(false);
            v.set_size_request(320, 240);
            v.add_css_class("photo");
            return v.upcast();
        }
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.add_css_class("voice-message");
        let icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
        row.append(&icon);
        let controls = gtk::MediaControls::new(Some(&media));
        controls.set_size_request(260, -1);
        controls.set_hexpand(true);
        row.append(&controls);
        row.upcast()
    }

    /// Records a voice or video message, then sends it on *Send*.
    fn record(self: &Rc<Self>, video: bool) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let path = match app.core.clip_path(self.persona().as_deref(), video) {
            Ok(p) => p,
            Err(e) => return self.toast(&format!("Couldn't record: {e}")),
        };
        type Frame = (u32, u32, Vec<u8>);
        let frame: std::sync::Arc<std::sync::Mutex<Option<Frame>>> = Default::default();
        let latest = frame.clone();
        let preview: clip::OnFrame = Box::new(move |w, h, rgba| {
            if let Ok(mut f) = latest.lock() {
                *f = Some((w, h, rgba.to_vec()));
            }
        });
        let rec = match clip::Recorder::start(&path, video, 0, Some(preview)) {
            Ok(r) => r,
            Err(e) => return self.toast(&format!("Couldn't record: {e}")),
        };
        let limit = rec.limit();
        let rec = Rc::new(RefCell::new(Some(rec)));
        let d = ui::alert(
            if video {
                "Video message"
            } else {
                "Voice message"
            },
            "",
            &[("cancel", "Cancel"), ("send", "Send")],
        );
        d.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let picture = gtk::Picture::new();
        if video {
            picture.set_size_request(320, 240);
            picture.add_css_class("photo");
            b.append(&picture);
        }
        let time = gtk::Label::new(Some(&format!(
            "● 0:00 / {}",
            clip::clock(limit.as_millis() as u64)
        )));
        time.add_css_class("recording");
        time.add_css_class("numeric");
        b.append(&time);
        d.set_extra_child(Some(&b));
        let (weak, r, dialog) = (self.weak_self.clone(), rec.clone(), d.clone());
        let tick = glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            let Some(rec) = r.borrow().as_ref().map(|x| (x.elapsed(), x.error())) else {
                return glib::ControlFlow::Break;
            };
            if let Some(e) = rec.1 {
                r.borrow_mut().take();
                if let Some(t) = weak.upgrade() {
                    t.toast(&format!("Recording stopped: {e}"));
                }
                dialog.close();
                return glib::ControlFlow::Break;
            }
            time.set_label(&format!(
                "● {} / {}",
                clip::clock(rec.0.as_millis() as u64),
                clip::clock(limit.as_millis() as u64)
            ));
            if let Some((w, h, rgba)) = frame.lock().ok().and_then(|mut f| f.take()) {
                let tex = gdk::MemoryTexture::new(
                    w as i32,
                    h as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &glib::Bytes::from_owned(rgba),
                    w as usize * 4,
                );
                picture.set_paintable(Some(&tex));
            }
            if rec.0 >= limit {
                // Long enough: it goes as it is.
                if let (Some(t), Some(rec)) = (weak.upgrade(), r.borrow_mut().take()) {
                    t.send_clip(rec, video);
                }
                dialog.close();
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        let tick = Rc::new(RefCell::new(Some(tick)));
        let (weak, r) = (self.weak_self.clone(), rec.clone());
        d.connect_response(None, move |_, response| {
            if let Some(t) = tick.borrow_mut().take()
                && glib::MainContext::default().find_source_by_id(&t).is_some()
            {
                t.remove();
            }
            // Cancelling drops the recording, which deletes it.
            let rec = r.borrow_mut().take();
            if response == "send"
                && let (Some(t), Some(rec)) = (weak.upgrade(), rec)
            {
                t.send_clip(rec, video);
            }
        });
        d.present(Some(&app.window));
    }

    /// Finishes a recording and sends it.
    fn send_clip(self: &Rc<Self>, rec: clip::Recorder, video: bool) {
        let Some(node) = self.node() else { return };
        let (group, device) = (self.group(), self.device());
        self.toast("Sending…");
        let weak = self.weak_self.clone();
        bg(
            move || -> Result<(), String> {
                let done = rec.finish()?;
                let data = std::fs::read(&done.path).map_err(|e| e.to_string())?;
                if data.len() as u64 > node.max_file_size() {
                    let _ = std::fs::remove_file(&done.path);
                    return Err(format!(
                        "it's larger than {}",
                        ui::human_size(node.max_file_size())
                    ));
                }
                let name = done
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let options = FileOptions {
                    clip: Some(Clip {
                        video,
                        duration_ms: done.duration_ms,
                    }),
                    ..FileOptions::default()
                };
                let location = Some(done.path.display().to_string());
                match &group {
                    Some(g) => node.send_group_file(g.clone(), name, data, location, options),
                    None => node.send_file(device, name, data, location, options),
                }
                .map_err(|e| e.to_string())
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                if let Err(e) = r {
                    t.toast(&format!("Couldn't send: {e}"));
                }
                t.reload();
                if let Some(a) = t.app.upgrade() {
                    a.refresh_soon();
                }
            },
        );
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

    // ----- Search -----

    /// Opens the search (Ctrl+F), or selects its text if open.
    pub fn open_search(&self) {
        self.search_bar.set_search_mode(true);
        self.search_entry.grab_focus();
        self.search_entry.select_region(0, -1);
    }

    /// Searches for `query` with `key`, one of its matches, selected: how
    /// the app-wide search opens a message.
    pub fn find(self: &Rc<Self>, query: &str, key: Key) {
        *self.want.borrow_mut() = Some(key.clone());
        self.search_bar.set_search_mode(true);
        if self.search_entry.text() == query {
            self.search();
        } else {
            // Searches once the entry's short delay is over.
            self.search_entry.set_text(query);
        }
        self.jump_to(key);
    }

    fn search(self: &Rc<Self>) {
        let query = self.search_entry.text().trim().to_owned();
        let generation = self.search_gen.get().wrapping_add(1);
        self.search_gen.set(generation);
        if query.is_empty() {
            self.want.take();
            self.hits.borrow_mut().clear();
            self.select_hit(0);
            return;
        }
        let Some(node) = self.node() else { return };
        let (group, device) = (self.group(), self.device());
        let weak = self.weak_self.clone();
        bg(
            move || {
                match group {
                    Some(g) => node.search_group_messages(g, query, SEARCH_LIMIT),
                    None => node.search_messages(device, query, SEARCH_LIMIT),
                }
                .unwrap_or_default()
            },
            move |found| {
                let Some(t) = weak.upgrade() else { return };
                if t.search_gen.get() != generation {
                    return;
                }
                let hits: Vec<Key> = found.into_iter().map(|e| (e.at_ms, e.device)).collect();
                let at = t
                    .want
                    .take()
                    .and_then(|k| hits.iter().position(|h| *h == k))
                    .unwrap_or(0);
                *t.hits.borrow_mut() = hits;
                t.select_hit(at);
            },
        );
    }

    /// Goes to the next match back in time (`older`) or forward, wrapping.
    fn step(self: &Rc<Self>, older: bool) {
        let n = self.hits.borrow().len();
        if n == 0 {
            return;
        }
        let at = self.hit_at.get() % n;
        self.select_hit(if older {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        });
    }

    /// Selects match `i` (newest first) and shows it, or with no matches
    /// just says so.
    fn select_hit(self: &Rc<Self>, i: usize) {
        self.hit_at.set(i);
        let (key, n) = {
            let hits = self.hits.borrow();
            (hits.get(i).cloned(), hits.len())
        };
        let searching = !self.search_entry.text().trim().is_empty();
        self.search_count.set_text(&match n {
            _ if !searching => String::new(),
            0 => "No matches".into(),
            // The search stops at SEARCH_LIMIT; there may be more.
            _ if n >= SEARCH_LIMIT as usize => format!("{} of {n}+", i + 1),
            _ => format!("{} of {n}", i + 1),
        });
        self.search_nav.set_sensitive(n > 0);
        match key {
            Some(k) => self.jump_to(k),
            None => {
                self.current.borrow_mut().take();
                self.mark_matches();
            }
        }
    }

    /// The search bar closed: no more marks.
    fn close_search(&self) {
        self.search_gen.set(self.search_gen.get().wrapping_add(1));
        self.search_entry.set_text("");
        self.search_count.set_text("");
        self.search_nav.set_sensitive(false);
        self.hits.borrow_mut().clear();
        self.want.take();
        self.current.borrow_mut().take();
        self.mark_matches();
        self.input.grab_focus();
    }

    /// Scrolls to a message and highlights it, loading older history
    /// first when it isn't loaded.
    pub fn jump_to(self: &Rc<Self>, key: Key) {
        *self.current.borrow_mut() = Some(key.clone());
        self.mark_matches();
        if self.bubbles.borrow().iter().any(|(k, _)| *k == key) {
            self.jump.borrow_mut().take();
            self.scroll_to(&key);
            return;
        }
        *self.jump.borrow_mut() = Some(key);
        // Before the first load, it finishes the jump itself.
        if self.loaded.get() {
            self.reload();
        }
    }

    /// After a load: scrolls to the message to jump to if it's in now,
    /// loads further back if it may be older, else gives up on it.
    fn finish_jump(self: &Rc<Self>) {
        let Some(key) = self.jump.borrow().clone() else {
            return;
        };
        if self.bubbles.borrow().iter().any(|(k, _)| *k == key) {
            self.jump.borrow_mut().take();
            self.scroll_to(&key);
        } else if older_than_loaded(&self.entries.borrow(), self.limit.get(), key.0) {
            self.reload();
        } else {
            // Deleted, or disappeared, since it was found.
            self.jump.borrow_mut().take();
            self.scroll_to_end();
        }
    }

    /// Marks the bubbles of matches, and the highlighted message.
    fn mark_matches(&self) {
        let all = self.hits.borrow();
        let hits: HashSet<&Key> = all.iter().collect();
        let current = self.current.borrow();
        for (k, b) in self.bubbles.borrow().iter() {
            set_class(b, "search-match", hits.contains(k));
            set_class(b, "search-current", current.as_ref() == Some(k));
        }
    }

    /// Scrolls so the message is in the middle of the view.
    fn scroll_to(&self, key: &Key) {
        let Some(bubble) = self
            .bubbles
            .borrow()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, b)| b.clone())
        else {
            return;
        };
        let scroll = self.scroll.clone();
        // After layout, as in `scroll_to_end`.
        glib::idle_add_local_once(move || {
            center(&scroll, &bubble);
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                center(&scroll, &bubble);
            });
        });
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
        // Clearing the composer also says we stopped typing.
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

    /// GIPHY's GIFs once the user has agreed to GIPHY seeing their
    /// searches; until then, or without an API key, GIF files of their own.
    fn gifs(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if !app.core.settings().opted_in(settings::GIPHY) {
            let d = ui::alert(
                "Search GIFs with GIPHY?",
                "GIPHY will see what you search for and this computer's IP address. \
                 It won't see who you send GIFs to, and your contacts' devices never contact it. \
                 You can turn this off in Preferences.",
                &[("files", "Choose a GIF file"), ("giphy", "Use GIPHY")],
            );
            d.set_response_appearance("giphy", adw::ResponseAppearance::Suggested);
            let weak = self.weak_self.clone();
            let core = app.core.clone();
            d.connect_response(None, move |_, r| {
                let Some(t) = weak.upgrade() else { return };
                // Once this dialog has gone: one presented while it closes
                // never shows.
                if r == "giphy" {
                    core.settings().set_flag(settings::GIPHY, true);
                    glib::idle_add_local_once(move || t.gifs());
                } else if r == "files" {
                    glib::idle_add_local_once(move || t.pick_files());
                }
            });
            d.present(Some(&app.window));
            return;
        }
        let key = gifs::key(app.core.settings().text(settings::GIPHY_KEY));
        if key.is_empty() {
            let (weak, core) = (self.weak_self.clone(), app.core.clone());
            ui::ask_text(
                &app.window,
                "GIPHY API key",
                "This copy of Threnody was built without one. Get a free key at developers.giphy.com.",
                "API key",
                "",
                "Save",
                move |k| {
                    if let (Some(t), false) = (weak.upgrade(), k.is_empty()) {
                        core.settings().set_text(settings::GIPHY_KEY, &k);
                        glib::idle_add_local_once(move || t.gifs());
                    }
                },
            );
            return;
        }
        let (files, weak, core) = (
            self.weak_self.clone(),
            self.weak_self.clone(),
            app.core.clone(),
        );
        gifs::picker(
            &app.window,
            key,
            move || {
                if let Some(t) = files.upgrade() {
                    t.pick_files();
                }
            },
            move || core.settings().set_text(settings::GIPHY_KEY, ""),
            move |g| {
                if let Some(t) = weak.upgrade() {
                    t.send_gif(g);
                }
            },
        );
    }

    /// Downloads a GIF from GIPHY, keeps it with the chat's photos, and
    /// offers to send it like a picked one.
    fn send_gif(self: &Rc<Self>, g: gifs::Gif) {
        let (Some(app), Some(node)) = (self.app.upgrade(), self.node()) else {
            return;
        };
        self.toast("Getting the GIF…");
        let (core, persona, weak) = (app.core.clone(), self.persona(), self.weak_self.clone());
        bg(
            move || {
                let bytes = gifs::get(&g.full, node.max_file_size())?;
                let id: String =
                    g.id.chars()
                        .filter(char::is_ascii_alphanumeric)
                        .take(32)
                        .collect();
                let name = format!(
                    "gif-{}.gif",
                    if id.is_empty() { "giphy".into() } else { id }
                );
                core.keep(persona.as_deref(), &name, &bytes, false)
                    .ok_or_else(|| "couldn't save it".to_owned())
            },
            move |kept: Result<String, String>| {
                let Some(t) = weak.upgrade() else { return };
                match kept {
                    Ok(path) => t.confirm_files(vec![path.into()]),
                    Err(e) => t.toast(&format!("Couldn't get the GIF: {e}")),
                }
            },
        );
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
        let strip = self
            .app
            .upgrade()
            .is_none_or(|a| a.core.settings().flag(settings::STRIP));
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
                    let mut name = p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let data = match std::fs::read(p) {
                        Ok(d) if !strip => d,
                        Ok(d) => match prepare(p, name.clone(), d) {
                            Ok((n, d)) => {
                                name = n;
                                d
                            }
                            Err(e) => {
                                failed.push(e);
                                continue;
                            }
                        },
                        Err(e) => {
                            failed.push(format!("{name}: {e}"));
                            continue;
                        }
                    };
                    let data = match data {
                        d if d.len() as u64 <= max => d,
                        _ => {
                            failed.push(format!("{name} is larger than {}", ui::human_size(max)));
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
                        clip: None,
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

/// Whether a message at `at_ms` may be older than all of `entries`, the
/// last `limit` of the conversation: there are more and they start later.
fn older_than_loaded(entries: &[HistoryEntry], limit: u32, at_ms: u64) -> bool {
    entries.len() >= limit as usize && entries.first().is_some_and(|e| e.at_ms > at_ms)
}

fn set_class(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

/// Scrolls `scroll` so `w`, inside it, is in the middle of the view.
fn center(scroll: &gtk::ScrolledWindow, w: &impl IsA<gtk::Widget>) {
    let Some(view) = scroll.child() else { return };
    let Some(p) = w.compute_point(&view, &gtk::graphene::Point::new(0.0, 0.0)) else {
        return;
    };
    let adj = scroll.vadjustment();
    let h = f64::from(w.height());
    adj.set_value(adj.value() + f64::from(p.y()) - (adj.page_size() - h) / 2.0);
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

/// Plays an animated GIF in `pic` (GTK draws only its first frame) while
/// the picture exists; it only advances while it's on screen.
fn animate(pic: &gtk::Picture, path: &std::path::Path) {
    let is_gif = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"));
    if !is_gif {
        return;
    }
    let Ok(anim) = gdk::gdk_pixbuf::PixbufAnimation::from_file(path) else {
        return;
    };
    if anim.is_static_image() {
        return;
    }
    let iter = anim.iter(None);
    let weak = pic.downgrade();
    glib::timeout_add_local(std::time::Duration::from_millis(20), move || {
        let Some(pic) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if pic.is_mapped() && iter.advance(std::time::SystemTime::now()) {
            pic.set_paintable(Some(&gdk::Texture::for_pixbuf(&iter.pixbuf())));
        }
        glib::ControlFlow::Continue
    });
}

/// Image formats the node can't strip metadata from (it does JPEG, PNG
/// and WebP; GIFs carry none worth the name): they're sent as JPEG.
const CONVERT: [&str; 6] = ["heic", "heif", "avif", "bmp", "tif", "tiff"];
/// Larger images are refused rather than decoded (decompression bombs).
const MAX_PIXELS: i64 = 100_000_000;
/// Converted photos are scaled to fit this, as the Android app does.
const MAX_SIDE: i32 = 4096;

/// An image the node can't strip, re-encoded as JPEG (which then carries
/// no metadata): the name and bytes to send. Others pass through.
fn prepare(
    path: &std::path::Path,
    name: String,
    data: Vec<u8>,
) -> Result<(String, Vec<u8>), String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !CONVERT.contains(&ext.as_str()) {
        return Ok((name, data));
    }
    let unreadable =
        || format!("{name}: can't remove its location and camera details, so it wasn't sent");
    let (_, w, h) = gdk::gdk_pixbuf::Pixbuf::file_info(path).ok_or_else(unreadable)?;
    if i64::from(w) * i64::from(h) > MAX_PIXELS || w <= 0 || h <= 0 {
        return Err(format!("{name} is too large a picture"));
    }
    let pixbuf = if w.max(h) > MAX_SIDE {
        gdk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, MAX_SIDE, MAX_SIDE, true)
    } else {
        gdk::gdk_pixbuf::Pixbuf::from_file(path)
    }
    .map_err(|_| unreadable())?;
    // Turned upright as the camera meant, since the turn itself is metadata.
    let upright = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    let jpeg = upright
        .save_to_bufferv("jpeg", &[("quality", "92")])
        .map_err(|_| unreadable())?;
    let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
    Ok((format!("{stem}.jpg"), jpeg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photos_the_node_cant_strip_go_as_jpeg() {
        let dir = std::env::temp_dir().join(format!("threnody-prepare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tiff = dir.join("scan.tiff");
        let p = gdk::gdk_pixbuf::Pixbuf::new(gdk::gdk_pixbuf::Colorspace::Rgb, false, 8, 40, 30)
            .unwrap();
        p.fill(0x3366_99ff);
        p.savev(&tiff, "tiff", &[]).unwrap();
        let data = std::fs::read(&tiff).unwrap();
        let (name, out) = prepare(&tiff, "scan.tiff".into(), data).unwrap();
        assert_eq!(name, "scan.jpg");
        assert_eq!(&out[..2], &[0xff, 0xd8], "a JPEG");

        // Formats the node strips itself pass through untouched.
        let png = dir.join("a.png");
        let (name, out) = prepare(&png, "a.png".into(), b"as is".to_vec()).unwrap();
        assert!(name == "a.png" && out == b"as is");

        // One it can't read isn't sent with its metadata.
        let heic = dir.join("broken.heic");
        std::fs::write(&heic, b"not a picture").unwrap();
        assert!(prepare(&heic, "broken.heic".into(), b"not a picture".to_vec()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
