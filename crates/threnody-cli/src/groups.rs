//! `/group` commands: MLS groups on the shared [`GroupNode`], which
//! persists them and delivers their traffic (Appendix F).

use anyhow::{Result, anyhow, bail};
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
  /groups                               list groups and members
  /g <group> <text>                     send to a group";

pub struct GroupUi {
    groups: GroupNode,
}

fn short(id: &GroupId) -> String {
    id[..3].iter().map(|b| format!("{b:02x}")).collect()
}

impl GroupUi {
    /// Loads persisted groups for `identity` from `home`.
    pub fn load(home: &Home, identity: &Identity) -> Result<Self> {
        Ok(Self {
            groups: GroupNode::load(home, identity)?,
        })
    }

    pub fn count(&self) -> usize {
        self.groups.list().len()
    }

    fn label(&self, id: &GroupId) -> String {
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

    fn show(&self, name: &dyn Fn(&PublicIdentity) -> String, updates: Vec<Update>) {
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
                Update::Left { group } => {
                    println!("* you were removed from group #{}", short(&group));
                }
                Update::Text { group, from, text } => {
                    println!("[{}] <{}> {text}", self.label(&group), name(&from));
                }
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
            Ok(updates) => self.show(name, updates),
            Err(e) => println!("! group message from {}: {e}", name(&peer)),
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
                self.show(name, updates);
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
                self.show(name, updates);
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
