//! Meeting points (Appendix N, *Meeting points*): reaching a device that
//! no hole punch gets through to, such as two people each behind a VPN.
//!
//! A device that can't count on being reached directly (its NAT looks
//! symmetric, or it is an anonymous identity, which never shows its
//! address) keeps an anonymous link, under a throwaway identity, to a
//! volunteer relay and registers a random cookie there (`Listen`). It
//! puts the relay and cookie in its rendezvous records, handle and invite
//! records, and an anonymous identity's invite links. Whoever may reach it
//! builds an onion circuit whose last relay is that volunteer and EXTENDs
//! with the cookie: the relay passes the CREATE down the registered link,
//! the device answers as a circuit's last hop does, and the finder BEGINs
//! a session over the circuit as over any other.
//!
//! The relay learns the device's address but not who it is (the link's
//! identity is thrown away) nor who reaches it (a session inside the
//! circuit, and the finder's address hidden behind the circuit's first
//! relay when it has one). Nothing changes for the device's NAT: the link
//! is outgoing, as any web page's is.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use threnody_core::crypto::random_bytes;
use threnody_core::rendezvous::MeetingPoint;
use threnody_core::{Fingerprint, PublicIdentity};
use tokio::sync::oneshot;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};
use crate::onion::{Hop, OnionMsg, OnionRoute};

/// Cookies a relay listens for at once, in all and from one network.
const MAX_LISTENERS: usize = 4096;
const MAX_LISTENERS_PER_NETWORK: usize = 64;
/// How often a device checks its meeting point, and refreshes it (which
/// also keeps the link's NAT mapping and the relay's idle timer alive).
const KEEP_EVERY: Duration = Duration::from_secs(60);
/// How long a relay has to say it listens.
const LISTEN_WAIT: Duration = Duration::from_secs(15);
/// How long a finder gives a meeting point to answer.
pub(crate) const MEET_FOR: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct MeetingState {
    /// Relay side: cookie → the anonymous link registered for it.
    listeners: Mutex<HashMap<[u8; 16], PublicIdentity>>,
    /// Our own meeting point and the link it rides on.
    own: Mutex<Option<(MeetingPoint, PublicIdentity)>>,
    /// `Listen`s waiting for their `Listening`.
    waiting: Mutex<HashMap<[u8; 16], oneshot::Sender<()>>>,
    /// Keep a meeting point whatever the NAT looks like (tests, and users
    /// who know they're behind a VPN).
    forced: AtomicBool,
    /// Wakes the loop: something changed that may call for a meeting point.
    kick: tokio::sync::Notify,
}

impl Node {
    fn meet_state(&self) -> &MeetingState {
        &self.shared.meeting
    }

    // ----- Relay side -----

    /// A device on anonymous link `from` asks us to splice circuits that
    /// name `cookie` onto that link. Only volunteers listen. A new cookie
    /// takes a token (bound to the cookie); refreshing one is free.
    pub(crate) fn meeting_listen(
        &self,
        from: PublicIdentity,
        cookie: [u8; 16],
        token: Option<&[u8]>,
    ) {
        if !self.volunteering() {
            return;
        }
        let known = lock(&self.meet_state().listeners).get(&cookie) == Some(&from);
        if !known {
            let Some(t) = token else { return };
            if !self.accept_token(t, &cookie) {
                return;
            }
            let net = self
                .anon_addr(&from)
                .map(|a| crate::volunteer::network(a.ip()));
            let mut l = lock(&self.meet_state().listeners);
            // One cookie per link: a new one replaces the old.
            l.retain(|_, p| *p != from);
            let here = l
                .values()
                .filter(|p| self.anon_addr(p).map(|a| crate::volunteer::network(a.ip())) == net)
                .count();
            if l.len() >= MAX_LISTENERS || here >= MAX_LISTENERS_PER_NETWORK {
                return;
            }
            l.insert(cookie, from);
        }
        self.onion_reply_listening(from, cookie);
    }

    fn onion_reply_listening(&self, to: PublicIdentity, cookie: [u8; 16]) {
        self.send_anon_msg(&to, &OnionMsg::Listening { cookie });
    }

    /// The link registered for `cookie`, if any.
    pub(crate) fn meeting_link(&self, cookie: &[u8; 16]) -> Option<PublicIdentity> {
        lock(&self.meet_state().listeners).get(cookie).copied()
    }

