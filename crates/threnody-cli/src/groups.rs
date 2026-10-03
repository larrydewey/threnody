//! `/group` commands: MLS groups delivered over the node's 1:1 sessions.

use anyhow::{Result, anyhow, bail};
use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity, PublicIdentity};
use threnody_groups::{GroupEvent, GroupId, GroupWire, Groups, Output};
use threnody_net::Node;

pub const HELP: &str = "\
  /group new <name>                     create a group (you own it)
  /group invite <group> <peer>          add a peer (they must be online)
  /group accept [n]                     accept a pending invitation
  /group remove <group> <peer>          remove a member (owner only)
  /groups                               list groups and members
  /g <group> <text>                     send to a group";

pub struct GroupUi {
    groups: Groups,
    /// Invitations awaiting consent: (group, name, inviter).
    pending: Vec<(GroupId, String, PublicIdentity)>,
    /// Where group state is persisted (encrypted under the identity).
    home: Home,
    identity: Identity,
}

const STATE: &str = "groups";

fn short(id: &GroupId) -> String {
    id[..3].iter().map(|b| format!("{b:02x}")).collect()
}

impl GroupUi {
    /// Loads persisted groups for `identity` from `home`.
    pub fn load(home: &Home, identity: &Identity) -> Result<Self> {
        let groups = match home.load_state(identity, STATE)? {
            Some(bytes) => Groups::restore(identity, &bytes)?,
            None => Groups::new(identity),
        };
        Ok(Self {
            groups,
            pending: Vec::new(),
            home: Home::new(home.dir()),
            identity: Identity::from_seed(&identity.seed()),
        })
    }

    pub fn count(&self) -> usize {
        self.groups.list().len()
    }

    fn save(&self) {
        let r = self
            .groups
            .export()
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(self.home.save_state(&self.identity, STATE, &b)?));
        if let Err(e) = r {
            println!("! could not save group state: {e:#}");
        }
    }

    fn label(&self, id: &GroupId) -> String {
        let name = self
            .groups
            .list()
            .into_iter()
            .find(|(g, ..)| g == id)
            .map(|(_, n, ..)| n)
            .unwrap_or_default();
        format!("{name}#{}", short(id))
    }

    /// Finds a group by exact name, `name#hex`, or hex id prefix.
    fn find(&self, q: &str) -> Result<GroupId> {
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

    /// Delivers outgoing group traffic and prints events.
    fn apply(&mut self, node: &Node, name: &dyn Fn(&PublicIdentity) -> String, out: Output) {
        // Persist before anything leaves: a crash must not lose an epoch
        // that peers have already moved to.
        self.save();
        for o in out.send {
            let sent = o.wire.encode().map_err(anyhow::Error::from).and_then(|b| {
                node.send(&o.to, AppMessage::Group(b))
                    .map_err(anyhow::Error::from)
            });
            if sent.is_err() {
                println!("! {} is offline; group message not delivered", name(&o.to));
            }
        }
        for e in out.events {
            self.show(node, name, e);
        }
    }

    fn show(&mut self, node: &Node, name: &dyn Fn(&PublicIdentity) -> String, e: GroupEvent) {
        match e {
            GroupEvent::Joined { group, owner, .. } => {
                println!(
                    "* joined group {} (owner {})",
                    self.label(&group),
                    name(&owner)
                );
            }
            GroupEvent::InviteRequested {
                group,
                name: gname,
                peer,
            } => {
                // Mutually approved contacts are trusted to add us; anyone
                // else needs explicit consent.
                let trusted = node
                    .contacts()
                    .get(&peer)
                    .is_some_and(|c| c.mutually_approved());
                if trusted {
                    match self.groups.accept_invite(&group, peer) {
                        Ok(out) => self.apply(node, name, out),
                        Err(e) => println!("! invitation from {}: {e}", name(&peer)),
                    }
                } else {
                    self.pending.push((group, gname.clone(), peer));
                    println!(
                        "* {} invites you to group {gname:?}. /group accept {} to join",
                        name(&peer),
                        self.pending.len()
                    );
                }
            }
            GroupEvent::MemberAdded { group, member } => {
                println!("* {} joined {}", name(&member), self.label(&group));
            }
            GroupEvent::MemberRemoved { group, member } => {
                println!("* {} left {}", name(&member), self.label(&group));
            }
            GroupEvent::Left { group } => {
                println!("* you were removed from group #{}", short(&group))
            }
            GroupEvent::Text { group, from, text } => {
                println!("[{}] <{}> {text}", self.label(&group), name(&from));
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
        let res = GroupWire::decode(payload)
            .map_err(anyhow::Error::from)
            .and_then(|w| self.groups.handle(peer, w).map_err(anyhow::Error::from));
        match res {
            Ok(out) => self.apply(node, name, out),
            Err(e) => println!("! group message from {}: {e:#}", name(&peer)),
        }
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
                self.save();
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
                let out = self.groups.invite(&g, p)?;
                println!("* invitation sent to {}", name(&p));
                self.apply(node, name, out);
            }
            "accept" => {
                if self.pending.is_empty() {
                    bail!("no pending invitations");
                }
                let n: usize = it.next().map_or(Ok(self.pending.len()), str::parse)?;
                if n == 0 || n > self.pending.len() {
                    bail!("pick 1..={}", self.pending.len());
                }
                let (g, gname, peer) = self.pending.remove(n - 1);
                let out = self.groups.accept_invite(&g, peer)?;
                println!("* accepting {gname:?} from {}", name(&peer));
                self.apply(node, name, out);
            }
            "remove" | "kick" => {
                let (g, p) = (it.next(), it.next());
                let (Some(g), Some(p)) = (g, p) else {
                    bail!("usage: /group remove <group> <peer>")
                };
                let (g, p) = (self.find(g)?, resolve(p)?);
                let out = self.groups.remove(&g, &p)?;
                self.apply(node, name, out);
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
            let names: Vec<String> = members.iter().map(name).collect();
            println!(
                "  {}  owner {}  members: {}",
                self.label(&id),
                name(&owner),
                names.join(", ")
            );
        }
        for (i, (_, g, p)) in self.pending.iter().enumerate() {
            println!("  invitation {}: {g:?} from {}", i + 1, name(p));
        }
    }

    /// `/g <group> <text>`.
    pub fn say(
        &mut self,
        node: &Node,
        name: &dyn Fn(&PublicIdentity) -> String,
        args: &str,
    ) -> Result<()> {
        let (g, text) = args
            .split_once(' ')
            .ok_or_else(|| anyhow!("usage: /g <group> <text>"))?;
        let g = self.find(g)?;
        let out = self.groups.send_text(&g, text)?;
        self.apply(node, name, out);
        Ok(())
    }
}
