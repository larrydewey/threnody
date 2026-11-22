//! Cards (Appendix V): a typed item a text can carry, which apps draw as a
//! card with its own actions: a contact, a Wi-Fi network, a place, an
//! event, or a secret (a login, a payment card, a note), as from a
//! password manager.
//!
//! A card travels with its text, whose body is the card's
//! [`Card::fallback_text`]: an app without cards shows that. Receivers
//! keep a card only if it is well formed ([`Card::clean`]); otherwise the
//! text stands alone.
//!
//! ```text
//! card     = { 0: kind, ? 1: flags, ? 2: expires_in_s, 3: item }
//! flags    = 1 view once | 2 no forwarding (always set for a secret)
//! contact  = { 0: name, ? 1: invite link, ? 2: handle, ? 3: [* phone],
//!              ? 4: [* email], ? 5: address, ? 6: note }
//! wifi     = { 0: ssid, ? 1: password, 2: security, ? 3: hidden }
//! location = { 0: latitude e7 (int), 1: longitude e7 (int), ? 2: label,
//!              ? 3: accuracy m }
//! event    = { 0: title, 1: start ms, ? 2: end ms, ? 3: all day,
//!              ? 4: place, ? 5: note }
//! secret   = { 0: secret kind, 1: title, 2: [* [name, value, hidden]],
//!              ? 3: source }
//! ```

use crate::cbor;
use crate::error::{Error, Result};

/// `flags`: the receiver sees the card once; opening it erases it.
pub const CARD_VIEW_ONCE: u64 = 1;
/// `flags`: apps offer no way to forward it (a secret always has it).
pub const CARD_NO_FORWARD: u64 = 2;
/// Local only, never sent: a view-once card that was opened, so its
/// contents are gone.
pub const CARD_OPENED: u64 = 1 << 32;
const SENT_FLAGS: u64 = CARD_VIEW_ONCE | CARD_NO_FORWARD;

pub const MAX_CARD_NAME: usize = 100;
pub const MAX_CARD_TEXT: usize = 500;
pub const MAX_CARD_VALUE: usize = 2000;
pub const MAX_CARD_LIST: usize = 5;
pub const MAX_SECRET_FIELDS: usize = 16;
/// A Wi-Fi network's name is at most 32 bytes, its WPA passphrase at most
/// 63 characters (a 64-digit hex key otherwise).
pub const MAX_SSID_BYTES: usize = 32;
pub const MAX_WIFI_PASSWORD: usize = 64;
/// Longest a card may stay before it disappears: a week.
pub const MAX_CARD_EXPIRY_S: u32 = 7 * 24 * 3600;

const KIND_CONTACT: u64 = 1;
const KIND_WIFI: u64 = 2;
const KIND_LOCATION: u64 = 3;
const KIND_EVENT: u64 = 4;
const KIND_SECRET: u64 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub item: CardItem,
    /// [`CARD_VIEW_ONCE`], [`CARD_NO_FORWARD`] (and locally [`CARD_OPENED`]).
    pub flags: u64,
    /// Gone this long after it is sent (0 = as the chat's timer says).
    pub expires_in_s: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardItem {
    Contact(ContactCard),
    Wifi(WifiCard),
    Location(LocationCard),
    Event(EventCard),
    Secret(SecretCard),
}

