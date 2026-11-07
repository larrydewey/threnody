//! `/group` commands: MLS groups on the shared [`GroupNode`], which
//! persists them and delivers their traffic (Appendix F).

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use threnody_core::history::{ConversationId, FileNote};
use threnody_core::store::Home;
use threnody_core::{Identity, PublicIdentity};
use threnody_groups::node::{GroupNode, Invite, Update};
use threnody_groups::{GroupId, MemberRole};
use threnody_net::Node;

pub const HELP: &str = "\
  /group new <name>                     create a group (you own it)
  /group invite <group> <peer>          add a contact (every device of theirs)
  /group accept [n]                     accept a pending invitation
  /group decline [n]                    decline a pending invitation
  /group remove <group> <peer>          remove a member (owners; admins remove members)
  /group leave <group>                  leave a group (the last owner transfers or deletes)
  /group delete <group>                 delete a group for everyone (owners)
  /group admin <group> <peer>           make a member an admin (owners)
  /group unadmin <group> <peer>         take admin away; your own name: step down
  /group owner <group> <peer>           hand the group over (you become an admin)
  /group successor <group> <peer|none>  who may take over if you go silent (an admin)
  /group claim <group>                  as successor, take over a silent owner's group
  /group coordinate <group>             apply changes from this owner device now
  /groups                               list groups, members and roles
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
                    say!(
                        "* joined group {} (owner {})",
                        self.label(&group),
                        name(&owner)
                    );
                }
                Update::Invited { invite: i, .. } => say!(
                    "* {} invites you to group {:?}. /group accept {} to join",
                    name(&i.from),
                    i.name,
                    self.groups.invites().len()
                ),
                Update::MemberAdded { group, member, by } => {
                    let by = actor(node, &member, by).map(|p| name(&p));
                    say!(
                        "* {}",
                        added_notice(&name(&member), &self.label(&group), by.as_deref())
                    );
                }
                Update::MemberRemoved { group, member, by } => {
                    let by = actor(node, &member, by).map(|p| name(&p));
                    say!(
                        "* {}",
                        removed_notice(&name(&member), &self.label(&group), by.as_deref())
                    );
                }
                Update::ChangeFailed { group, reason } => {
                    say!(
                        "! couldn't apply your change to {}: {reason}",
                        self.label(&group)
                    );
                }
                Update::UpgradePending { group, outdated } => {
                    let names: Vec<String> = outdated.iter().map(name).collect();
                    say!(
                        "* admins can be added to {} once {} update Threnody",
                        self.label(&group),
                        names.join(", ")
                    );
                }
                Update::OutOfStep { group } => say!(
                    "! this device fell out of step with {}; asking the others to bring it back",
                    self.label(&group)
                ),
                Update::RoleChanged {
                    group,
                    member,
                    role,
                } => {
                    let who = (member != node.identity()).then(|| name(&member));
                    say!(
                        "* {}",
                        role_notice(who.as_deref(), role, &self.label(&group))
                    );
                }
                Update::Left { group } => {
                    say!("* you were removed from group #{}", short(&group));
                }
                Update::Text { group, from, .. } if crate::out::private() => {
                    say!(
                        "[{}] <{}> sent a message (/ghistory to read it)",
                        self.label(&group),
                        name(&from)
                    );
                }
                Update::Text {
                    group,
                    from,
                    text,
                    ours: true,
                    ..
                } => {
                    say!("[{}] <me, on {}> {text}", self.label(&group), name(&from));
                }
                Update::Text {
                    group,
                    from,
                    text,
                    mentioned,
                    ..
                } => {
                    if mentioned {
                        say_part!("\x07");
                    }
                    say!("[{}] <{}> {text}", self.label(&group), name(&from));
                }
                Update::File {
                    group,
                    from,
                    name: file,
                    forwarded: _,
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
                        Ok(p) => say!(
                            "[{}] <{who}> sent {file}{mark} ({} bytes) -> {}",
                            self.label(&group),
                            data.len(),
                            p.display()
                        ),
                        Err(e) => say!("! could not save {file} from {who}: {e:#}"),
                    }
                    if !caption.is_empty() && !crate::out::private() {
                        say!("[{}] <{who}> {caption}", self.label(&group));
                    }
                    let note = FileNote {
                        name: file,
                        size: data.len() as u64,
                        location: saved.ok().map(|p| p.display().to_string()),
                        sensitive,
                        album,
                        clip,
                        forwarded: false,
                    };
                    self.groups
                        .record_file(node, &group, &from, note, &caption, id);
                }
                // The command line takes no calls.
                Update::CallLeg { .. } => {}
                Update::Reacted { group, from } => {
                    say!("[{}] * {} reacted", self.label(&group), name(&from));
                }
                Update::Voted { group, from } => {
                    say!("[{}] * {} voted", self.label(&group), name(&from));
                }
                Update::Pinned { group, from, pin } => say!(
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
            Err(e) => say!("! group message from {}: {e}", name(&peer)),
        }
    }

    /// What waited at shutdown (see `GroupNode::resume_all`).
    pub fn resume(&mut self, node: &Node, name: &dyn Fn(&PublicIdentity) -> String) {
        let updates = self.groups.resume_all(node);
        self.show(node, name, updates);
    }

    /// Devices were removed from their accounts: they lose their roles and
    /// their place in our groups, and none of our groups delivers to them
    /// any more.
    pub fn revoked(
        &mut self,
        node: &Node,
        name: &dyn Fn(&PublicIdentity) -> String,
        removed: &[PublicIdentity],
    ) {
        let updates = self.groups.revoked(node, removed);
        self.show(node, name, updates);
    }

    /// Adds our newly linked `device` to our groups.
    pub fn linked(&mut self, node: &Node, device: &PublicIdentity) {
        self.groups.linked(node, device);
    }

    /// Sends lists a contact changed on to the groups they're also shared
    /// with (see `Node::take_list_forwards`).
    pub fn forward_lists(&mut self, node: &Node) {
        for (bytes, g) in node.take_list_forwards() {
            let _ = self.groups.send_list(node, &g, bytes);
        }
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
            say!("* delivered {sent} held group message(s) to {}", name(peer));
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

    /// `q` among `g`'s members (by name or fingerprint prefix), or else a
    /// contact: members needn't be contacts.
    fn member(
        &self,
        g: &GroupId,
        q: &str,
        name: &dyn Fn(&PublicIdentity) -> String,
        resolve: &dyn Fn(&str) -> Result<PublicIdentity>,
    ) -> Result<PublicIdentity> {
        let members = self
            .groups
            .list()
            .into_iter()
            .find(|(id, ..)| id == g)
            .map(|(.., m)| m)
            .unwrap_or_default();
        let hits: Vec<PublicIdentity> = members
            .into_iter()
            .filter(|m| {
                name(m) == q
                    || (q.len() >= 4
                        && threnody_core::identity::fingerprint_matches_prefix(&m.fingerprint(), q))
            })
            .collect();
        match hits.as_slice() {
            [p] => Ok(*p),
            [] => resolve(q),
            _ => bail!("{q:?} matches several members"),
        }
    }

    /// Our changes still waiting for the coordinator, if any.
    fn pending_note(&self, g: &GroupId, name: &dyn Fn(&PublicIdentity) -> String) {
        let n = self.groups.pending_changes(g);
        if n == 0 {
            return;
        }
        if self.groups.is_lead(g) {
            // We coordinate: they wait only for members to update, if at all.
            let outdated = self.groups.outdated(g);
            if !outdated.is_empty() {
                let names: Vec<String> = outdated.iter().map(name).collect();
                say!(
                    "    {n} change(s) waiting for {} to update Threnody",
                    names.join(", ")
                );
            }
            return;
        }
        if let Ok(r) = self.groups.roster(g) {
            say!(
                "    {n} change(s) waiting for {} to come online (/group coordinate to apply them from an owner device)",
                name(&r.lead)
            );
        }
    }

    /// Picks pending invitation `n` (1-based; the latest by default).
    fn pending(&self, n: Option<&str>) -> Result<Invite> {
        let invites = self.groups.invites();
        if invites.is_empty() {
            bail!("no pending invitations");
        }
        let n: usize = n.map_or(Ok(invites.len()), str::parse)?;
        if n == 0 || n > invites.len() {
            bail!("pick 1..={}", invites.len());
        }
        Ok(invites[n - 1].clone())
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
                say!(
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
                say!("* invitation sent to {} ({n} device(s))", name(&p));
            }
            "accept" => {
                let i = self.pending(it.next())?;
                say!("* accepting {}", self.label(&i.group));
                // That inviter's, even if others invite us to the same id.
                let updates = self.groups.accept_from(node, &i.group, &i.from)?;
                self.show(node, name, updates);
            }
            "decline" => {
                let i = self.pending(it.next())?;
                say!("* declined {}", self.label(&i.group));
                self.groups.decline_from(&i.group, &i.from);
            }
            "remove" | "kick" => {
                let (g, p) = (it.next(), it.next());
                let (Some(g), Some(p)) = (g, p) else {
                    bail!("usage: /group remove <group> <peer>")
                };
                let g = self.find(g)?;
                let p = self.member(&g, p, name, resolve)?;
                let updates = self.groups.remove(node, &g, &p)?;
                self.show(node, name, updates);
            }
            "leave" => {
                let g = self.find(
                    it.next()
                        .ok_or_else(|| anyhow!("usage: /group leave <group>"))?,
                )?;
                let label = self.label(&g);
                self.groups.leave(node, &g)?;
                say!("* left {label}");
            }
            "delete" => {
                let g = self.find(
                    it.next()
                        .ok_or_else(|| anyhow!("usage: /group delete <group>"))?,
                )?;
                let label = self.label(&g);
                self.groups.delete(node, &g)?;
                say!("* deleted {label} for everyone");
            }
            cmd @ ("admin" | "unadmin" | "owner" | "successor") => {
                let (g, p) = (it.next(), it.next());
                let (Some(g), Some(p)) = (g, p) else {
                    bail!("usage: /group {cmd} <group> <peer>")
                };
                let g = self.find(g)?;
                let updates = if cmd == "successor" && p == "none" {
                    self.groups.set_successor(node, &g, None)?
                } else {
                    let p = self.member(&g, p, name, resolve)?;
                    match cmd {
                        "admin" => self.groups.promote(node, &g, p)?,
                        "unadmin" => self.groups.demote(node, &g, p)?,
                        "owner" => self.groups.transfer(node, &g, p)?,
                        _ => self.groups.set_successor(node, &g, Some(p))?,
                    }
                };
                self.show(node, name, updates);
                self.pending_note(&g, name);
            }
            "claim" => {
                let g = self.find(
                    it.next()
                        .ok_or_else(|| anyhow!("usage: /group claim <group>"))?,
                )?;
                if !self.groups.can_claim(node, &g, threnody_core::now_ms()) {
                    bail!("only the successor, once the owner has been silent for 14 days");
                }
                let updates = self.groups.claim(node, &g)?;
                self.show(node, name, updates);
            }
            "coordinate" => {
                let g = self.find(
                    it.next()
                        .ok_or_else(|| anyhow!("usage: /group coordinate <group>"))?,
                )?;
                let updates = self.groups.take_over(node, &g)?;
                self.show(node, name, updates);
                say!("* this device now applies changes to {}", self.label(&g));
            }
            "list" => self.list(name),
            other => bail!("unknown /group {other}; try /help"),
        }
        Ok(())
    }

    pub fn list(&self, name: &dyn Fn(&PublicIdentity) -> String) {
        let groups = self.groups.list();
        if groups.is_empty() {
            say!("No groups. /group new <name>");
        }
        for (id, _, lead, members) in groups {
            let roster = self.groups.roster(&id).ok();
            let names: Vec<String> = members
                .iter()
                .map(|m| {
                    let mut s = name(m);
                    if let Some(r) = &roster {
                        match r.role(m) {
                            MemberRole::Member => {}
                            role => s.push_str(&format!(" ({})", role_name(role))),
                        }
                        if r.successors.contains(m) {
                            s.push_str(" (successor)");
                        }
                    }
                    let held = self.groups.held_for(m);
                    if held > 0 {
                        s.push_str(&format!(" ({held} held)"));
                    }
                    s
                })
                .collect();
            say!(
                "  {}  coordinated by {}  members: {}",
                self.label(&id),
                name(&lead),
                names.join(", ")
            );
            self.pending_note(&id, name);
        }
        for (i, inv) in self.groups.invites().iter().enumerate() {
            say!(
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
                say!("     {}", crate::chat::quoted(q, name));
            }
            say!(
                "{:>3}  [{}] <{who}> {}{}",
                i + 1,
                crate::chat::clock(e.at_ms),
                if e.poll.is_some() { "📊 " } else { "" },
                threnody_core::history::snippet(e)
            );
            if let Some(p) = &e.poll {
                for (j, (o, n)) in p.options.iter().zip(p.counts()).enumerate() {
                    say!("       {}. {o} ({n})", j + 1);
                }
                if p.closed {
                    say!("       (closed)");
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

/// Who made a membership change of `member`'s, as its notice names them:
/// `Actor::Itself` for the member itself (it joined or left, or no one
/// said), `Actor::Us` for this device or another of our own.
#[derive(Debug, PartialEq)]
enum Actor<P> {
    Itself,
    Us,
    Other(P),
}

impl<P> Actor<P> {
    fn map<Q>(self, f: impl FnOnce(P) -> Q) -> Actor<Q> {
        match self {
            Actor::Itself => Actor::Itself,
            Actor::Us => Actor::Us,
            Actor::Other(p) => Actor::Other(f(p)),
        }
    }

    fn as_deref(&self) -> Actor<&str>
    where
        P: AsRef<str>,
    {
        match self {
            Actor::Itself => Actor::Itself,
            Actor::Us => Actor::Us,
            Actor::Other(p) => Actor::Other(p.as_ref()),
        }
    }
}

fn actor(
    node: &Node,
    member: &PublicIdentity,
    by: Option<PublicIdentity>,
) -> Actor<PublicIdentity> {
    classify(member, by, |p| {
        *p == node.identity() || node.is_own_device(p)
    })
}

fn classify(
    member: &PublicIdentity,
    by: Option<PublicIdentity>,
    ours: impl Fn(&PublicIdentity) -> bool,
) -> Actor<PublicIdentity> {
    match by {
        Some(by) if by == *member => Actor::Itself,
        Some(by) if ours(&by) => Actor::Us,
        Some(by) => Actor::Other(by),
        None => Actor::Itself,
    }
}

fn added_notice(member: &str, group: &str, by: Actor<&str>) -> String {
    match by {
        Actor::Itself => format!("{member} joined {group}"),
        Actor::Us => format!("you added {member} to {group}"),
        Actor::Other(by) => format!("{member} was added to {group} by {by}"),
    }
}

fn removed_notice(member: &str, group: &str, by: Actor<&str>) -> String {
    match by {
        Actor::Itself => format!("{member} left {group}"),
        Actor::Us => format!("you removed {member} from {group}"),
        Actor::Other(by) => format!("{member} was removed from {group} by {by}"),
    }
}

/// `member`: `None` for this device.
fn role_notice(member: Option<&str>, role: MemberRole, group: &str) -> String {
    let role = match role {
        MemberRole::Owner => "an owner",
        MemberRole::Admin => "an admin",
        MemberRole::Member => "a member",
    };
    match member {
        Some(m) => format!("{m} is now {role} in {group}"),
        None => format!("you are now {role} in {group}"),
    }
}

fn role_name(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Owner => "owner",
        MemberRole::Admin => "admin",
        MemberRole::Member => "member",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_notices_name_who_did_it() {
        let ids: Vec<PublicIdentity> = (0..3).map(|_| Identity::generate().public()).collect();
        let (me, carol, dave) = (ids[0], ids[1], ids[2]);
        let ours = |p: &PublicIdentity| *p == me;
        // Removed by us (the device that removed her used to say she left).
        assert_eq!(classify(&carol, Some(me), ours), Actor::Us);
        assert_eq!(
            removed_notice("carol", "G", Actor::Us),
            "you removed carol from G"
        );
        // Removed by someone else.
        assert_eq!(classify(&carol, Some(dave), ours), Actor::Other(dave));
        assert_eq!(
            removed_notice("carol", "G", Actor::Other("dave")),
            "carol was removed from G by dave"
        );
        // Her own leave, even when we committed it.
        assert_eq!(classify(&carol, Some(carol), ours), Actor::Itself);
        assert_eq!(classify(&carol, None, ours), Actor::Itself);
        assert_eq!(removed_notice("carol", "G", Actor::Itself), "carol left G");
        // Our own device leaving is a leave too.
        assert_eq!(classify(&me, Some(me), ours), Actor::Itself);

        assert_eq!(
            added_notice("carol", "G", Actor::Us),
            "you added carol to G"
        );
        assert_eq!(
            added_notice("carol", "G", Actor::Other("dave")),
            "carol was added to G by dave"
        );
        assert_eq!(added_notice("carol", "G", Actor::Itself), "carol joined G");
    }

    #[test]
    fn role_notices_say_you() {
        assert_eq!(
            role_notice(None, MemberRole::Admin, "G"),
            "you are now an admin in G"
        );
        assert_eq!(
            role_notice(Some("carol"), MemberRole::Admin, "G"),
            "carol is now an admin in G"
        );
        assert_eq!(
            role_notice(Some("carol"), MemberRole::Member, "G"),
            "carol is now a member in G"
        );
        assert_eq!(
            role_notice(None, MemberRole::Owner, "G"),
            "you are now an owner in G"
        );
    }
}
