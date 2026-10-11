//! `/group` commands: MLS groups on the shared [`GroupNode`], which
//! persists them and delivers their traffic (Appendix F).

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use threnody_core::history::{ConversationId, FileNote};
use threnody_core::store::Home;
use threnody_core::{Identity, PublicIdentity};
use threnody_groups::GroupId;
use threnody_groups::node::{GroupNode, Update};
use threnody_net::Node;

pub const HELP: &str = "\
  /group new <name>                     create a group (you own it)
  /group invite <group> <peer>          add a contact (every device of theirs)
  /group accept [n]                     accept a pending invitation
  /group decline [n]                    decline a pending invitation
  /group remove <group> <peer>          remove a member (owner only)
  /group leave <group>                  leave (or, as owner, delete) a group
  /groups                               list groups and members
  /g <group> <text>                     send to a group
  /ghistory <group> [count]             a group's recent messages, numbered
  /gr <group> <n> <text>                reply to message n of the last /ghistory, quoting it
  /gpoll <group> [-m] <question> | <option> | <option>...
                                        ask a poll (-m: several choices allowed)
  /gvote <group> <n> [choice...]        vote in poll n of the last /ghistory (none: take it back)
  /gfile <group> [-s] <path>... [| caption]
                                        send files (photos) to a group";

pub struct GroupUi {
    groups: GroupNode,
    downloads: PathBuf,
    /// What the last `/ghistory` numbered, for `/gr`.
    shown: Option<(GroupId, Vec<threnody_core::history::Entry>)>,
}

fn short(id: &GroupId) -> String {
    id[..3].iter().map(|b| format!("{b:02x}")).collect()
}

impl GroupUi {
    /// Loads persisted groups for `identity` from `home`.
    pub fn load(home: &Home, identity: &Identity, downloads: PathBuf) -> Result<Self> {
        Ok(Self {
            groups: GroupNode::load(home, identity)?,
            downloads,
            shown: None,
        })
    }

    pub fn count(&self) -> usize {
        self.groups.list().len()
    }

    pub fn label(&self, id: &GroupId) -> String {
        let name = self
            .groups
            .list()
            .into_iter()
            .find(|(g, ..)| g == id)
            .map(|(_, n, ..)| n)
            .or_else(|| {
                self.groups
                    .invites()
                    .iter()
                    .find(|i| i.group == *id)
                    .map(|i| i.name.clone())
            })
            .unwrap_or_default();
        format!("{name}#{}", short(id))
    }

    /// Finds a group by exact name, `name#hex`, or hex id prefix.
    pub fn find(&self, q: &str) -> Result<GroupId> {
        let q = q.trim();
        let hits: Vec<GroupId> = self
            .groups
            .list()
            .into_iter()
            .filter(|(id, name, ..)| {
                let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
                name == q
                    || format!("{name}#{}", short(id)) == q
                    || (q.len() >= 2 && hex.starts_with(q.trim_start_matches('#')))
            })
            .map(|(id, ..)| id)
            .collect();
        match hits.as_slice() {
            [id] => Ok(*id),
            [] => bail!("no group matches {q:?}; see /groups"),
            _ => bail!("{q:?} matches several groups; use name#id"),
        }
    }

