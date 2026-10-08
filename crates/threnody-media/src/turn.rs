//! A TURN server (RFC 8656) for exactly one client and one peer, on
//! loopback: how WebRTC's media reaches a Threnody call.
//!
//! WebRTC is configured to use only relay candidates, from this server, so
//! it never reveals or uses the device's own addresses. Everything it
//! sends to the peer's relayed address comes out of [`Turn::handle`] as
//! [`Output::ToPeer`], for the call to seal and carry; what the call
//! receives goes back in through [`Turn::from_peer`] as if it came from
//! that address. Each side's relayed address is a fixed documentation
//! address ([`relayed_address`]), so the two sides' candidates pair up.
//!
//! The server answers only to the credentials it was made with (random per
//! call) and only to the first client address that authenticates, so other
//! local programs can't use it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

const MAGIC: u32 = 0x2112_A442;
const HEADER: usize = 20;

mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
    pub const CHANNEL_BIND: u16 = 0x009;
}

mod class {
    pub const REQUEST: u16 = 0;
    pub const INDICATION: u16 = 1;
    pub const SUCCESS: u16 = 2;
    pub const ERROR: u16 = 3;
}

mod attr {
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const FINGERPRINT: u16 = 0x8028;
}

const REALM: &str = "threnody";
const LIFETIME_S: u32 = 3600;

/// The relayed address each side's WebRTC gets: one for the caller, one
/// for the callee (TEST-NET-1, RFC 5737; never routed). The port is high
/// because WebRTC ignores remote candidates on most ports below 1024.
pub fn relayed_address(caller: bool) -> SocketAddr {
    let last = if caller { 1 } else { 2 };
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)), 40_000)
}

fn message_type(method: u16, class: u16) -> u16 {
    (method & 0x000F)
        | ((method & 0x0070) << 1)
        | ((method & 0x0F80) << 2)
        | ((class & 1) << 4)
        | ((class & 2) << 7)
}

fn split_type(t: u16) -> (u16, u16) {
    let method = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let class = ((t >> 4) & 1) | ((t >> 7) & 2);
    (method, class)
}

/// A parsed STUN message: its type, transaction id and attributes (with
/// where each starts, for integrity checks).
struct Message<'a> {
    method: u16,
    class: u16,
    txid: [u8; 12],
    attrs: Vec<(u16, &'a [u8], usize)>,
    raw: &'a [u8],
}

impl<'a> Message<'a> {
    fn parse(b: &'a [u8]) -> Option<Self> {
        if b.len() < HEADER || b[0] & 0xC0 != 0 {
            return None;
        }
        let t = u16::from_be_bytes([b[0], b[1]]);
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        if u32::from_be_bytes(b[4..8].try_into().ok()?) != MAGIC || b.len() != HEADER + len {
            return None;
        }
        let (method, class) = split_type(t);
        let mut attrs = Vec::new();
        let mut at = HEADER;
        while at + 4 <= b.len() {
            let ty = u16::from_be_bytes([b[at], b[at + 1]]);
            let l = u16::from_be_bytes([b[at + 2], b[at + 3]]) as usize;
            let v = b.get(at + 4..at + 4 + l)?;
            attrs.push((ty, v, at));
            at += 4 + l.div_ceil(4) * 4;
        }
        Some(Self {
            method,
            class,
            txid: b[8..20].try_into().ok()?,
            attrs,
            raw: b,
        })
    }

    fn get(&self, ty: u16) -> Option<&'a [u8]> {
        self.attrs.iter().find(|a| a.0 == ty).map(|a| a.1)
    }

    /// Whether MESSAGE-INTEGRITY is present and right for `key`.
    fn verify(&self, key: &[u8; 16]) -> bool {
        let Some(&(_, mi, at)) = self.attrs.iter().find(|a| a.0 == attr::MESSAGE_INTEGRITY) else {
            return false;
        };
        // The length field covers up to and including MESSAGE-INTEGRITY.
        let mut covered = self.raw[..at].to_vec();
        let len = (at + 24 - HEADER) as u16;
        covered[2..4].copy_from_slice(&len.to_be_bytes());
        let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("any key length");
        mac.update(&covered);
        mac.verify_slice(mi).is_ok()
    }
}

