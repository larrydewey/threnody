//! Lists (see `threnody_core::lists`): checklists on all our devices,
//! shareable with a contact or a group, where everyone can change them.

use threnody_core::lists::{List, ListId, MAX_INDENT, MAX_TITLE};

use crate::{Result, ThrenodyNode, fail};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ListItemInfo {
    pub id: u64,
    /// Inline markdown.
    pub text: String,
    pub done: bool,
    /// A section heading rather than something to tick.
    pub heading: bool,
    pub indent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ListInfo {
    /// Hex id.
    pub id: String,
    pub title: String,
    /// In order.
    pub items: Vec<ListItemInfo>,
    /// The chats it's shared with (chat keys, as folders name them).
    pub shared_with: Vec<String>,
    /// When it last changed, by anyone.
    pub updated_ms: u64,
}

fn hex(id: &ListId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<ListId> {
    let b: Vec<u8> = (0..s.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    b.try_into().map_err(|_| fail("not a list id"))
}

pub(crate) fn list_hex(id: &[u8; 16]) -> String {
    hex(id)
}

impl ThrenodyNode {
    fn list_info(&self, l: &threnody_net::lists::SharedList) -> ListInfo {
        let items = l.list.items();
        ListInfo {
            id: hex(&l.list.id),
            title: l.list.title.clone(),
            updated_ms: l
                .list
                .items
                .iter()
                .map(|i| i.stamp.ms)
                .chain([l.list.title_stamp.ms])
                .max()
                .unwrap_or(0),
            items: items
                .iter()
                .map(|i| ListItemInfo {
                    id: i.id,
                    text: i.text.clone(),
                    done: i.done,
                    heading: i.heading,
                    indent: i.indent,
                })
                .collect(),
            shared_with: l.shared.iter().map(|c| self.chat_key(*c)).collect(),
        }
    }

    /// Changes a list and sends it on, to the groups it's shared with too.
    fn change_list(
        &self,
        id: &str,
        f: impl FnOnce(&mut List, threnody_core::lists::Stamp) -> bool,
    ) -> Result<bool> {
        let id = unhex(id)?;
        let _guard = self.rt.enter();
        let Some((bytes, groups)) = self.node.edit_list(&id, f) else {
            return Ok(false);
        };
        self.list_to_groups(bytes, &groups);
        Ok(true)
    }

    fn list_to_groups(&self, bytes: Vec<u8>, groups: &[[u8; 16]]) {
        for g in groups {
            let _ = self.group_node().send_list(&self.node, g, bytes.clone());
        }
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Every list, most recently changed first.
    pub fn lists(&self) -> Vec<ListInfo> {
        let mut v: Vec<ListInfo> = self
            .node
            .lists()
            .iter()
            .map(|l| self.list_info(l))
            .collect();
        v.sort_by_key(|l| std::cmp::Reverse(l.updated_ms));
        v
    }

    pub fn list(&self, id: String) -> Option<ListInfo> {
        let id = unhex(&id).ok()?;
        self.node.list(&id).map(|l| self.list_info(&l))
    }

    /// Starts a list (on all our devices); returns its id.
    pub fn create_list(&self, title: String) -> Result<String> {
        if title.trim().is_empty() {
            return Err(fail("a list needs a name"));
        }
        let _guard = self.rt.enter();
        Ok(hex(&self.node.create_list(&title)))
    }

    pub fn rename_list(&self, id: String, title: String) -> Result<()> {
        let title: String = title.trim().chars().take(MAX_TITLE).collect();
        if title.is_empty() {
            return Err(fail("a list needs a name"));
        }
        self.change_list(&id, |l, s| {
            l.title = title;
            l.title_stamp = s;
            true
        })?;
        Ok(())
    }

    /// Deletes a list on all our devices; whoever it's shared with keeps theirs.
    pub fn delete_list(&self, id: String) -> Result<()> {
        let id = unhex(&id)?;
        let _guard = self.rt.enter();
        self.node.delete_list(&id);
        Ok(())
    }

    /// Adds an item after `after` (none: at the top); returns its id.
    pub fn add_list_item(
        &self,
        id: String,
        after: Option<u64>,
        text: String,
        heading: bool,
    ) -> Result<u64> {
        let mut new = 0;
        self.change_list(&id, |l, s| {
            new = l.add(after, text.trim(), heading, s);
            true
        })?;
        Ok(new)
    }

    pub fn edit_list_item(&self, id: String, item: u64, text: String) -> Result<()> {
        self.change_list(&id, |l, s| {
            l.change(item, s, |i| i.text = text.trim().to_owned())
        })?;
        Ok(())
    }

    pub fn set_list_item_done(&self, id: String, item: u64, done: bool) -> Result<()> {
        self.change_list(&id, |l, s| l.change(item, s, |i| i.done = done))?;
        Ok(())
    }

    /// Moves an item to just after `after` (none: the top).
    pub fn move_list_item(&self, id: String, item: u64, after: Option<u64>) -> Result<()> {
        self.change_list(&id, |l, s| {
            if after == Some(item) {
                return false;
            }
            // Its own place doesn't count when working out the new one.
            let mut without = l.clone();
            without.items.retain(|i| i.id != item);
            let pos = without.pos_after(after);
            l.change(item, s, |i| i.pos = pos)
        })?;
        Ok(())
    }

    pub fn indent_list_item(&self, id: String, item: u64, indent: u8) -> Result<()> {
        self.change_list(&id, |l, s| {
            l.change(item, s, |i| i.indent = indent.min(MAX_INDENT))
        })?;
        Ok(())
    }

    pub fn delete_list_item(&self, id: String, item: u64) -> Result<()> {
        self.change_list(&id, |l, s| l.change(item, s, |i| i.deleted = true))?;
        Ok(())
    }

    /// Deletes every ticked item; returns how many.
    pub fn clear_done_items(&self, id: String) -> Result<u32> {
        let mut n = 0;
        self.change_list(&id, |l, s| {
            let done: Vec<u64> = l
                .items()
                .iter()
                .filter(|i| i.done && !i.heading)
                .map(|i| i.id)
                .collect();
            for d in &done {
                l.change(*d, s, |i| i.deleted = true);
            }
            n = done.len() as u32;
            n > 0
        })?;
        Ok(n)
    }

    /// Adds markdown lines (`- [ ] item`, `- [x] done`, `# heading`, or
    /// plain lines) at the end; returns how many items.
    pub fn add_list_markdown(&self, id: String, markdown: String) -> Result<u32> {
        let mut n = 0;
        self.change_list(&id, |l, s| {
            n = l.add_markdown(&markdown, s) as u32;
            n > 0
        })?;
        Ok(n)
    }

    /// The list as markdown, to copy anywhere.
    pub fn list_markdown(&self, id: String) -> Result<String> {
        let id = unhex(&id)?;
        self.node
            .list(&id)
            .map(|l| l.list.to_markdown())
            .ok_or_else(|| fail("no such list"))
    }

    /// Shares a list with a chat (a contact, a group): everyone there can
    /// change it, and the chat shows that it was shared.
    pub fn share_list(&self, id: String, chat: String) -> Result<()> {
        let lid = unhex(&id)?;
        let conv = self.chat_conv(&chat)?;
        let _guard = self.rt.enter();
        if let Some((bytes, groups)) = self.node.share_list(&lid, conv) {
            self.list_to_groups(bytes, &groups);
        }
        Ok(())
    }

    /// Stops sending changes to a chat (they keep their copy).
    pub fn unshare_list(&self, id: String, chat: String) -> Result<()> {
        let lid = unhex(&id)?;
        let conv = self.chat_conv(&chat)?;
        self.node.unshare_list(&lid, conv);
        Ok(())
    }
}
