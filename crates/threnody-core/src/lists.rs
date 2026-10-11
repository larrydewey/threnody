//! Lists: markdown-style checklists (shopping, gift ideas, …), kept on
//! all our devices and shareable with a contact or a group.
//!
//! Everyone who has a list may change it at once, so a list merges rather
//! than being replaced: each item, and the title, is a register that the
//! latest change wins (ties go to the larger author id). A deleted item
//! stays as a tombstone so the deletion spreads. Items are ordered by a
//! position number; inserting between two takes the middle.

use const_cbor::Decoder;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::error::{Error, Result};

/// Most lists, items per list (tombstones included), and characters in a
/// title or an item.
pub const MAX_LISTS: usize = 200;
pub const MAX_ITEMS: usize = 1000;
pub const MAX_TITLE: usize = 80;
pub const MAX_ITEM_TEXT: usize = 500;
/// Deepest nesting of an item.
pub const MAX_INDENT: u8 = 3;

/// A list's id: random, shared by everyone who has it.
pub type ListId = [u8; 16];

/// When something changed, and who changed it: the larger wins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp {
    pub ms: u64,
    pub by: [u8; 32],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub id: u64,
    /// Inline markdown.
    pub text: String,
    pub done: bool,
    /// A section heading rather than something to tick.
    pub heading: bool,
    pub indent: u8,
    /// Where it goes: items sort by this, then by id.
    pub pos: f64,
    pub deleted: bool,
    pub stamp: Stamp,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct List {
    pub id: ListId,
    pub title: String,
    pub title_stamp: Stamp,
    /// Every item, tombstones too, in no order (see [`List::items`]).
    pub items: Vec<Item>,
}

impl List {
    /// The items to show, in order.
    pub fn items(&self) -> Vec<&Item> {
        let mut v: Vec<&Item> = self.items.iter().filter(|i| !i.deleted).collect();
        v.sort_by(|a, b| a.pos.total_cmp(&b.pos).then(a.id.cmp(&b.id)));
        v
    }

    /// The position for an item going right after `after` (None: first).
    pub fn pos_after(&self, after: Option<u64>) -> f64 {
        let items = self.items();
        let at = after.and_then(|a| items.iter().position(|i| i.id == a));
        let (lo, hi) = match at {
            Some(k) => (Some(items[k].pos), items.get(k + 1).map(|i| i.pos)),
            None => (None, items.first().map(|i| i.pos)),
        };
        match (lo, hi) {
            (Some(l), Some(h)) => (l + h) / 2.0,
            (Some(l), None) => l + 1.0,
            (None, Some(h)) => h - 1.0,
            (None, None) => 0.0,
        }
    }

    /// Changes item `id` with `f`, stamping it; false if there's none.
    pub fn change(&mut self, id: u64, stamp: Stamp, f: impl FnOnce(&mut Item)) -> bool {
        let Some(i) = self.items.iter_mut().find(|i| i.id == id && !i.deleted) else {
            return false;
        };
        f(i);
        i.text = clip(&i.text, MAX_ITEM_TEXT);
        i.indent = i.indent.min(MAX_INDENT);
        i.stamp = stamp;
        true
    }

    /// Adds an item after `after`; returns its id.
    pub fn add(&mut self, after: Option<u64>, text: &str, heading: bool, stamp: Stamp) -> u64 {
        let id = u64::from_le_bytes(crate::crypto::random_bytes()).max(1);
        let pos = self.pos_after(after);
        let indent = after
            .and_then(|a| self.items.iter().find(|i| i.id == a))
            .map_or(0, |i| if heading { 0 } else { i.indent });
        self.items.push(Item {
            id,
            text: clip(text, MAX_ITEM_TEXT),
            done: false,
            heading,
            indent,
            pos,
            deleted: false,
            stamp,
        });
        id
    }