    fn show(&self, node: &Node, name: &dyn Fn(&PublicIdentity) -> String, updates: Vec<Update>) {
        for u in updates {
            match u {
                Update::Joined { group, owner, .. } => {
                    println!(
                        "* joined group {} (owner {})",
                        self.label(&group),
                        name(&owner)
                    );
                }
                Update::Invited(i) => println!(
                    "* {} invites you to group {:?}. /group accept {} to join",
                    name(&i.from),
                    i.name,
                    self.groups.invites().len()
                ),
                Update::MemberAdded { group, member } => {
                    println!("* {} joined {}", name(&member), self.label(&group));
                }
                Update::MemberRemoved { group, member } => {
                    println!("* {} left {}", name(&member), self.label(&group));
                }
                Update::RoleChanged {
                    group: _,
                    member,
                    role,
                } => {
                    let role_str = match role {
                        threnody_groups::MemberRole::Owner => "owner",
                        threnody_groups::MemberRole::Admin => "admin",
                        threnody_groups::MemberRole::Member => "member",
                    };
                    println!("* {} is now {}", name(&member), role_str);
                }
                Update::Left { group } => {
                    println!("* you were removed from group #{}", short(&group));
                }
                Update::Text {
                    group,
                    from,
                    text,
                    ours: true,
                    ..
                } => {
                    println!("[{}] <me, on {}> {text}", self.label(&group), name(&from));
                }
                Update::Text {
                    group,
                    from,
                    text,
                    mentioned,
                    ..
                } => {
                    if mentioned {
                        print!("\x07");
                    }
                    println!("[{}] <{}> {text}", self.label(&group), name(&from));
                }
                Update::File {
                    group,
                    from,
                    name: file,
                    data,
                    ours,
                    sensitive,
                    caption,
                    album,
                    id,
                    clip,
                } => {
                    let who = if ours {
                        format!("me, on {}", name(&from))
                    } else {
                        name(&from)
                    };
                    let saved = crate::chat::save_download(&self.downloads, &file, &data);
                    let mark = crate::chat::file_mark(sensitive, clip);
                    match &saved {
                        Ok(p) => println!(
                            "[{}] <{who}> sent {file}{mark} ({} bytes) -> {}",
                            self.label(&group),
                            data.len(),
                            p.display()
                        ),
                        Err(e) => println!("! could not save {file} from {who}: {e:#}"),
                    }
                    if !caption.is_empty() {
                        println!("[{}] <{who}> {caption}", self.label(&group));
                    }
                    let note = FileNote {
                        name: file,
                        size: data.len() as u64,
                        location: saved.ok().map(|p| p.display().to_string()),
                        sensitive,
                        album,
                        clip,
                    };
                    self.groups
                        .record_file(node, &group, &from, note, &caption, id);
                }
                Update::Reacted { group, from } => {
                    println!("[{}] * {} reacted", self.label(&group), name(&from));
                }
                Update::Voted { group, from } => {
                    println!("[{}] * {} voted", self.label(&group), name(&from));
                }
                Update::Pinned { group, from, pin } => println!(
                    "[{}] * {} {} a message",
                    self.label(&group),
                    name(&from),
                    if pin { "pinned" } else { "unpinned" }
                ),
            }
        }
    }

    /// Handles an incoming `AppMessage::Group` payload from `peer`.
    pub fn incoming(
        &mut self,
        node: &Node,
        name: &dyn Fn(&PublicIdentity) -> String,
        peer: PublicIdentity,
        payload: &[u8],
    ) {
        match self.groups.incoming(node, peer, payload) {
            Ok(updates) => self.show(node, name, updates),
            Err(e) => println!("! group message from {}: {e}", name(&peer)),
        }
    }

    /// Adds our newly linked `device` to our groups.
    pub fn linked(&mut self, node: &Node, device: &PublicIdentity) {
        self.groups.linked(node, device);
    }

    /// Sends group messages held for `peer`, who just connected.
    pub fn connected(
        &mut self,
        node: &Node,
        name: &dyn Fn(&PublicIdentity) -> String,
        peer: &PublicIdentity,
    ) {
        let held = self.groups.held_for(peer);
        self.groups.connected(node, peer);
        let sent = held - self.groups.held_for(peer);
        if sent > 0 {
            println!("* delivered {sent} held group message(s) to {}", name(peer));
        }
    }

    /// Sends `origin` a receipt for the group message it asked us to
    /// forward to `member`, who has now acknowledged it.
    pub fn relayed(
        &mut self,
        node: &Node,
        member: &PublicIdentity,
        group: &GroupId,
        reference: u64,
        origin: &PublicIdentity,
    ) {
        self.groups.relayed(node, member, group, reference, origin);
    }

    /// Picks pending invitation `n` (1-based; the latest by default).
    fn pending(&self, n: Option<&str>) -> Result<GroupId> {
        let invites = self.groups.invites();
        if invites.is_empty() {
            bail!("no pending invitations");
        }
        let n: usize = n.map_or(Ok(invites.len()), str::parse)?;
        if n == 0 || n > invites.len() {
            bail!("pick 1..={}", invites.len());
        }
        Ok(invites[n - 1].group)
    }

