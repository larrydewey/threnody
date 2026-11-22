//! Cards (Appendix V): a contact, a Wi-Fi network, a place, an event or a
//! secret sent as a message the app draws as a card.
//!
//! A card goes with `send_card`, `send_group_card` or `write_note_card`;
//! its text is made from it (what an app without cards shows). History
//! entries carry it as `HistoryEntry::card`, except a view-once card's
//! contents, which only `open_card` (and its group and note forms) give,
//! once.

use threnody_core::card::{
    CARD_NO_FORWARD, CARD_VIEW_ONCE, Card, CardItem, ContactCard, EventCard, LocationCard,
    SecretCard, SecretField, SecretKind, WifiCard, WifiSecurity,
};
use threnody_core::history::ConversationId;

use crate::{Result, ThrenodyNode, entry_at, fail};

/// A card, as the app shows or sends it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CardInfo {
    pub item: CardItemInfo,
    /// The receiver sees it once: `open_card` gives its contents and
    /// erases them.
    pub view_once: bool,
    /// Don't offer to forward it (always true for a secret or a view-once
    /// card).
    pub no_forward: bool,
    /// A view-once card already opened, or our own copy of a secret or
    /// view-once card we sent: only its kind (and a secret's title) is
    /// left.
    pub opened: bool,
    /// Gone this long after it is sent (0: as the chat's timer says; at
    /// most a week).
    pub expires_in_s: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CardItemInfo {
    /// Someone to get in touch with: on Threnody (`invite`, a
    /// `threnody://` link, or `handle`), or by phone, email or post.
    /// Empty strings and lists mean none.
    Contact {
        name: String,
        invite: String,
        handle: String,
        phones: Vec<String>,
        emails: Vec<String>,
        address: String,
        note: String,
    },
    /// `security` is "open", "wpa" (WPA, WPA2 or WPA3 Personal) or "wep".
    Wifi {
        ssid: String,
        password: String,
        security: String,
        hidden: bool,
    },
    /// A place in degrees × 10⁷ (WGS 84); `accuracy_m` 0 when unknown.
    Location {
        latitude_e7: i32,
        longitude_e7: i32,
        label: String,
        accuracy_m: u32,
    },
    /// `end_ms` 0 when it has no end.
    Event {
        title: String,
        start_ms: u64,
        end_ms: u64,
        all_day: bool,
        place: String,
        note: String,
    },
    /// `kind` is "login", "card" (a payment card), "note" or "other".
    /// Field names apps know: username, password, url, otp, number,
    /// holder, expiry, cvc, pin, text. `source` names where it came from
    /// ("symbolon"), for offering to save it back there.
    Secret {
        kind: String,
        title: String,
        fields: Vec<SecretFieldInfo>,
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SecretFieldInfo {
    pub name: String,
    pub value: String,
    /// Shown covered until revealed.
    pub hidden: bool,
}

impl From<&Card> for CardInfo {
    fn from(c: &Card) -> Self {
        let item = match &c.item {
            CardItem::Contact(c) => CardItemInfo::Contact {
                name: c.name.clone(),
                invite: c.invite.clone(),
                handle: c.handle.clone(),
                phones: c.phones.clone(),
                emails: c.emails.clone(),
                address: c.address.clone(),
                note: c.note.clone(),
            },
            CardItem::Wifi(w) => CardItemInfo::Wifi {
                ssid: w.ssid.clone(),
                password: w.password.clone(),
                security: match w.security {
                    WifiSecurity::Open => "open",
                    WifiSecurity::Wpa => "wpa",
                    WifiSecurity::Wep => "wep",
                }
                .into(),
                hidden: w.hidden,
            },
            CardItem::Location(l) => CardItemInfo::Location {
                latitude_e7: l.lat_e7,
                longitude_e7: l.lon_e7,
                label: l.label.clone(),
                accuracy_m: l.accuracy_m,
            },
            CardItem::Event(e) => CardItemInfo::Event {
                title: e.title.clone(),
                start_ms: e.start_ms,
                end_ms: e.end_ms,
                all_day: e.all_day,
                place: e.place.clone(),
                note: e.note.clone(),
            },
            CardItem::Secret(s) => CardItemInfo::Secret {
                kind: match s.kind {
                    SecretKind::Login => "login",
                    SecretKind::PaymentCard => "card",
                    SecretKind::Note => "note",
                    SecretKind::Other => "other",
                }
                .into(),
                title: s.title.clone(),
                fields: s
                    .fields
                    .iter()
                    .map(|f| SecretFieldInfo {
                        name: f.name.clone(),
                        value: f.value.clone(),
                        hidden: f.hidden,
                    })
                    .collect(),
                source: s.source.clone(),
            },
        };
        Self {
            item,
            view_once: c.view_once(),
            no_forward: !c.forwardable(),
            opened: c.opened(),
            expires_in_s: c.expires_in_s,
        }
    }
}

impl From<CardInfo> for Card {
    fn from(c: CardInfo) -> Self {
        let item = match c.item {
            CardItemInfo::Contact {
                name,
                invite,
                handle,
                phones,
                emails,
                address,
                note,
            } => CardItem::Contact(ContactCard {
                name,
                invite,
                handle,
                phones,
                emails,
                address,
                note,
            }),
            CardItemInfo::Wifi {
                ssid,
                password,
                security,
                hidden,
            } => CardItem::Wifi(WifiCard {
                ssid,
                password,
                security: match security.as_str() {
                    "open" => WifiSecurity::Open,
                    "wep" => WifiSecurity::Wep,
                    _ => WifiSecurity::Wpa,
                },
                hidden,
            }),
            CardItemInfo::Location {
                latitude_e7,
                longitude_e7,
                label,
                accuracy_m,
            } => CardItem::Location(LocationCard {
                lat_e7: latitude_e7,
                lon_e7: longitude_e7,
                label,
                accuracy_m,
            }),
            CardItemInfo::Event {
                title,
                start_ms,
                end_ms,
                all_day,
                place,
                note,
            } => CardItem::Event(EventCard {
                title,
                start_ms,
                end_ms,
                all_day,
                place,
                note,
            }),
            CardItemInfo::Secret {
                kind,
                title,
                fields,
                source,
            } => CardItem::Secret(SecretCard {
                kind: match kind.as_str() {
                    "login" => SecretKind::Login,
                    "card" => SecretKind::PaymentCard,
                    "note" => SecretKind::Note,
                    _ => SecretKind::Other,
                },
                title,
                fields: fields
                    .into_iter()
                    .map(|f| SecretField {
                        name: f.name,
                        value: f.value,
                        hidden: f.hidden,
                    })
                    .collect(),
                source,
            }),
        };
        Self {
            item,
            flags: u64::from(c.view_once) * CARD_VIEW_ONCE
                + u64::from(c.no_forward) * CARD_NO_FORWARD,
            expires_in_s: c.expires_in_s,
        }
    }
}

/// What a history entry shows of its card: all of it, but a view-once
/// card someone sent us keeps its contents for `open_card`.
pub(crate) fn shown(c: &Card, outgoing: bool) -> CardInfo {
    if c.view_once() && !c.opened() && !outgoing {
        let mut info = CardInfo::from(&c.emptied());
        info.opened = false;
        info
    } else {
        CardInfo::from(c)
    }
}

/// A card in one line, for a chat list: its icon and what it is ("💳
/// Joint Visa", "📶 Home"), or that it's to be viewed once.
pub(crate) fn summary(c: &CardInfo) -> String {
    if c.view_once && !c.opened {
        return "👁 View once".into();
    }
    match &c.item {
        CardItemInfo::Contact { name, .. } => format!("👤 {name}"),
        CardItemInfo::Wifi { ssid, .. } => format!("📶 {ssid}"),
        CardItemInfo::Location { label, .. } => {
            format!("📍 {}", if label.is_empty() { "Location" } else { label })
        }
        CardItemInfo::Event { title, .. } => format!("📅 {title}"),
        CardItemInfo::Secret { kind, title, .. } => {
            let icon = match kind.as_str() {
                "login" => "🔑",
                "card" => "💳",
                _ => "🔒",
            };
            format!("{icon} {title}")
        }
    }
}

/// The text a card would send: what an app without cards shows, and the
/// one-line summary for a chat list. Empty if it isn't a card.
#[uniffi::export]
pub fn card_text(card: CardInfo) -> String {
    Card::from(card)
        .clean()
        .map(|c| c.fallback_text())
        .unwrap_or_default()
}

#[uniffi::export]
impl ThrenodyNode {
    /// Sends `card` to `peer`, as `send_text` sends text; returns how
    /// many devices it went to or was kept for. A secret or view-once
    /// card fails unless every device of theirs shows cards.
    pub fn send_card(&self, peer: String, card: CardInfo) -> Result<u32> {
        let p = self.resolve(&peer)?;
        self.node.seek(&p);
        self.try_reach(&p);
        let _guard = self.rt.enter();
        let extras = threnody_net::history::Extras {
            card: Some(card.into()),
            ..Default::default()
        };
        let r = self
            .node
            .send_message_with(&p, "", extras, threnody_net::history::local_id())
            .map_err(fail)?;
        Ok(u32::try_from(r.live + r.sealed + r.queued).unwrap_or(u32::MAX))
    }

    /// Sends `card` to `group`. Secrets and view-once cards go to one
    /// person only, so they fail here.
    pub fn send_group_card(&self, group: String, card: CardInfo) -> Result<()> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        let extras = threnody_net::history::Extras {
            card: Some(card.into()),
            ..Default::default()
        };
        self.group_node()
            .send_text_with(&self.node, &g, "", extras)
            .map_err(fail)
    }

    /// Keeps `card` among the notes to self.
    pub fn write_note_card(&self, card: CardInfo) -> Result<()> {
        let card = Card::from(card)
            .clean()
            .ok_or_else(|| fail("that card is empty or malformed"))?;
        let _guard = self.rt.enter();
        self.node.write_note_card("", None, None, None, Some(card));
        Ok(())
    }

    /// The card of the message at `at_ms` from `device` in the chat with
    /// `peer`, to show in full. A view-once card from them gives its
    /// contents this once and is emptied.
    pub fn open_card(&self, peer: String, at_ms: u64, device: String) -> Result<Option<CardInfo>> {
        let p = self.resolve(&peer)?;
        let conv = self.node.conversation_for(&p);
        Ok(self.open_in(conv, at_ms, &device))
    }

    /// `open_card` for a message in `group`.
    pub fn open_group_card(
        &self,
        group: String,
        at_ms: u64,
        device: String,
    ) -> Result<Option<CardInfo>> {
        let g = self.group_id(&group)?;
        Ok(self.open_in(ConversationId::Group(g), at_ms, &device))
    }

    /// `open_card` for a note to self.
    pub fn open_note_card(&self, at_ms: u64, device: String) -> Option<CardInfo> {
        self.open_in(self.node.notes(), at_ms, &device)
    }
}

impl ThrenodyNode {
    fn open_in(&self, conv: ConversationId, at_ms: u64, device: &str) -> Option<CardInfo> {
        self.node
            .open_card(conv, entry_at(at_ms, device))
            .map(|c| CardInfo::from(&c))
    }
}

/// A payment card's network from its number: "visa", "mastercard",
/// "amex", "discover", "diners", "jcb", "unionpay", or "" when unknown.
#[uniffi::export]
pub fn card_brand(number: String) -> String {
    let d: String = number.chars().filter(char::is_ascii_digit).collect();
    let p = |n: usize| d.get(..n).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let brand = if d.starts_with('4') {
        "visa"
    } else if (51..=55).contains(&p(2)) || (2221..=2720).contains(&p(4)) {
        "mastercard"
    } else if p(2) == 34 || p(2) == 37 {
        "amex"
    } else if p(4) == 6011 || p(2) == 65 || (644..=649).contains(&p(3)) {
        "discover"
    } else if p(2) == 36 || p(2) == 38 || (300..=305).contains(&p(3)) {
        "diners"
    } else if (3528..=3589).contains(&p(4)) {
        "jcb"
    } else if p(2) == 62 {
        "unionpay"
    } else {
        ""
    };
    brand.into()
}

/// A card number in its network's groups ("4111 1111 1111 1111", Amex
/// "3782 822463 10005"), digits only, at most 19.
#[uniffi::export]
pub fn format_card_number(number: String) -> String {
    let d: Vec<char> = number
        .chars()
        .filter(char::is_ascii_digit)
        .take(19)
        .collect();
    let groups: &[usize] = match card_brand(d.iter().collect()).as_str() {
        "amex" => &[4, 6, 5],
        "diners" if d.len() == 14 => &[4, 6, 4],
        _ => &[4, 4, 4, 4, 3],
    };
    let mut out = String::new();
    let mut i = 0;
    for g in groups {
        if i >= d.len() {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(&d[i..(i + g).min(d.len())]);
        i += g;
    }
    out
}

/// Whether a card number could be real: 12 to 19 digits passing the Luhn
/// check, which catches a mistyped digit.
#[uniffi::export]
pub fn card_number_valid(number: String) -> bool {
    let d: Vec<u32> = number.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(12..=19).contains(&d.len())
        || number
            .chars()
            .any(|c| !c.is_ascii_digit() && c != ' ' && c != '-')
    {
        return false;
    }
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            if i % 2 == 1 {
                if x * 2 > 9 { x * 2 - 9 } else { x * 2 }
            } else {
                x
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// An expiry as typed, made "MM/YY" as it goes: "1" stays, "12" becomes
/// "12/", "1229" becomes "12/29"; a first digit over 1 is a month on its
/// own ("4" becomes "04/").
#[uniffi::export]
pub fn format_card_expiry(typed: String) -> String {
    let mut d: String = typed.chars().filter(char::is_ascii_digit).take(4).collect();
    if d.len() == 1 && d.as_bytes()[0] > b'1' {
        d.insert(0, '0');
    }
    match d.len() {
        0 | 1 => d,
        2 if typed.ends_with('/') || typed.len() <= 2 => format!("{d}/"),
        2 => d,
        _ => format!("{}/{}", &d[..2], &d[2..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_numbers_are_recognised_grouped_and_checked() {
        assert_eq!(card_brand("4111 1111 1111 1111".into()), "visa");
        assert_eq!(card_brand("5500005555555559".into()), "mastercard");
        assert_eq!(card_brand("2223000048400011".into()), "mastercard");
        assert_eq!(card_brand("378282246310005".into()), "amex");
        assert_eq!(card_brand("6011111111111117".into()), "discover");
        assert_eq!(card_brand("9".into()), "");
        assert_eq!(
            format_card_number("4111111111111111".into()),
            "4111 1111 1111 1111"
        );
        assert_eq!(
            format_card_number("37828224631000".into()),
            "3782 822463 1000"
        );
        assert_eq!(format_card_number("41a1-1".into()), "4111");
        assert!(card_number_valid("4111 1111 1111 1111".into()));
        assert!(card_number_valid("378282246310005".into()));
        assert!(!card_number_valid("4111 1111 1111 1112".into()));
        assert!(!card_number_valid("4111".into()));
        assert_eq!(format_card_expiry("1".into()), "1");
        assert_eq!(format_card_expiry("4".into()), "04/");
        assert_eq!(format_card_expiry("12".into()), "12/");
        assert_eq!(format_card_expiry("12/".into()), "12/");
        assert_eq!(format_card_expiry("12/2".into()), "12/2");
        assert_eq!(format_card_expiry("1229".into()), "12/29");
    }
}