/// Someone to get in touch with: on Threnody (their invite link or
/// handle), or by phone, email or post.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContactCard {
    pub name: String,
    pub invite: String,
    pub handle: String,
    pub phones: Vec<String>,
    pub emails: Vec<String>,
    pub address: String,
    pub note: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WifiSecurity {
    Open,
    /// WPA, WPA2 or WPA3 Personal.
    #[default]
    Wpa,
    Wep,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WifiCard {
    pub ssid: String,
    pub password: String,
    pub security: WifiSecurity,
    pub hidden: bool,
}

/// A place, in degrees × 10⁷ (WGS 84).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocationCard {
    pub lat_e7: i32,
    pub lon_e7: i32,
    pub label: String,
    /// How far off it may be, in metres (0 = unknown).
    pub accuracy_m: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventCard {
    pub title: String,
    pub start_ms: u64,
    /// 0 = no end given.
    pub end_ms: u64,
    pub all_day: bool,
    pub place: String,
    pub note: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SecretKind {
    #[default]
    Other,
    Login,
    PaymentCard,
    Note,
}

/// One field of a secret. Apps know these names: `username`, `password`,
/// `url`, `otp` (an `otpauth://` URI), `number`, `holder`, `expiry`,
/// `cvc`, `pin` and `text`; any other is shown under its name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecretField {
    pub name: String,
    pub value: String,
    /// Shown covered until revealed (a password, a card's number).
    pub hidden: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecretCard {
    pub kind: SecretKind,
    pub title: String,
    pub fields: Vec<SecretField>,
    /// Where it came from (`symbolon`), for the app to offer saving it
    /// back there.
    pub source: String,
}

impl Card {
    pub fn new(item: CardItem) -> Self {
        Self {
            item,
            flags: 0,
            expires_in_s: 0,
        }
    }

    pub fn is_secret(&self) -> bool {
        matches!(self.item, CardItem::Secret(_))
    }

    pub fn view_once(&self) -> bool {
        self.flags & CARD_VIEW_ONCE != 0
    }

    pub fn opened(&self) -> bool {
        self.flags & CARD_OPENED != 0
    }

    /// Whether apps may offer to forward it.
    pub fn forwardable(&self) -> bool {
        self.flags & (CARD_NO_FORWARD | CARD_VIEW_ONCE) == 0 && !self.is_secret()
    }

    /// Whether only peers that understand cards may get it: what it holds
    /// must not go out as plain text (a secret), or must not stay (view
    /// once), and an app without cards would do either.
    pub fn needs_cards(&self) -> bool {
        self.is_secret() || self.view_once()
    }

    /// This card made safe to keep and show, or None if it isn't a card
    /// at all: every text trimmed, cut to its limit and rid of control
    /// characters and bidirectional overrides; lists cut short; a secret
    /// always marked not to be forwarded; an invite link or handle that
    /// doesn't parse dropped. Unknown flags go.
    pub fn clean(mut self) -> Option<Self> {
        self.flags &= SENT_FLAGS;
        self.expires_in_s = self.expires_in_s.min(MAX_CARD_EXPIRY_S);
        self.item = match self.item {
            CardItem::Contact(c) => {
                let invite = line(&c.invite, 1000);
                let invite = if invite.parse::<crate::invite::InviteLink>().is_ok() {
                    invite
                } else {
                    String::new()
                };
                let handle = line(&c.handle, MAX_CARD_NAME);
                let handle = if handle.parse::<crate::handle::Handle>().is_ok() {
                    handle
                } else {
                    String::new()
                };
                let list = |v: &[String], max: usize| -> Vec<String> {
                    v.iter()
                        .map(|s| line(s, max))
                        .filter(|s| !s.is_empty())
                        .take(MAX_CARD_LIST)
                        .collect()
                };
                let c = ContactCard {
                    name: line(&c.name, MAX_CARD_NAME),
                    invite,
                    handle,
                    phones: list(&c.phones, 40),
                    emails: list(&c.emails, 254),
                    address: text(&c.address, MAX_CARD_TEXT),
                    note: text(&c.note, MAX_CARD_TEXT),
                };
                let reachable = !c.invite.is_empty()
                    || !c.handle.is_empty()
                    || !c.phones.is_empty()
                    || !c.emails.is_empty()
                    || !c.address.is_empty();
                if c.name.is_empty() || !reachable {
                    return None;
                }
                CardItem::Contact(c)
            }
            CardItem::Wifi(w) => {
                let ssid = cut_bytes(&line(&w.ssid, MAX_SSID_BYTES), MAX_SSID_BYTES);
                if ssid.is_empty() {
                    return None;
                }
                let password = if w.security == WifiSecurity::Open {
                    String::new()
                } else {
                    w.password
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(MAX_WIFI_PASSWORD)
                        .collect()
                };
                CardItem::Wifi(WifiCard {
                    ssid,
                    password,
                    security: w.security,
                    hidden: w.hidden,
                })
            }
            CardItem::Location(l) => {
                if !(-900_000_000..=900_000_000).contains(&l.lat_e7)
                    || !(-1_800_000_000..=1_800_000_000).contains(&l.lon_e7)
                {
                    return None;
                }
                CardItem::Location(LocationCard {
                    label: line(&l.label, MAX_CARD_NAME),
                    accuracy_m: l.accuracy_m.min(1_000_000),
                    ..l
                })
            }
            CardItem::Event(e) => {
                let title = line(&e.title, MAX_CARD_NAME * 2);
                if title.is_empty() || e.start_ms == 0 {
                    return None;
                }
                CardItem::Event(EventCard {
                    title,
                    start_ms: e.start_ms,
                    end_ms: if e.end_ms >= e.start_ms { e.end_ms } else { 0 },
                    all_day: e.all_day,
                    place: text(&e.place, MAX_CARD_TEXT),
                    note: text(&e.note, MAX_CARD_TEXT),
                })
            }
            CardItem::Secret(s) => {
                self.flags |= CARD_NO_FORWARD;
                let fields: Vec<SecretField> = s
                    .fields
                    .iter()
                    .map(|f| SecretField {
                        name: line(&f.name, 40).to_lowercase(),
                        value: text(&f.value, MAX_CARD_VALUE),
                        hidden: f.hidden,
                    })
                    .filter(|f| !f.name.is_empty() && !f.value.is_empty())
                    .take(MAX_SECRET_FIELDS)
                    .collect();
                let title = line(&s.title, MAX_CARD_NAME);
                if title.is_empty() || fields.is_empty() {
                    return None;
                }
                CardItem::Secret(SecretCard {
                    kind: s.kind,
                    title,
                    fields,
                    source: line(&s.source, 32).to_lowercase(),
                })
            }
        };
        Some(self)
    }

    /// What it says to an app without cards, which shows the text alone.
    /// A secret says only that it is one: its contents never go as plain
    /// text.
    pub fn fallback_text(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        match &self.item {
            CardItem::Contact(c) => {
                out.push(format!("👤 {}", c.name));
                out.extend(c.phones.iter().cloned());
                out.extend(c.emails.iter().cloned());
                if !c.address.is_empty() {
                    out.push(c.address.clone());
                }
                if !c.handle.is_empty() {
                    out.push(c.handle.clone());
                }
                if !c.invite.is_empty() {
                    out.push(c.invite.clone());
                }
            }
            CardItem::Wifi(w) => {
                out.push(format!("📶 Wi-Fi: {}", w.ssid));
                if !w.password.is_empty() {
                    out.push(format!("Password: {}", w.password));
                }
            }
            CardItem::Location(l) => {
                if !l.label.is_empty() {
                    out.push(format!("📍 {}", l.label));
                }
                out.push(format!(
                    "{}{}, {}",
                    if l.label.is_empty() { "📍 " } else { "" },
                    degrees(l.lat_e7),
                    degrees(l.lon_e7)
                ));
            }
            CardItem::Event(e) => {
                out.push(format!("📅 {}", e.title));
                out.push(utc(e.start_ms, e.all_day));
                if !e.place.is_empty() {
                    out.push(e.place.clone());
                }
            }
            CardItem::Secret(s) => {
                out.push(format!(
                    "🔒 {} (sent securely: update Threnody to open it)",
                    s.title
                ));
            }
        }
        if self.view_once() && !self.is_secret() {
            return "👁 A card you can view once (update Threnody to open it)".into();
        }
        out.join("\n")
    }

    /// Our own copy of a card we sent: a secret or a view-once card keeps
    /// only what it was, not what it held, so it can't be read from our
    /// history later.
    pub fn sender_copy(&self) -> Self {
        if self.needs_cards() {
            self.emptied()
        } else {
            self.clone()
        }
    }

    /// The card with its contents gone (a view-once card once opened):
    /// a secret keeps its kind and title, the rest only their kind.
    pub fn emptied(&self) -> Self {
        let item = match &self.item {
            CardItem::Secret(s) => CardItem::Secret(SecretCard {
                kind: s.kind,
                title: s.title.clone(),
                fields: Vec::new(),
                source: s.source.clone(),
            }),
            CardItem::Contact(_) => CardItem::Contact(ContactCard::default()),
            CardItem::Wifi(_) => CardItem::Wifi(WifiCard::default()),
            CardItem::Location(_) => CardItem::Location(LocationCard::default()),
            CardItem::Event(_) => CardItem::Event(EventCard::default()),
        };
        Self {
            item,
            flags: self.flags | CARD_OPENED,
            expires_in_s: self.expires_in_s,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(256, |e| {
            let flags = self.flags;
            e.map_len(2 + usize::from(flags != 0) + usize::from(self.expires_in_s != 0))?;
            let kind = match &self.item {
                CardItem::Contact(_) => KIND_CONTACT,
                CardItem::Wifi(_) => KIND_WIFI,
                CardItem::Location(_) => KIND_LOCATION,
                CardItem::Event(_) => KIND_EVENT,
                CardItem::Secret(_) => KIND_SECRET,
            };
            e.u8(0)?.uint(kind)?;
            if flags != 0 {
                e.u8(1)?.uint(flags)?;
            }
            if self.expires_in_s != 0 {
                e.u8(2)?.u32(self.expires_in_s)?;
            }
            e.u8(3)?;
            match &self.item {
                CardItem::Contact(c) => {
                    let n = 1
                        + usize::from(!c.invite.is_empty())
                        + usize::from(!c.handle.is_empty())
                        + usize::from(!c.phones.is_empty())
                        + usize::from(!c.emails.is_empty())
                        + usize::from(!c.address.is_empty())
                        + usize::from(!c.note.is_empty());
                    e.map_len(n)?.u8(0)?.str(&c.name)?;
                    if !c.invite.is_empty() {
                        e.u8(1)?.str(&c.invite)?;
                    }
                    if !c.handle.is_empty() {
                        e.u8(2)?.str(&c.handle)?;
                    }
                    for (k, list) in [(3u8, &c.phones), (4, &c.emails)] {
                        if !list.is_empty() {
                            e.u8(k)?.array_len(list.len())?;
                            for s in list {
                                e.str(s)?;
                            }
                        }
                    }
                    if !c.address.is_empty() {
                        e.u8(5)?.str(&c.address)?;
                    }
                    if !c.note.is_empty() {
                        e.u8(6)?.str(&c.note)?;
                    }
                }
                CardItem::Wifi(w) => {
                    e.map_len(2 + usize::from(!w.password.is_empty()) + usize::from(w.hidden))?;
                    e.u8(0)?.str(&w.ssid)?;
                    if !w.password.is_empty() {
                        e.u8(1)?.str(&w.password)?;
                    }
                    e.u8(2)?.uint(match w.security {
                        WifiSecurity::Open => 0,
                        WifiSecurity::Wpa => 1,
                        WifiSecurity::Wep => 2,
                    })?;
                    if w.hidden {
                        e.u8(3)?.bool(true)?;
                    }
                }
                CardItem::Location(l) => {
                    e.map_len(
                        2 + usize::from(!l.label.is_empty()) + usize::from(l.accuracy_m != 0),
                    )?;
                    e.u8(0)?.i32(l.lat_e7)?;
                    e.u8(1)?.i32(l.lon_e7)?;
                    if !l.label.is_empty() {
                        e.u8(2)?.str(&l.label)?;
                    }
                    if l.accuracy_m != 0 {
                        e.u8(3)?.u32(l.accuracy_m)?;
                    }
                }
                CardItem::Event(v) => {
                    e.map_len(
                        2 + usize::from(v.end_ms != 0)
                            + usize::from(v.all_day)
                            + usize::from(!v.place.is_empty())
                            + usize::from(!v.note.is_empty()),
                    )?;
                    e.u8(0)?.str(&v.title)?;
                    e.u8(1)?.uint(v.start_ms)?;
                    if v.end_ms != 0 {
                        e.u8(2)?.uint(v.end_ms)?;
                    }
                    if v.all_day {
                        e.u8(3)?.bool(true)?;
                    }
                    if !v.place.is_empty() {
                        e.u8(4)?.str(&v.place)?;
                    }
                    if !v.note.is_empty() {
                        e.u8(5)?.str(&v.note)?;
                    }
                }
                CardItem::Secret(s) => {
                    e.map_len(3 + usize::from(!s.source.is_empty()))?;
                    e.u8(0)?.uint(match s.kind {
                        SecretKind::Other => 0,
                        SecretKind::Login => 1,
                        SecretKind::PaymentCard => 2,
                        SecretKind::Note => 3,
                    })?;
                    e.u8(1)?.str(&s.title)?;
                    e.u8(2)?.array_len(s.fields.len())?;
                    for f in &s.fields {
                        e.array_len(3)?
                            .str(&f.name)?
                            .str(&f.value)?
                            .bool(f.hidden)?;
                    }
                    if !s.source.is_empty() {
                        e.u8(3)?.str(&s.source)?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Decodes a card as sent (or as kept, with [`CARD_OPENED`]); not yet
    /// [`Card::clean`]ed. An unknown kind is an error: the text stands
    /// alone.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = const_cbor::Decoder::new(b);
        let (mut kind, mut flags, mut expires, mut item) = (None, 0, 0, None);
        cbor::read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u64()?),
                1 => flags = d.u64()?,
                2 => expires = d.u32()?,
                3 => {
                    let k = kind.ok_or(Error::Malformed("card item before its kind"))?;
                    item = Some(decode_item(k, d)?);
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        cbor::finish(&dec)?;
        Ok(Self {
            item: cbor::required(item, "card item")?,
            flags,
            expires_in_s: expires,
        })
    }
}

fn decode_item(kind: u64, d: &mut const_cbor::Decoder<'_>) -> Result<CardItem> {
    let strs = |d: &mut const_cbor::Decoder<'_>| -> Result<Vec<String>> {
        let n = d.array_len()?;
        if n > 64 {
            return Err(Error::Malformed("card list too long"));
        }
        (0..n).map(|_| Ok(d.str()?.to_owned())).collect()
    };
    Ok(match kind {
        KIND_CONTACT => {
            let mut c = ContactCard::default();
            cbor::read_map(d, |k, d| {
                match k {
                    0 => c.name = d.str()?.to_owned(),
                    1 => c.invite = d.str()?.to_owned(),
                    2 => c.handle = d.str()?.to_owned(),
                    3 => c.phones = strs(d)?,
                    4 => c.emails = strs(d)?,
                    5 => c.address = d.str()?.to_owned(),
                    6 => c.note = d.str()?.to_owned(),
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            CardItem::Contact(c)
        }
        KIND_WIFI => {
            let mut w = WifiCard::default();
            cbor::read_map(d, |k, d| {
                match k {
                    0 => w.ssid = d.str()?.to_owned(),
                    1 => w.password = d.str()?.to_owned(),
                    2 => {
                        w.security = match d.u64()? {
                            0 => WifiSecurity::Open,
                            2 => WifiSecurity::Wep,
                            _ => WifiSecurity::Wpa,
                        }
                    }
                    3 => w.hidden = d.bool()?,
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            CardItem::Wifi(w)
        }
        KIND_LOCATION => {
            let mut l = LocationCard::default();
            cbor::read_map(d, |k, d| {
                match k {
                    0 => l.lat_e7 = d.i32()?,
                    1 => l.lon_e7 = d.i32()?,
                    2 => l.label = d.str()?.to_owned(),
                    3 => l.accuracy_m = d.u32()?,
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            CardItem::Location(l)
        }
        KIND_EVENT => {
            let mut v = EventCard::default();
            cbor::read_map(d, |k, d| {
                match k {
                    0 => v.title = d.str()?.to_owned(),
                    1 => v.start_ms = d.u64()?,
                    2 => v.end_ms = d.u64()?,
                    3 => v.all_day = d.bool()?,
                    4 => v.place = d.str()?.to_owned(),
                    5 => v.note = d.str()?.to_owned(),
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            CardItem::Event(v)
        }
        KIND_SECRET => {
            let mut s = SecretCard::default();
            cbor::read_map(d, |k, d| {
                match k {
                    0 => {
                        s.kind = match d.u64()? {
                            1 => SecretKind::Login,
                            2 => SecretKind::PaymentCard,
                            3 => SecretKind::Note,
                            _ => SecretKind::Other,
                        }
                    }
                    1 => s.title = d.str()?.to_owned(),
                    2 => {
                        let n = d.array_len()?;
                        if n > 64 {
                            return Err(Error::Malformed("too many secret fields"));
                        }
                        for _ in 0..n {
                            if d.array_len()? != 3 {
                                return Err(Error::Malformed("secret field"));
                            }
                            s.fields.push(SecretField {
                                name: d.str()?.to_owned(),
                                value: d.str()?.to_owned(),
                                hidden: d.bool()?,
                            });
                        }
                    }
                    3 => s.source = d.str()?.to_owned(),
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            CardItem::Secret(s)
        }
        _ => return Err(Error::Malformed("unknown card kind")),
    })
}

/// Bidirectional overrides and isolates, which could make a card read
/// differently from what it says.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// `s` on one line: words joined by single spaces, without control
/// characters, at most `max` characters.
fn line(s: &str, max: usize) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control() && !is_bidi_control(*c))
        .take(max)
        .collect()
}

/// `s` trimmed, keeping its line breaks, without other control
/// characters, at most `max` characters.
fn text(s: &str, max: usize) -> String {
    s.trim()
        .chars()
        .filter(|&c| c == '\n' || (!c.is_control() && !is_bidi_control(c)))
        .take(max)
        .collect()
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn cut_bytes(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn degrees(e7: i32) -> String {
    format!("{:.6}", f64::from(e7) / 1e7)
}

/// A UTC date (and time, unless `all_day`) for the fallback text.
fn utc(ms: u64, all_day: bool) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil(days);
    if all_day {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        let t = secs % 86_400;
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
            t / 3600,
            t % 3600 / 60
        )
    }
}

/// The calendar date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(c: &Card) -> Card {
        Card::decode(&c.encode().unwrap()).unwrap()
    }

    #[test]
    fn every_kind_survives_the_wire() {
        let cards = [
            Card::new(CardItem::Contact(ContactCard {
                name: "Ada".into(),
                phones: vec!["+1 555 0100".into()],
                emails: vec!["ada@example.org".into()],
                address: "1 Main St\nSpringfield".into(),
                note: "plumber".into(),
                ..Default::default()
            })),
            Card::new(CardItem::Wifi(WifiCard {
                ssid: "Home".into(),
                password: "correct horse".into(),
                security: WifiSecurity::Wpa,
                hidden: true,
            })),
            Card::new(CardItem::Location(LocationCard {
                lat_e7: -337_000_000,
                lon_e7: 1_512_000_000,
                label: "Café".into(),
                accuracy_m: 12,
            })),
            Card::new(CardItem::Event(EventCard {
                title: "Dinner".into(),
                start_ms: 1_795_000_000_000,
                end_ms: 1_795_003_600_000,
                all_day: false,
                place: "Ours".into(),
                note: String::new(),
            })),
            Card {
                item: CardItem::Secret(SecretCard {
                    kind: SecretKind::PaymentCard,
                    title: "Visa".into(),
                    fields: vec![
                        SecretField {
                            name: "number".into(),
                            value: "4111 1111 1111 1111".into(),
                            hidden: true,
                        },
                        SecretField {
                            name: "expiry".into(),
                            value: "12/29".into(),
                            hidden: false,
                        },
                    ],
                    source: "symbolon".into(),
                }),
                flags: CARD_VIEW_ONCE,
                expires_in_s: 3600,
            },
        ];
        for c in cards {
            let back = round(&c);
            assert_eq!(back, c);
            let clean = back.clean().unwrap();
            if clean.is_secret() {
                assert_eq!(clean.flags, CARD_VIEW_ONCE | CARD_NO_FORWARD);
            } else {
                assert_eq!(clean, c);
            }
        }
    }

    #[test]
    fn cleaning_drops_what_cant_be_shown_safely() {
        // No name, or no way to reach them: not a contact card.
        let c = |name: &str, phone: &str| {
            Card::new(CardItem::Contact(ContactCard {
                name: name.into(),
                phones: vec![phone.into()],
                ..Default::default()
            }))
            .clean()
        };
        assert!(c("", "1").is_none());
        assert!(c("Ada", " ").is_none());
        // Overrides that reverse text, and line breaks in a name, go.
        let got = c("A\u{202e}da\nLovelace", "1").unwrap();
        let CardItem::Contact(got) = got.item else {
            panic!()
        };
        assert_eq!(got.name, "Ada Lovelace");
        // An invite link that isn't one is dropped, not shown.
        let card = Card::new(CardItem::Contact(ContactCard {
            name: "Bob".into(),
            invite: "https://evil.example/".into(),
            phones: vec!["2".into()],
            ..Default::default()
        }))
        .clean()
        .unwrap();
        let CardItem::Contact(got) = card.item else {
            panic!()
        };
        assert!(got.invite.is_empty());
        // Off the globe.
        assert!(
            Card::new(CardItem::Location(LocationCard {
                lat_e7: 910_000_000,
                ..Default::default()
            }))
            .clean()
            .is_none()
        );
        // A secret with nothing in it.
        assert!(
            Card::new(CardItem::Secret(SecretCard {
                title: "x".into(),
                ..Default::default()
            }))
            .clean()
            .is_none()
        );
        // Flags only an app keeps locally can't be sent.
        let mut w = Card::new(CardItem::Wifi(WifiCard {
            ssid: "x".repeat(40),
            ..Default::default()
        }));
        w.flags = CARD_OPENED | CARD_VIEW_ONCE;
        let w = w.clean().unwrap();
        assert_eq!(w.flags, CARD_VIEW_ONCE);
        let CardItem::Wifi(w) = w.item else { panic!() };
        assert_eq!(w.ssid.len(), MAX_SSID_BYTES);
    }

    #[test]
    fn secrets_never_go_as_plain_text_nor_stay_with_the_sender() {
        let s = Card::new(CardItem::Secret(SecretCard {
            kind: SecretKind::Login,
            title: "Bank".into(),
            fields: vec![SecretField {
                name: "password".into(),
                value: "hunter2".into(),
                hidden: true,
            }],
            source: String::new(),
        }))
        .clean()
        .unwrap();
        assert!(s.needs_cards() && !s.forwardable());
        assert!(!s.fallback_text().contains("hunter2"));
        let mine = s.sender_copy();
        assert!(!mine.encode().unwrap().windows(7).any(|w| w == b"hunter2"));
        let CardItem::Secret(m) = &mine.item else {
            panic!()
        };
        assert_eq!(m.title, "Bank");
        assert!(mine.opened());
        // A plain card stays as it was.
        let w = Card::new(CardItem::Wifi(WifiCard {
            ssid: "Home".into(),
            password: "pw".into(),
            ..Default::default()
        }));
        assert_eq!(w.sender_copy(), w);
        assert!(w.fallback_text().contains("Password: pw"));
    }

    #[test]
    fn fallback_dates_read_in_utc() {
        assert_eq!(utc(0, false), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400_000, true), "2000-02-29");
        assert_eq!(utc(1_795_000_000_000, false), "2026-11-18 11:06 UTC");
    }
}
