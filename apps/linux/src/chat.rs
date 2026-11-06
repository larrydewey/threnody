//! One open conversation: header, trust banner, messages and composer.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use threnody_ffi::{
    Clip, FileFetch, FileOptions, GroupBanner, GroupInfo, GroupMemberAction, GroupMemberRow,
    HistoryEntry, MemberRole, TickMark,
};

use crate::core::{self, ChatRef, Conversation, Core, Node, Target};
use crate::ui::{self, bg};
use crate::viewer::{self, MAX_PIXELS, MAX_SIDE, Thumbs};
use crate::window::App;
use crate::{clip, editor, gifs, schedule, settings};

/// How many messages a chat loads (more to reach an older search match).
const HISTORY: u32 = 1000;
/// How many messages *Media, files and links* looks through (Android's).
const MEDIA_HISTORY: u32 = 10_000;
/// Long side of photos decoded for the chat and the send dialog.
const THUMB_SIDE: i32 = 640;
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
/// Opens the review of a pending credential offer or ask.
type Review = Box<dyn Fn(&Rc<App>)>;
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];
/// A message by its time and sending device, as search finds it.
pub type Key = (u64, String);

/// A file on screen coming from another of our devices: the entry (time
/// and sender), its state label, and its size.
type Fetching = ((u64, String), gtk::Label, u64);

pub struct ChatView {
    app: Weak<App>,
    conv: RefCell<Conversation>,
    root: adw::ToolbarView,
    title: adw::WindowTitle,
    banner: gtk::Box,
    /// Credential offers and asks from this contact, waiting for an answer.
    cred_banner: gtk::Box,
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
    /// Sensitive voice and video messages the user uncovered, by time
    /// and sender. (Sensitive photos open in the viewer instead.)
    shown: RefCell<Vec<(u64, String)>>,
    /// Photos decoded for the bubbles, kept while the chat is open (it
    /// redraws them all whenever a message comes).
    thumbs: Thumbs,
    /// Files on screen coming from our other devices, by time and sender:
    /// their state labels and sizes, kept current once a second.
    fetching: RefCell<Vec<Fetching>>,
    fetch_poll: RefCell<Option<glib::SourceId>>,
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
    /// Above the composer while replying: what the reply quotes.
    reply_bar: gtk::Box,
    /// Above the composer: how many messages are scheduled here, and
    /// when the next goes; clicking lists them.
    scheduled_bar: gtk::Button,
    scheduled: RefCell<Vec<threnody_ffi::ScheduledInfo>>,
    /// The chat has a disappearing timer (it applies to scheduled
    /// messages when they're sent).
    timer_on: Cell<bool>,
    /// "Send later…", from the send button's right-click or long-press.
    later_menu: gtk::PopoverMenu,
    /// Under the header: the pinned messages; clicking steps through them.
    pin_bar: gtk::Button,
    pins: RefCell<Vec<HistoryEntry>>,
    pin_shown: Cell<usize>,
    /// In a group: whether we may pin (owner or admin).
    may_pin: Cell<bool>,
    /// Members offered for an "@" being typed.
    mention_popover: gtk::Popover,
    /// Mentions in the composer: where ("@Name" between two marks), whose
    /// device, and the label put in.
    mentions: RefCell<Vec<(gtk::TextMark, gtk::TextMark, String, String)>>,
    /// The message the next one replies to.
    reply_to: RefCell<Option<HistoryEntry>>,
    /// Weak reference to self for creating weak pointers in callbacks.
    weak_self: Weak<ChatView>,
}