/// Builds a STUN message.
struct Builder {
    b: Vec<u8>,
}

impl Builder {
    fn new(method: u16, class: u16, txid: &[u8; 12]) -> Self {
        let mut b = Vec::with_capacity(128);
        b.extend_from_slice(&message_type(method, class).to_be_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&MAGIC.to_be_bytes());
        b.extend_from_slice(txid);
        Self { b }
    }

    fn attr(mut self, ty: u16, v: &[u8]) -> Self {
        self.b.extend_from_slice(&ty.to_be_bytes());
        self.b.extend_from_slice(&(v.len() as u16).to_be_bytes());
        self.b.extend_from_slice(v);
        self.b.resize(self.b.len().div_ceil(4) * 4, 0);
        self.set_len();
        self
    }

    fn xor_addr(self, ty: u16, a: SocketAddr, txid: &[u8; 12]) -> Self {
        let v = xor_address(a, txid);
        self.attr(ty, &v)
    }

    fn set_len(&mut self) {
        let len = (self.b.len() - HEADER) as u16;
        self.b[2..4].copy_from_slice(&len.to_be_bytes());
    }

    /// Adds MESSAGE-INTEGRITY and FINGERPRINT.
    fn finish(mut self, key: Option<&[u8; 16]>) -> Vec<u8> {
        if let Some(key) = key {
            // The length must already count the integrity attribute.
            let len = (self.b.len() + 24 - HEADER) as u16;
            self.b[2..4].copy_from_slice(&len.to_be_bytes());
            let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("any key length");
            mac.update(&self.b);
            let tag = mac.finalize().into_bytes();
            self = self.attr(attr::MESSAGE_INTEGRITY, &tag);
        }
        let len = (self.b.len() + 8 - HEADER) as u16;
        self.b[2..4].copy_from_slice(&len.to_be_bytes());
        let crc = crc32(&self.b) ^ 0x5354_554E;
        self.attr(attr::FINGERPRINT, &crc.to_be_bytes()).b
    }
}

fn xor_address(a: SocketAddr, txid: &[u8; 12]) -> Vec<u8> {
    let port = a.port() ^ (MAGIC >> 16) as u16;
    let mut v = vec![0];
    match a.ip() {
        IpAddr::V4(ip) => {
            v.push(1);
            v.extend_from_slice(&port.to_be_bytes());
            let x = u32::from(ip) ^ MAGIC;
            v.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            v.push(2);
            v.extend_from_slice(&port.to_be_bytes());
            let mut mask = MAGIC.to_be_bytes().to_vec();
            mask.extend_from_slice(txid);
            v.extend(ip.octets().iter().zip(mask).map(|(a, m)| a ^ m));
        }
    }
    v
}

