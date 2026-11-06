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

use std::ffi::OsStr;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// NetworkManager connection-profile name for a group.
pub fn profile(offer: &DirectOffer) -> String {
    format!("threnody-{}", offer.ssid)
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

/// Joins the group; returns the profile name to pass to [`leave`].
/// `scratch` is a private directory for the short-lived password file
/// (the passphrase never appears on a command line).
pub async fn join(offer: &DirectOffer, scratch: &Path) -> Result<String> {
    join_with(OsStr::new("nmcli"), offer, scratch).await
}

async fn join_with(program: &OsStr, offer: &DirectOffer, scratch: &Path) -> Result<String> {
    check_offer(offer)?;
    let name = profile(offer);
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

/// Deletes profiles of groups joined before a crash (named as
/// [`profile`] names them); returns how many.
pub async fn forget_leftovers() -> Result<usize> {
    let list = nmcli(&["-t", "-f", "NAME", "connection", "show"]).await?;
    let mut n = 0;
    for name in list.lines().filter(|l| l.starts_with("threnody-DIRECT-")) {
        if leave(name).await.is_ok() {
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
    let profile = join(offer, scratch).await?;
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
#[derive(Clone, Default)]
pub struct Groups(Arc<Inner>);

#[derive(Default)]
struct Inner {
    joined: Arc<Mutex<Vec<Joined>>>,
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
    pub fn new() -> Self {
        Self::default()
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
            let _ = forget_leftovers().await;
        }
        let j = join_and_connect(node, peer, offer, scratch).await?;
        lock(&self.0.joined).push(j);
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

    /// Leaves every joined group, and deletes profiles a crash left
    /// behind; returns how many groups were left.
    pub async fn leave_all(&self, log: &Log) -> usize {
        let all = std::mem::take(&mut *lock(&self.0.joined));
        let mut n = 0;
        for j in all {
            match leave(&j.profile).await {
                Ok(()) => n += 1,
                Err(e) => log(format!("leaving {}: {e:#}", j.profile)),
            }
        }
        self.0.cleaned.store(true, Ordering::SeqCst);
        n + forget_leftovers().await.unwrap_or(0)
    }

    /// The peers whose groups are joined.
    pub fn peers(&self) -> Vec<PublicIdentity> {
        lock(&self.0.joined).iter().map(|j| j.peer).collect()
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
    fn profile_names_follow_the_ssid() {
        assert_eq!(profile(&offer()), "threnody-DIRECT-th-abcd");
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
        let e = join_with(program, &bad, dir.path()).await.unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("passphrase") && !e.contains("running nmcli"),
            "{e}"
        );
        // A good offer gets as far as running the program.
        let e = join_with(program, &offer(), dir.path()).await.unwrap_err();
        assert!(format!("{e:#}").contains("running nmcli"), "{e:#}");
    }
}
