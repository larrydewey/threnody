//! Joining a peer's Wi-Fi Direct group through NetworkManager (Appendix L).
//!
//! Linux joins as a client of the group the peer (typically a phone)
//! created. NetworkManager connects a Wi-Fi interface to the group's
//! network with the passphrase the peer sent over the encrypted session;
//! hosting a group would need root access to wpa_supplicant. The profile
//! is temporary: no autoconnect, no default route, removed on leave.
//!
//! [`Groups`] keeps track of what was joined: one group at a time (a
//! computer has one Wi-Fi interface), left once no session runs over it.
//! Profile names carry a tag of the identity that joined, so one Threnody
//! process (the app, a CLI with another home) never deletes another's.

use std::ffi::OsStr;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use threnody_core::PublicIdentity;
use threnody_net::{DirectOffer, Node};
use tokio::process::Command;

use crate::Log;

/// Wi-Fi P2P group SSIDs always start with this (Android's are
/// `DIRECT-th-xxxx`). Only such networks are joined, so a contact can't
/// steer this computer onto an ordinary network.
pub const SSID_PREFIX: &str = "DIRECT-";

/// How long a joined group is kept without a session over it.
pub const JOIN_GRACE: Duration = Duration::from_secs(60);

/// How often joined groups are checked for sessions.
const SWEEP_EVERY: Duration = Duration::from_secs(15);

/// Checks an offer before anything is handed to NetworkManager: a
/// `DIRECT-` SSID of 1 to 32 bytes without control characters, a
/// passphrase of 8 to 63 printable ASCII characters (WPA2's rule; a
/// newline would add lines to the passwd-file), and an `ip:port` address.
/// The node already refuses other offers; this guards strings that come
/// from elsewhere (an app's FFI call).
pub fn check_offer(o: &DirectOffer) -> Result<()> {
    if o.ssid.is_empty() || o.ssid.len() > 32 || o.ssid.chars().any(char::is_control) {
        bail!("Wi-Fi Direct offer: bad network name");
    }
    if !o.ssid.starts_with(SSID_PREFIX) {
        bail!("Wi-Fi Direct offer: the network name doesn't start with {SSID_PREFIX}");
    }
    if !(8..=63).contains(&o.passphrase.len())
        || !o.passphrase.bytes().all(|b| (b' '..=b'~').contains(&b))
    {
        bail!("Wi-Fi Direct offer: bad passphrase");
    }
    if o.addr.parse::<SocketAddr>().is_err() {
        bail!("Wi-Fi Direct offer: bad group address");
    }
    Ok(())
}

/// A short tag for the profiles `id` joins: derived from the identity
/// without showing it, since every local user can list profile names.
pub fn owner_tag(id: &PublicIdentity) -> String {
    let k = blake3::derive_key("threnody 2026-11 wifi-direct profile owner", id.as_bytes());
    k[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// NetworkManager connection-profile name for a group joined by the
/// identity tagged `owner` ([`owner_tag`]).
pub fn profile(owner: &str, offer: &DirectOffer) -> String {
    format!("threnody-{owner}-{}", offer.ssid)
}

/// Profiles named before profile names carried an owner. Another process
/// may still use one, so only inactive ones count as leftovers.
const LEGACY_PREFIX: &str = "threnody-DIRECT-";

/// The fields of one line of `nmcli -t` output: `:` separates them, and
/// `\` escapes `:` and `\` inside them.
fn terse_fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let (Some(n), Some(f)) = (chars.next(), out.last_mut()) {
                    f.push(n);
                }
            }
            ':' => out.push(String::new()),
            c => {
                if let Some(f) = out.last_mut() {
                    f.push(c);
                }
            }
        }
    }
    out
}

/// The profiles to delete as leftovers, from `nmcli -t -f NAME,ACTIVE
/// connection show`: `owner`'s, and inactive ones from before owners.
fn leftovers(owner: &str, list: &str) -> Vec<String> {
    let mine = format!("threnody-{owner}-{SSID_PREFIX}");
    list.lines()
        .filter_map(|l| {
            let f = terse_fields(l);
            let (name, active) = (f.first()?, f.get(1).map_or("", String::as_str));
            (name.starts_with(&mine) || (name.starts_with(LEGACY_PREFIX) && active == "no"))
                .then(|| name.clone())
        })
        .collect()
}

/// The passwd-file line carrying the passphrase: exactly one line, since
/// [`check_offer`] refuses passphrases with line breaks.
fn passwd_line(o: &DirectOffer) -> String {
    format!("802-11-wireless-security.psk:{}\n", o.passphrase)
}