    /// `/group ...` and `/groups`.
    pub fn command(
        &mut self,
        node: &Node,
        name: &dyn Fn(&PublicIdentity) -> String,
        resolve: &dyn Fn(&str) -> Result<PublicIdentity>,
        args: &str,
    ) -> Result<()> {
        let mut it = args.split_whitespace();
        match it.next().unwrap_or("list") {
            "new" => {
                let gname = it.collect::<Vec<_>>().join(" ");
                if gname.is_empty() {
                    bail!("usage: /group new <name>");
                }
                let id = self.groups.create(&gname)?;
                println!(
                    "* created {} — invite with /group invite {} <peer>",
                    self.label(&id),
                    self.label(&id)
                );
            }
            "invite" | "add" => {
                let (g, p) = (it.next(), it.next());
                let (Some(g), Some(p)) = (g, p) else {
                    bail!("usage: /group invite <group> <peer>")
                };
                let (g, p) = (self.find(g)?, resolve(p)?);
                let n = self.groups.invite(node, &g, &p)?;
                println!("* invitation sent to {} ({n} device(s))", name(&p));
            }
            "accept" => {
                let g = self.pending(it.next())?;
                println!("* accepting {}", self.label(&g));
                let updates = self.groups.accept(node, &g)?;
                self.show(node, name, updates);
            }
            "decline" => {
                let g = self.pending(it.next())?;
                println!("* declined {}", self.label(&g));
                self.groups.decline(&g);
            }
            "remove" | "kick" => {
                let (g, p) = (it.next(), it.next());
                let (Some(g), Some(p)) = (g, p) else {
                    bail!("usage: /group remove <group> <peer>")
                };
                let (g, p) = (self.find(g)?, resolve(p)?);
                let updates = self.groups.remove(node, &g, &p)?;
                self.show(node, name, updates);
            }
            "leave" | "delete" => {
                let g = self.find(
                    it.next()
                        .ok_or_else(|| anyhow!("usage: /group leave <group>"))?,
                )?;
                let label = self.label(&g);
                self.groups.leave(node, &g)?;
                println!("* left {label}");
            }
            "list" => self.list(name),
            other => bail!("unknown /group {other}; try /help"),
        }
        Ok(())
    }

    pub fn list(&self, name: &dyn Fn(&PublicIdentity) -> String) {
        let groups = self.groups.list();
        if groups.is_empty() {
            println!("No groups. /group new <name>");
        }
        for (id, _, owner, members) in groups {
            let names: Vec<String> = members
                .iter()
                .map(|m| {
                    let held = self.groups.held_for(m);
                    if held > 0 {
                        format!("{} ({held} held)", name(m))
                    } else {
                        name(m)
                    }
                })
                .collect();
            println!(
                "  {}  owner {}  members: {}",
                self.label(&id),
                name(&owner),
                names.join(", ")
            );
        }
        for (i, inv) in self.groups.invites().iter().enumerate() {
            println!(
                "  invitation {}: {:?} from {}",
                i + 1,
                inv.name,
                name(&inv.from)
            );
        }
    }

    /// `/gfile <group> [-s] <path>... [| caption]`: returns the number of
    /// files and bytes sent.
    pub async fn send_file(&mut self, node: &Node, args: &str) -> Result<(usize, usize)> {
        let (g, rest) = args
            .split_once(' ')
            .ok_or_else(|| anyhow!("usage: /gfile <group> [-s] <path>... [| caption]"))?;
        let g = self.find(g)?;
        let files = crate::chat::read_files(rest).await?;
        let (n, len) = (files.len(), files.iter().map(|f| f.data.len()).sum());
        for f in files {
            self.groups.send_file(node, &g, f)?;
        }
        Ok((n, len))
    }