impl Drop for ChatView {
    fn drop(&mut self) {
        // A popover's parent doesn't take it down with itself.
        self.mention_popover.unparent();
        self.later_menu.unparent();
        self.save_draft();
    }
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
        // Calls, with a contact or a group: voice, or video.
        if conv.invite.is_none() {
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
                        if c.group.is_some() {
                            app.start_group_call(&c, video);
                        } else {
                            app.start_call(&c, video);
                        }
                    }
                });
                header.pack_end(&call);
            }
        }

        let banner = gtk::Box::new(gtk::Orientation::Vertical, 6);
        banner.add_css_class("chat-banner");
        banner.set_visible(false);
        let cred_banner = gtk::Box::new(gtk::Orientation::Vertical, 6);
        cred_banner.add_css_class("chat-banner");
        cred_banner.set_visible(false);

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
        let note = conv.chat.target == Target::Note;
        send.set_tooltip_text(Some(if note {
            "Send (Enter)"
        } else {
            "Send (Enter) · Send later (Ctrl+Enter or right-click)"
        }));
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
        body.append(&cred_banner);
        let pin_bar = gtk::Button::new();
        pin_bar.add_css_class("flat");
        pin_bar.add_css_class("pin-bar");
        pin_bar.set_visible(false);
        pin_bar.set_tooltip_text(Some("Show the pinned message"));
        body.append(&pin_bar);
        body.append(&stack);
        let root = adw::ToolbarView::new();
        root.add_top_bar(&header);
        root.add_top_bar(&search_bar);
        root.set_content(Some(&body));
        let scheduled_bar = gtk::Button::new();
        scheduled_bar.add_css_class("flat");
        scheduled_bar.add_css_class("pin-bar");
        scheduled_bar.set_visible(false);
        scheduled_bar.set_tooltip_text(Some("Show the scheduled messages"));
        root.add_bottom_bar(
            &adw::Clamp::builder()
                .maximum_size(860)
                .child(&scheduled_bar)
                .build(),
        );
        let later_items = gio::Menu::new();
        later_items.append(Some("Send later…"), Some("chat.send-later"));
        let later_menu = gtk::PopoverMenu::from_model(Some(&later_items));
        later_menu.set_parent(&send);
        let reply_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        reply_bar.add_css_class("reply-bar");
        reply_bar.set_visible(false);
        root.add_bottom_bar(
            &adw::Clamp::builder()
                .maximum_size(860)
                .child(&reply_bar)
                .build(),
        );
        root.add_bottom_bar(&composer_clamp);

        let this = Rc::new_cyclic(|weak_self| Self {
            app: Rc::downgrade(app),
            conv: RefCell::new(conv.clone()),
            root,
            title,
            banner,
            cred_banner,
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
            thumbs: Thumbs::default(),
            fetching: RefCell::new(Vec::new()),
            fetch_poll: RefCell::new(None),
            players: RefCell::new(HashMap::new()),
            typing_row,
            peer_typing: Cell::new(false),
            sent_read: RefCell::new(HashSet::new()),
            typing_timeout: RefCell::new(None),
            sending_typing: Cell::new(false),
            reply_bar,
            scheduled_bar,
            scheduled: RefCell::new(Vec::new()),
            timer_on: Cell::new(false),
            later_menu,
            reply_to: RefCell::new(None),
            pin_bar,
            pins: RefCell::new(Vec::new()),
            pin_shown: Cell::new(0),
            may_pin: Cell::new(false),
            mention_popover: {
                let p = gtk::Popover::new();
                p.set_autohide(false);
                p.set_has_arrow(false);
                p.set_position(gtk::PositionType::Top);
                p.set_parent(&input_scroll);
                p
            },
            mentions: RefCell::new(Vec::new()),
            weak_self: weak_self.clone(),
            peer_quiet: RefCell::new(None),
            typing_sent_at: Cell::new(None),
        });

        let weak = Rc::downgrade(&this);
        this.pin_bar.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.next_pin();
            }
        });
        let weak = Rc::downgrade(&this);
        send.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.send();
            }
        });
        let weak = Rc::downgrade(&this);
        this.scheduled_bar.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.scheduled_list();
            }
        });
        // Right-click or long-press on Send offers to send later. Both
        // claim the press, so it doesn't also send at once.
        if !note {
            let actions = gio::SimpleActionGroup::new();
            let later = gio::SimpleAction::new("send-later", None);
            let weak = Rc::downgrade(&this);
            later.connect_activate(move |_, _| {
                if let Some(t) = weak.upgrade() {
                    t.send_later();
                }
            });
            actions.add_action(&later);
            send.insert_action_group("chat", Some(&actions));
            let right = gtk::GestureClick::new();
            right.set_button(gdk::BUTTON_SECONDARY);
            right.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(&this);
            right.connect_pressed(move |g, _, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(t) = weak.upgrade() {
                    t.offer_later();
                }
            });
            send.add_controller(right);
            let hold = gtk::GestureLongPress::new();
            hold.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(&this);
            hold.connect_pressed(move |g, _, _| {
                // Else the release still clicks, and sends now.
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(t) = weak.upgrade() {
                    t.offer_later();
                }
            });
            send.add_controller(hold);
        }
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
        // Enter sends; Shift+Enter starts a new line; Ctrl+Enter sends later.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
            // Checked first: "Enter without Shift" would take it otherwise.
            // Notes to self aren't scheduled: there it sends, as before.
            if enter
                && mods.contains(gdk::ModifierType::CONTROL_MASK)
                && let Some(t) = weak.upgrade()
                && !t.is_note()
            {
                t.send_later();
                return glib::Propagation::Stop;
            }
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
            t.confirm_files(paths, String::new(), false);
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
                t.offer_mentions(buf);
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
        this.seek();
        this.input.grab_focus();
        this
    }

    /// A contact's chat opened: look for them across the internet now,
    /// in case they aren't connected (as Android does).
    fn seek(&self) {
        if self.group().is_some() || self.is_note() {
            return;
        }
        let (Some(app), Some(node)) = (self.app.upgrade(), self.node()) else {
            return;
        };
        let (core, device) = (app.core.clone(), self.device());
        ui::bg_quiet(move || {
            if let Err(e) = node.seek(device) {
                core.say(format!("! seek: {e}"));
            }
        });
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
            Target::Contact(_) | Target::Note => None,
        }
    }

    /// On screen in the active window: what's here is read (unread counts).
    fn mark_seen(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        if !app.window.is_active() || self.persona().is_some() {
            return;
        }
        let key = match &self.conv.borrow().chat.target {
            Target::Contact(k) | Target::Group(k) => k.clone(),
            Target::Note => "notes".into(),
        };
        let Some(node) = self.node() else { return };
        ui::bg_quiet(move || {
            let _ = node.mark_seen(key);
        });
    }

    /// Notes to self rather than a chat with someone.
    fn is_note(&self) -> bool {
        self.conv.borrow().chat.target == Target::Note
    }

    fn dest(&self) -> Dest {
        match (self.is_note(), self.group()) {
            (true, _) => Dest::Note,
            (false, Some(g)) => Dest::Group(g),
            (false, None) => Dest::Contact(self.device()),
        }
    }

    /// Forwards messages (one, or an album's photos) to a chat the user
    /// picks, marked as forwarded.
    fn forward(self: &Rc<Self>, entries: &[HistoryEntry]) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let persona = self.persona();
        let targets: Vec<Conversation> = app
            .core
            .conversations()
            .into_iter()
            .filter(|c| c.chat.persona == persona && c.invite.is_none() && !c.is_request())
            .collect();
        let names: Vec<String> = targets.iter().map(|c| c.title.clone()).collect();
        let (weak, entries) = (self.weak_self.clone(), entries.to_vec());
        ui::choose(
            &app.window,
            "Forward to",
            "It arrives marked as forwarded.",
            &names,
            None,
            move |i| {
                let Some(t) = weak.upgrade() else { return };
                let Some(c) = targets.get(i).cloned() else {
                    return;
                };
                let entries = entries.clone();
                let title = c.title.clone();
                t.run(
                    "forward",
                    move |n| {
                        let dest = match &c.chat.target {
                            Target::Note => Dest::Note,
                            Target::Group(g) => Dest::Group(g.clone()),
                            Target::Contact(_) => Dest::Contact(c.device.clone()),
                        };
                        // Every file first: half an album isn't sent.
                        let mut data = Vec::with_capacity(entries.len());
                        for e in &entries {
                            data.push(match &e.file {
                                Some(f) => {
                                    let path = f.location.clone().unwrap_or_default();
                                    Some(std::fs::read(&path).map_err(|x| {
                                        threnody_ffi::ThrenodyError::Failed {
                                            reason: format!("that file isn't on this device: {x}"),
                                        }
                                    })?)
                                }
                                None => None,
                            });
                        }
                        let album = if entries.len() > 1 {
                            threnody_ffi::album_id()
                        } else {
                            0
                        };
                        for (i, (e, data)) in entries.iter().zip(data).enumerate() {
                            match (&e.file, data, dest.clone()) {
                                (Some(f), Some(data), dest) => {
                                    let options = FileOptions {
                                        sensitive: f.sensitive,
                                        // An album's caption is its first entry's.
                                        caption: if i == 0 {
                                            e.text.clone()
                                        } else {
                                            String::new()
                                        },
                                        album,
                                        clip: f.clip,
                                        forwarded: true,
                                    };
                                    send_file_to(
                                        n,
                                        &dest,
                                        f.name.clone(),
                                        data,
                                        f.location.clone(),
                                        options,
                                    )?;
                                }
                                (_, _, Dest::Note) => n.write_note(e.text.clone(), None, None)?,
                                (_, _, Dest::Group(g)) => {
                                    n.forward_group_text(g, e.text.clone())?;
                                }
                                (_, _, Dest::Contact(d)) => {
                                    n.forward_text(d, e.text.clone())?;
                                }
                            }
                        }
                        Ok(())
                    },
                    Some(format!("Forwarded to {title}")),
                );
            },
        );
    }

    /// Keeps what's in the composer, encrypted, for next time.
    fn save_draft(&self) {
        let Some(node) = self.node() else { return };
        if !self.loaded.get() {
            return;
        }
        let buf = self.input.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        let dest = self.dest();
        bg(
            move || match dest {
                Dest::Note => node.set_note_draft(text),
                Dest::Group(g) => node.set_group_draft(g, text),
                Dest::Contact(d) => node.set_draft(d, text),
            },
            |_| {},
        );
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

    /// "Faster link": the contact (a phone nearby) hosts a Wi-Fi Direct
    /// group and this computer joins it. Asks first while joining is off.
    fn wifi_direct(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let go = {
            let weak = self.weak_self.clone();
            move || {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                let c = t.conv.borrow().clone();
                let core = app.core.clone();
                let weak = t.weak_self.clone();
                bg(
                    move || core.faster_link(&c.devices, &c.device, &c.title),
                    move |r| {
                        let Some(t) = weak.upgrade() else { return };
                        match r {
                            Ok(m) => t.toast(&m),
                            Err(e) => t.toast(&format!("Couldn't ask for Wi-Fi Direct: {e}")),
                        }
                    },
                );
            }
        };
        if app.core.settings().opted_in(settings::WIFI_DIRECT) {
            go();
            return;
        }
        let core = app.core.clone();
        ui::confirm(
            &app.window,
            "Turn on Wi-Fi Direct?",
            "Your contact creates a Wi-Fi Direct group and this computer's Wi-Fi joins it, \
             leaving its network until the link ends. You can turn this off in Preferences.",
            "Turn on",
            false,
            move || {
                core.settings().set_flag(settings::WIFI_DIRECT, true);
                go();
            },
        );
    }

    /// Tells the contact (not a group) we started or stopped typing: on a
    /// change, and again now and then while typing goes on.
    fn notify_typing(&self, active: bool) {
        if self.group().is_some() || self.is_note() {
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
        self.mark_seen();
        if self.group().is_some() || self.is_note() {
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

        if c.chat.target == Target::Note {
            self.title.set_subtitle("On all your devices");
        } else if let Some(invite) = &c.invite {
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
            match g.my_role {
                MemberRole::Owner => parts.push("you're the owner".into()),
                MemberRole::Admin => parts.push("you're an admin".into()),
                MemberRole::Member => {}
            }
            if let Some(l) = &persona_label {
                parts.insert(0, format!("🎭 as {l}"));
            }
            self.title.set_subtitle(&parts.join(" · "));
            self.group_banner(g);
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
            // Onion routing couldn't hide the last session or deposit:
            // say so quietly until dismissed.
            let notice = app.as_ref().and_then(|a| {
                let notices = a.notices.borrow();
                std::iter::once(&c.device)
                    .chain(&c.devices)
                    .find_map(|d| notices.get(&(c.chat.persona.clone(), d.clone())).cloned())
            });
            if let Some(n) = notice {
                self.show_banner(&n, false, &[("OK", Self::dismiss_notice as Action)]);
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

    fn dismiss_notice(self: &Rc<Self>) {
        let c = self.conv.borrow().clone();
        if let Some(app) = self.app.upgrade() {
            let mut notices = app.notices.borrow_mut();
            for d in std::iter::once(&c.device).chain(&c.devices) {
                notices.remove(&(c.chat.persona.clone(), d.clone()));
            }
        }
        self.header();
    }

    /// The one banner `GroupInfo` asks for (the same as Android's).
    fn group_banner(self: &Rc<Self>, g: &GroupInfo) {
        let name = |fp: &str| {
            self.node()
                .map_or_else(|| core::short(fp), |n| Core::name_of(&n, fp))
        };
        if g.update_needed {
            self.show_banner(
                "This group's roles come from a newer Threnody. Update to change them here.",
                true,
                &[],
            );
        }
        match &g.banner {
            Some(GroupBanner::OutOfStep) => self.show_banner(
                "This device fell out of step with the group. Asking the others to bring it back…",
                true,
                &[],
            ),
            Some(GroupBanner::CanClaim { owner }) => self.show_banner(
                &format!(
                    "{} hasn't been seen in this group for 14 days. You're next in line.",
                    name(owner)
                ),
                false,
                &[("Take over as owner", Self::claim_group as Action)],
            ),
            Some(GroupBanner::PendingChanges { count, coordinator }) => {
                let text = format!(
                    "{count} {} waiting for {} to come online.",
                    if *count == 1 {
                        "change is"
                    } else {
                        "changes are"
                    },
                    name(coordinator)
                );
                if g.my_role == MemberRole::Owner {
                    self.show_banner(
                        &text,
                        false,
                        &[("Apply from this device", Self::take_over_group as Action)],
                    );
                } else {
                    self.show_banner(&text, false, &[]);
                }
            }
            Some(GroupBanner::UpgradePending { outdated }) => {
                let names: Vec<String> = outdated.iter().map(|o| name(o)).collect();
                self.show_banner(
                    &format!(
                        "Admins can be added once {} update Threnody.",
                        names.join(", ")
                    ),
                    false,
                    &[],
                );
            }
            Some(GroupBanner::OnlyYou) if g.my_role != MemberRole::Member => self.show_banner(
                "Only you are in this group. Invite contacts to it.",
                false,
                &[("Invite", Self::invite_to_group as Action)],
            ),
            Some(GroupBanner::OnlyYou) => {
                self.show_banner("Only you are in this group.", false, &[]);
            }
            None => {}
        }
    }

    fn claim_group(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(info) = self.conv.borrow().group.clone() else {
            return;
        };
        let weak = self.weak_self.clone();
        ui::confirm(
            &app.window,
            &format!("Take over {}?", info.name),
            "You become its owner. The old owner becomes an admin, and this can't be undone.",
            "Take over",
            true,
            move || {
                let Some(t) = weak.upgrade() else { return };
                let Some(g) = t.group() else { return };
                t.run("take over", move |n| n.claim_group_ownership(g), None);
            },
        );
    }

    fn take_over_group(self: &Rc<Self>) {
        let Some(g) = self.group() else { return };
        self.run("apply the changes", move |n| n.take_over_group(g), None);
    }

    /// One line per credential offer or ask waiting here, each with
    /// Review (as on Android). Later, in the review, leaves it waiting.
    fn show_credentials(self: &Rc<Self>, pending: Option<threnody_ffi::PendingCredentials>) {
        while let Some(child) = self.cred_banner.first_child() {
            self.cred_banner.remove(&child);
        }
        let p = pending.unwrap_or(threnody_ffi::PendingCredentials {
            offers: Vec::new(),
            asks: Vec::new(),
        });
        self.cred_banner
            .set_visible(!p.offers.is_empty() || !p.asks.is_empty());
        let title = self.conv.borrow().title.clone();
        let persona = self.persona();
        let line = |text: String, review: Review| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let l = ui::label(&text, &[]);
            l.set_hexpand(true);
            row.append(&l);
            let b = gtk::Button::with_label("Review");
            b.add_css_class("suggested-action");
            b.set_valign(gtk::Align::Center);
            let weak = self.app.clone();
            b.connect_clicked(move |_| {
                if let Some(app) = weak.upgrade() {
                    review(&app);
                }
            });
            row.append(&b);
            self.cred_banner.append(&row);
        };
        for o in p.offers {
            let text = format!("{title} offers you a credential ({}).", o.schema);
            let persona = persona.clone();
            line(
                text,
                Box::new(move |app| app.credential_offer(persona.clone(), o.clone())),
            );
        }
        for a in p.asks {
            let text = if a.keys.is_empty() {
                format!(
                    "{title} asks you to prove you hold a credential ({}).",
                    a.schema
                )
            } else {
                format!(
                    "{title} asks you to prove {} ({}).",
                    a.keys.join(", "),
                    a.schema
                )
            };
            let persona = persona.clone();
            line(
                text,
                Box::new(move |app| app.credential_ask(persona.clone(), a.clone())),
            );
        }
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
            if g.my_role != MemberRole::Member {
                add("Invite contacts", "invite", Self::invite_to_group);
            }
            add("Media, files and links", "media", Self::media);
            add("New poll…", "poll", Self::new_poll);
            add("Disappearing messages", "timer", Self::disappearing);
            add("Notifications", "notify", Self::notifications);
            add("Clear chat…", "clear", Self::clear);
            if g.owned {
                add("Delete group…", "delete", Self::delete_group);
            }
            add("Leave group…", "leave", Self::leave_group);
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
            add("Notifications", "notify", Self::notifications);
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
            // Never for an anonymous identity: a group would show nearby
            // devices who it is.
            if c.connected && c.approved && c.chat.persona.is_none() {
                add(
                    "Faster link (Wi-Fi Direct)",
                    "wifi-direct",
                    Self::wifi_direct,
                );
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
        let (group, device, note) = (self.group(), self.device(), self.is_note());
        let (limit, until) = (self.limit.get(), self.jump.borrow().as_ref().map(|k| k.0));
        let weak = self.weak_self.clone();
        bg(
            move || {
                let fetch = |n| match &group {
                    _ if note => node.note_history(n).unwrap_or_default(),
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
                let (pins, may_pin) = match &group {
                    _ if note => (node.pinned_notes(), true),
                    Some(g) => (
                        node.group_pinned(g.clone()).unwrap_or_default(),
                        node.may_pin_in_group(g.clone()).unwrap_or(false),
                    ),
                    None => (node.pinned(device.clone()).unwrap_or_default(), true),
                };
                let draft = match &group {
                    _ if note => node.note_draft(),
                    Some(g) => node.group_draft(g.clone()).unwrap_or_default(),
                    None => node.draft(device.clone()).unwrap_or_default(),
                };
                let (scheduled, timer) = match &group {
                    _ if note => (Vec::new(), false),
                    Some(g) => (
                        node.group_scheduled(g.clone()).unwrap_or_default(),
                        node.group_disappearing(g.clone())
                            .is_ok_and(|t| t.is_some()),
                    ),
                    None => (
                        node.scheduled(device.clone()).unwrap_or_default(),
                        node.disappearing(device.clone()).is_ok_and(|t| t.is_some()),
                    ),
                };
                let pending = (group.is_none() && !note)
                    .then(|| node.pending_credentials(device.clone()).ok())
                    .flatten();
                (n, entries, pins, may_pin, draft, scheduled, timer, pending)
            },
            move |(n, entries, pins, may_pin, draft, scheduled, timer, pending)| {
                let Some(t) = weak.upgrade() else { return };
                t.show_credentials(pending);
                // What we left unsent here last time.
                if !t.loaded.get() && !draft.is_empty() {
                    let buf = t.input.buffer();
                    if buf.char_count() == 0 {
                        buf.set_text(&draft);
                    }
                }
                t.loading.set(false);
                t.limit.set(n);
                t.may_pin.set(may_pin);
                t.show_pins(pins);
                t.timer_on.set(timer);
                t.show_scheduled(scheduled);
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
        self.fetching.borrow_mut().clear();
        let node = self.node();
        let group = self.group().is_some();
        let mut last_day = String::new();
        let mut prev: Option<&HistoryEntry> = None;
        for run in album_runs(&entries) {
            let e = &entries[run.start];
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
            self.messages
                .append(&self.bubble(&entries[run.clone()], sender));
            prev = entries.get(run.end - 1);
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

    /// A message's bubble; a run of an album's photos makes one bubble.
    fn bubble(&self, run: &[HistoryEntry], sender: Option<String>) -> gtk::Box {
        let e = &run[0];
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
        if e.forwarded {
            bubble.append(&ui::label("↪ Forwarded", &["caption", "dim-label"]));
        }
        if let Some(q) = &e.quote {
            bubble.append(&self.quote_view(q));
        }
        if let (Some(p), None) = (&e.poll, &e.file) {
            let q = ui::label(&e.text, &["message-text", "heading"]);
            q.set_selectable(true);
            q.set_max_width_chars(60);
            bubble.append(&q);
            bubble.append(&self.poll_view(e, p));
        } else if let Some(f) = &e.file {
            if run.len() > 1 {
                bubble.append(&self.album_view(run));
            } else {
                bubble.append(&self.file_view(e, f));
            }
            if !e.text.is_empty() {
                let caption = ui::label(&e.text, &["message-text"]);
                caption.set_selectable(true);
                bubble.append(&caption);
            }
        } else {
            let text = ui::label(&e.text, &["message-text"]);
            text.set_selectable(true);
            text.set_max_width_chars(60);
            // Links are clickable; mentions show our name for them.
            let markup = if e.mentions.is_empty() {
                linkify(&e.text)
            } else {
                Some(self.mention_markup(e))
            };
            if let Some(markup) = markup {
                text.set_markup(&markup);
                // Links ask before opening in the browser.
                let weak = self.app.clone();
                text.connect_activate_link(move |_, uri| {
                    if let Some(app) = weak.upgrade() {
                        ui::confirm_link(&app.window, uri);
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
            // Sent from another of our devices: its ticks as that device
            // passed them on, never a lone "sent", which only it knows.
            if self
                .node()
                .is_some_and(|n| n.device_fingerprint() != e.device)
            {
                bits.push("from your other device".into());
            }
            match e.tick {
                TickMark::Read => bits.push("👁️".into()),
                TickMark::Delivered => bits.push("✓✓".into()),
                TickMark::Partial {
                    delivered,
                    recipients,
                } => bits.push(format!("✓ {delivered}/{recipients}")),
                TickMark::Sent => bits.push("✓".into()),
                TickMark::None => {}
            }
        }
        let m = gtk::Label::new(Some(&bits.join(" ")));
        m.add_css_class("caption");
        m.add_css_class("bubble-meta");
        meta.append(&m);
        if let Some(id) = &e.list {
            let open = gtk::Button::with_label("Open list");
            open.add_css_class("pill");
            open.set_halign(gtk::Align::Start);
            let (weak, id) = (self.app.clone(), id.clone());
            open.connect_clicked(move |_| {
                if let Some(app) = weak.upgrade() {
                    app.open_lists(Some(id.clone()));
                }
            });
            bubble.append(&open);
        }
        bubble.append(&meta);
        row.append(&bubble);
        // Search finds any photo of an album in its bubble.
        self.bubbles.borrow_mut().extend(
            run.iter()
                .map(|e| ((e.at_ms, e.device.clone()), bubble.clone())),
        );

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
        let (weak, entries, anchor) = (self.weak_self.clone(), run.to_vec(), bubble.clone());
        click.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            if let Some(t) = weak.upgrade() {
                t.message_menu(&anchor, &entries, x, y);
            }
        });
        bubble.add_controller(click);
        let press = gtk::GestureLongPress::new();
        press.set_touch_only(true);
        let (weak, entries, anchor) = (self.weak_self.clone(), run.to_vec(), bubble.clone());
        press.connect_pressed(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                t.message_menu(&anchor, &entries, x, y);
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
        if path.is_none() && f.location.is_none() && f.fetch != FileFetch::NotHere {
            return self.fetch_view(e, f);
        }
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
        if core::is_image(&f.name) && path.is_some() {
            let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
            frame.set_size_request(320, 240);
            let cell = self.photo_cell(e, f, (240, 180), gtk::ContentFit::Contain);
            cell.set_vexpand(true);
            frame.append(&cell);
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

    /// An album's photos as a two-column grid of square cells.
    fn album_view(&self, run: &[HistoryEntry]) -> gtk::Widget {
        let grid = gtk::Grid::builder()
            .column_spacing(4)
            .row_spacing(4)
            .column_homogeneous(true)
            .build();
        let cells = run.iter().filter_map(|e| Some((e, e.file.as_ref()?)));
        for (i, (e, f)) in cells.enumerate() {
            let cell = self.photo_cell(e, f, (130, 130), gtk::ContentFit::Cover);
            cell.add_css_class("album-cell");
            let i = i32::try_from(i).unwrap_or(0);
            grid.attach(&cell, i % 2, i / 2, 1, 1);
        }
        grid.upcast()
    }

    /// One photo in the chat, about `size`: a sensitive one (sent or
    /// received) covered and not even decoded, the others decoded off
    /// the main thread. Clicking opens it in the viewer, never another app.
    fn photo_cell(
        &self,
        e: &HistoryEntry,
        f: &threnody_ffi::FileInfo,
        size: (i32, i32),
        fit: gtk::ContentFit,
    ) -> gtk::Widget {
        let path = f
            .location
            .as_ref()
            .map(std::path::PathBuf::from)
            .filter(|p| p.exists());
        let Some(p) = path else {
            // An album's photo another of our devices may have.
            if f.location.is_none() && f.fetch != FileFetch::NotHere {
                return self.fetch_view(e, f);
            }
            let l = gtk::Label::new(Some("📷 not available"));
            l.add_css_class("dim-label");
            l.add_css_class("caption");
            l.add_css_class("photo-missing");
            l.set_size_request(size.0, size.1);
            return l.upcast();
        };
        let open = {
            let (weak, e, f, p) = (self.weak_self.clone(), e.clone(), f.clone(), p.clone());
            move || {
                if let Some(t) = weak.upgrade() {
                    t.view_photo(&e, &f, &p);
                }
            }
        };
        if f.sensitive {
            let b = gtk::Button::with_label("Sensitive photo · click to view");
            b.add_css_class("sensitive-cover");
            b.set_size_request(size.0, size.1);
            b.set_tooltip_text(Some("Opens in the viewer"));
            b.connect_clicked(move |_| open());
            return b.upcast();
        }
        let pic = gtk::Picture::new();
        if viewer::is_gif(&f.name) {
            // GTK decodes GIFs itself, here: only reasonable ones.
            if viewer::checked_size(&p).is_ok() {
                pic.set_filename(Some(&p));
                ui::animate(&pic, &p);
            }
        } else {
            viewer::thumbnail(
                &pic,
                Rc::default(),
                p.clone(),
                THUMB_SIDE,
                self.thumbs.clone(),
            );
        }
        pic.set_can_shrink(true);
        pic.set_content_fit(fit);
        pic.set_size_request(size.0, size.1);
        pic.add_css_class("photo");
        pic.set_cursor_from_name(Some("pointer"));
        pic.set_tooltip_text(Some(&f.name));
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_released(move |_, _, _, _| open());
        pic.add_controller(click);
        pic.upcast()
    }

    /// Opens a photo in the viewer; an edited copy is offered for sending
    /// here, sensitive if the photo was.
    fn view_photo(&self, e: &HistoryEntry, f: &threnody_ffi::FileInfo, path: &std::path::Path) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let weak = self.weak_self.clone();
        let send: viewer::SendEdited = Rc::new(move |draft, sensitive| {
            if let Some(t) = weak.upgrade() {
                t.confirm_files(vec![draft], String::new(), sensitive);
            }
        });
        let photo = viewer::Photo {
            path: path.to_owned(),
            name: f.name.clone(),
            caption: e.text.clone(),
            sensitive: f.sensitive,
        };
        viewer::show(&app, self.persona(), photo, Some(send));
    }

    /// A file whose contents another of our devices may have: what it is,
    /// how far getting it got, and a button to get it.
    fn fetch_view(&self, e: &HistoryEntry, f: &threnody_ffi::FileInfo) -> gtk::Widget {
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
            Some(c) => core::file_label(&f.name, f.sensitive, "", Some(c)),
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
        let state = ui::label(&fetch_text(f.size, f.fetch), &["caption", "dim-label"]);
        text.append(&state);
        row.append(&text);
        if matches!(f.fetch, FileFetch::Idle | FileFetch::Unavailable) {
            let get = gtk::Button::with_label("Get");
            get.set_tooltip_text(Some("Get it from your other device"));
            get.add_css_class("pill");
            get.set_valign(gtk::Align::Center);
            let (weak, at, device) = (self.weak_self.clone(), e.at_ms, e.device.clone());
            get.connect_clicked(move |b| {
                b.set_sensitive(false);
                if let Some(t) = weak.upgrade() {
                    t.fetch(at, device.clone());
                }
            });
            row.append(&get);
        } else {
            self.fetching
                .borrow_mut()
                .push(((e.at_ms, e.device.clone()), state, f.size));
            self.poll_fetches();
        }
        row.upcast()
    }

    /// Asks our other devices for the file `device` sent at `at`.
    fn fetch(self: &Rc<Self>, at: u64, device: String) {
        let dest = self.dest();
        self.run(
            "get that file",
            move |n| {
                match dest {
                    Dest::Note => n.fetch_note_file(at, device),
                    Dest::Group(g) => n.fetch_group_file(g, at, device),
                    Dest::Contact(d) => n.fetch_file(d, at, device),
                }
                .map(|_| ())
            },
            None,
        );
    }

    /// Keeps the progress of files on screen current, once a second while
    /// any is coming or waiting (cheap: no history is read). A file that
    /// arrived, or that none of our devices has, reloads the chat.
    fn poll_fetches(&self) {
        if self.fetch_poll.borrow().is_some() {
            return;
        }
        let weak = self.weak_self.clone();
        let id = glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            let Some(t) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let node = t.node();
            if t.fetching.borrow().is_empty() || node.is_none() {
                t.fetch_poll.replace(None);
                return glib::ControlFlow::Break;
            }
            let (weak, node) = (weak.clone(), node.expect("checked"));
            bg(
                move || node.file_fetches(),
                move |now| {
                    let Some(t) = weak.upgrade() else { return };
                    let mut changed = false;
                    for ((at, device), label, size) in t.fetching.borrow().iter() {
                        match now.iter().find(|p| p.at_ms == *at && p.device == *device) {
                            Some(p) if p.fetch != FileFetch::Unavailable => {
                                label.set_text(&fetch_text(*size, p.fetch));
                            }
                            _ => changed = true,
                        }
                    }
                    if changed {
                        t.reload();
                    }
                },
            );
            glib::ControlFlow::Continue
        });
        self.fetch_poll.replace(Some(id));
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
        let dest = self.dest();
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
                send_file_to(&node, &dest, name, data, location, options).map_err(|e| e.to_string())
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

    /// Opens a file that isn't a photo in the app the desktop picks.
    fn open_file(&self, path: &std::path::Path) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let weak = self.app.clone();
        ui::open_externally(&app.window, path, move || {
            if let Some(a) = weak.upgrade() {
                a.toast("No application can open this file");
            }
        });
    }

    /// A message's menu. For an album (`run`: its photos), reply, react
    /// and pin use the first photo; forward and delete take them all.
    fn message_menu(self: &Rc<Self>, anchor: &gtk::Box, run: &[HistoryEntry], x: f64, y: f64) {
        let e = &run[0];
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
        if e.id != 0 && !self.is_note() {
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
        {
            let (weak, entry) = (self.weak_self.clone(), e.clone());
            item(
                "Reply",
                Box::new(move || {
                    if let Some(t) = weak.upgrade() {
                        t.reply(Some(entry.clone()));
                    }
                }),
            );
        }
        if e.outgoing && e.poll.as_ref().is_some_and(|p| !p.closed) {
            let (weak, id) = (self.weak_self.clone(), e.id);
            item(
                "End poll",
                Box::new(move || {
                    if let Some(t) = weak.upgrade()
                        && let Some(g) = t.group()
                    {
                        t.run(
                            "end the poll",
                            move |n| n.close_group_poll(g, id).map(|_| ()),
                            None,
                        );
                    }
                }),
            );
        }
        if e.id != 0 && self.may_pin.get() {
            let pinned = self.pins.borrow().iter().any(|p| p.id == e.id);
            let (weak, entry) = (self.weak_self.clone(), e.clone());
            item(
                if pinned { "Unpin" } else { "Pin" },
                Box::new(move || {
                    if let Some(t) = weak.upgrade() {
                        t.pin(&entry, !pinned);
                    }
                }),
            );
        }
        if e.poll.is_none() {
            let (weak, entries) = (self.weak_self.clone(), run.to_vec());
            item(
                "Forward",
                Box::new(move || {
                    if let Some(t) = weak.upgrade() {
                        t.forward(&entries);
                    }
                }),
            );
        }
        if e.file.is_none() && !e.text.is_empty() {
            let (text, a) = (e.text.clone(), anchor.clone());
            item("Copy text", Box::new(move || a.clipboard().set_text(&text)));
        }
        // Notes have no one to edit or delete for.
        let group = self.group().is_some() || self.is_note();
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
        let (weak, entries) = (self.weak_self.clone(), run.to_vec());
        item(
            "Delete for me",
            Box::new(move || {
                if let Some(t) = weak.upgrade() {
                    t.delete(&entries, false);
                }
            }),
        );
        if run.iter().all(|e| e.outgoing && e.id != 0) && !group {
            let (weak, entries) = (self.weak_self.clone(), run.to_vec());
            item(
                "Delete for everyone",
                Box::new(move || {
                    if let Some(t) = weak.upgrade() {
                        t.delete(&entries, true);
                    }
                }),
            );
        }
        pop.set_child(Some(&b));
        pop.popup();
    }

    /// Shows the pinned messages in the bar (the newest first).
    fn show_pins(self: &Rc<Self>, pins: Vec<HistoryEntry>) {
        let same = pins
            .iter()
            .map(|e| e.id)
            .eq(self.pins.borrow().iter().map(|e| e.id));
        if !same {
            self.pin_shown.set(0);
        }
        *self.pins.borrow_mut() = pins;
        self.draw_pin_bar();
    }

    fn draw_pin_bar(self: &Rc<Self>) {
        let pins = self.pins.borrow();
        self.pin_bar.set_visible(!pins.is_empty());
        let Some(e) = pins
            .iter()
            .rev()
            .nth(self.pin_shown.get().min(pins.len().saturating_sub(1)))
        else {
            return;
        };
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        b.append(&gtk::Image::from_icon_name("view-pin-symbolic"));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        let title = if pins.len() > 1 {
            format!(
                "Pinned message {} of {}",
                self.pin_shown.get() + 1,
                pins.len()
            )
        } else {
            "Pinned message".into()
        };
        text.append(&ui::label(&title, &["caption-heading", "accent"]));
        let snippet = if e.text.is_empty() {
            e.file.as_ref().map(|f| f.name.clone()).unwrap_or_default()
        } else {
            e.text.clone()
        };
        let l = ui::label(&snippet, &["caption"]);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&l);
        b.append(&text);
        self.pin_bar.set_child(Some(&b));
    }

    /// Shows the pinned message in the bar, then puts the next one there.
    fn next_pin(self: &Rc<Self>) {
        let key = {
            let pins = self.pins.borrow();
            if pins.is_empty() {
                return;
            }
            let i = self.pin_shown.get().min(pins.len() - 1);
            self.pin_shown.set((i + 1) % pins.len());
            pins.iter()
                .rev()
                .nth(i)
                .map(|e| (e.at_ms, e.device.clone()))
        };
        if let Some(k) = key {
            self.jump_to(k);
        }
        self.draw_pin_bar();
    }

    fn pin(self: &Rc<Self>, e: &HistoryEntry, pin: bool) {
        let Some(node) = self.node() else { return };
        let (group, device, id) = (self.group(), self.device(), e.id);
        let note = self.is_note();
        let weak = self.weak_self.clone();
        bg(
            move || match group {
                _ if note => Ok(node.pin_note(id, pin)),
                Some(g) => node.pin_in_group(g, id, pin).map_err(|e| e.to_string()),
                None => node.pin(device, id, pin).map_err(|e| e.to_string()),
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                if let Err(e) = r {
                    t.toast(&format!("Couldn't pin: {e}"));
                }
                t.reload();
            },
        );
    }

    /// A poll's options with their share of the votes; clicking one votes.
    fn poll_view(&self, e: &HistoryEntry, p: &threnody_ffi::PollInfo) -> gtk::Widget {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let mut status = vec![
            if p.multi { "Choose any" } else { "Choose one" }.to_owned(),
            format!("{} voted", p.voters),
        ];
        if p.closed {
            status.push("closed".into());
        }
        b.append(&ui::label(&status.join(" · "), &["caption", "dim-label"]));
        let mine: Vec<u32> = (0..p.options.len() as u32)
            .filter(|i| p.options[*i as usize].mine)
            .collect();
        for (i, o) in p.options.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let text = ui::label(
                &format!("{}{}", if o.mine { "✓ " } else { "" }, o.text),
                if o.mine { &["heading"] } else { &[] },
            );
            text.set_hexpand(true);
            top.append(&text);
            top.append(&ui::label(&o.votes.to_string(), &["caption", "dim-label"]));
            row.append(&top);
            let bar = gtk::LevelBar::for_interval(0.0, f64::from(p.voters.max(1)));
            bar.set_value(f64::from(o.votes));
            row.append(&bar);
            let button = gtk::Button::builder().child(&row).build();
            button.add_css_class("flat");
            button.add_css_class("poll-option");
            button.set_sensitive(!p.closed);
            let choice = i as u32;
            let choices: Vec<u32> = match (p.multi, mine.contains(&choice)) {
                (true, true) => mine.iter().copied().filter(|c| *c != choice).collect(),
                (true, false) => mine.iter().copied().chain([choice]).collect(),
                (false, true) => Vec::new(),
                (false, false) => vec![choice],
            };
            let (weak, id) = (self.weak_self.clone(), e.id);
            button.connect_clicked(move |_| {
                let Some(t) = weak.upgrade() else { return };
                let Some(g) = t.group() else { return };
                let c = choices.clone();
                t.run("vote", move |n| n.vote_in_group(g, id, c).map(|_| ()), None);
            });
            b.append(&button);
        }
        b.upcast()
    }

    /// Asks the group a question.
    fn new_poll(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let d = ui::alert(
            "New poll",
            "Members pick from the options; you can end the poll later.",
            &[("cancel", "Cancel"), ("ok", "Send")],
        );
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let question = gtk::Entry::builder().placeholder_text("Question").build();
        form.append(&question);
        let options = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let add_option = {
            let options = options.clone();
            move || {
                let n = {
                    let mut n = 0;
                    let mut c = options.first_child();
                    while let Some(w) = c {
                        n += 1;
                        c = w.next_sibling();
                    }
                    n
                };
                if n < 12 {
                    options.append(
                        &gtk::Entry::builder()
                            .placeholder_text(format!("Option {}", n + 1))
                            .build(),
                    );
                }
            }
        };
        add_option();
        add_option();
        form.append(&options);
        let more = gtk::Button::with_label("Add an option");
        more.add_css_class("flat");
        more.connect_clicked(move |_| add_option());
        form.append(&more);
        let multi = gtk::CheckButton::with_label("Allow several choices");
        form.append(&multi);
        d.set_extra_child(Some(&form));
        let weak = self.weak_self.clone();
        d.connect_response(None, move |_, r| {
            if r != "ok" {
                return;
            }
            let Some(t) = weak.upgrade() else { return };
            let Some(g) = t.group() else { return };
            let mut opts = Vec::new();
            let mut c = options.first_child();
            while let Some(w) = c {
                if let Some(e) = w.downcast_ref::<gtk::Entry>() {
                    let o = e.text().trim().to_owned();
                    if !o.is_empty() {
                        opts.push(o);
                    }
                }
                c = w.next_sibling();
            }
            let q = question.text().trim().to_owned();
            if q.is_empty() || opts.len() < 2 {
                t.toast("A poll needs a question and at least two options.");
                return;
            }
            let multi = multi.is_active();
            t.run(
                "send the poll",
                move |n| n.send_group_poll(g, q, opts, multi),
                None,
            );
        });
        d.present(Some(&app.window));
    }

    /// `e`'s text as markup, with mentions as "@" and our name for them
    /// (ours highlighted), and links clickable.
    fn mention_markup(&self, e: &HistoryEntry) -> String {
        let chars: Vec<char> = e.text.chars().collect();
        let segment = |a: usize, b: usize| -> String {
            let t: String = chars[a.min(chars.len())..b.min(chars.len())]
                .iter()
                .collect();
            linkify(&t).unwrap_or_else(|| glib::markup_escape_text(&t).to_string())
        };
        let mut out = String::new();
        let mut from = 0usize;
        for m in &e.mentions {
            let (a, b) = (m.start as usize, (m.start + m.len) as usize);
            if a < from || b > chars.len() {
                continue;
            }
            out.push_str(&segment(from, a));
            let name = glib::markup_escape_text(&format!("@{}", self.quote_author(&m.device)));
            if m.mine {
                out.push_str(&format!(
                    "<span weight=\"bold\" bgcolor=\"#3584e4\" bgalpha=\"25%\">{name}</span>"
                ));
            } else {
                out.push_str(&format!("<b>{name}</b>"));
            }
            from = b;
        }
        out.push_str(&segment(from, chars.len()));
        out
    }

    /// In a group, offers the members matching an "@" being typed.
    fn offer_mentions(self: &Rc<Self>, buf: &gtk::TextBuffer) {
        let pop = &self.mention_popover;
        // An edited mention is just text again.
        self.mentions.borrow_mut().retain(|(a, b, _, label)| {
            let keep =
                buf.text(&buf.iter_at_mark(a), &buf.iter_at_mark(b), false) == label.as_str();
            if !keep {
                buf.delete_mark(a);
                buf.delete_mark(b);
            }
            keep
        });
        let Some(info) = self.conv.borrow().group.clone() else {
            return pop.popdown();
        };
        let Some(node) = self.node() else { return };
        let cursor = buf.iter_at_mark(&buf.get_insert());
        let line_start = {
            let mut i = cursor;
            i.set_line_offset(0);
            i
        };
        let before = buf.text(&line_start, &cursor, false).to_string();
        let Some(at) = before.rfind('@') else {
            return pop.popdown();
        };
        let query = &before[at + 1..];
        let starts_word = before[..at].chars().last().is_none_or(char::is_whitespace);
        if !starts_word || query.chars().any(char::is_whitespace) {
            return pop.popdown();
        }
        let me = node.device_fingerprint();
        let hits: Vec<(String, String)> = info
            .members
            .iter()
            .filter(|m| **m != me)
            .map(|m| (m.clone(), Core::name_of(&node, m)))
            .filter(|(_, n)| n.to_lowercase().starts_with(&query.to_lowercase()))
            .take(6)
            .collect();
        if hits.is_empty() {
            return pop.popdown();
        }
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        for (_, name) in &hits {
            list.append(&ui::label(name, &[]));
        }
        let at_chars = before[..at].chars().count() as i32;
        let (weak, hits2) = (self.weak_self.clone(), hits.clone());
        list.connect_row_activated(move |_, row| {
            let Some(t) = weak.upgrade() else { return };
            let Some((device, name)) = hits2.get(row.index() as usize).cloned() else {
                return;
            };
            let buf = t.input.buffer();
            let mut start = buf.iter_at_mark(&buf.get_insert());
            start.set_line_offset(at_chars);
            let mut end = buf.iter_at_mark(&buf.get_insert());
            buf.delete(&mut start, &mut end);
            let label = format!("@{name}");
            let a = buf.create_mark(None, &start, true);
            buf.insert(&mut start, &label);
            let b = buf.create_mark(None, &start, true);
            buf.insert(&mut start, " ");
            t.mentions.borrow_mut().push((a, b, device, label));
            t.mention_popover.popdown();
            t.input.grab_focus();
        });
        pop.set_child(Some(&list));
        pop.popup();
    }

    /// The mentions in the composer, as the node counts (characters), and
    /// forgets them.
    fn take_mentions(&self, buf: &gtk::TextBuffer) -> Vec<threnody_ffi::MentionInfo> {
        let full = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        let lead = (full.chars().count() - full.trim_start().chars().count()) as i32;
        let mut out: Vec<threnody_ffi::MentionInfo> = self
            .mentions
            .borrow()
            .iter()
            .filter_map(|(a, b, device, label)| {
                let (a, b) = (buf.iter_at_mark(a), buf.iter_at_mark(b));
                (buf.text(&a, &b, false) == label.as_str()).then(|| threnody_ffi::MentionInfo {
                    start: (a.offset() - lead).max(0) as u32,
                    len: (b.offset() - a.offset()) as u32,
                    device: device.clone(),
                    mine: false,
                })
            })
            .collect();
        out.sort_by_key(|m| m.start);
        for (a, b, ..) in self.mentions.borrow_mut().drain(..) {
            buf.delete_mark(&a);
            buf.delete_mark(&b);
        }
        self.mention_popover.popdown();
        out
    }

    /// Who sent a quoted message, by device: us, or their name.
    fn quote_author(&self, author: &str) -> String {
        match self.node() {
            Some(n) if n.device_fingerprint() == author => "You".into(),
            Some(n) => Core::name_of(&n, author),
            None => core::short(author),
        }
    }

    /// What a reply quotes, above its text; clicking it shows the original.
    fn quote_view(&self, q: &threnody_ffi::QuoteInfo) -> gtk::Widget {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        b.add_css_class("quote");
        b.append(&ui::label(
            &self.quote_author(&q.author),
            &["caption-heading", "accent"],
        ));
        let text = if q.text.is_empty() {
            ui::label("Original message not found", &["caption", "dim-label"])
        } else {
            ui::label(&q.text, &["caption"])
        };
        text.set_lines(3);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.set_max_width_chars(50);
        b.append(&text);
        let button = gtk::Button::builder().child(&b).build();
        button.add_css_class("flat");
        button.add_css_class("quote-button");
        button.set_tooltip_text(Some("Show the original"));
        let (weak, q) = (self.weak_self.clone(), q.clone());
        button.connect_clicked(move |_| {
            let Some(t) = weak.upgrade() else { return };
            let key = t
                .entries
                .borrow()
                .iter()
                .rev()
                .find(|e| quotes(&q, e))
                .map(|e| (e.at_ms, e.device.clone()));
            match key {
                Some(k) => t.jump_to(k),
                None => t.toast("The original isn't loaded here."),
            }
        });
        button.upcast()
    }

    /// Starts (or with `None`, cancels) a reply to `e`.
    fn reply(self: &Rc<Self>, e: Option<HistoryEntry>) {
        while let Some(c) = self.reply_bar.first_child() {
            self.reply_bar.remove(&c);
        }
        self.reply_bar.set_visible(e.is_some());
        if let Some(e) = &e {
            let who = if e.outgoing {
                "yourself".to_owned()
            } else {
                self.quote_author(&e.device)
            };
            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_hexpand(true);
            text.append(&ui::label(
                &format!("Replying to {who}"),
                &["caption-heading", "accent"],
            ));
            let snippet = if e.text.is_empty() {
                e.file.as_ref().map(|f| f.name.clone()).unwrap_or_default()
            } else {
                e.text.clone()
            };
            let l = ui::label(&snippet, &["caption", "dim-label"]);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&l);
            self.reply_bar.append(&text);
            let close = gtk::Button::from_icon_name("window-close-symbolic");
            close.add_css_class("flat");
            close.add_css_class("circular");
            close.set_valign(gtk::Align::Center);
            close.set_tooltip_text(Some("Cancel the reply"));
            let weak = self.weak_self.clone();
            close.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.reply(None);
                }
            });
            self.reply_bar.append(&close);
            self.input.grab_focus();
        }
        *self.reply_to.borrow_mut() = e;
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

    /// Deletes messages: one, or every photo of an album.
    fn delete(self: &Rc<Self>, entries: &[HistoryEntry], everyone: bool) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let (group, device, entries) = (self.group(), self.device(), entries.to_vec());
        let note = self.is_note();
        let ok = everyone.then(|| {
            "Deleted. Their devices delete it too, if they're running a current version.".to_owned()
        });
        app.run(
            self.persona(),
            "delete",
            move |n| {
                // With a chat's message ids, all in one go.
                if !note && group.is_none() && entries.iter().all(|e| e.id != 0) {
                    let ids = entries.iter().map(|e| e.id).collect();
                    return n.delete_messages(device, ids, everyone).map(|_| ());
                }
                for e in entries {
                    match &group {
                        _ if note => n.delete_note(e.at_ms, e.device).map(|_| ()),
                        Some(g) => n
                            .delete_group_entry(g.clone(), e.at_ms, e.device)
                            .map(|_| ()),
                        None if e.id != 0 => n
                            .delete_messages(device.clone(), vec![e.id], everyone)
                            .map(|_| ()),
                        None => n
                            .delete_entry(device.clone(), e.at_ms, e.device)
                            .map(|_| ()),
                    }?;
                }
                Ok(())
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

    /// The composer's text, if there's any to send.
    fn composed(&self) -> Option<String> {
        let buf = self.input.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .trim()
            .to_owned();
        (!text.is_empty()).then_some(text)
    }

    /// Shows "Send later…" by the send button.
    fn offer_later(&self) {
        if self.composed().is_some() {
            self.later_menu.popup();
        }
    }

    /// Asks when, then schedules what's in the composer (with the reply
    /// and mentions). The composer is left alone until a time is picked,
    /// so cancelling loses nothing.
    fn send_later(self: &Rc<Self>) {
        if self.is_note() || self.composed().is_none() {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let weak = self.weak_self.clone();
        schedule::pick_time(&app.window, None, move |due| {
            if let Some(t) = weak.upgrade() {
                t.schedule_composed(due);
            }
        });
    }

    fn schedule_composed(self: &Rc<Self>, due: u64) {
        if !schedule::in_future(due) {
            return self.toast("Pick a time in the future");
        }
        let Some(text) = self.composed() else { return };
        let Some(node) = self.node() else { return };
        let buf = self.input.buffer();
        let mentions = self.take_mentions(&buf);
        // Clearing the composer also says we stopped typing.
        buf.set_text("");
        let (group, device) = (self.group(), self.device());
        let (at, d) = self
            .reply_to
            .borrow()
            .as_ref()
            .map(|e| (e.at_ms, e.device.clone()))
            .unzip();
        self.reply(None);
        let weak = self.weak_self.clone();
        let sent = text.clone();
        bg(
            move || match group {
                Some(g) => node.schedule_group_text(g, sent, due, at, d, mentions),
                None => node.schedule_text(device, sent, due, at, d),
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                match r {
                    Ok(_) => t.toast(&format!("Scheduled for {}", schedule::when_label(due))),
                    Err(e) => {
                        t.toast(&e.to_string());
                        let buf = t.input.buffer();
                        if buf.char_count() == 0 {
                            buf.set_text(&text);
                        }
                    }
                }
                t.reload();
            },
        );
    }

    /// The scheduled bar: "🕓 2 scheduled · next Tue 08:00"; hidden with
    /// nothing scheduled.
    fn show_scheduled(&self, items: Vec<threnody_ffi::ScheduledInfo>) {
        self.scheduled_bar.set_visible(!items.is_empty());
        if !items.is_empty() {
            let failed = items.iter().filter(|i| i.failed.is_some()).count();
            let mut text = format!("🕓 {} scheduled", items.len());
            if let Some(next) = items.iter().find(|i| i.failed.is_none()) {
                text.push_str(&format!(" · next {}", schedule::when_label(next.due_ms)));
            }
            if failed > 0 {
                text.push_str(&format!(" · ⚠ {failed} not sent"));
            }
            let l = ui::label(&text, &["caption"]);
            if failed > 0 {
                l.add_css_class("warning");
            } else {
                l.add_css_class("accent");
            }
            self.scheduled_bar.set_child(Some(&l));
        }
        *self.scheduled.borrow_mut() = items;
    }

    /// This chat's scheduled messages, to send now, edit, move or delete.
    fn scheduled_list(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let items = self.scheduled.borrow().clone();
        if items.is_empty() {
            return;
        }
        let weak = self.weak_self.clone();
        schedule::list_dialog(
            &app.window,
            &items,
            self.timer_on.get(),
            move |act, item| {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                let id = item.id;
                match act {
                    schedule::Act::SendNow => t.run(
                        "send it",
                        move |n| being_sent(n.send_scheduled_now(id)),
                        None,
                    ),
                    schedule::Act::Edit => {
                        let weak = t.weak_self.clone();
                        ui::ask_text(
                            &app.window,
                            "Edit scheduled message",
                            "",
                            "Message",
                            &item.text,
                            "Save",
                            move |text| {
                                let Some(t) = weak.upgrade() else { return };
                                t.run(
                                    "edit it",
                                    move |n| being_sent(n.edit_scheduled(id, text)?),
                                    None,
                                );
                            },
                        );
                    }
                    schedule::Act::Move => {
                        let weak = t.weak_self.clone();
                        schedule::pick_time(&app.window, Some(item.due_ms), move |due| {
                            let Some(t) = weak.upgrade() else { return };
                            if !schedule::in_future(due) {
                                return t.toast("Pick a time in the future");
                            }
                            t.run(
                                "change the time",
                                move |n| being_sent(n.reschedule(id, due)?),
                                None,
                            );
                        });
                    }
                    schedule::Act::Delete => {
                        let weak = t.weak_self.clone();
                        ui::confirm(
                            &app.window,
                            "Delete this scheduled message?",
                            "It won't be sent.",
                            "Delete",
                            true,
                            move || {
                                let Some(t) = weak.upgrade() else { return };
                                t.run(
                                    "delete it",
                                    move |n| being_sent(n.cancel_scheduled(id)),
                                    None,
                                );
                            },
                        );
                    }
                }
            },
        );
    }

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
        let mentions = self.take_mentions(&buf);
        // Clearing the composer also says we stopped typing.
        buf.set_text("");
        let (group, device) = (self.group(), self.device());
        let quote = self
            .reply_to
            .borrow()
            .as_ref()
            .map(|e| (e.at_ms, e.device.clone()));
        self.reply(None);
        let weak = self.weak_self.clone();
        let sent = text.clone();
        let note = self.is_note();
        bg(
            move || match group {
                _ if note => {
                    let (at, d) = quote.unzip();
                    node.write_note(sent, at, d)
                        .map(|()| None)
                        .map_err(|e| e.to_string())
                }
                Some(g) if quote.is_some() || !mentions.is_empty() => {
                    let (at, d) = quote.unzip();
                    node.send_group_message(g, sent, at, d, mentions)
                        .map(|()| None)
                        .map_err(|e| e.to_string())
                }
                Some(g) => node
                    .send_group_text(g, sent)
                    .map(|()| None)
                    .map_err(|e| e.to_string()),
                // It reaches the contact directly, by relay, or sealed for mailboxes.
                None => match match quote {
                    Some((at, d)) => node.send_reply(device, sent, at, d),
                    None => node.send_text(device, sent),
                } {
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
                let how = core::Keep {
                    clip: false,
                    sensitive: false,
                    disappears: false,
                };
                core.keep(persona.as_deref(), &name, &bytes, how)
                    .ok_or_else(|| "couldn't save it".to_owned())
            },
            move |kept: Result<String, String>| {
                let Some(t) = weak.upgrade() else { return };
                match kept {
                    Ok(path) => t.confirm_files(vec![path.into()], String::new(), false),
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
                t.confirm_files(paths, String::new(), false);
            }
        });
    }

    /// Asks for a caption and whether the files are sensitive, then sends.
    /// Photos show as thumbnails with *Edit*; an edit replaces the photo
    /// here and keeps the caption and *Sensitive* as they were.
    fn confirm_files(
        self: &Rc<Self>,
        paths: Vec<std::path::PathBuf>,
        caption: String,
        sensitive: bool,
    ) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let name_of = |p: &std::path::Path| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let images = paths.iter().filter(|p| core::is_image(&name_of(p))).count();
        let what = match (paths.len(), images == paths.len()) {
            (1, true) => "a photo".to_owned(),
            (1, false) => "a file".to_owned(),
            (n, true) => format!("{n} photos"),
            (n, false) => format!("{n} files"),
        };
        let d = ui::alert(
            &format!("Send {what}"),
            "",
            &[("cancel", "Cancel"), ("send", "Send")],
        );
        d.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let caption_entry = gtk::Entry::builder()
            .placeholder_text("Add a message")
            .text(&caption)
            .activates_default(true)
            .build();
        let sensitive_check = gtk::CheckButton::with_label(if images > 0 {
            "Sensitive: they see it covered until they click it"
        } else {
            "Sensitive: shown covered until opened"
        });
        sensitive_check.set_active(sensitive);
        // Closed to edit a photo: its drafts stay for the dialog that
        // comes back.
        let editing = Rc::new(Cell::new(false));
        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let thumbs = Thumbs::default();
        for (i, p) in paths.iter().enumerate() {
            let name = name_of(p);
            let cell: gtk::Widget = if core::is_image(&name) && !viewer::is_gif(&name) {
                let overlay = gtk::Overlay::new();
                let pic = gtk::Picture::builder()
                    .can_shrink(true)
                    .content_fit(gtk::ContentFit::Cover)
                    .build();
                pic.add_css_class("photo");
                viewer::thumbnail(&pic, Rc::default(), p.clone(), 2 * 104, thumbs.clone());
                overlay.set_child(Some(&pic));
                let edit = gtk::Button::with_label("Edit");
                edit.add_css_class("osd");
                edit.add_css_class("pill");
                edit.set_halign(gtk::Align::End);
                edit.set_valign(gtk::Align::End);
                edit.set_margin_end(4);
                edit.set_margin_bottom(4);
                edit.set_tooltip_text(Some("Draw or add text"));
                let (weak, dd, paths, caption_entry, sensitive_check, editing) = (
                    self.weak_self.clone(),
                    d.clone(),
                    paths.clone(),
                    caption_entry.clone(),
                    sensitive_check.clone(),
                    editing.clone(),
                );
                edit.connect_clicked(move |_| {
                    let (Some(t), Some(app)) =
                        (weak.upgrade(), weak.upgrade().and_then(|t| t.app.upgrade()))
                    else {
                        return;
                    };
                    let caption = caption_entry.text().trim().to_owned();
                    let sensitive = sensitive_check.is_active();
                    editing.set(true);
                    dd.close();
                    let (weak, paths, persona) = (t.weak_self.clone(), paths.clone(), t.persona());
                    let core = app.core.clone();
                    let source = paths[i].clone();
                    editor::open(
                        &app,
                        persona.clone(),
                        source,
                        name_of(&paths[i]),
                        move |r| {
                            let Some(t) = weak.upgrade() else { return };
                            let mut paths = paths.clone();
                            if let Some(draft) = r {
                                // An earlier edit of this photo is replaced.
                                if core.is_edit_draft(persona.as_deref(), &paths[i]) {
                                    let _ = std::fs::remove_file(&paths[i]);
                                }
                                paths[i] = draft;
                            }
                            t.confirm_files(paths, caption.clone(), sensitive);
                        },
                    );
                });
                overlay.add_overlay(&edit);
                overlay.upcast()
            } else if viewer::is_gif(&name) && viewer::checked_size(p).is_ok() {
                let pic = gtk::Picture::for_filename(p);
                pic.set_can_shrink(true);
                pic.set_content_fit(gtk::ContentFit::Cover);
                pic.add_css_class("photo");
                ui::animate(&pic, p);
                pic.upcast()
            } else {
                let file = gtk::Box::new(gtk::Orientation::Vertical, 4);
                file.set_valign(gtk::Align::Center);
                let icon = gtk::Image::from_icon_name("text-x-generic-symbolic");
                icon.set_pixel_size(32);
                file.append(&icon);
                let l = gtk::Label::builder()
                    .label(&name)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .max_width_chars(12)
                    .build();
                l.add_css_class("caption");
                file.append(&l);
                file.upcast()
            };
            cell.set_size_request(104, 104);
            cell.set_tooltip_text(Some(&name));
            cell.add_css_class("send-thumb");
            strip.append(&cell);
        }
        let strip_scroll = gtk::ScrolledWindow::builder()
            .child(&strip)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .min_content_height(112)
            .propagate_natural_width(true)
            .build();
        b.append(&strip_scroll);
        b.append(&caption_entry);
        b.append(&sensitive_check);
        if images > 0 && app.core.settings().flag(settings::STRIP) {
            b.append(&ui::label(
                "Location and camera details are removed before sending.",
                &["caption", "dim-label"],
            ));
        }
        d.set_extra_child(Some(&b));
        let weak = self.weak_self.clone();
        d.connect_response(None, move |_, r| {
            let Some(t) = weak.upgrade() else { return };
            if r == "send" {
                t.send_files(
                    paths.clone(),
                    caption_entry.text().trim().to_owned(),
                    sensitive_check.is_active(),
                );
            } else if !editing.get()
                && let Some(app) = t.app.upgrade()
            {
                // Cancelled: edited copies go.
                let persona = t.persona();
                for p in paths
                    .iter()
                    .filter(|p| app.core.is_edit_draft(persona.as_deref(), p))
                {
                    let _ = std::fs::remove_file(p);
                }
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
        let (Some(node), Some(app)) = (self.node(), self.app.upgrade()) else {
            return;
        };
        let dest = self.dest();
        let strip = app.core.settings().flag(settings::STRIP);
        let (core, persona) = (app.core.clone(), self.persona());
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
                    let mut converted = false;
                    let data = match std::fs::read(p) {
                        Ok(d) if !strip => d,
                        Ok(d) => match prepare(p, name.clone(), d) {
                            Ok((n, d, c)) => {
                                name = n;
                                converted = c;
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
                        forwarded: false,
                    };
                    // A converted photo can never be made again from the
                    // original: keep what was sent, privately, so our
                    // other devices can have it too. An edited photo is
                    // otherwise kept with the chat's photos once sent;
                    // until then it stays a draft, which the sweep can't
                    // take.
                    let draft = core.is_edit_draft(persona.as_deref(), p);
                    let kept = if converted {
                        core.keep_private(persona.as_deref(), &name, &data)
                    } else {
                        None
                    };
                    let adopt_to = if draft && kept.is_none() {
                        match core.edit_destination(persona.as_deref(), p) {
                            Ok(d) => Some(d),
                            Err(e) => {
                                failed.push(format!("{name}: {e}"));
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let location = kept
                        .or_else(|| adopt_to.as_ref().map(|d| d.display().to_string()))
                        .unwrap_or_else(|| p.display().to_string());
                    match send_file_to(&node, &dest, name.clone(), data, Some(location), options) {
                        Err(e) => {
                            if let Some(d) = &adopt_to {
                                let _ = std::fs::remove_file(d);
                            }
                            failed.push(format!("{name}: {e}"));
                        }
                        Ok(()) if draft => match &adopt_to {
                            Some(d) => {
                                if let Err(e) = core::adopt_edit(p, d) {
                                    core.say(format!("! keeping an edited photo: {e}"));
                                }
                            }
                            // What was sent is kept: the draft goes.
                            None => {
                                let _ = std::fs::remove_file(p);
                            }
                        },
                        Ok(()) => {}
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

    /// When this chat notifies: always, only for mentions and replies
    /// (groups), or never.
    fn notifications(self: &Rc<Self>) {
        use threnody_ffi::NotifyLevel;
        let Some(node) = self.node() else { return };
        let (group, device) = (self.group(), self.device());
        let current = match &group {
            Some(g) => node.group_notify_level(g.clone()),
            None => node.notify_level(device.clone()),
        }
        .unwrap_or(NotifyLevel::All);
        let levels: Vec<(NotifyLevel, &str)> = if group.is_some() {
            vec![
                (NotifyLevel::All, "All messages"),
                (NotifyLevel::Mentions, "Only mentions and replies to me"),
                (NotifyLevel::Off, "Off"),
            ]
        } else {
            vec![
                (NotifyLevel::All, "All messages"),
                (NotifyLevel::Off, "Off"),
            ]
        };
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let options: Vec<String> = levels.iter().map(|(_, l)| (*l).to_owned()).collect();
        let at = levels.iter().position(|(l, _)| *l == current);
        let weak = self.weak_self.clone();
        ui::choose(
            &app.window,
            "Notifications",
            "Messages still arrive and count as unread; this only decides what notifies.",
            &options,
            at,
            move |i| {
                let Some(t) = weak.upgrade() else { return };
                let level = levels[i].0;
                let (group, device) = (group.clone(), device.clone());
                t.run(
                    "change notifications",
                    move |n| match group {
                        Some(g) => n.set_group_notify_level(g, level),
                        None => n.set_notify_level(device, level),
                    },
                    None,
                );
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
        // The invitation shown: others may invite us to the same group.
        let from = self.conv.borrow().invite.as_ref().map(|i| i.from.clone());
        self.run(
            "join",
            move |n| match from {
                Some(from) => n.accept_group_invite_from(g, from),
                None => n.accept_group_invite(g),
            },
            Some("Joined".into()),
        );
    }

    fn decline_group(self: &Rc<Self>) {
        let Some(g) = self.group() else { return };
        // Only the invitation shown.
        let from = self.conv.borrow().invite.as_ref().map(|i| i.from.clone());
        self.run(
            "decline",
            move |n| match from {
                Some(from) => n.decline_group_invite_from(g, from),
                None => n.decline_group_invite(g),
            },
            None,
        );
        if let Some(a) = self.app.upgrade() {
            a.close_chat();
        }
    }

    /// The members, one row per account (as on Android), with what we
    /// may do to each.
    fn members(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(info) = self.conv.borrow().group.clone() else {
            return;
        };
        let Some(node) = self.node() else { return };
        let rows = match node.group_member_rows(info.id.clone()) {
            Ok(r) => r,
            Err(e) => return self.toast(&format!("Couldn't list members: {e}")),
        };
        let names: Vec<String> = rows
            .iter()
            .map(|r| {
                let mut n = if r.is_me {
                    "You".to_owned()
                } else {
                    Core::name_of(&node, &r.fingerprint)
                };
                match r.role {
                    MemberRole::Owner => n.push_str(" (owner)"),
                    MemberRole::Admin => n.push_str(" (admin)"),
                    MemberRole::Member => {}
                }
                if r.successor {
                    n.push_str(" (successor)");
                }
                n
            })
            .collect();
        let body = match info.my_role {
            MemberRole::Member => "Owners and admins can add and remove members.",
            _ => {
                "Click a member for what you can do. Owners and admins can add and remove members."
            }
        };
        let weak = self.weak_self.clone();
        let shown = names.clone();
        ui::choose(&app.window, &info.name, body, &shown, None, move |i| {
            let Some(t) = weak.upgrade() else { return };
            if !rows[i].actions.is_empty() {
                t.member_actions(rows[i].clone(), names[i].clone());
            }
        });
    }

    fn member_actions(self: &Rc<Self>, row: GroupMemberRow, label: String) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(info) = self.conv.borrow().group.clone() else {
            return;
        };
        let labels: Vec<String> = row
            .actions
            .iter()
            .map(|a| action_label(*a).to_owned())
            .collect();
        let weak = self.weak_self.clone();
        ui::choose(&app.window, &label, "", &labels, None, move |i| {
            let Some(t) = weak.upgrade() else { return };
            let Some(app) = t.app.upgrade() else { return };
            let a = row.actions[i];
            let name = if row.is_me {
                "You".to_owned()
            } else {
                t.node().map_or_else(
                    || core::short(&row.fingerprint),
                    |n| Core::name_of(&n, &row.fingerprint),
                )
            };
            let (weak, fp, gname) = (
                Rc::downgrade(&t),
                row.fingerprint.clone(),
                info.name.clone(),
            );
            ui::confirm(
                &app.window,
                &format!("{}?", action_label(a)),
                &action_confirm(a, &name, &gname),
                action_label(a),
                matches!(
                    a,
                    GroupMemberAction::Remove
                        | GroupMemberAction::MakeOwner
                        | GroupMemberAction::RemoveThisDevice
                ),
                move || {
                    let Some(t) = weak.upgrade() else { return };
                    let Some(g) = t.group() else { return };
                    let fp = fp.clone();
                    t.run(
                        &action_label(a).to_lowercase(),
                        move |n| match a {
                            GroupMemberAction::MakeAdmin => n.promote_in_group(g, fp),
                            GroupMemberAction::RemoveAdmin | GroupMemberAction::StepDown => {
                                n.demote_in_group(g, fp)
                            }
                            GroupMemberAction::MakeSuccessor => n.set_group_successor(g, Some(fp)),
                            GroupMemberAction::NoLongerSuccessor => n.set_group_successor(g, None),
                            GroupMemberAction::MakeOwner => n.transfer_group_ownership(g, fp),
                            GroupMemberAction::UseAsCoordinator => {
                                let me = n.device_fingerprint();
                                n.use_as_coordinator(g, me)
                            }
                            GroupMemberAction::Remove => n.remove_from_group(g, fp),
                            GroupMemberAction::RemoveThisDevice => n.remove_lost_coordinator(g),
                        },
                        None,
                    );
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
        // The last owner account, with others in the group: hand it over
        // first.
        let rows = self
            .node()
            .and_then(|n| n.group_member_rows(info.id.clone()).ok())
            .unwrap_or_default();
        let last_owner = info.owned
            && !rows.iter().any(|r| !r.is_me && r.role == MemberRole::Owner)
            && rows.iter().any(|r| !r.is_me);
        if last_owner {
            return self.choose_new_owner(&info, rows);
        }
        let weak = self.weak_self.clone();
        ui::confirm(
            &app.window,
            &format!("Leave {}?", info.name),
            "You stop receiving its messages. Its history stays on your devices; it can invite you again.",
            "Leave",
            true,
            move || {
                let Some(t) = weak.upgrade() else { return };
                let Some(g) = t.group() else { return };
                t.run("leave", move |n| n.leave_group(g), None);
            },
        );
    }

    /// The last owner leaving: someone else owns the group first.
    fn choose_new_owner(self: &Rc<Self>, info: &GroupInfo, rows: Vec<GroupMemberRow>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(node) = self.node() else { return };
        // Admins first.
        let mut rows: Vec<GroupMemberRow> = rows.into_iter().filter(|r| !r.is_me).collect();
        rows.sort_by_key(|r| r.role != MemberRole::Admin);
        let names: Vec<String> = rows
            .iter()
            .map(|r| {
                let mut n = Core::name_of(&node, &r.fingerprint);
                if r.role == MemberRole::Admin {
                    n.push_str(" (admin)");
                }
                n
            })
            .collect();
        let (weak, gname) = (self.weak_self.clone(), info.name.clone());
        ui::choose(
            &app.window,
            "Choose a new owner first",
            &format!("{gname} goes on without you. To end it for everyone, delete it instead."),
            &names.clone(),
            None,
            move |i| {
                let Some(t) = weak.upgrade() else { return };
                let Some(app) = t.app.upgrade() else { return };
                let (weak, fp) = (Rc::downgrade(&t), rows[i].fingerprint.clone());
                ui::confirm(
                    &app.window,
                    &format!("Make {} the owner and leave?", names[i]),
                    "This can't be undone.",
                    "Leave",
                    true,
                    move || {
                        let Some(t) = weak.upgrade() else { return };
                        let Some(g) = t.group() else { return };
                        let fp = fp.clone();
                        t.run(
                            "hand over the group",
                            move |n| n.transfer_group_ownership_and_leave(g, fp),
                            None,
                        );
                    },
                );
            },
        );
    }

    fn delete_group(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(info) = self.conv.borrow().group.clone() else {
            return;
        };
        let weak = self.weak_self.clone();
        ui::confirm(
            &app.window,
            &format!("Delete {}?", info.name),
            "Everyone is removed and the group ends. Its history stays on your devices.",
            "Delete",
            true,
            move || {
                let Some(t) = weak.upgrade() else { return };
                let Some(g) = t.group() else { return };
                t.run("delete the group", move |n| n.delete_group(g), None);
            },
        );
    }

    /// The conversation's photos, files and links, newest first.
    /// *Media, files and links*: the chat's photos, files and links, the
    /// newest first, from its last [`MEDIA_HISTORY`] messages.
    fn media(self: &Rc<Self>) {
        let Some(node) = self.node() else { return };
        let (group, device, note) = (self.group(), self.device(), self.is_note());
        let weak = self.weak_self.clone();
        bg(
            move || match &group {
                _ if note => node.note_history(MEDIA_HISTORY).unwrap_or_default(),
                Some(g) => node
                    .group_history(g.clone(), MEDIA_HISTORY)
                    .unwrap_or_default(),
                None => node.history(device, MEDIA_HISTORY).unwrap_or_default(),
            },
            move |entries| {
                if let Some(t) = weak.upgrade() {
                    t.show_media(Rc::new(entries));
                }
            },
        );
    }

    fn show_media(self: &Rc<Self>, entries: Rc<Vec<HistoryEntry>>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let lists = media_lists(&entries);
        let d = adw::Dialog::builder()
            .title("Media, files and links")
            .content_width(600)
            .content_height(640)
            .build();
        let stack = adw::ViewStack::new();
        let empty = |what: &str| -> gtk::Widget {
            adw::StatusPage::builder()
                .description(format!(
                    "No {what} in this conversation (disappearing messages take theirs with them)."
                ))
                .build()
                .upcast()
        };

        let photos: gtk::Widget = if lists.photos.is_empty() {
            empty("photos")
        } else {
            self.photo_grid(&entries, &lists.photos).upcast()
        };
        stack
            .add_titled(
                &photos,
                Some("photos"),
                &format!("Photos · {}", lists.photos.len()),
            )
            .set_icon_name(Some("image-x-generic-symbolic"));

        let files: gtk::Widget = if lists.files.is_empty() {
            empty("files")
        } else {
            let page = adw::PreferencesPage::new();
            let group = adw::PreferencesGroup::new();
            for &i in &lists.files {
                let (e, Some(f)) = (&entries[i], &entries[i].file) else {
                    continue;
                };
                let title = match f.clip {
                    Some(c) => core::file_label(&f.name, f.sensitive, "", Some(c)),
                    None if f.sensitive => format!("📎 Sensitive file: {}", f.name),
                    None => format!("📎 {}", f.name),
                };
                let mut sub = format!("{} · {}", ui::human_size(f.size), ui::time_label(e.at_ms));
                if !e.text.is_empty() {
                    sub.push_str(&format!(" · {}", e.text));
                }
                let row = ui::link_button_row(&title, &sub);
                match f
                    .location
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.exists())
                {
                    Some(p) => {
                        let weak = self.weak_self.clone();
                        row.connect_activated(move |_| {
                            if let Some(t) = weak.upgrade() {
                                t.open_file(&p);
                            }
                        });
                    }
                    None => row.set_activatable(false),
                }
                group.add(&row);
            }
            page.add(&group);
            page.upcast()
        };
        stack
            .add_titled(
                &files,
                Some("files"),
                &format!("Files · {}", lists.files.len()),
            )
            .set_icon_name(Some("text-x-generic-symbolic"));

        let links: gtk::Widget = if lists.links.is_empty() {
            empty("links")
        } else {
            let page = adw::PreferencesPage::new();
            let group = adw::PreferencesGroup::new();
            for (url, i) in &lists.links {
                let row = ui::link_button_row(url, &ui::time_label(entries[*i].at_ms));
                let (w, u) = (app.window.clone(), url.clone());
                row.connect_activated(move |_| ui::confirm_link(&w, &u));
                group.add(&row);
            }
            page.add(&group);
            page.upcast()
        };
        stack
            .add_titled(
                &links,
                Some("links"),
                &format!("Links · {}", lists.links.len()),
            )
            .set_icon_name(Some("web-browser-symbolic"));

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &adw::ViewSwitcher::builder()
                .stack(&stack)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build(),
        ));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&stack));
        d.set_child(Some(&view));
        d.present(Some(&app.window));
    }

    /// The photos of `entries` (`photos`: their indices) as a grid that
    /// decodes only the cells on screen, off the main thread. Sensitive
    /// photos are covered and never decoded here; ones not on this
    /// computer can't be opened.
    fn photo_grid(
        self: &Rc<Self>,
        entries: &Rc<Vec<HistoryEntry>>,
        photos: &[usize],
    ) -> gtk::ScrolledWindow {
        /// One cell's widgets, and the photo it shows now.
        struct Cell {
            pic: gtk::Picture,
            cover: gtk::Label,
            bound: Rc<RefCell<Option<std::path::PathBuf>>>,
        }
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for &i in photos {
            store.append(&glib::BoxedAnyObject::new(i));
        }
        let cells: Rc<RefCell<HashMap<gtk::Overlay, Cell>>> = Rc::default();
        let cache = Thumbs::default();
        let factory = gtk::SignalListItemFactory::new();
        let c = cells.clone();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let overlay = gtk::Overlay::new();
            overlay.set_size_request(120, 120);
            overlay.add_css_class("album-cell");
            let pic = gtk::Picture::builder()
                .can_shrink(true)
                .content_fit(gtk::ContentFit::Cover)
                .build();
            let cover = gtk::Label::new(None);
            cover.add_css_class("photo-cover");
            cover.set_hexpand(true);
            cover.set_vexpand(true);
            overlay.set_child(Some(&pic));
            overlay.add_overlay(&cover);
            item.set_child(Some(&overlay));
            c.borrow_mut().insert(
                overlay,
                Cell {
                    pic,
                    cover,
                    bound: Rc::default(),
                },
            );
        });
        let (c, e) = (cells.clone(), entries.clone());
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let (Some(overlay), Some(obj)) = (
                item.child().and_downcast::<gtk::Overlay>(),
                item.item().and_downcast::<glib::BoxedAnyObject>(),
            ) else {
                return;
            };
            let cells = c.borrow();
            let Some(cell) = cells.get(&overlay) else {
                return;
            };
            let Some(f) = e.get(*obj.borrow::<usize>()).and_then(|e| e.file.as_ref()) else {
                return;
            };
            let path = f
                .location
                .as_ref()
                .map(std::path::PathBuf::from)
                .filter(|p| p.exists());
            let cover = match (&path, f.sensitive) {
                (Some(_), true) => Some(("🔒", "Sensitive photo")),
                (None, _) => Some(("📷", "Photo not on this computer")),
                (Some(_), false) => None,
            };
            match (cover, path) {
                (Some((icon, what)), _) => {
                    *cell.bound.borrow_mut() = None;
                    cell.pic.set_paintable(gdk::Paintable::NONE);
                    cell.cover.set_text(icon);
                    cell.cover.set_tooltip_text(Some(what));
                    cell.cover.set_visible(true);
                }
                (None, Some(p)) => {
                    cell.cover.set_visible(false);
                    cell.cover.set_tooltip_text(None);
                    viewer::thumbnail(&cell.pic, cell.bound.clone(), p, 240, cache.clone());
                }
                (None, None) => {}
            }
        });
        let c = cells.clone();
        factory.connect_unbind(move |_, item| {
            let Some(overlay) = item
                .downcast_ref::<gtk::ListItem>()
                .and_then(|i| i.child().and_downcast::<gtk::Overlay>())
            else {
                return;
            };
            if let Some(cell) = c.borrow().get(&overlay) {
                *cell.bound.borrow_mut() = None;
                cell.pic.set_paintable(gdk::Paintable::NONE);
            }
        });
        let grid = gtk::GridView::builder()
            .model(&gtk::NoSelection::new(Some(store)))
            .factory(&factory)
            .min_columns(3)
            .max_columns(5)
            .single_click_activate(true)
            .build();
        let (weak, e, order) = (self.weak_self.clone(), entries.clone(), photos.to_vec());
        grid.connect_activate(move |_, pos| {
            let Some(t) = weak.upgrade() else { return };
            let Some(entry) = order.get(pos as usize).and_then(|&i| e.get(i)) else {
                return;
            };
            let Some(f) = &entry.file else { return };
            if let Some(p) = f
                .location
                .as_ref()
                .map(std::path::PathBuf::from)
                .filter(|p| p.exists())
            {
                // Sensitive ones too: uncovered only in the viewer.
                t.view_photo(entry, f, &p);
            }
        });
        gtk::ScrolledWindow::builder()
            .child(&grid)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build()
    }
}

/// A chat's photos, files and links, as indices into its entries, the
/// newest first. Voice and video messages count as files; a photo counts
/// whether or not it is on this computer.
#[derive(Debug, Default, PartialEq, Eq)]
struct MediaLists {
    photos: Vec<usize>,
    files: Vec<usize>,
    links: Vec<(String, usize)>,
}

fn media_lists(entries: &[HistoryEntry]) -> MediaLists {
    let mut out = MediaLists::default();
    for (i, e) in entries.iter().enumerate().rev() {
        if let Some(f) = &e.file {
            if core::is_image(&f.name) && f.clip.is_none() {
                out.photos.push(i);
            } else {
                out.files.push(i);
            }
        }
        out.links.extend(urls(&e.text).into_iter().map(|u| (u, i)));
    }
    out
}

/// How a chat's entries are drawn: a run of two or more photos of one
/// album, consecutive and from one sender in one direction, is one
/// bubble; anything else is a run of one.
fn album_runs(entries: &[HistoryEntry]) -> Vec<std::ops::Range<usize>> {
    fn album_photo(e: &HistoryEntry) -> Option<(u64, &str, bool)> {
        e.file
            .as_ref()
            .filter(|f| f.album != 0 && f.clip.is_none() && core::is_image(&f.name))
            .map(|f| (f.album, e.device.as_str(), e.outgoing))
    }
    let mut runs = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let mut end = i + 1;
        if let Some(key) = album_photo(&entries[i]) {
            while end < entries.len() && album_photo(&entries[end]) == Some(key) {
                end += 1;
            }
        }
        runs.push(i..end);
        i = end;
    }
    runs
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

/// Image formats the node can't strip metadata from (it does JPEG, PNG
/// and WebP; GIFs carry none worth the name): they're sent as JPEG.
const CONVERT: [&str; 6] = ["heic", "heif", "avif", "bmp", "tif", "tiff"];

/// An image the node can't strip, re-encoded as JPEG (which then carries
/// no metadata): the name and bytes to send, and whether it was converted.
/// Others pass through. Its content decides as well as its name, so a HEIC
/// called "photo.jpg" is converted too (and keeps its name).
fn prepare(
    path: &std::path::Path,
    name: String,
    data: Vec<u8>,
) -> Result<(String, Vec<u8>, bool), String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !CONVERT.contains(&ext.as_str()) && !threnody_core::media::needs_converting(&data) {
        return Ok((name, data, false));
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
    Ok((format!("{stem}.jpg"), jpeg, true))
}

/// What a file whose contents another of our devices may have says.
fn fetch_text(size: u64, s: FileFetch) -> String {
    let state = match s {
        FileFetch::NotHere => "not on this device".into(),
        FileFetch::Idle => "on your other device".into(),
        FileFetch::Waiting => "waiting for your other device…".into(),
        FileFetch::Fetching { received, total } => format!(
            "downloading… {}%",
            (received * 100).checked_div(total).unwrap_or(100)
        ),
        FileFetch::Unavailable => "not on your other devices".into(),
    };
    format!("{} · {state}", ui::human_size(size))
}

/// Where a chat's messages go.
#[derive(Clone)]
enum Dest {
    Group(String),
    Contact(String),
    Note,
}

/// Sends a file to `dest` (into the notes to self, it stays where it is).
fn send_file_to(
    node: &Node,
    dest: &Dest,
    name: String,
    data: Vec<u8>,
    location: Option<String>,
    options: FileOptions,
) -> Result<(), threnody_ffi::ThrenodyError> {
    match dest {
        Dest::Group(g) => node.send_group_file(g.clone(), name, data, location, options),
        Dest::Contact(d) => node.send_file(d.clone(), name, data, location, options),
        Dest::Note => node.add_note_file(name, data.len() as u64, location, options),
    }
}

/// Whether `q` quotes `e`: by message id, or for a message too old to
/// have one, by sender and text (as the node matches them).
/// A call on one scheduled message: false means it's being sent right
/// now (or already went), so nothing changed.
fn being_sent(done: bool) -> Result<(), threnody_ffi::ThrenodyError> {
    if done {
        Ok(())
    } else {
        Err(threnody_ffi::ThrenodyError::Failed {
            reason: "it's being sent now".into(),
        })
    }
}

fn quotes(q: &threnody_ffi::QuoteInfo, e: &HistoryEntry) -> bool {
    if q.id != 0 {
        return e.id == q.id;
    }
    let snippet = if e.text.is_empty() {
        e.file.as_ref().map_or("", |f| f.name.as_str())
    } else {
        e.text.as_str()
    };
    e.device == q.author && !q.text.is_empty() && snippet.starts_with(&q.text)
}

fn action_label(a: GroupMemberAction) -> &'static str {
    match a {
        GroupMemberAction::MakeAdmin => "Make admin",
        GroupMemberAction::RemoveAdmin => "Remove as admin",
        GroupMemberAction::MakeSuccessor => "Make successor",
        GroupMemberAction::NoLongerSuccessor => "No longer successor",
        GroupMemberAction::MakeOwner => "Make owner",
        GroupMemberAction::UseAsCoordinator => "Apply changes from this device",
        GroupMemberAction::Remove => "Remove from group",
        GroupMemberAction::RemoveThisDevice => "Remove the lost coordinating device",
        GroupMemberAction::StepDown => "Step down as admin",
    }
}

/// What a confirm says before each member action (the same as Android's).
fn action_confirm(a: GroupMemberAction, name: &str, group: &str) -> String {
    match a {
        GroupMemberAction::MakeAdmin => format!(
            "{name} can invite and remove members and pin messages. Members will see which devices are yours and {name}'s."
        ),
        GroupMemberAction::RemoveAdmin => format!("{name} stays in {group} as a member."),
        GroupMemberAction::MakeSuccessor => {
            format!("If you're not seen in {group} for 14 days, {name} may take it over.")
        }
        GroupMemberAction::NoLongerSuccessor => {
            format!("Nobody will be able to take {group} over if you disappear.")
        }
        GroupMemberAction::MakeOwner => {
            format!("{name} owns {group} from now on. You become an admin and can't undo this.")
        }
        GroupMemberAction::UseAsCoordinator => {
            format!("Changes to {group} are applied from this device from now on.")
        }
        GroupMemberAction::Remove => {
            format!("They stop receiving new messages in {group}. They can be invited again later.")
        }
        GroupMemberAction::RemoveThisDevice => format!(
            "This device takes over applying changes and removes your lost device from {group}."
        ),
        GroupMemberAction::StepDown => format!("You stay in {group} as a member."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A history entry; `file`: (name, album, clip).
    fn entry(
        at_ms: u64,
        device: &str,
        file: Option<(&str, u64, bool)>,
        text: &str,
    ) -> HistoryEntry {
        HistoryEntry {
            at_ms,
            outgoing: false,
            device: device.into(),
            text: text.into(),
            disappearing: false,
            file: file.map(|(name, album, clip)| threnody_ffi::FileInfo {
                name: name.into(),
                size: 1,
                location: None,
                sensitive: false,
                album,
                clip: clip.then_some(Clip {
                    video: false,
                    duration_ms: 1000,
                }),
                fetch: FileFetch::NotHere,
            }),
            delivered: false,
            read: false,
            id: at_ms,
            edited: false,
            recipients: 0,
            delivered_to: 0,
            reactions: Vec::new(),
            quote: None,
            mentions: Vec::new(),
            poll: None,
            forwarded: false,
            list: None,
            tick: threnody_ffi::TickMark::None,
        }
    }

    #[test]
    fn albums_group_consecutive_photos_from_one_sender() {
        let entries = [
            entry(1, "d1", None, "hi"),
            entry(2, "d1", Some(("a.jpg", 1, false)), ""),
            entry(3, "d1", Some(("b.png", 1, false)), ""),
            entry(4, "d2", Some(("c.jpg", 1, false)), ""),
            entry(5, "d2", Some(("d.jpg", 0, false)), ""),
            entry(6, "d2", Some(("e.jpg", 2, false)), ""),
            entry(7, "d2", Some(("f.webp", 2, false)), ""),
        ];
        assert_eq!(album_runs(&entries), vec![0..1, 1..3, 3..4, 4..5, 5..7]);
    }

    #[test]
    fn mixed_albums_stay_apart() {
        // A photo and a PDF of one album: separate bubbles.
        let mixed = [
            entry(1, "d", Some(("a.jpg", 9, false)), ""),
            entry(2, "d", Some(("b.pdf", 9, false)), ""),
        ];
        assert_eq!(album_runs(&mixed), vec![0..1, 1..2]);
        // A voice message in an album.
        let clip = [
            entry(1, "d", Some(("a.jpg", 9, false)), ""),
            entry(2, "d", Some(("voice.ogg", 9, true)), ""),
        ];
        assert_eq!(album_runs(&clip), vec![0..1, 1..2]);
        // One device, one album, but one sent and one received.
        let mut both = [
            entry(1, "d", Some(("a.jpg", 9, false)), ""),
            entry(2, "d", Some(("b.jpg", 9, false)), ""),
        ];
        both[0].outgoing = true;
        assert_eq!(album_runs(&both), vec![0..1, 1..2]);
    }

    #[test]
    fn a_lone_album_photo_is_a_run_of_one() {
        let one = [entry(1, "d", Some(("a.jpg", 5, false)), "caption")];
        assert_eq!(album_runs(&one), vec![0..1]);
        assert!(album_runs(&[]).is_empty());
    }

    #[test]
    fn media_lists_are_newest_first() {
        let mut on_device = entry(1, "d", Some(("a.jpg", 0, false)), "");
        if let Some(f) = on_device.file.as_mut() {
            f.location = Some("/tmp/a.jpg".into());
        }
        let entries = [
            on_device,
            entry(2, "d", None, "see https://example.org/a and http://x.test."),
            entry(3, "d", Some(("report.pdf", 0, false)), ""),
            entry(4, "d", Some(("voice.ogg", 0, true)), ""),
            entry(
                5,
                "d",
                Some(("b.png", 0, false)),
                "caption https://example.org/b",
            ),
        ];
        let l = media_lists(&entries);
        // b.png isn't on this computer: a photo all the same.
        assert_eq!(l.photos, vec![4, 0]);
        assert_eq!(l.files, vec![3, 2]);
        assert_eq!(
            l.links,
            vec![
                ("https://example.org/b".to_owned(), 4),
                ("https://example.org/a".to_owned(), 1),
                ("http://x.test".to_owned(), 1),
            ]
        );
    }

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
        let (name, out, converted) = prepare(&tiff, "scan.tiff".into(), data.clone()).unwrap();
        assert_eq!(name, "scan.jpg");
        assert!(converted);
        assert_eq!(&out[..2], &[0xff, 0xd8], "a JPEG");

        // Whatever it is called.
        let misnamed = dir.join("misnamed.jpg");
        std::fs::copy(&tiff, &misnamed).unwrap();
        let (name, out, converted) = prepare(&misnamed, "misnamed.jpg".into(), data).unwrap();
        assert_eq!(name, "misnamed.jpg");
        assert!(converted, "though its name stayed");
        assert_eq!(&out[..2], &[0xff, 0xd8], "a JPEG");

        // Formats the node strips itself pass through untouched.
        let png = dir.join("a.png");
        let (name, out, converted) = prepare(&png, "a.png".into(), b"as is".to_vec()).unwrap();
        assert!(name == "a.png" && out == b"as is" && !converted);

        // One it can't read isn't sent with its metadata.
        let heic = dir.join("broken.heic");
        std::fs::write(&heic, b"not a picture").unwrap();
        assert!(prepare(&heic, "broken.heic".into(), b"not a picture".to_vec()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