    /// Takes in another copy: the newer of each item and of the title win.
    /// Returns true if anything here changed.
    pub fn merge(&mut self, other: &List) -> bool {
        let mut changed = false;
        if other.title_stamp > self.title_stamp {
            self.title = other.title.clone();
            self.title_stamp = other.title_stamp;
            changed = true;
        }
        for o in &other.items {
            match self.items.iter().position(|i| i.id == o.id) {
                Some(k) if o.stamp > self.items[k].stamp => {
                    self.items[k] = o.clone();
                    changed = true;
                }
                Some(_) => {}
                None if self.items.len() < MAX_ITEMS => {
                    self.items.push(o.clone());
                    changed = true;
                }
                None => {}
            }
        }
        changed
    }

    /// The list as markdown: a heading, then `- [ ]` items.
    pub fn to_markdown(&self) -> String {
        let mut out = format!("# {}\n\n", self.title);
        for i in self.items() {
            let pad = "  ".repeat(usize::from(i.indent));
            if i.heading {
                out.push_str(&format!("\n## {}\n", i.text));
            } else {
                out.push_str(&format!(
                    "{pad}- [{}] {}\n",
                    if i.done { "x" } else { " " },
                    i.text
                ));
            }
        }
        out
    }

    /// Adds markdown lines at the end: `- [ ]`/`- [x]` items, plain
    /// `-`/`*` bullets, `#` headings, and other lines as items. Returns how
    /// many were added.
    pub fn add_markdown(&mut self, md: &str, stamp: Stamp) -> usize {
        let mut last = self.items().last().map(|i| i.id);
        let mut n = 0;
        for line in md.lines() {
            let spaces = line.len() - line.trim_start().len();
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let (text, done, heading) = if let Some(h) = t.strip_prefix('#') {
                (h.trim_start_matches('#').trim(), false, true)
            } else if let Some(r) = t.strip_prefix("- [ ]").or_else(|| t.strip_prefix("* [ ]")) {
                (r.trim(), false, false)
            } else if let Some(r) = t
                .strip_prefix("- [x]")
                .or_else(|| t.strip_prefix("- [X]"))
                .or_else(|| t.strip_prefix("* [x]"))
            {
                (r.trim(), true, false)
            } else if let Some(r) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                (r.trim(), false, false)
            } else {
                (t, false, false)
            };
            if text.is_empty() || self.items.len() >= MAX_ITEMS {
                continue;
            }
            let id = self.add(last, text, heading, stamp);
            self.change(id, stamp, |i| {
                i.done = done;
                i.indent = if heading { 0 } else { (spaces / 2) as u8 };
            });
            last = Some(id);
            n += 1;
        }
        n
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let hint = 128 + self.items.iter().map(|i| i.text.len() + 80).sum::<usize>();
        cbor::to_vec(hint, |e| {
            e.map_len(4)?;
            e.u8(0)?.bytes(&self.id)?;
            e.u8(1)?.str(&self.title)?;
            e.u8(2)?
                .array_len(2)?
                .u64(self.title_stamp.ms)?
                .bytes(&self.title_stamp.by)?;
            e.u8(3)?.array_len(self.items.len())?;
            for i in &self.items {
                let flags =
                    u8::from(i.done) | (u8::from(i.heading) << 1) | (u8::from(i.deleted) << 2);
                e.array_len(7)?
                    .u64(i.id)?
                    .str(if i.deleted { "" } else { &i.text })?
                    .u8(flags)?
                    .u8(i.indent)?
                    .u64(i.pos.to_bits())?
                    .u64(i.stamp.ms)?
                    .bytes(&i.stamp.by)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let mut l = Self::default();
        let mut id = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => id = Some(fixed_bytes::<16>(d)?),
                1 => l.title = clip(d.str()?, MAX_TITLE),
                2 => {
                    if d.array_len()? != 2 {
                        return Err(Error::Malformed("list stamp"));
                    }
                    l.title_stamp = Stamp {
                        ms: d.u64()?,
                        by: fixed_bytes::<32>(d)?,
                    };
                }
                3 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 7 {
                            return Err(Error::Malformed("list item"));
                        }
                        let id = d.u64()?;
                        let text = clip(d.str()?, MAX_ITEM_TEXT);
                        let flags = d.u8()?;
                        let indent = d.u8()?.min(MAX_INDENT);
                        let pos = f64::from_bits(d.u64()?);
                        let stamp = Stamp {
                            ms: d.u64()?,
                            by: fixed_bytes::<32>(d)?,
                        };
                        if l.items.len() < MAX_ITEMS
                            && pos.is_finite()
                            && !l.items.iter().any(|i| i.id == id)
                        {
                            l.items.push(Item {
                                id,
                                text,
                                done: flags & 1 != 0,
                                heading: flags & 2 != 0,
                                deleted: flags & 4 != 0,
                                indent,
                                pos,
                                stamp,
                            });
                        }
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        l.id = required(id, "list id")?;
        Ok(l)
    }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(ms: u64, by: u8) -> Stamp {
        Stamp { ms, by: [by; 32] }
    }

    #[test]
    fn items_order_insert_and_round_trip() {
        let mut l = List {
            id: [1; 16],
            title: "Groceries".into(),
            ..List::default()
        };
        let milk = l.add(None, "milk", false, st(1, 1));
        let eggs = l.add(Some(milk), "eggs", false, st(2, 1));
        let bread = l.add(Some(milk), "bread", false, st(3, 1));
        let top = l.add(None, "Dairy", true, st(4, 1));
        let order: Vec<&str> = l.items().iter().map(|i| i.text.as_str()).collect();
        assert_eq!(order, ["Dairy", "milk", "bread", "eggs"]);
        assert!(l.change(eggs, st(5, 1), |i| i.done = true));
        assert!(l.change(bread, st(6, 1), |i| i.deleted = true));
        assert_eq!(l.items().len(), 3);
        let back = List::decode(&l.encode().unwrap()).unwrap();
        assert_eq!(
            back,
            List {
                items: back.items.clone(),
                ..l.clone()
            }
        );
        assert_eq!(
            back.items().iter().map(|i| i.id).collect::<Vec<_>>(),
            [top, milk, eggs]
        );
    }

    #[test]
    fn concurrent_edits_merge_newest_wins() {
        let mut a = List {
            id: [2; 16],
            ..List::default()
        };
        let x = a.add(None, "socks", false, st(1, 1));
        let mut b = a.clone();
        // Both change it; B's is later. A also adds an item, B deletes x.
        a.change(x, st(5, 1), |i| i.text = "wool socks".into());
        let y = a.add(Some(x), "scarf", false, st(6, 1));
        b.change(x, st(7, 2), |i| i.done = true);
        assert!(a.merge(&b) && b.merge(&a));
        assert_eq!(a, b, "both end the same");
        let xi = a.items.iter().find(|i| i.id == x).unwrap();
        assert!(xi.done && xi.text == "socks", "the later change wins whole");
        assert!(a.items.iter().any(|i| i.id == y));
        assert!(!a.merge(&b.clone()), "nothing new");
        // A deletion spreads.
        b.change(y, st(9, 2), |i| i.deleted = true);
        a.merge(&b);
        assert_eq!(a.items().len(), 1);
    }

    #[test]
    fn markdown_in_and_out() {
        let mut l = List {
            id: [3; 16],
            title: "Christmas".into(),
            ..List::default()
        };
        let n = l.add_markdown(
            "## Mom\n- [ ] scarf\n  - [x] blue one\n* candles\n\nbook",
            st(1, 1),
        );
        assert_eq!(n, 5);
        let items = l.items();
        assert!(items[0].heading && items[0].text == "Mom");
        assert!(items[2].done && items[2].indent == 1);
        assert_eq!(items[4].text, "book");
        let md = l.to_markdown();
        assert!(md.starts_with("# Christmas"));
        assert!(md.contains("  - [x] blue one\n") && md.contains("## Mom"));
    }
}