    /// `/ghistory <group> [count]`: the latest messages, numbered for `/gr`.
    pub fn history(
        &mut self,
        node: &Node,
        name: &dyn Fn(&PublicIdentity) -> String,
        args: &str,
    ) -> Result<()> {
        let mut it = args.split_whitespace();
        let g = self.find(it.next().unwrap_or(""))?;
        let n: usize = it.next().map_or(Ok(20), str::parse)?;
        let h = node.history(ConversationId::Group(g))?;
        let recent = h.recent(n).to_vec();
        for (i, e) in recent.iter().enumerate() {
            let who = if e.outgoing {
                "me".to_owned()
            } else {
                PublicIdentity::from_bytes(&e.device).map_or_else(|_| "?".into(), |d| name(&d))
            };
            if let Some(q) = &e.quote {
                println!("     {}", crate::chat::quoted(q, name));
            }
            println!(
                "{:>3}  [{}] <{who}> {}{}",
                i + 1,
                crate::chat::clock(e.at_ms),
                if e.poll.is_some() { "📊 " } else { "" },
                threnody_core::history::snippet(e)
            );
            if let Some(p) = &e.poll {
                for (j, (o, n)) in p.options.iter().zip(p.counts()).enumerate() {
                    println!("       {}. {o} ({n})", j + 1);
                }
                if p.closed {
                    println!("       (closed)");
                }
            }
        }
        self.shown = Some((g, recent));
        Ok(())
    }

    /// `/gr <group> <n> <text>`: replies to message `n` of the last
    /// `/ghistory`, quoting it.
    pub fn reply(&mut self, node: &Node, args: &str) -> Result<()> {
        let mut it = args.splitn(3, ' ');
        let (Some(g), Some(n), Some(text)) = (it.next(), it.next(), it.next()) else {
            bail!("usage: /gr <group> <n> <text>");
        };
        let g = self.find(g)?;
        let n: usize = n
            .parse()
            .map_err(|_| anyhow!("usage: /gr <group> <n> <text>"))?;
        let e = match &self.shown {
            Some((s, shown)) if *s == g => shown.get(n.wrapping_sub(1)).cloned(),
            _ => bail!(
                "run /ghistory {} first; /gr uses its numbers",
                self.label(&g)
            ),
        }
        .ok_or_else(|| anyhow!("no message {n} in the last /ghistory"))?;
        let quote = threnody_core::message::Quote::of(&e);
        self.groups.send_reply(node, &g, text.trim(), Some(quote))?;
        Ok(())
    }

    /// `/gpoll <group> [-m] <question> | <option> | <option>...`
    pub fn poll(&mut self, node: &Node, args: &str) -> Result<()> {
        let usage = "usage: /gpoll <group> [-m] <question> | <option> | <option>...";
        let (g, rest) = args.split_once(' ').ok_or_else(|| anyhow!(usage))?;
        let g = self.find(g)?;
        let (multi, rest) = match rest.trim_start().strip_prefix("-m ") {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let mut parts = rest.split('|').map(str::trim);
        let question = parts.next().unwrap_or("");
        let options: Vec<String> = parts.map(str::to_owned).collect();
        self.groups.send_poll(node, &g, question, options, multi)?;
        Ok(())
    }

    /// `/gvote <group> <n> [choice...]`: choices count from 1.
    pub fn vote(&mut self, node: &Node, args: &str) -> Result<()> {
        let mut it = args.split_whitespace();
        let (Some(g), Some(n)) = (it.next(), it.next()) else {
            bail!("usage: /gvote <group> <n> [choice...]");
        };
        let g = self.find(g)?;
        let n: usize = n.parse()?;
        let choices = it
            .map(|c| c.parse::<u32>().map(|c| c.saturating_sub(1)))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let e = match &self.shown {
            Some((s, shown)) if *s == g => shown.get(n.wrapping_sub(1)).cloned(),
            _ => bail!(
                "run /ghistory {} first; /gvote uses its numbers",
                self.label(&g)
            ),
        }
        .ok_or_else(|| anyhow!("no message {n} in the last /ghistory"))?;
        if e.poll.is_none() {
            bail!("message {n} isn't a poll");
        }
        if !self.groups.vote(node, &g, e.message_id(), &choices)? {
            bail!("nothing changed (or the poll is closed)");
        }
        Ok(())
    }

    /// `/g <group> <text>`.
    pub fn say(&mut self, node: &Node, args: &str) -> Result<()> {
        let (g, text) = args
            .split_once(' ')
            .ok_or_else(|| anyhow!("usage: /g <group> <text>"))?;
        let g = self.find(g)?;
        self.groups.send_text(node, &g, text)?;
        Ok(())
    }
}