fn parse_xor_address(v: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes([*v.get(2)?, *v.get(3)?]) ^ (MAGIC >> 16) as u16;
    let ip = match v.get(1)? {
        1 => {
            let x = u32::from_be_bytes(v.get(4..8)?.try_into().ok()?) ^ MAGIC;
            IpAddr::V4(Ipv4Addr::from(x))
        }
        2 => {
            let mut mask = MAGIC.to_be_bytes().to_vec();
            mask.extend_from_slice(txid);
            let o: Vec<u8> = v.get(4..20)?.iter().zip(mask).map(|(a, m)| a ^ m).collect();
            let o: [u8; 16] = o.try_into().ok()?;
            IpAddr::from(o)
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// CRC-32 (IEEE), for STUN's FINGERPRINT.
fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c ^= u32::from(x);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// What [`Turn::handle`] wants done.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// Send this datagram back to the client at this address.
    ToClient(SocketAddr, Vec<u8>),
    /// Carry this payload to the peer.
    ToPeer(Vec<u8>),
}

pub struct Turn {
    username: String,
    key: [u8; 16],
    nonce: String,
    relayed: SocketAddr,
    peer: SocketAddr,
    /// The client that allocated; nobody else is served after that.
    client: Option<SocketAddr>,
    /// Channel number bound to the peer, if any.
    channel: Option<u16>,
}

impl Turn {
    /// A server for one call. `caller` picks which side's relayed address
    /// is ours (the other is the peer's).
    pub fn new(username: &str, password: &str, caller: bool) -> Self {
        let key: [u8; 16] = Md5::digest(format!("{username}:{REALM}:{password}")).into();
        let nonce: String = threnody_core::crypto::random_bytes::<12>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Self {
            username: username.to_owned(),
            key,
            nonce,
            relayed: relayed_address(caller),
            peer: relayed_address(!caller),
            client: None,
            channel: None,
        }
    }

    /// The client that allocated, once one has.
    pub fn client(&self) -> Option<SocketAddr> {
        self.client
    }

    /// Handles a datagram from a local client at `from`.
    pub fn handle(&mut self, from: SocketAddr, b: &[u8]) -> Option<Output> {
        if self.client.is_some_and(|c| c != from) {
            return None;
        }
        // ChannelData: 0b01 in the top bits.
        if b.first().is_some_and(|x| x & 0xC0 == 0x40) {
            let ch = u16::from_be_bytes([b[0], *b.get(1)?]);
            let len = u16::from_be_bytes([*b.get(2)?, *b.get(3)?]) as usize;
            if self.client.is_none() || Some(ch) != self.channel {
                return None;
            }
            return Some(Output::ToPeer(b.get(4..4 + len)?.to_vec()));
        }
        let m = Message::parse(b)?;
        match (m.method, m.class) {
            (method::BINDING, class::REQUEST) => Some(Output::ToClient(
                from,
                Builder::new(method::BINDING, class::SUCCESS, &m.txid)
                    .xor_addr(attr::XOR_MAPPED_ADDRESS, from, &m.txid)
                    .finish(None),
            )),
            (method::SEND, class::INDICATION) => {
                let to = parse_xor_address(m.get(attr::XOR_PEER_ADDRESS)?, &m.txid)?;
                (self.client.is_some() && to == self.peer)
                    .then(|| Output::ToPeer(m.get(attr::DATA).unwrap_or_default().to_vec()))
            }
            (_, class::REQUEST) => Some(Output::ToClient(from, self.request(from, &m))),
            _ => None,
        }
    }

    fn request(&mut self, from: SocketAddr, m: &Message<'_>) -> Vec<u8> {
        let nonce = self.nonce.clone();
        let err = |code: u16, reason: &str, challenge: bool| {
            let mut v = vec![0, 0, (code / 100) as u8, (code % 100) as u8];
            v.extend_from_slice(reason.as_bytes());
            let mut b = Builder::new(m.method, class::ERROR, &m.txid).attr(attr::ERROR_CODE, &v);
            if challenge {
                b = b
                    .attr(attr::REALM, REALM.as_bytes())
                    .attr(attr::NONCE, nonce.as_bytes());
            }
            b.finish(None)
        };
        let authed = m.get(attr::USERNAME) == Some(self.username.as_bytes())
            && m.get(attr::NONCE) == Some(self.nonce.as_bytes())
            && m.verify(&self.key);
        if !authed {
            return err(401, "Unauthorized", true);
        }
        let ok = || Builder::new(m.method, class::SUCCESS, &m.txid);
        let lifetime = |b: Builder| b.attr(attr::LIFETIME, &LIFETIME_S.to_be_bytes());
        let peer_ok = || {
            m.get(attr::XOR_PEER_ADDRESS)
                .and_then(|v| parse_xor_address(v, &m.txid))
                == Some(self.peer)
        };
        let reply = match m.method {
            method::ALLOCATE => {
                if self.client.is_some() {
                    return err(437, "Allocation Mismatch", false);
                }
                self.client = Some(from);
                lifetime(
                    ok().xor_addr(attr::XOR_RELAYED_ADDRESS, self.relayed, &m.txid)
                        .xor_addr(attr::XOR_MAPPED_ADDRESS, from, &m.txid),
                )
            }
            _ if self.client.is_none() => return err(437, "Allocation Mismatch", false),
            method::REFRESH => lifetime(ok()),
            // Only the peer's relayed address can be reached.
            method::CREATE_PERMISSION if peer_ok() => ok(),
            method::CHANNEL_BIND if peer_ok() => {
                let Some(ch) = m
                    .get(attr::CHANNEL_NUMBER)
                    .and_then(|v| v.get(..2))
                    .map(|v| u16::from_be_bytes([v[0], v[1]]))
                    .filter(|c| (0x4000..=0x7FFF).contains(c))
                else {
                    return err(400, "Bad Request", false);
                };
                self.channel = Some(ch);
                ok()
            }
            method::CREATE_PERMISSION | method::CHANNEL_BIND => {
                return err(403, "Forbidden", false);
            }
            _ => return err(400, "Bad Request", false),
        };
        reply.finish(Some(&self.key))
    }

    /// What to send the client for `payload` from the peer, once it has
    /// allocated.
    pub fn from_peer(&self, payload: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
        let client = self.client?;
        let out = match self.channel {
            Some(ch) => {
                let mut b = Vec::with_capacity(payload.len() + 4);
                b.extend_from_slice(&ch.to_be_bytes());
                b.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                b.extend_from_slice(payload);
                b
            }
            None => {
                let txid = threnody_core::crypto::random_bytes::<12>();
                Builder::new(method::DATA, class::INDICATION, &txid)
                    .xor_addr(attr::XOR_PEER_ADDRESS, self.peer, &txid)
                    .attr(attr::DATA, payload)
                    .finish(None)
            }
        };
        Some((client, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client's request, authenticated when `auth` is `(user, pass, nonce)`.
    fn request(method: u16, attrs: &[(u16, Vec<u8>)], auth: Option<(&str, &str, &str)>) -> Vec<u8> {
        let txid = [7u8; 12];
        let mut b = Builder::new(method, class::REQUEST, &txid);
        for (t, v) in attrs {
            b = b.attr(*t, v);
        }
        let Some((u, p, n)) = auth else {
            return b.finish(None);
        };
        let key: [u8; 16] = Md5::digest(format!("{u}:{REALM}:{p}")).into();
        b.attr(attr::USERNAME, u.as_bytes())
            .attr(attr::REALM, REALM.as_bytes())
            .attr(attr::NONCE, n.as_bytes())
            .finish(Some(&key))
    }

    fn reply(o: Option<Output>) -> Vec<u8> {
        match o {
            Some(Output::ToClient(_, b)) => b,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn types_and_crc() {
        assert_eq!(message_type(method::ALLOCATE, class::REQUEST), 0x0003);
        assert_eq!(message_type(method::ALLOCATE, class::SUCCESS), 0x0103);
        assert_eq!(message_type(method::ALLOCATE, class::ERROR), 0x0113);
        assert_eq!(message_type(method::DATA, class::INDICATION), 0x0017);
        assert_eq!(split_type(0x0113), (method::ALLOCATE, class::ERROR));
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let tx = [9u8; 12];
        for a in ["192.0.2.1:9", "[2001:db8::1]:4242"] {
            let a: SocketAddr = a.parse().unwrap();
            assert_eq!(parse_xor_address(&xor_address(a, &tx), &tx), Some(a));
        }
    }

    #[test]
    fn allocates_only_with_credentials_and_relays_to_the_peer() {
        let mut t = Turn::new("u", "secret", true);
        let client: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        // REQUESTED-TRANSPORT: UDP (17).
        let transport = vec![17, 0, 0, 0];

        // Unauthenticated: challenged with realm and nonce.
        let r = reply(t.handle(
            client,
            &request(method::ALLOCATE, &[(0x0019, transport.clone())], None),
        ));
        let m = Message::parse(&r).unwrap();
        assert_eq!((m.method, m.class), (method::ALLOCATE, class::ERROR));
        let nonce = std::str::from_utf8(m.get(attr::NONCE).unwrap())
            .unwrap()
            .to_owned();

        // Wrong password: challenged again.
        let bad = request(
            method::ALLOCATE,
            &[(0x0019, transport.clone())],
            Some(("u", "nope", &nonce)),
        );
        assert_eq!(
            Message::parse(&reply(t.handle(client, &bad)))
                .unwrap()
                .class,
            class::ERROR
        );

        let good = request(
            method::ALLOCATE,
            &[(0x0019, transport.clone())],
            Some(("u", "secret", &nonce)),
        );
        let r = reply(t.handle(client, &good));
        let m = Message::parse(&r).unwrap();
        assert_eq!(m.class, class::SUCCESS);
        assert!(m.verify(&t.key), "the reply is authenticated");
        assert_eq!(
            parse_xor_address(m.get(attr::XOR_RELAYED_ADDRESS).unwrap(), &m.txid),
            Some(relayed_address(true))
        );
        assert_eq!(t.client(), Some(client));

        // Someone else on this machine is ignored now.
        let other: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        assert_eq!(t.handle(other, &good), None);

        // Permission and channel only for the peer's relayed address.
        let peer = xor_address(relayed_address(false), &[7; 12]);
        let elsewhere = xor_address("198.51.100.1:9".parse().unwrap(), &[7; 12]);
        let auth = Some(("u", "secret", nonce.as_str()));
        let perm = |p: &Vec<u8>| {
            request(
                method::CREATE_PERMISSION,
                &[(attr::XOR_PEER_ADDRESS, p.clone())],
                auth,
            )
        };
        assert_eq!(
            Message::parse(&reply(t.handle(client, &perm(&peer))))
                .unwrap()
                .class,
            class::SUCCESS
        );
        assert_eq!(
            Message::parse(&reply(t.handle(client, &perm(&elsewhere))))
                .unwrap()
                .class,
            class::ERROR
        );

        // Before a channel: Send indications out, Data indications in.
        let send = Builder::new(method::SEND, class::INDICATION, &[7; 12])
            .attr(attr::XOR_PEER_ADDRESS, &peer)
            .attr(attr::DATA, b"rtp")
            .finish(None);
        assert_eq!(
            t.handle(client, &send),
            Some(Output::ToPeer(b"rtp".to_vec()))
        );
        let (to, data) = t.from_peer(b"back").unwrap();
        assert_eq!(to, client);
        let m = Message::parse(&data).unwrap();
        assert_eq!((m.method, m.class), (method::DATA, class::INDICATION));
        assert_eq!(m.get(attr::DATA), Some(&b"back"[..]));

        // With a channel: ChannelData both ways.
        let bind = request(
            method::CHANNEL_BIND,
            &[
                (attr::CHANNEL_NUMBER, vec![0x40, 0x01, 0, 0]),
                (attr::XOR_PEER_ADDRESS, peer.clone()),
            ],
            auth,
        );
        assert_eq!(
            Message::parse(&reply(t.handle(client, &bind)))
                .unwrap()
                .class,
            class::SUCCESS
        );
        assert_eq!(
            t.handle(client, &[0x40, 0x01, 0, 3, b'a', b'b', b'c']),
            Some(Output::ToPeer(b"abc".to_vec()))
        );
        assert_eq!(
            t.handle(client, &[0x40, 0x02, 0, 1, b'x']),
            None,
            "unbound channel"
        );
        assert_eq!(
            t.from_peer(b"hi").unwrap().1,
            [0x40, 0x01, 0, 2, b'h', b'i']
        );

        // STUN binding requests are answered without credentials.
        let bind_req = Builder::new(method::BINDING, class::REQUEST, &[1; 12]).finish(None);
        let r = reply(t.handle(client, &bind_req));
        let m = Message::parse(&r).unwrap();
        assert_eq!((m.method, m.class), (method::BINDING, class::SUCCESS));
    }
}
