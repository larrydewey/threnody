//! The main window: the conversation list beside the open chat, and the
//! app-wide dialogs (invites, groups, devices, profile, anonymous
//! identities, preferences, diagnostics).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use threnody_ffi::{NodeEvent, ProfileAttr};

use crate::chat::ChatView;
use crate::core::{self, ChatRef, Conversation, Core, Node, Target, UiEvent};
use crate::settings;
use crate::ui::{self, bg};

pub struct App {
    pub app: adw::Application,
    pub window: adw::ApplicationWindow,
    pub core: Arc<Core>,
    split: adw::NavigationSplitView,
    toasts: adw::ToastOverlay,
    list: gtk::ListBox,
    list_stack: gtk::Stack,
    content: adw::NavigationPage,
    convs: RefCell<Vec<Conversation>>,
    unread: RefCell<HashMap<ChatRef, u32>>,
    chat: RefCell<Option<Rc<ChatView>>>,
    refresh_pending: Cell<bool>,
    /// Ignore selection changes while the list is rebuilt.
    rebuilding: Cell<bool>,
    log: RefCell<Option<gtk::TextBuffer>>,
}

impl App {
    pub fn new(
        app: &adw::Application,
        window: &adw::ApplicationWindow,
        core: Arc<Core>,
        events: async_channel::Receiver<UiEvent>,
    ) -> Rc<Self> {
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);

        let empty = adw::StatusPage::builder()
            .icon_name("system-users-symbolic")
            .title("No contacts yet")
            .description(
                "Show your invite to someone, or paste theirs. \
                 Their link opens here when you click it.",
            )
            .build();
        let empty_buttons = gtk::Box::new(gtk::Orientation::Vertical, 8);
        empty_buttons.set_halign(gtk::Align::Center);
        let show = gtk::Button::with_label("Show my invite");
        show.add_css_class("pill");
        show.add_css_class("suggested-action");
        show.set_action_name(Some("win.invite"));
        let add = gtk::Button::with_label("Add a contact");
        add.add_css_class("pill");
        add.set_action_name(Some("win.add-contact"));
        empty_buttons.append(&show);
        empty_buttons.append(&add);
        empty.set_child(Some(&empty_buttons));

        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let list_stack = gtk::Stack::new();
        list_stack.add_named(&scroller, Some("list"));
        list_stack.add_named(&empty, Some("empty"));

        let header = adw::HeaderBar::new();
        let invite = gtk::Button::from_icon_name("threnody-qr-symbolic");
        invite.set_tooltip_text(Some("My invite"));
        invite.set_action_name(Some("win.invite"));
        header.pack_start(&invite);

        let add_menu = gio::Menu::new();
        add_menu.append(Some("Add a contact"), Some("win.add-contact"));
        add_menu.append(Some("New group"), Some("win.new-group"));
        add_menu.append(Some("Anonymous invite"), Some("win.anonymous-invite"));
        let add_button = gtk::MenuButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New conversation")
            .menu_model(&add_menu)
            .build();