    /// An anonymous link ended: whatever listened on it stops, and our own
    /// meeting point goes if it rode on it.
    pub(crate) fn meeting_link_closed(&self, peer: &PublicIdentity) {
        lock(&self.meet_state().listeners).retain(|_, p| p != peer);
        let lost = {
            let mut own = lock(&self.meet_state().own);
            let lost = own.as_ref().is_some_and(|(_, l)| l == peer);
            if lost {
                *own = None;
            }
            lost
        };
        if lost {
            self.meeting_changed();
        }
    }

    /// Whether the anonymous link with `peer` carries a meeting point
    /// (ours, or one we listen for), so it isn't closed for being idle.
    pub(crate) fn meeting_busy(&self, peer: &PublicIdentity) -> bool {
        lock(&self.meet_state().own)
            .as_ref()
            .is_some_and(|(_, l)| l == peer)
            || lock(&self.meet_state().listeners)
                .values()
                .any(|p| p == peer)
    }

    // ----- Device side -----

    /// The relay answered our `Listen`.
    pub(crate) fn meeting_listening(&self, _from: PublicIdentity, cookie: [u8; 16]) {
        if let Some(tx) = lock(&self.meet_state().waiting).remove(&cookie) {
            let _ = tx.send(());
        }
    }

    /// Keeps a meeting point whatever our NAT looks like (or stops).
    pub fn set_meeting_point(&self, on: bool) {
        self.meet_state().forced.store(on, Ordering::Relaxed);
        self.meet_state().kick.notify_one();
    }

    /// Looks again at whether we want a meeting point (our NAT or our
    /// relays changed).
    pub(crate) fn kick_meeting(&self) {
        self.meet_state().kick.notify_one();
    }

    /// Whether we want a meeting point: we can use volunteers, and either
    /// we never show an address (an anonymous identity), our NAT looks
    /// symmetric, or the user asked for one.
    fn wants_meeting(&self) -> bool {
        self.use_volunteers()
            && (self.meet_state().forced.load(Ordering::Relaxed)
                || self.shared.persona
                || (self.reach_enabled() && self.reachability().symmetric))
    }

    /// Our meeting points, for our records and invites.
    pub fn own_meetings(&self) -> Vec<MeetingPoint> {
        lock(&self.meet_state().own)
            .as_ref()
            .map(|(m, _)| *m)
            .into_iter()
            .collect()
    }

    /// Keeps our meeting point up while we want one.
    pub(crate) async fn meeting_loop(&self) {
        loop {
            if self.wants_meeting() {
                let own = *lock(&self.meet_state().own);
                match own {
                    // Refreshing keeps the link and its NAT mapping alive.
                    Some((m, link)) => {
                        if !self.listen_at(link, m.cookie, None).await {
                            self.meeting_link_closed(&link);
                        }
                    }
                    None => {
                        if let Err(e) = self.open_meeting().await {
                            self.emit(Event::ReachNote {
                                note: format!("no meeting point: {e}"),
                            });
                        }
                    }
                }
            } else if lock(&self.meet_state().own).take().is_some() {
                self.meeting_changed();
            }
            if !self.pause(KEEP_EVERY, &self.meet_state().kick).await {
                break;
            }
        }
    }

    async fn open_meeting(&self) -> Result<()> {
        let mut relays = self.volunteer_relays();
        relays.sort_by_key(|_| u64::from_le_bytes(random_bytes()));
        for r in relays.into_iter().filter(|r| self.can_pay(&r.identity)) {
            // A meeting point names an address, not a name to look up.
            let Some(addr) = r.addrs.iter().find_map(|a| a.parse::<SocketAddr>().ok()) else {
                continue;
            };
            // A throwaway identity: the relay never learns who listens.
            let Ok(link) = self
                .anon_dial(&addr.to_string(), Some(r.identity.fingerprint()), true)
                .await
            else {
                continue;
            };
            let cookie: [u8; 16] = random_bytes();
            let Some(token) = self.mint_token(&r.identity, &cookie) else {
                continue;
            };
            if self.listen_at(link, cookie, Some(token)).await {
                let point = MeetingPoint {
                    relay: r.identity.fingerprint(),
                    addr,
                    cookie,
                };
                *lock(&self.meet_state().own) = Some((point, link));
                self.emit(Event::ReachNote {
                    note: format!("meeting point at {}", r.identity.fingerprint()),
                });
                self.meeting_changed();
                return Ok(());
            }
        }
        Err(NetError::NoRoute(
            "no volunteer relay to meet at (subscribe to a directory)".into(),
        ))
    }