async fn nmcli(args: &[&str]) -> Result<String> {
    nmcli_with(OsStr::new("nmcli"), args).await
}

async fn nmcli_with(program: &OsStr, args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow!("nmcli timed out"))?
    .context("running nmcli (is NetworkManager installed?)")?;
    if !out.status.success() {
        bail!(
            "nmcli {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Joins the group as the identity tagged `owner`; returns the profile
/// name to pass to [`leave`]. `scratch` is a private directory for the
/// short-lived password file (the passphrase never appears on a command
/// line).
pub async fn join(owner: &str, offer: &DirectOffer, scratch: &Path) -> Result<String> {
    join_with(OsStr::new("nmcli"), owner, offer, scratch).await
}

async fn join_with(
    program: &OsStr,
    owner: &str,
    offer: &DirectOffer,
    scratch: &Path,
) -> Result<String> {
    check_offer(offer)?;
    let name = profile(owner, offer);
    let _ = nmcli_with(program, &["connection", "delete", &name]).await;
    nmcli_with(
        program,
        &[
            "connection",
            "add",
            "type",
            "wifi",
            "con-name",
            &name,
            "ssid",
            &offer.ssid,
            "wifi-sec.key-mgmt",
            "wpa-psk",
            "wifi-sec.psk-flags",
            "2", // not saved: supplied at activation
            "connection.autoconnect",
            "no",
            "ipv4.method",
            "auto",
            "ipv4.never-default",
            "yes",
            "ipv4.ignore-auto-dns",
            "yes",
            "ipv6.method",
            "disabled",
        ],
    )
    .await?;
    let pw = scratch.join(format!(".{name}.pw"));
    write_private(&pw, &passwd_line(offer))?;
    let up = nmcli_with(
        program,
        &[
            "--wait",
            "45",
            "connection",
            "up",
            "id",
            &name,
            "passwd-file",
            &pw.to_string_lossy(),
        ],
    )
    .await;
    let _ = std::fs::remove_file(&pw);
    if let Err(e) = up {
        let _ = nmcli_with(program, &["connection", "delete", &name]).await;
        return Err(e);
    }
    Ok(name)
}

/// Leaves the group and forgets the profile; NetworkManager then returns
/// the interface to its usual network.
pub async fn leave(name: &str) -> Result<()> {
    nmcli(&["connection", "delete", name]).await.map(|_| ())
}

/// Deletes the profiles of groups the identity tagged `owner` joined
/// before a crash, and inactive ones named before profile names carried
/// an owner; returns how many. Other identities' profiles stay.
pub async fn forget_leftovers(owner: &str) -> Result<usize> {
    let list = nmcli(&["-t", "-f", "NAME,ACTIVE", "connection", "show"]).await?;
    let mut n = 0;
    for name in leftovers(owner, &list) {
        if leave(&name).await.is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// A group we joined.
#[derive(Clone, Debug)]
pub struct Joined {
    pub peer: PublicIdentity,
    pub ssid: String,
    /// NetworkManager profile, for [`leave`].
    pub profile: String,
    /// The group owner's address inside the group.
    pub ip: IpAddr,
    pub at: Instant,
}

/// The joined groups to leave: older than [`JOIN_GRACE`], with no session
/// to their peer at the group owner's IP. The IP, not `ip:port`, so a
/// session the owner dialled back in from another port counts too.
pub fn stale(joined: &[Joined], live: &[(PublicIdentity, IpAddr)], now: Instant) -> Vec<usize> {
    joined
        .iter()
        .enumerate()
        .filter(|(_, j)| {
            now.saturating_duration_since(j.at) > JOIN_GRACE
                && !live.iter().any(|(p, ip)| *p == j.peer && *ip == j.ip)
        })
        .map(|(i, _)| i)
        .collect()
}

/// What to do with an offer.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// That peer's group is joined already: keep it.
    AlreadyJoined,
    /// Join it, leaving these (indexes into the joined list) first: one
    /// Wi-Fi interface, one group.
    Join { leave_first: Vec<usize> },
}

pub fn plan(joined: &[Joined], peer: &PublicIdentity, offer: &DirectOffer) -> Plan {
    if joined
        .iter()
        .any(|j| j.peer == *peer && j.ssid == offer.ssid)
    {
        return Plan::AlreadyJoined;
    }
    Plan::Join {
        leave_first: (0..joined.len()).collect(),
    }
}

/// Joins the group and dials `peer` inside it, pinned; the new session
/// replaces the slower one. A group whose dial fails is left at once.
pub async fn join_and_connect(
    node: &Node,
    peer: PublicIdentity,
    offer: &DirectOffer,
    scratch: &Path,
) -> Result<Joined> {
    check_offer(offer)?;
    let ip = offer.addr.parse::<SocketAddr>()?.ip();
    let profile = join(&owner_tag(&node.identity()), offer, scratch).await?;
    if let Err(e) = node.connect(&offer.addr, Some(peer.fingerprint())).await {
        let _ = leave(&profile).await;
        return Err(anyhow!(
            "connecting over Wi-Fi Direct to {}: {e}",
            offer.addr
        ));
    }
    Ok(Joined {
        peer,
        ssid: offer.ssid.clone(),
        profile,
        ip,
        at: Instant::now(),
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The groups one node joined, left automatically once no session runs
/// over them. Clones share them.
#[derive(Clone)]
pub struct Groups(Arc<Inner>);

struct Inner {
    /// [`owner_tag`] of the node's identity.
    owner: String,
    joined: Arc<Mutex<Vec<Joined>>>,
    /// Bumped by [`Groups::leave_all`], with `joined` locked: a join that
    /// started before leaves its group instead of keeping it.
    generation: AtomicU64,
    /// Checks joined groups for sessions; stopped when the last clone goes.
    sweeper: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// While a join runs (one at a time).
    joining: AtomicBool,
    /// Whether leftovers from a crash were deleted yet.
    cleaned: AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(t) = lock(&self.sweeper).take() {
            t.abort();
        }
    }
}

/// Clears the joining flag however a join ends.
struct Joining<'a>(&'a AtomicBool);

impl Drop for Joining<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Groups {
    /// The groups `id` (the node's identity) joins.
    pub fn new(id: &PublicIdentity) -> Self {
        Self(Arc::new(Inner {
            owner: owner_tag(id),
            joined: Arc::default(),
            generation: AtomicU64::new(0),
            sweeper: Mutex::new(None),
            joining: AtomicBool::new(false),
            cleaned: AtomicBool::new(false),
        }))
    }

    /// Keeps a group just joined, unless [`Groups::leave_all`] ran since
    /// `generation`; then returns it, to leave.
    fn keep(&self, generation: u64, j: Joined) -> Option<Joined> {
        let mut joined = lock(&self.0.joined);
        if self.0.generation.load(Ordering::SeqCst) != generation {
            return Some(j);
        }
        joined.push(j);
        None
    }

    /// Takes every joined group, and turns joins still running into
    /// groups to leave once joined.
    fn take_all(&self) -> Vec<Joined> {
        let mut joined = lock(&self.0.joined);
        self.0.generation.fetch_add(1, Ordering::SeqCst);
        std::mem::take(&mut *joined)
    }

    /// Joins `peer`'s group and dials it, unless it is joined already.
    /// Any other group is left first. Must run inside a tokio runtime.
    pub async fn join(
        &self,
        node: &Node,
        peer: PublicIdentity,
        offer: &DirectOffer,
        scratch: &Path,
        log: Log,
    ) -> Result<()> {
        check_offer(offer)?;
        if self.0.joining.swap(true, Ordering::SeqCst) {
            bail!("already joining a Wi-Fi Direct group");
        }
        let _joining = Joining(&self.0.joining);
        let generation = self.0.generation.load(Ordering::SeqCst);
        let leaving: Vec<Joined> = {
            let mut joined = lock(&self.0.joined);
            match plan(&joined, &peer, offer) {
                Plan::AlreadyJoined => return Ok(()),
                Plan::Join { leave_first } => {
                    let mut out = Vec::new();
                    for i in leave_first.into_iter().rev() {
                        out.push(joined.remove(i));
                    }
                    out
                }
            }
        };
        for j in leaving {
            match leave(&j.profile).await {
                Ok(()) => log(format!(
                    "left {}'s Wi-Fi Direct group {} for another",
                    j.peer.fingerprint(),
                    j.ssid
                )),
                Err(e) => log(format!("leaving {}: {e:#}", j.profile)),
            }
        }
        if !self.0.cleaned.swap(true, Ordering::SeqCst) {
            let _ = forget_leftovers(&self.0.owner).await;
        }
        if self.0.generation.load(Ordering::SeqCst) != generation {
            bail!("Wi-Fi Direct was left while joining");
        }
        let j = join_and_connect(node, peer, offer, scratch).await?;
        if let Some(j) = self.keep(generation, j) {
            if let Err(e) = leave(&j.profile).await {
                log(format!("leaving {}: {e:#}", j.profile));
            }
            bail!("Wi-Fi Direct was left while joining");
        }
        self.start_sweeper(node, log);
        Ok(())
    }

    fn start_sweeper(&self, node: &Node, log: Log) {
        let mut sweeper = lock(&self.0.sweeper);
        if sweeper.is_some() {
            return;
        }
        // Only the list and the node: holding `Inner` would keep it alive.
        let (joined, node) = (Arc::clone(&self.0.joined), node.clone());
        *sweeper = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_EVERY).await;
                let live: Vec<(PublicIdentity, IpAddr)> = node
                    .sessions()
                    .into_iter()
                    .map(|s| (s.peer, s.addr.ip().to_canonical()))
                    .collect();
                let gone: Vec<Joined> = {
                    let mut joined = lock(&joined);
                    let stale = stale(&joined, &live, Instant::now());
                    stale.into_iter().rev().map(|i| joined.remove(i)).collect()
                };
                for j in gone {
                    match leave(&j.profile).await {
                        Ok(()) => log(format!(
                            "left {}'s Wi-Fi Direct group {} (link ended)",
                            j.peer.fingerprint(),
                            j.ssid
                        )),
                        Err(e) => log(format!("leaving {}: {e:#}", j.profile)),
                    }
                }
            }
        }));
    }

    /// Leaves every joined group, and a join still running once it is
    /// done (deleting its profile now cuts it short), and deletes profiles
    /// a crash left behind; returns how many groups were left.
    pub async fn leave_all(&self, log: &Log) -> usize {
        let all = self.take_all();
        let mut n = 0;
        for j in all {
            match leave(&j.profile).await {
                Ok(()) => n += 1,
                Err(e) => log(format!("leaving {}: {e:#}", j.profile)),
            }
        }
        self.0.cleaned.store(true, Ordering::SeqCst);
        n + forget_leftovers(&self.0.owner).await.unwrap_or(0)
    }

    /// The peers whose groups are joined.
    pub fn peers(&self) -> Vec<PublicIdentity> {
        lock(&self.0.joined).iter().map(|j| j.peer).collect()
    }

    /// Whether a group is joined, or a join is running: whether
    /// [`Groups::leave_all`] has anything to do besides leftovers.
    pub fn busy(&self) -> bool {
        self.0.joining.load(Ordering::SeqCst) || !lock(&self.0.joined).is_empty()
    }
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> DirectOffer {
        DirectOffer {
            ssid: "DIRECT-th-abcd".into(),
            passphrase: "abcdefgh2345".into(),
            addr: "192.168.49.1:7450".into(),
        }
    }

    fn someone() -> PublicIdentity {
        threnody_core::Identity::generate().public()
    }

    fn joined(peer: PublicIdentity, ssid: &str, ip: &str, at: Instant) -> Joined {
        Joined {
            peer,
            ssid: ssid.into(),
            profile: format!("threnody-{ssid}"),
            ip: ip.parse().unwrap(),
            at,
        }
    }

    #[test]
    fn check_offer_rejects_injection() {
        assert!(check_offer(&offer()).is_ok());
        let bad = |f: &dyn Fn(&mut DirectOffer)| {
            let mut o = offer();
            f(&mut o);
            check_offer(&o).is_err()
        };
        assert!(bad(&|o| o.passphrase = "abcdefgh\nmode=ap".into()));
        assert!(bad(&|o| o.passphrase = "abcdefgh\r".into()));
        assert!(bad(&|o| o.passphrase = "a".repeat(7)));
        assert!(bad(&|o| o.passphrase = "a".repeat(64)));
        assert!(bad(&|o| o.ssid = String::new()));
        assert!(bad(&|o| o.ssid = format!("DIRECT-{}", "x".repeat(26))));
        assert!(bad(&|o| o.ssid = "HomeWifi".into()));
        assert!(bad(&|o| o.addr = "x".into()));
    }

    #[test]
    fn one_group_at_a_time() {
        let (alice, bob) = (someone(), someone());
        let o = offer();
        assert_eq!(
            plan(&[], &alice, &o),
            Plan::Join {
                leave_first: vec![]
            }
        );
        let now = Instant::now();
        let on = [joined(alice, &o.ssid, "192.168.49.1", now)];
        assert_eq!(plan(&on, &alice, &o), Plan::AlreadyJoined);
        let mut other = offer();
        other.ssid = "DIRECT-th-wxyz".into();
        for (peer, offer) in [(alice, &other), (bob, &other), (bob, &o)] {
            assert_eq!(
                plan(&on, &peer, offer),
                Plan::Join {
                    leave_first: vec![0]
                }
            );
        }
    }

    #[test]
    fn passwd_file_is_one_line() {
        let line = passwd_line(&offer());
        assert_eq!(line, "802-11-wireless-security.psk:abcdefgh2345\n");
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn profile_names_carry_the_owner_and_the_ssid() {
        let (a, b) = (someone(), someone());
        let tag = owner_tag(&a);
        assert_eq!(tag.len(), 8);
        assert_eq!(tag, owner_tag(&a));
        assert_ne!(tag, owner_tag(&b));
        assert!(!a.fingerprint().compact().to_lowercase().contains(&tag));
        assert_eq!(
            profile("0a1b2c3d", &offer()),
            "threnody-0a1b2c3d-DIRECT-th-abcd"
        );
    }

    #[test]
    fn only_our_own_or_idle_old_profiles_are_leftovers() {
        let list = "Home:yes\n\
threnody-0a1b2c3d-DIRECT-th-abcd:yes\n\
threnody-0a1b2c3d-DIRECT-th-x\\:y:no\n\
threnody-99999999-DIRECT-th-abcd:yes\n\
threnody-99999999-DIRECT-th-wxyz:no\n\
threnody-DIRECT-th-old:no\n\
threnody-DIRECT-th-busy:yes\n";
        assert_eq!(
            leftovers("0a1b2c3d", list),
            vec![
                "threnody-0a1b2c3d-DIRECT-th-abcd",
                "threnody-0a1b2c3d-DIRECT-th-x:y",
                "threnody-DIRECT-th-old",
            ]
        );
        assert_eq!(terse_fields("a\\\\b\\:c:d"), vec!["a\\b:c", "d"]);
    }

    #[test]
    fn a_join_finishing_after_leave_all_is_left() {
        let (me, alice) = (someone(), someone());
        let g = Groups::new(&me);
        let now = Instant::now();
        // Not left in between: kept.
        let generation = g.0.generation.load(Ordering::SeqCst);
        assert!(
            g.keep(generation, joined(alice, "DIRECT-a", "192.168.49.1", now))
                .is_none()
        );
        assert_eq!(g.peers(), vec![alice]);
        assert!(g.busy());
        // Left while a second join ran: that one is handed back to leave.
        let generation = g.0.generation.load(Ordering::SeqCst);
        assert_eq!(g.take_all().len(), 1);
        let back = g.keep(generation, joined(alice, "DIRECT-b", "192.168.49.1", now));
        assert_eq!(back.map(|j| j.ssid).as_deref(), Some("DIRECT-b"));
        assert!(g.peers().is_empty());
        assert!(!g.busy());
    }

    #[test]
    fn stale_groups_wait_for_grace_then_need_a_session() {
        let (alice, bob) = (someone(), someone());
        let now = Instant::now();
        let ip: IpAddr = "192.168.49.1".parse().unwrap();
        let young = joined(alice, "DIRECT-a", "192.168.49.1", now);
        assert!(stale(&[young], &[], now + Duration::from_secs(30)).is_empty());
        let old = joined(alice, "DIRECT-a", "192.168.49.1", now);
        let later = now + JOIN_GRACE + Duration::from_secs(1);
        let groups = [old];
        // A session to that peer at the owner's IP, whatever its port.
        assert!(stale(&groups, &[(alice, ip)], later).is_empty());
        // Only a session at another IP, or another peer at that IP.
        let elsewhere: IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!(stale(&groups, &[(alice, elsewhere)], later), vec![0]);
        assert_eq!(stale(&groups, &[(bob, ip)], later), vec![0]);
        assert_eq!(stale(&groups, &[], later), vec![0]);
    }

    #[tokio::test]
    async fn join_refuses_bad_offers_before_nmcli() {
        let dir = tempfile::tempdir().unwrap();
        let program = OsStr::new("/nonexistent/nmcli");
        let mut bad = offer();
        bad.passphrase = "abcdefgh\nmode=ap".into();
        let e = join_with(program, "0a1b2c3d", &bad, dir.path())
            .await
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("passphrase") && !e.contains("running nmcli"),
            "{e}"
        );
        // A good offer gets as far as running the program.
        let e = join_with(program, "0a1b2c3d", &offer(), dir.path())
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("running nmcli"), "{e:#}");
    }
}