        let main_menu = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Your profile"), Some("win.profile"));
        s1.append(Some("Anonymous identities"), Some("win.personas"));
        s1.append(Some("Credentials"), Some("win.credentials"));
        s1.append(Some("Devices"), Some("win.devices"));
        main_menu.append_section(None, &s1);
        let s2 = gio::Menu::new();
        s2.append(Some("Link a new device"), Some("win.link-device"));
        s2.append(
            Some("Join another device's account"),
            Some("win.join-account"),
        );
        main_menu.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("Preferences"), Some("win.preferences"));
        s3.append(Some("Diagnostics"), Some("win.diagnostics"));
        s3.append(Some("About Threnody"), Some("win.about"));
        s3.append(Some("Quit"), Some("app.quit"));
        main_menu.append_section(None, &s3);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main menu")
            .menu_model(&main_menu)
            .primary(true)
            .build();
        header.pack_end(&menu_button);
        header.pack_end(&add_button);

        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.add_top_bar(&header);
        sidebar_view.set_content(Some(&list_stack));
        let sidebar = adw::NavigationPage::builder()
            .title("Threnody")
            .child(&sidebar_view)
            .build();

        let content = adw::NavigationPage::builder()
            .title("Threnody")
            .child(&placeholder())
            .build();

        let split = adw::NavigationSplitView::new();
        split.set_sidebar(Some(&sidebar));
        split.set_content(Some(&content));
        split.set_min_sidebar_width(280.0);
        split.set_max_sidebar_width(380.0);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));

        // Narrow windows show one pane at a time.
        let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            640.0,
            adw::LengthUnit::Sp,
        ));
        bp.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(bp);

        let this = Rc::new(Self {
            app: app.clone(),
            window: window.clone(),
            core,
            split,
            toasts,
            list,
            list_stack,
            content,
            convs: RefCell::new(Vec::new()),
            unread: RefCell::new(HashMap::new()),
            chat: RefCell::new(None),
            refresh_pending: Cell::new(false),
            rebuilding: Cell::new(false),
            log: RefCell::new(None),
        });
        this.actions();
        this.watch(events);
        this.watch_window();
        this.watch_network();

        let weak = Rc::downgrade(&this);
        this.list.connect_row_selected(move |_, row| {
            let (Some(this), Some(row)) = (weak.upgrade(), row) else {
                return;
            };
            if this.rebuilding.get() {
                return;
            }
            let i = usize::try_from(row.index()).unwrap_or(0);
            let conv = this.convs.borrow().get(i).cloned();
            if let Some(c) = conv {
                this.open_chat(c);
            }
        });
        this.refresh();
        this
    }

    pub fn toast(&self, text: &str) {
        let t = adw::Toast::new(text);
        t.set_timeout(4);
        self.toasts.add_toast(t);
    }

    // ----- The conversation list -----

    /// Reloads the list soon (events often come in bursts).
    pub fn refresh_soon(self: &Rc<Self>) {
        if self.refresh_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            if let Some(this) = weak.upgrade() {
                this.refresh_pending.set(false);
                this.refresh();
            }
        });
    }

    pub fn refresh(self: &Rc<Self>) {
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        bg(
            move || core.conversations(),
            move |convs| {
                if let Some(this) = weak.upgrade() {
                    this.show_conversations(convs);
                }
            },
        );
    }

    fn show_conversations(self: &Rc<Self>, convs: Vec<Conversation>) {
        self.rebuilding.set(true);
        let open = self.chat.borrow().as_ref().map(|c| c.chat());
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let unread = self.unread.borrow();
        for c in &convs {
            self.list
                .append(&self.row(c, unread.get(&c.chat).copied().unwrap_or(0)));
        }
        drop(unread);
        if let Some(open) = &open
            && let Some(i) = convs.iter().position(|c| c.chat == *open)
        {
            {
                self.list.select_row(
                    self.list
                        .row_at_index(i32::try_from(i).unwrap_or(0))
                        .as_ref(),
                );
                if let Some(chat) = self.chat.borrow().as_ref() {
                    chat.update(convs[i].clone());
                }
            }
        }
        self.list_stack
            .set_visible_child_name(if convs.is_empty() { "empty" } else { "list" });
        *self.convs.borrow_mut() = convs;
        self.rebuilding.set(false);
    }

    fn row(&self, c: &Conversation, unread: u32) -> gtk::ListBoxRow {
        let title = match &c.chat.persona {
            Some(p) => format!(
                "🎭 {} · {}",
                c.title,
                self.core.persona_label(p).unwrap_or_default()
            ),
            None => c.title.clone(),
        };
        let subtitle = if let Some(i) = &c.invite {
            format!("Group invitation from {}", core::short(&i.from))
        } else if c.is_request() {
            "Message request · click to review".into()
        } else if let Some(e) = &c.last {
            let who = if e.outgoing {
                "You: ".into()
            } else if c.group.is_some() {
                let node = self.core.node(c.chat.persona.as_deref()).ok();
                node.map(|n| format!("{}: ", Core::name_of(&n, &e.device)))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            format!("{who}{}", core::preview(e))
        } else if let Some(g) = &c.group {
            if g.members.len() == 1 {
                "1 member".into()
            } else {
                format!("{} members", g.members.len())
            }
        } else if c.approved && c.verified {
            "Approved · verified".into()
        } else if c.approved {
            "Approved".into()
        } else {
            "Not approved yet".into()
        };

        let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        b.set_margin_top(6);
        b.set_margin_bottom(6);
        let avatar = adw::Avatar::new(36, Some(&c.title), true);
        if c.group.is_some() || c.invite.is_some() {
            avatar.set_icon_name(Some("system-users-symbolic"));
            avatar.set_show_initials(false);
        }
        let avatar_overlay = gtk::Overlay::new();
        avatar_overlay.set_child(Some(&avatar));
        if c.connected {
            let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            dot.add_css_class("online-dot");
            dot.set_halign(gtk::Align::End);
            dot.set_valign(gtk::Align::End);
            dot.set_tooltip_text(Some("Connected"));
            avatar_overlay.add_overlay(&dot);
        }
        avatar_overlay.set_valign(gtk::Align::Center);
        b.append(&avatar_overlay);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let t = gtk::Label::builder()
            .label(&title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .build();
        t.add_css_class(if unread > 0 { "heading" } else { "body" });
        top.append(&t);
        if let Some(e) = &c.last {
            let time = gtk::Label::new(Some(&ui::list_time(e.at_ms)));
            time.add_css_class("dim-label");
            time.add_css_class("caption");
            top.append(&time);
        }
        text.append(&top);
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let s = gtk::Label::builder()
            .label(&subtitle)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .single_line_mode(true)
            .build();
        s.add_css_class("caption");
        if c.is_request() || c.invite.is_some() {
            s.add_css_class("accent");
        } else {
            s.add_css_class("dim-label");
        }
        bottom.append(&s);
        if unread > 0 {
            let badge = gtk::Label::new(Some(&unread.to_string()));
            badge.add_css_class("unread-badge");
            bottom.append(&badge);
        }
        text.append(&bottom);
        b.append(&text);

        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&b));

        // Right-click: clear or delete.
        let menu = gio::Menu::new();
        let target = chat_variant(&c.chat);
        menu.append_item(&{
            let i = gio::MenuItem::new(Some("Clear chat…"), None);
            i.set_action_and_target_value(Some("win.clear-chat"), Some(&target.to_variant()));
            i
        });
        if c.group.is_none() && c.invite.is_none() {
            menu.append_item(&{
                let i = gio::MenuItem::new(Some("Delete contact…"), None);
                i.set_action_and_target_value(
                    Some("win.delete-contact"),
                    Some(&target.to_variant()),
                );
                i
            });
        }
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let r = row.clone();
        click.connect_pressed(move |_, _, x, y| {
            // Made on demand and dropped on close: the row may be rebuilt.
            let pop = gtk::PopoverMenu::from_model(Some(&menu));
            pop.set_parent(&r);
            pop.set_has_arrow(false);
            #[allow(clippy::cast_possible_truncation)]
            pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            pop.connect_closed(|p| {
                let p = p.clone();
                glib::idle_add_local_once(move || p.unparent());
            });
            pop.popup();
        });
        row.add_controller(click);
        row
    }

    fn conversation(&self, chat: &ChatRef) -> Option<Conversation> {
        self.convs
            .borrow()
            .iter()
            .find(|c| c.chat == *chat)
            .cloned()
    }

    /// The conversation a device (or group) of `persona` belongs to.
    fn conversation_of(&self, persona: Option<&str>, peer_or_group: &str) -> Option<Conversation> {
        self.convs
            .borrow()
            .iter()
            .find(|c| {
                c.chat.persona.as_deref() == persona
                    && (c.devices.iter().any(|d| d == peer_or_group)
                        || matches!(&c.chat.target, Target::Group(g) | Target::Contact(g) if g == peer_or_group))
            })
            .cloned()
    }

    pub fn open_chat(self: &Rc<Self>, conv: Conversation) {
        let same = self
            .chat
            .borrow()
            .as_ref()
            .is_some_and(|c| c.chat() == conv.chat);
        if !same {
            let view = ChatView::new(self, conv.clone());
            self.content.set_child(Some(view.widget()));
            self.content.set_title(&conv.title);
            *self.chat.borrow_mut() = Some(view);
        }
        if self.unread.borrow_mut().remove(&conv.chat).is_some() {
            self.refresh_soon();
        }
        self.app.withdraw_notification(&notification_id(&conv.chat));
        if let Some(i) = self.convs.borrow().iter().position(|c| c.chat == conv.chat) {
            self.rebuilding.set(true);
            self.list.select_row(
                self.list
                    .row_at_index(i32::try_from(i).unwrap_or(0))
                    .as_ref(),
            );
            self.rebuilding.set(false);
        }
        self.split.set_show_content(true);
        self.update_watching();
    }

    /// Opens the chat with the contact (any device) or group, once the
    /// list knows it.
    pub fn open_when_listed(self: &Rc<Self>, persona: Option<String>, peer: String) {
        let weak = Rc::downgrade(self);
        let core = self.core.clone();
        bg(
            move || core.conversations(),
            move |convs| {
                let Some(this) = weak.upgrade() else { return };
                this.show_conversations(convs);
                if let Some(c) = this.conversation_of(persona.as_deref(), &peer) {
                    this.open_chat(c);
                }
            },
        );
    }

    /// The chat was closed (contact deleted, group left).
    pub fn close_chat(self: &Rc<Self>) {
        *self.chat.borrow_mut() = None;
        self.content.set_child(Some(&placeholder()));
        self.content.set_title("Threnody");
        self.split.set_show_content(false);
        self.update_watching();
        self.refresh();
    }

    fn update_watching(&self) {
        let open = self.chat.borrow().as_ref().map(|c| c.chat());
        let shown = self.window.is_active()
            && self.window.is_visible()
            && (!self.split.is_collapsed() || self.split.shows_content());
        *self
            .core
            .watching
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = open.filter(|_| shown);
    }

    // ----- Events -----

    fn watch(self: &Rc<Self>, events: async_channel::Receiver<UiEvent>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(e) = events.recv().await {
                let Some(this) = weak.upgrade() else { return };
                this.handle(e);
            }
        });
    }

    fn handle(self: &Rc<Self>, e: UiEvent) {
        let (persona, event) = match e {
            UiEvent::Log(line) => {
                if let Some(buf) = self.log.borrow().as_ref() {
                    buf.insert(&mut buf.end_iter(), &format!("{line}\n"));
                }
                return;
            }
            UiEvent::PersonasChanged => return self.refresh_soon(),
            UiEvent::Node { persona, event } => (persona, event),
        };
        // Which conversation it concerns, and whether it's news.
        let (about, news): (Option<String>, Option<(String, String)>) = match &event {
            NodeEvent::Message { peer, text, .. } => (
                Some(peer.clone()),
                Some((self.title_of(persona.as_deref(), peer), text.clone())),
            ),
            NodeEvent::File {
                peer,
                name,
                sensitive,
                caption,
                ..
            } => (
                Some(peer.clone()),
                Some((
                    self.title_of(persona.as_deref(), peer),
                    core::file_label(name, *sensitive, caption),
                )),
            ),
            NodeEvent::GroupMessage {
                group,
                from,
                text,
                ours,
            } => (
                Some(group.clone()),
                (!ours).then(|| {
                    (
                        self.group_title(persona.as_deref(), group),
                        format!("{}: {text}", self.name(persona.as_deref(), from)),
                    )
                }),
            ),
            NodeEvent::GroupFile {
                group,
                from,
                name,
                ours,
                sensitive,
                caption,
                ..
            } => (
                Some(group.clone()),
                (!ours).then(|| {
                    (
                        self.group_title(persona.as_deref(), group),
                        format!(
                            "{}: {}",
                            self.name(persona.as_deref(), from),
                            core::file_label(name, *sensitive, caption)
                        ),
                    )
                }),
            ),
            NodeEvent::GroupInvited { group, name, from } => (
                Some(group.clone()),
                Some((
                    name.clone(),
                    format!(
                        "{} invites you to join",
                        self.name(persona.as_deref(), from)
                    ),
                )),
            ),
            // Not even who: a stranger's name can be a message in itself.
            NodeEvent::MessageRequest { peer, .. } => (
                Some(peer.clone()),
                Some(("Threnody".into(), "New message request".into())),
            ),
            NodeEvent::IdentityRevealed { peer, .. } => (
                Some(peer.clone()),
                Some((
                    self.title_of(persona.as_deref(), peer),
                    "Proved who they are. Open the chat to add them.".into(),
                )),
            ),
            NodeEvent::AccountChanged { account, added, .. }
                if !added.is_empty() && *account != self.core.main.account_fingerprint() =>
            {
                let c = added
                    .iter()
                    .find_map(|d| self.conversation_of(persona.as_deref(), d))
                    .or_else(|| self.conversation_of(persona.as_deref(), account));
                match c {
                    Some(c) if c.any_verified => (
                        Some(account.clone()),
                        Some((
                            format!("Safety alert: {}", c.title),
                            format!(
                                "{} added a device you haven't verified. Compare safety numbers.",
                                c.title
                            ),
                        )),
                    ),
                    _ => (Some(account.clone()), None),
                }
            }
            NodeEvent::Connected { peer, .. }
            | NodeEvent::Disconnected { peer, .. }
            | NodeEvent::MessageEdited { peer }
            | NodeEvent::MessagesDeleted { peer, .. }
            | NodeEvent::ApprovalChanged { peer, .. }
            | NodeEvent::ProfileChanged { peer } => (Some(peer.clone()), None),
            NodeEvent::Delivered { peer, group } | NodeEvent::Reacted { peer, group } => {
                (Some(group.clone().unwrap_or_else(|| peer.clone())), None)
            }
            NodeEvent::Typing { peer, active } => {
                if let Some(chat) = self.chat.borrow().as_ref() {
                    let conv = self.conversation_of(persona.as_deref(), peer);
                    if conv.as_ref().is_some_and(|c| c.chat == chat.chat()) {
                        chat.set_typing(*active);
                    }
                }
                (None, None)
            }
            NodeEvent::Read { peer } => {
                if let Some(chat) = self.chat.borrow().as_ref() {
                    let conv = self.conversation_of(persona.as_deref(), peer);
                    if conv.as_ref().is_some_and(|c| c.chat == chat.chat()) {
                        chat.reload();
                    }
                }
                (Some(peer.clone()), None)
            }
            NodeEvent::GroupJoined { group, .. }
            | NodeEvent::GroupMembersChanged { group, .. }
            | NodeEvent::GroupLeft { group } => (Some(group.clone()), None),
            NodeEvent::CredentialOffered { offer } => {
                self.credential_offer(persona.clone(), offer.clone());
                (
                    Some(offer.peer.clone()),
                    Some((
                        self.title_of(persona.as_deref(), &offer.peer),
                        "Offers you a credential".into(),
                    )),
                )
            }
            NodeEvent::CredentialAsked { ask } => {
                self.credential_ask(persona.clone(), ask.clone());
                (
                    Some(ask.peer.clone()),
                    Some((
                        self.title_of(persona.as_deref(), &ask.peer),
                        "Asks you to prove something".into(),
                    )),
                )
            }
            NodeEvent::CredentialReceived { peer, schema } => {
                self.toast(&format!(
                    "Got a credential ({schema}) from {}",
                    self.title_of(persona.as_deref(), peer)
                ));
                (None, None)
            }
            NodeEvent::CredentialPresented {
                peer,
                issuer,
                schema,
                attributes,
                pseudonym,
                ..
            } => {
                let shown: Vec<String> = attributes
                    .iter()
                    .map(|a| format!("{}: {}", a.key, a.value))
                    .collect();
                let body = format!(
                    "{} proved a credential ({schema}) issued by {}.\n\n{}\n\nTheir pseudonym for you: {}",
                    self.title_of(persona.as_deref(), peer),
                    self.name(persona.as_deref(), issuer),
                    if shown.is_empty() {
                        "Nothing else was shown.".to_owned()
                    } else {
                        shown.join("\n")
                    },
                    &pseudonym[..16.min(pseudonym.len())]
                );
                ui::alert("Credential proved", &body, &[("ok", "OK")]).present(Some(&self.window));
                (None, None)
            }
            NodeEvent::CredentialFailed { peer, reason, .. } => {
                self.toast(&format!(
                    "Credential with {}: {reason}",
                    self.title_of(persona.as_deref(), peer)
                ));
                (None, None)
            }
            NodeEvent::ThisDeviceRemoved => {
                self.toast("This device was removed from your account.");
                (None, None)
            }
            NodeEvent::DeviceLinked { .. } => {
                self.toast("A new device joined your account.");
                (None, None)
            }
            _ => (None, None),
        };

        let conv = about
            .as_deref()
            .and_then(|a| self.conversation_of(persona.as_deref(), a));
        // Reload the open chat if it's about it.
        if let Some(chat) = self.chat.borrow().as_ref() {
            let open = chat.chat();
            let matches = conv.as_ref().is_some_and(|c| c.chat == open)
                || about.as_deref().is_some_and(|a| match &open.target {
                    Target::Group(g) => g == a && open.persona == persona,
                    Target::Contact(_) => false,
                });
            if matches {
                chat.reload();
            }
        }
        if let Some((title, body)) = news {
            let watching = self
                .core
                .watching
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let chat = conv.as_ref().map(|c| c.chat.clone()).or_else(|| {
                about.as_ref().map(|a| ChatRef {
                    persona: persona.clone(),
                    target: if matches!(
                        event,
                        NodeEvent::GroupMessage { .. }
                            | NodeEvent::GroupFile { .. }
                            | NodeEvent::GroupInvited { .. }
                    ) {
                        Target::Group(a.clone())
                    } else {
                        Target::Contact(a.clone())
                    },
                })
            });
            if let Some(chat) = chat
                && watching.as_ref() != Some(&chat)
            {
                *self.unread.borrow_mut().entry(chat.clone()).or_default() += 1;
                self.notify(&chat, &title, &body);
            }
        }
        self.refresh_soon();
    }

    fn title_of(&self, persona: Option<&str>, peer: &str) -> String {
        self.conversation_of(persona, peer)
            .map_or_else(|| self.name(persona, peer), |c| c.title)
    }

    fn group_title(&self, persona: Option<&str>, group: &str) -> String {
        self.conversation_of(persona, group)
            .map_or_else(|| "Group".into(), |c| c.title)
    }

    fn name(&self, persona: Option<&str>, fp: &str) -> String {
        self.core
            .node(persona)
            .map_or_else(|_| core::short(fp), |n| Core::name_of(&n, fp))
    }

    /// Shows a notification; unless the user turned private notifications
    /// off, it says only "New message".
    fn notify(&self, chat: &ChatRef, title: &str, body: &str) {
        let private = self.core.settings().flag(settings::PRIVATE_NOTIFICATIONS);
        let n = if private {
            let n = gio::Notification::new("Threnody");
            n.set_body(Some("New message"));
            n
        } else {
            let n = gio::Notification::new(title);
            n.set_body(Some(body));
            n
        };
        n.set_default_action_and_target_value(
            "app.open-chat",
            Some(&chat_variant(chat).to_variant()),
        );
        self.app.send_notification(Some(&notification_id(chat)), &n);
    }

    /// Opens a chat from a notification's target.
    pub fn open_variant(self: &Rc<Self>, v: &str) {
        let Some(chat) = parse_chat(v) else { return };
        self.window.present();
        if let Some(c) = self.conversation(&chat) {
            self.open_chat(c);
        }
    }

    fn watch_window(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |w| {
            if let Some(this) = weak.upgrade() {
                // Contacts' records are polled at the foreground rate
                // whether or not the window has focus: a desktop runs on
                // mains power, and a contact's punch only lasts minutes.
                let on = w.is_active();
                this.update_watching();
                if on && let Some(c) = this.chat.borrow().as_ref() {
                    this.app.withdraw_notification(&notification_id(&c.chat()));
                    c.shown();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.split.connect_show_content_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.update_watching();
            }
        });
        // Closing the window keeps the node running for messages, unless
        // the user turned that off.
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |w| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if this.core.settings().flag(settings::BACKGROUND) {
                w.set_visible(false);
                this.update_watching();
                glib::Propagation::Stop
            } else {
                this.app.quit();
                glib::Propagation::Proceed
            }
        });
    }

    fn watch_network(self: &Rc<Self>) {
        let monitor = gio::NetworkMonitor::default();
        let core = self.core.clone();
        let metered = monitor.is_network_metered();
        ui::bg_quiet(move || core.set_metered(metered));
        let weak = Rc::downgrade(self);
        let last = Rc::new(Cell::new(monitor.is_network_available()));
        monitor.connect_network_changed(move |m, available| {
            let Some(this) = weak.upgrade() else { return };
            let core = this.core.clone();
            let metered = m.is_network_metered();
            let was = last.replace(available);
            ui::bg_quiet(move || {
                core.set_metered(metered);
                if available {
                    core.network_changed();
                } else if was {
                    core.say("* network lost");
                }
            });
        });
    }

    // ----- Actions -----

    fn actions(self: &Rc<Self>) {
        let add = |name: &str, f: fn(&Rc<Self>)| {
            let a = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            self.window.add_action(&a);
        };
        add("invite", |t| t.show_invite(None));
        add("add-contact", |t| t.add_contact(""));
        add("new-group", |t| t.new_group(None));
        add("anonymous-invite", Self::new_persona);
        add("profile", |t| t.edit_profile(None));
        add("personas", Self::personas);
        add("devices", Self::devices);
        add("link-device", Self::link_device);
        add("join-account", |t| t.join_account(""));
        add("preferences", Self::preferences);
        add("diagnostics", Self::diagnostics);
        add("credentials", Self::credentials);
        add("about", Self::about);

        let with_chat = |name: &str, f: fn(&Rc<Self>, Conversation)| {
            let a = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let weak = Rc::downgrade(self);
            a.connect_activate(move |_, v| {
                let Some(this) = weak.upgrade() else { return };
                let chat = v.and_then(glib::Variant::str).and_then(parse_chat);
                if let Some(c) = chat.and_then(|c| this.conversation(&c)) {
                    f(&this, c);
                }
            });
            self.window.add_action(&a);
        };
        with_chat("clear-chat", |t, c| t.clear_chat(&c));
        with_chat("delete-contact", |t, c| t.delete_contact(&c));
    }

    /// Runs `work` against the node of `persona`; shows `ok` or the error.
    pub fn run(
        self: &Rc<Self>,
        persona: Option<String>,
        what: &str,
        work: impl FnOnce(&Node) -> Result<(), threnody_ffi::ThrenodyError> + Send + 'static,
        ok: Option<String>,
    ) {
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        let what = what.to_owned();
        bg(
            move || {
                let node = core.node(persona.as_deref())?;
                work(&node).map_err(|e| e.to_string())
            },
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                match r {
                    Ok(()) => {
                        if let Some(ok) = ok {
                            this.toast(&ok);
                        }
                    }
                    Err(e) => this.toast(&format!("Couldn't {what}: {e}")),
                }
                this.refresh();
                if let Some(c) = this.chat.borrow().as_ref() {
                    c.reload();
                }
            },
        );
    }

    pub fn show_invite(self: &Rc<Self>, persona: Option<String>) {
        match self.core.invite(persona.as_deref()) {
            Ok(link) => {
                let (heading, body, caption) = match &persona {
                    None => (
                        "Your invite".to_owned(),
                        "Let the other person scan this, or send them the link. \
                         Anyone with it can contact this device.",
                        format!("Device {}", self.core.main.device_fingerprint()),
                    ),
                    Some(id) => (
                        format!(
                            "Anonymous invite · {}",
                            self.core.persona_label(id).unwrap_or_default()
                        ),
                        "Whoever uses this reaches your anonymous identity, not you. \
                         It works while this computer is on this network.",
                        self.core
                            .node(Some(id))
                            .map(|n| format!("Anonymous identity {}", n.device_fingerprint()))
                            .unwrap_or_default(),
                    ),
                };
                ui::show_code(&self.window, &heading, body, &link, &caption);
            }
            Err(e) => self.toast(&e),
        }
    }

    pub fn add_contact(self: &Rc<Self>, initial: &str) {
        let weak = Rc::downgrade(self);
        ui::ask_text(
            &self.window,
            "Add a contact",
            "Paste their invite link. You'll compare safety numbers together later.",
            "threnody://…",
            initial,
            "Connect",
            move |link| {
                if let Some(this) = weak.upgrade() {
                    this.connect(link);
                }
            },
        );
    }

    /// Dials an invite (or offers to join for a device link code).
    pub fn connect(self: &Rc<Self>, link: String) {
        if link.is_empty() {
            return;
        }
        if link.starts_with("threnody-link://") {
            return self.join_account(&link);
        }
        self.toast("Connecting…");
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        bg(
            move || core.main.connect(link).map_err(|e| e.to_string()),
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                match r {
                    Ok(peer) => {
                        this.toast("Connected");
                        this.open_when_listed(None, peer);
                    }
                    Err(e) => this.toast(&format!("Couldn't connect: {e}")),
                }
            },
        );
    }

    pub fn new_group(self: &Rc<Self>, persona: Option<String>) {
        let weak = Rc::downgrade(self);
        ui::ask_text(
            &self.window,
            "New group",
            "You'll own the group: only you can add and remove members. \
             Everyone in it sees who else is.",
            "Group name",
            "",
            "Create",
            move |name| {
                let Some(this) = weak.upgrade() else { return };
                if name.is_empty() {
                    return;
                }
                let core = this.core.clone();
                let persona = persona.clone();
                let weak = Rc::downgrade(&this);
                bg(
                    {
                        let persona = persona.clone();
                        move || {
                            core.node(persona.as_deref())?
                                .create_group(name)
                                .map_err(|e| e.to_string())
                        }
                    },
                    move |r| {
                        let Some(this) = weak.upgrade() else { return };
                        match r {
                            Ok(id) => this.open_when_listed(persona, id),
                            Err(e) => this.toast(&format!("Couldn't create the group: {e}")),
                        }
                    },
                );
            },
        );
    }

    fn new_persona(self: &Rc<Self>) {
        const BURN: [(&str, Option<u64>); 4] = [
            ("Keep it until I burn it", None),
            ("Burn after 1 day", Some(86_400_000)),
            ("Burn after 1 week", Some(7 * 86_400_000)),
            ("Burn after 4 weeks", Some(28 * 86_400_000)),
        ];
        let d = ui::alert(
            "Anonymous invite",
            "A new identity with its own keys and conversations. Nothing links it to you \
             unless you reveal it. Anyone you reach directly can still see your network address.",
            &[("cancel", "Cancel"), ("ok", "Create")],
        );
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let entry = gtk::Entry::builder()
            .placeholder_text("Your label for it (only you see this)")
            .activates_default(true)
            .build();
        let burn = gtk::DropDown::from_strings(&BURN.map(|(l, _)| l));
        b.append(&entry);
        b.append(&burn);
        d.set_extra_child(Some(&b));
        let weak = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            let Some(this) = weak.upgrade() else { return };
            if r != "ok" {
                return;
            }
            let label = Some(entry.text().trim().to_owned())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "Anonymous".into());
            let after = BURN[burn.selected() as usize].1;
            let core = this.core.clone();
            let weak = Rc::downgrade(&this);
            bg(
                move || {
                    let expires = after.map(|ms| threnody_core::now_ms() + ms);
                    core.create_persona(&label, expires)
                },
                move |r| {
                    let Some(this) = weak.upgrade() else { return };
                    match r {
                        Ok(id) => this.show_invite(Some(id)),
                        Err(e) => this.toast(&format!("Couldn't create it: {e}")),
                    }
                },
            );
        });
        d.present(Some(&self.window));
        let e = d.extra_child().and_then(|b| b.first_child());
        glib::idle_add_local_once(move || {
            if let Some(e) = e {
                e.grab_focus();
            }
        });
    }

    fn personas(self: &Rc<Self>) {
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        bg(
            move || core.personas(),
            move |list| {
                let Some(this) = weak.upgrade() else { return };
                if list.is_empty() {
                    let d = ui::alert(
                        "Anonymous identities",
                        "None yet. + → Anonymous invite makes one.",
                        &[("close", "Close"), ("make", "Make one")],
                    );
                    let weak = Rc::downgrade(&this);
                    d.connect_response(Some("make"), move |_, _| {
                        if let Some(t) = weak.upgrade() {
                            t.new_persona();
                        }
                    });
                    d.present(Some(&this.window));
                    return;
                }
                let names: Vec<String> = list
                    .iter()
                    .map(|p| {
                        let burns = p.expires_ms.map_or("kept until burned".to_owned(), |t| {
                            format!("burns {}", ui::time_label(t))
                        });
                        format!("🎭 {} · {burns}", p.label)
                    })
                    .collect();
                let weak = Rc::downgrade(&this);
                ui::choose(
                    &this.window,
                    "Anonymous identities",
                    "",
                    &names,
                    None,
                    move |i| {
                        if let Some(t) = weak.upgrade() {
                            t.persona_actions(list[i].id.clone(), list[i].label.clone());
                        }
                    },
                );
            },
        );
    }

    fn persona_actions(self: &Rc<Self>, id: String, label: String) {
        let options = [
            "Show its invite",
            "Its profile",
            "New group as it",
            "Rename",
            "Burn it",
        ]
        .map(String::from);
        let weak = Rc::downgrade(self);
        ui::choose(
            &self.window,
            &format!("🎭 {label}"),
            "",
            &options,
            None,
            move |i| {
                let Some(this) = weak.upgrade() else { return };
                match i {
                    0 => this.show_invite(Some(id.clone())),
                    1 => this.edit_profile(Some(id.clone())),
                    2 => this.new_group(Some(id.clone())),
                    3 => {
                        let (weak, id) = (Rc::downgrade(&this), id.clone());
                        ui::ask_text(
                            &this.window,
                            "Rename",
                            "Only you see this label.",
                            "Label",
                            &label,
                            "Save",
                            move |l| {
                                let Some(this) = weak.upgrade() else { return };
                                if l.is_empty() {
                                    return;
                                }
                                let (core, id, w) =
                                    (this.core.clone(), id.clone(), Rc::downgrade(&this));
                                bg(
                                    move || core.rename_persona(&id, &l),
                                    move |r| {
                                        if let (Some(t), Err(e)) = (w.upgrade(), r) {
                                            t.toast(&format!("Couldn't rename: {e}"));
                                        }
                                    },
                                );
                            },
                        );
                    }
                    _ => {
                        let (weak, id) = (Rc::downgrade(&this), id.clone());
                        ui::confirm(
                            &this.window,
                            &format!("Burn {label}?"),
                            "Its keys, contacts, messages and files are deleted for good. Nobody can reach it again.",
                            "Burn",
                            true,
                            move || {
                                let Some(this) = weak.upgrade() else { return };
                                if this.chat.borrow().as_ref().is_some_and(|c| {
                                    c.chat().persona.as_deref() == Some(id.as_str())
                                }) {
                                    this.close_chat();
                                }
                                let (core, id, w) =
                                    (this.core.clone(), id.clone(), Rc::downgrade(&this));
                                bg(
                                    move || core.burn_persona(&id),
                                    move |r| {
                                        if let Some(t) = w.upgrade() {
                                            t.toast(&match r {
                                                Ok(()) => "Burned".into(),
                                                Err(e) => format!("Couldn't burn it: {e}"),
                                            });
                                            t.refresh();
                                        }
                                    },
                                );
                            },
                        );
                    }
                }
            },
        );
    }

    /// Edits the profile of the main identity or a persona.
    pub fn edit_profile(self: &Rc<Self>, persona: Option<String>) {
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        let p = persona.clone();
        bg(
            move || core.node(p.as_deref()).map(|n| n.profile()),
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                let attrs = match r {
                    Ok(a) => a,
                    Err(e) => return this.toast(&e),
                };
                this.profile_dialog(persona, attrs);
            },
        );
    }

    fn profile_dialog(self: &Rc<Self>, persona: Option<String>, attrs: Vec<ProfileAttr>) {
        const SUGGESTED: [&str; 4] = ["name", "email", "phone", "about"];
        let d = adw::Dialog::builder()
            .title(if persona.is_some() {
                "This identity's profile"
            } else {
                "Your profile"
            })
            .content_width(440)
            .build();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
            .description(
                "Contacts see only the details you share with each of them \
                 (a chat's menu → Share your profile). Leave a field empty to remove it.",
            )
            .build();
        let mut keys: Vec<String> = SUGGESTED.iter().map(|s| (*s).to_owned()).collect();
        for a in &attrs {
            if !keys.contains(&a.key) {
                keys.push(a.key.clone());
            }
        }
        let rows: Vec<(String, adw::EntryRow)> = keys
            .into_iter()
            .map(|k| {
                let row = adw::EntryRow::builder().title(capitalize(&k)).build();
                if let Some(a) = attrs.iter().find(|a| a.key == k) {
                    row.set_text(&a.value);
                }
                group.add(&row);
                (k, row)
            })
            .collect();
        page.add(&group);
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        let header = adw::HeaderBar::new();
        header.pack_end(&save);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&page));
        d.set_child(Some(&view));
        let weak = Rc::downgrade(self);
        let dd = d.clone();
        save.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let attrs: Vec<ProfileAttr> = rows
                .iter()
                .filter_map(|(k, r)| {
                    let v = r.text().trim().to_owned();
                    (!v.is_empty()).then(|| ProfileAttr {
                        key: k.clone(),
                        value: v,
                    })
                })
                .collect();
            this.run(
                persona.clone(),
                "save",
                move |n| n.set_profile(attrs),
                Some("Profile saved".into()),
            );
            dd.close();
        });
        d.present(Some(&self.window));
    }

    fn devices(self: &Rc<Self>) {
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        bg(
            move || core.main.devices(),
            move |devices| {
                let Some(this) = weak.upgrade() else { return };
                let names: Vec<String> = devices
                    .iter()
                    .map(|d| {
                        format!(
                            "{}{} · {}",
                            d.name,
                            if d.this_device { " (this device)" } else { "" },
                            core::short(&d.fingerprint)
                        )
                    })
                    .collect();
                let weak = Rc::downgrade(&this);
                ui::choose(
                    &this.window,
                    "Your devices",
                    "Devices in one account share contacts and message history. Click one to rename it.",
                    &names,
                    None,
                    move |i| {
                        let Some(this) = weak.upgrade() else { return };
                        let d = devices[i].clone();
                        let weak = Rc::downgrade(&this);
                        ui::ask_text(
                            &this.window,
                            "Rename device",
                            "Your other devices and your contacts see this name.",
                            "Name",
                            &d.name,
                            "Save",
                            move |name| {
                                if let (Some(t), false) = (weak.upgrade(), name.is_empty()) {
                                    let fp = d.fingerprint.clone();
                                    let ok = format!("Renamed to {name}");
                                    t.run(
                                        None,
                                        "rename",
                                        move |n| n.rename_device(fp, name),
                                        Some(ok),
                                    );
                                }
                            },
                        );
                    },
                );
            },
        );
    }

    fn link_device(self: &Rc<Self>) {
        match self.core.link_code() {
            Ok(code) => ui::show_code(
                &self.window,
                "Link a new device",
                "On the new device, choose “Join another device's account” and scan or paste this. \
                 It works once. Only show it to your own devices.",
                &code,
                &format!("Account {}", self.core.main.account_fingerprint()),
            ),
            Err(e) => self.toast(&e),
        }
    }

    pub fn join_account(self: &Rc<Self>, initial: &str) {
        let weak = Rc::downgrade(self);
        ui::ask_text(
            &self.window,
            "Join another device's account",
            "Paste the link code your other device shows (its menu → Link a new device). \
             This device then shares its contacts and history.",
            "threnody-link://…",
            initial,
            "Join",
            move |code| {
                let Some(this) = weak.upgrade() else { return };
                if code.is_empty() {
                    return;
                }
                this.toast("Joining…");
                let core = this.core.clone();
                let weak = Rc::downgrade(&this);
                bg(
                    move || core.main.link_with(code).map_err(|e| e.to_string()),
                    move |r| {
                        let Some(this) = weak.upgrade() else { return };
                        match r {
                            Ok(account) => {
                                this.toast(&format!("Joined account {}", core::short(&account)))
                            }
                            Err(e) => this.toast(&format!("Couldn't join: {e}")),
                        }
                        this.refresh();
                    },
                );
            },
        );
    }

    fn preferences(self: &Rc<Self>) {
        let d = adw::PreferencesDialog::new();
        d.set_title("Preferences");
        let page = adw::PreferencesPage::new();
        let privacy = adw::PreferencesGroup::builder()
            .title("Metadata protection")
            .description("All on unless you switch them off.")
            .build();
        let toggle = |group: &adw::PreferencesGroup, key: &'static str, title: &str, sub: &str| {
            let row = adw::SwitchRow::builder()
                .title(title)
                .subtitle(sub)
                .active(self.core.settings().flag(key))
                .build();
            let core = self.core.clone();
            row.connect_active_notify(move |r| {
                core.settings().set_flag(key, r.is_active());
                let core = core.clone();
                ui::bg_quiet(move || core.apply_privacy());
            });
            group.add(&row);
        };
        toggle(
            &privacy,
            settings::COVER,
            "Cover traffic",
            "Each session sends a padded frame every 2 s (10 s on metered networks), so traffic doesn't show when you send",
        );
        toggle(
            &privacy,
            settings::ONION,
            "Onion routing first",
            "Contacts are reached through two-relay onion circuits when approved relays allow",
        );
        toggle(
            &privacy,
            settings::STRIP,
            "Remove photo metadata",
            "Photos are sent without location, camera or time details",
        );
        toggle(
            &privacy,
            settings::REACH,
            "Reach contacts over the internet",
            "Strangers in the public DHT see this computer's IP address, not who you talk to",
        );
        page.add(&privacy);

        let relays = adw::PreferencesGroup::builder()
            .title("Volunteer relays")
            .description(
                "When your contacts can't relay for you, circuits can go through volunteers \
                 listed by relay directories you subscribe to.",
            )
            .build();
        toggle(
            &relays,
            settings::VOLUNTEERS,
            "Route through volunteer relays",
            "Each hop is paid with an anonymous token; no relay learns who you are",
        );
        let dirs = adw::ActionRow::builder()
            .title("Relay directories")
            .subtitle(format!(
                "{} subscribed · {} relays usable",
                self.core.main.directories().len(),
                self.core.main.volunteer_relay_count()
            ))
            .activatable(true)
            .build();
        dirs.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let weak = Rc::downgrade(self);
        dirs.connect_activated(move |_| {
            if let Some(t) = weak.upgrade() {
                t.directories();
            }
        });
        relays.add(&dirs);
        page.add(&relays);

        let messages = adw::PreferencesGroup::builder().title("Messages").build();
        let timers: Vec<&str> = settings::TIMERS.iter().map(|(l, _)| *l).collect();
        let timer = adw::ComboRow::builder()
            .title("Default disappearing timer")
            .subtitle("For chats that haven't chosen their own")
            .model(&gtk::StringList::new(&timers))
            .build();
        let current = self.core.settings().number(settings::TIMER);
        timer.set_selected(
            u32::try_from(
                settings::TIMERS
                    .iter()
                    .position(|(_, s)| *s == current)
                    .unwrap_or(0),
            )
            .unwrap_or(0),
        );
        let core = self.core.clone();
        timer.connect_selected_notify(move |r| {
            let secs = settings::TIMERS[r.selected() as usize].1;
            core.settings().set_number(settings::TIMER, secs);
            let core = core.clone();
            ui::bg_quiet(move || core.apply_privacy());
        });
        messages.add(&timer);
        toggle(
            &messages,
            settings::PRIVATE_NOTIFICATIONS,
            "Private notifications",
            "Notifications say only “New message”, without sender or text",
        );
        toggle(
            &messages,
            settings::SEND_READ,
            "Send read receipts",
            "When you open a chat, the sender learns you've displayed their messages",
        );
        toggle(
            &messages,
            settings::SEND_TYPING,
            "Send typing indicators",
            "Contacts see “…” while you write to them",
        );
        toggle(
            &messages,
            settings::BACKGROUND,
            "Keep running when closed",
            "Closing the window keeps you reachable; quit from the menu",
        );
        page.add(&messages);
        d.add(&page);
        d.present(Some(&self.window));
    }

    fn diagnostics(self: &Rc<Self>) {
        let d = adw::Dialog::builder()
            .title("Diagnostics")
            .content_width(640)
            .content_height(520)
            .build();
        let info = gtk::Box::new(gtk::Orientation::Vertical, 4);
        info.set_margin_start(12);
        info.set_margin_end(12);
        info.set_margin_top(6);
        let core = &self.core;
        let sealed =
            threnody_ffi::identity_is_sealed(core.home.display().to_string()).unwrap_or(false);
        for line in [
            format!("Device {}", core.main.device_fingerprint()),
            format!("Account {}", core.main.account_fingerprint()),
            format!("Data {}", core.home.display()),
            format!(
                "Identity key {}",
                if sealed {
                    "sealed (system keyring or passphrase)"
                } else {
                    "stored unprotected"
                }
            ),
            format!(
                "Listening on port {}{}",
                core.port,
                Core::lan_ip()
                    .map(|ip| format!(" · {ip}"))
                    .unwrap_or_default()
            ),
        ] {
            let l = ui::label(&line, &["caption", "monospace"]);
            l.set_selectable(true);
            info.append(&l);
        }
        let reach = ui::label("", &["caption", "dim-label"]);
        info.append(&reach);
        let c = core.clone();
        let r = reach.clone();
        bg(
            move || c.main.reachability(),
            move |ri| {
                r.set_label(&if !ri.enabled {
                    "Internet reachability off".into()
                } else {
                    format!(
                        "Internet: {}{}{}",
                        if ri.online {
                            "in the DHT"
                        } else {
                            "not in the DHT yet"
                        },
                        if ri.addresses.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", ri.addresses.join(", "))
                        },
                        if ri.symmetric {
                            " · symmetric NAT"
                        } else {
                            ""
                        }
                    )
                });
            },
        );
        let buf = gtk::TextBuffer::new(None);
        buf.set_text(&core.log_text());
        let text = gtk::TextView::builder()
            .buffer(&buf)
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(12)
            .right_margin(12)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&text)
            .vexpand(true)
            .build();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        b.append(&info);
        b.append(&scroll);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&b));
        d.set_child(Some(&view));
        *self.log.borrow_mut() = Some(buf);
        let weak = Rc::downgrade(self);
        d.connect_closed(move |_| {
            if let Some(t) = weak.upgrade() {
                *t.log.borrow_mut() = None;
            }
        });
        d.present(Some(&self.window));
    }

    fn about(self: &Rc<Self>) {
        let a = adw::AboutDialog::builder()
            .application_name("Threnody")
            .application_icon(crate::APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments("Encrypted, metadata-resistant messaging with post-quantum hybrid cryptography.\n\nNot audited: don't rely on it for real-world safety yet.")
            .website("https://github.com/larrydewey/threnody")
            .license_type(gtk::License::MitX11)
            .build();
        a.present(Some(&self.window));
    }

    // ----- Relay directories and credentials -----

    fn directories(self: &Rc<Self>) {
        let d = adw::Dialog::builder()
            .title("Relay directories")
            .content_width(560)
            .content_height(480)
            .build();
        let page = adw::PreferencesPage::new();
        let list = adw::PreferencesGroup::builder()
            .description(
                "A directory lists volunteer relays and gives out anonymous tokens to pay them. \
                 Subscribing shows it this computer's address, never who you are. A relay is used \
                 only if enough of your directories list it.",
            )
            .build();
        let dirs = self.core.main.directories();
        if dirs.is_empty() {
            list.add(
                &adw::ActionRow::builder()
                    .title("No directories yet")
                    .build(),
            );
        }
        for dir in dirs {
            let until = dir
                .valid_until_ms
                .map_or("no relay list yet".to_owned(), |t| {
                    format!("valid until {}", ui::time_label(t))
                });
            let row = adw::ActionRow::builder()
                .title(format!("Directory {}", dir.id))
                .subtitle(format!(
                    "{} relays · {} tokens · {until}",
                    dir.relays, dir.tokens
                ))
                .build();
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.add_css_class("flat");
            remove.set_valign(gtk::Align::Center);
            remove.set_tooltip_text(Some("Unsubscribe"));
            let (core, id, dd) = (self.core.clone(), dir.id_hex.clone(), d.clone());
            remove.connect_clicked(move |_| {
                let (core, id) = (core.clone(), id.clone());
                ui::bg_quiet(move || core.unsubscribe_directory(&id));
                dd.close();
            });
            row.add_suffix(&remove);
            list.add(&row);
        }
        page.add(&list);
        let add = adw::PreferencesGroup::builder().title("Subscribe").build();
        let entry = adw::EntryRow::builder()
            .title("threnody-dir://…")
            .show_apply_button(true)
            .build();
        let weak = Rc::downgrade(self);
        let dd = d.clone();
        entry.connect_apply(move |e| {
            let Some(this) = weak.upgrade() else { return };
            let link = e.text().trim().to_owned();
            if link.is_empty() {
                return;
            }
            dd.close();
            this.toast("Subscribing…");
            let (core, weak) = (this.core.clone(), Rc::downgrade(&this));
            bg(
                move || core.subscribe_directory(&link),
                move |r| {
                    if let Some(t) = weak.upgrade() {
                        t.toast(&match r {
                            Ok(d) => {
                                format!("Subscribed: {} relays, {} tokens", d.relays, d.tokens)
                            }
                            Err(e) => format!("Couldn't subscribe: {e}"),
                        });
                    }
                },
            );
        });
        add.add(&entry);
        page.add(&add);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        d.set_child(Some(&view));
        d.present(Some(&self.window));
    }

    fn credentials(self: &Rc<Self>) {
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        bg(
            move || core.main.credentials(),
            move |creds| {
                let Some(this) = weak.upgrade() else { return };
                let d = adw::Dialog::builder()
                    .title("Credentials")
                    .content_width(520)
                    .content_height(460)
                    .build();
                let page = adw::PreferencesPage::new();
                let group = adw::PreferencesGroup::builder()
                    .description(
                        "Attributes others vouched for. When someone asks, you choose what to prove; \
                         nothing else is shown, and proofs can't be linked to each other.",
                    )
                    .build();
                if creds.is_empty() {
                    group.add(&adw::ActionRow::builder().title("None yet").build());
                }
                for c in creds {
                    let attrs: Vec<String> = c
                        .attributes
                        .iter()
                        .map(|a| format!("{}: {}", a.key, a.value))
                        .collect();
                    let row = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&c.schema))
                        .subtitle(glib::markup_escape_text(&format!(
                            "{} · from {}",
                            attrs.join(", "),
                            this.name(None, &c.issuer)
                        )))
                        .build();
                    let del = gtk::Button::from_icon_name("user-trash-symbolic");
                    del.add_css_class("flat");
                    del.set_valign(gtk::Align::Center);
                    let (core, id, dd) = (this.core.clone(), c.id, d.clone());
                    del.connect_clicked(move |_| {
                        core.main.delete_credential(id);
                        dd.close();
                    });
                    row.add_suffix(&del);
                    group.add(&row);
                }
                page.add(&group);
                let view = adw::ToolbarView::new();
                view.add_top_bar(&adw::HeaderBar::new());
                view.set_content(Some(&page));
                d.set_child(Some(&view));
                d.present(Some(&this.window));
            },
        );
    }

    fn credential_offer(
        self: &Rc<Self>,
        persona: Option<String>,
        offer: threnody_ffi::CredentialOfferRecord,
    ) {
        let attrs: Vec<String> = offer
            .attributes
            .iter()
            .map(|a| format!("{}: {}", a.key, a.value))
            .collect();
        let d = ui::alert(
            &format!(
                "{} offers you a credential",
                self.title_of(persona.as_deref(), &offer.peer)
            ),
            &format!(
                "{}\n\n{}\n\nKeep it to prove these later, choosing what to show each time.",
                offer.schema,
                attrs.join("\n")
            ),
            &[("decline", "Decline"), ("accept", "Accept")],
        );
        d.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
        let weak = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            let Some(this) = weak.upgrade() else { return };
            let id = offer.id;
            if r == "accept" {
                this.run(
                    persona.clone(),
                    "accept the credential",
                    move |n| n.accept_credential_offer(id),
                    None,
                );
            } else {
                this.run(
                    persona.clone(),
                    "decline",
                    move |n| n.decline_credential(id),
                    None,
                );
            }
        });
        d.present(Some(&self.window));
    }

    fn credential_ask(
        self: &Rc<Self>,
        persona: Option<String>,
        ask: threnody_ffi::CredentialAskRecord,
    ) {
        let Ok(node) = self.core.node(persona.as_deref()) else {
            return;
        };
        let who = self.title_of(persona.as_deref(), &ask.peer);
        let matching: Vec<threnody_ffi::CredentialRecord> = node
            .credentials()
            .into_iter()
            .filter(|c| c.schema == ask.schema)
            .collect();
        if matching.is_empty() {
            let d = ui::alert(
                &format!("{who} asks for a credential ({})", ask.schema),
                "You don't hold one.",
                &[("decline", "Decline")],
            );
            let id = ask.id;
            d.connect_response(None, move |_, _| {
                let _ = node.decline_credential(id);
            });
            d.present(Some(&self.window));
            return;
        }
        let d = ui::alert(
            &format!("{who} asks you to prove something"),
            &format!(
                "From your {} credential. Choose what to show; nothing else is revealed.",
                ask.schema
            ),
            &[("decline", "Decline"), ("present", "Prove")],
        );
        d.set_response_appearance("present", adw::ResponseAppearance::Suggested);
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let labels: Vec<String> = matching
            .iter()
            .map(|c| {
                format!(
                    "{} from {}",
                    c.schema,
                    self.name(persona.as_deref(), &c.issuer)
                )
            })
            .collect();
        let pick =
            gtk::DropDown::from_strings(&labels.iter().map(String::as_str).collect::<Vec<_>>());
        b.append(&pick);
        // Nothing is shown unless ticked.
        let checks: Vec<(String, gtk::CheckButton)> = ask
            .keys
            .iter()
            .map(|k| {
                let c = gtk::CheckButton::with_label(k);
                b.append(&c);
                (k.clone(), c)
            })
            .collect();
        if ask.keys.is_empty() {
            b.append(&ui::label("Only that you hold one.", &["dim-label"]));
        }
        d.set_extra_child(Some(&b));
        let weak = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            let Some(this) = weak.upgrade() else { return };
            let id = ask.id;
            if r != "present" {
                this.run(
                    persona.clone(),
                    "decline",
                    move |n| n.decline_credential(id),
                    None,
                );
                return;
            }
            let cred = matching[pick.selected() as usize].id;
            let keys: Vec<String> = checks
                .iter()
                .filter(|(_, c)| c.is_active())
                .map(|(k, _)| k.clone())
                .collect();
            this.run(
                persona.clone(),
                "prove it",
                move |n| n.present_credential(id, cred, keys),
                Some("Proof sent".into()),
            );
        });
        d.present(Some(&self.window));
    }

    // ----- Clearing and deleting -----

    pub fn clear_chat(self: &Rc<Self>, c: &Conversation) {
        let group = matches!(c.chat.target, Target::Group(_));
        let body = if group {
            "Its messages are deleted from this device. You stay in the group, and members keep their copies.".to_owned()
        } else {
            format!(
                "Your messages with {} are deleted from all your devices. They stay a contact, \
                 and you can keep talking. They keep their copy.",
                c.title
            )
        };
        let (weak, c2) = (Rc::downgrade(self), c.clone());
        ui::confirm(
            &self.window,
            &format!("Clear chat with {}?", c.title),
            &body,
            "Clear",
            true,
            move || {
                let Some(this) = weak.upgrade() else { return };
                let c = c2.clone();
                this.run(
                    c.chat.persona.clone(),
                    "clear",
                    move |n| match &c.chat.target {
                        Target::Group(g) => n.clear_group_conversation(g.clone()),
                        Target::Contact(_) => n.clear_conversation(c.device.clone()),
                    },
                    None,
                );
            },
        );
    }

    pub fn delete_contact(self: &Rc<Self>, c: &Conversation) {
        let (weak, c2) = (Rc::downgrade(self), c.clone());
        ui::confirm(
            &self.window,
            &format!("Delete {}?", c.title),
            "They leave your contacts and your messages with them are deleted, on all your devices. \
             They keep their copy. If they write again, it arrives as a new message request.",
            "Delete",
            true,
            move || {
                let Some(this) = weak.upgrade() else { return };
                if this
                    .chat
                    .borrow()
                    .as_ref()
                    .is_some_and(|v| v.chat() == c2.chat)
                {
                    this.close_chat();
                }
                let device = c2.device.clone();
                this.run(
                    c2.chat.persona.clone(),
                    "delete",
                    move |n| n.delete_conversation(device),
                    None,
                );
            },
        );
    }
}

fn placeholder() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name(crate::APP_ID)
        .title("Threnody")
        .description("Choose a conversation")
        .build()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// A conversation as a string, for actions and notifications.
fn chat_variant(c: &ChatRef) -> String {
    let (kind, key) = match &c.target {
        Target::Contact(k) => ("c", k),
        Target::Group(g) => ("g", g),
    };
    format!("{}|{kind}|{key}", c.persona.as_deref().unwrap_or(""))
}

fn parse_chat(s: &str) -> Option<ChatRef> {
    let mut parts = s.splitn(3, '|');
    let persona = parts.next()?;
    let kind = parts.next()?;
    let key = parts.next()?.to_owned();
    Some(ChatRef {
        persona: (!persona.is_empty()).then(|| persona.to_owned()),
        target: match kind {
            "g" => Target::Group(key),
            _ => Target::Contact(key),
        },
    })
}

fn notification_id(c: &ChatRef) -> String {
    format!("chat-{}", chat_variant(c).replace('|', "-"))
}