    /// Sends `Listen` for `cookie` over `link` and waits for the answer.
    async fn listen_at(
        &self,
        link: PublicIdentity,
        cookie: [u8; 16],
        token: Option<Vec<u8>>,
    ) -> bool {
        let (tx, rx) = oneshot::channel();
        lock(&self.meet_state().waiting).insert(cookie, tx);
        let sent = self.send_anon_msg(&link, &OnionMsg::Listen { cookie, token });
        let ok = sent && matches!(tokio::time::timeout(LISTEN_WAIT, rx).await, Ok(Ok(())));
        lock(&self.meet_state().waiting).remove(&cookie);
        ok
    }

    /// Our meeting point changed: records go out again with it.
    fn meeting_changed(&self) {
        self.republish_all();
    }

    // ----- Finder side -----

    /// Opens a session with `dest` at its meeting point `m`: through a
    /// volunteer relay we pay first when we can (so the meeting relay
    /// doesn't see our address), else straight from us to the meeting
    /// relay.
    pub async fn connect_meeting(
        &self,
        dest: Fingerprint,
        m: &MeetingPoint,
    ) -> Result<PublicIdentity> {
        if dest == self.identity().fingerprint() {
            return Err(NetError::NoRoute("ourselves".into()));
        }
        let relays = self.volunteer_relays();
        let meeting_relay = relays.iter().find(|r| r.identity.fingerprint() == m.relay);
        let pay_meeting = meeting_relay
            .filter(|r| self.can_pay(&r.identity))
            .map(|r| r.identity);
        let end = Hop {
            fp: dest,
            addr: None,
            volunteer: None,
            meet: Some(m.cookie),
        };
        let mut routes = Vec::new();
        let mut firsts: Vec<_> = relays
            .iter()
            .filter(|r| {
                r.identity.fingerprint() != m.relay
                    && r.identity.fingerprint() != dest
                    && self.can_pay(&r.identity)
            })
            .collect();
        firsts.sort_by_key(|_| u64::from_le_bytes(random_bytes()));
        for r1 in firsts.into_iter().take(2) {
            routes.push(OnionRoute {
                first: r1.identity,
                first_addr: Some(r1.addrs[0].clone()),
                hops: vec![
                    Hop {
                        fp: r1.identity.fingerprint(),
                        addr: None,
                        volunteer: Some(r1.identity),
                        meet: None,
                    },
                    Hop {
                        fp: m.relay,
                        addr: Some(m.addr.to_string()),
                        volunteer: pay_meeting,
                        meet: None,
                    },
                    end.clone(),
                ],
            });
        }
        // Straight to the meeting relay: it sees our address, but not who
        // we reach, and this works with no tokens at all. Its identity is
        // checked by the link's handshake against the fingerprint.
        if let Some(first) = meeting_relay.map(|r| r.identity).or_else(|| {
            self.contacts()
                .iter()
                .find(|c| c.fingerprint() == m.relay)
                .map(|c| c.key)
        }) {
            routes.push(OnionRoute {
                first,
                first_addr: Some(m.addr.to_string()),
                hops: vec![
                    Hop {
                        fp: m.relay,
                        addr: None,
                        volunteer: pay_meeting,
                        meet: None,
                    },
                    end,
                ],
            });
        } else if let Ok(first) = self
            .anon_dial(&m.addr.to_string(), Some(m.relay), true)
            .await
        {
            // A relay from someone else's directories: the link's
            // handshake tells us its identity, checked against the
            // fingerprint.
            routes.push(OnionRoute {
                first,
                first_addr: Some(m.addr.to_string()),
                hops: vec![
                    Hop {
                        fp: m.relay,
                        addr: None,
                        volunteer: None,
                        meet: None,
                    },
                    end,
                ],
            });
        }
        let mut last = NetError::NoRoute(format!("{dest} (meeting point)"));
        for route in routes {
            match tokio::time::timeout(MEET_FOR, self.meet_route(&route)).await {
                Ok(Ok(p)) if p.fingerprint() == dest => {
                    self.emit(Event::ReachNote {
                        note: format!("met {dest} at {}", m.relay),
                    });
                    return Ok(p);
                }
                Ok(Ok(_)) => last = NetError::NoRoute("the meeting point led elsewhere".into()),
                Ok(Err(e)) => last = e,
                Err(_) => last = NetError::Timeout,
            }
        }
        Err(last)
    }
}
