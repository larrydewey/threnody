//! Swapping an invite link for a fingerprint by tapping devices (Appendix T).
//!
//! The phone showing its invite is the *card* (Host Card Emulation on
//! Android); the device that taps it is the *reader*. The reader gets the
//! card's ordinary invite link and gives back only its own fingerprint,
//! both sealed under keys from a one-time X-Wing encapsulation, so a
//! passive eavesdropper learns neither.
//!
//! ```text
//! AID       = F0 74 68 72 65 6E 6F 64 79                  -- 0xF0 || "threnody"
//! SELECT    R→C  00 A4 04 00 09 AID 00
//!           C→R  "TN" max_version flags 90 00
//! OFFER     R→C  80 D0 P1=i P2=(ver<<1 | last) Lc data     -- chunk i of ek (1216 B)
//!           C→R  90 00
//! FETCH     R→C  80 D2 P1=i 00 00
//!           C→R  more || chunk_i || 90 00                  -- chunk i of ct || box_C
//! GIVE      R→C  80 D4 00 00 24 box_R
//!           C→R  90 00
//!
//! h           = KDF(TAP_TRANSCRIPT, [AID, [ver], ek, ct])
//! k_C || k_R  = KDF(TAP_KEYS, [ss, h])
//! box_C       = ChaCha20-Poly1305(k_C, 0^12, h, pad(link_C))
//! box_R       = ChaCha20-Poly1305(k_R, 0^12, h, fp_R)
//! pad(l)      = u16_be(len l) || l || 0^(MAX_LINK - len l)
//! ```
//!
//! Everything here is sans-IO: the apps only move bytes between this
//! module and their NFC stack.

use zeroize::Zeroizing;

use crate::crypto::aead::Suite;
use crate::crypto::hybrid::{CIPHERTEXT_LEN, HybridPublic, HybridSecret, PUBLIC_LEN};
use crate::crypto::kdf::{derive, label};
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::identity::Fingerprint;
use crate::invite::InviteLink;

/// The application identifier: 0xF0 || "threnody" (a proprietary AID).
/// The Android and iOS manifests list it as `F0746872656E6F6479`.
pub const AID: [u8; 9] = [0xF0, b't', b'h', b'r', b'e', b'n', b'o', b'd', b'y'];
/// The highest tap protocol version this implementation speaks.
pub const VERSION: u8 = 1;
/// Data bytes per APDU: short APDUs only.
pub const CHUNK: usize = 240;
/// The longest invite link a card hands over, in UTF-8 bytes.
pub const MAX_LINK: usize = 512;
const PADDED: usize = 2 + MAX_LINK;
const TAG: usize = 16;
const BOX_C: usize = PADDED + TAG;
const BOX_R: usize = 20 + TAG;
const REPLY: usize = CIPHERTEXT_LEN + BOX_C;
const OFFER_CHUNKS: usize = PUBLIC_LEN.div_ceil(CHUNK);
const REPLY_CHUNKS: usize = REPLY.div_ceil(CHUNK);
/// Round trips `run` allows before giving up (a full tap takes 15).
const MAX_ROUND_TRIPS: usize = 32;
const NONCE: [u8; 12] = [0; 12];

const CLA_ISO: u8 = 0x00;
const CLA_PROPRIETARY: u8 = 0x80;
const INS_SELECT: u8 = 0xA4;
const INS_OFFER: u8 = 0xD0;
const INS_FETCH: u8 = 0xD2;
const INS_GIVE: u8 = 0xD4;

pub const SW_OK: [u8; 2] = [0x90, 0x00];
/// A SELECT of another application.
pub const SW_NOT_FOUND: [u8; 2] = [0x6A, 0x82];
/// An instruction this card doesn't know.
pub const SW_INS: [u8; 2] = [0x6D, 0x00];
/// Out of order, nothing selected, or the exchange is already done.
pub const SW_STATE: [u8; 2] = [0x69, 0x85];
/// Bad lengths, or a sealed box that doesn't open.
pub const SW_DATA: [u8; 2] = [0x6A, 0x80];
/// A FETCH index out of range, or an OFFER in a version the card doesn't speak.
pub const SW_P1P2: [u8; 2] = [0x6A, 0x86];

/// Why a tap didn't complete. Apps map each to a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The tapped thing isn't a device showing a Threnody invite.
    NotAnInvite,
    /// The card speaks only versions this reader doesn't.
    Unsupported,
    /// The card broke the protocol, or what it sent doesn't open.
    Garbled,
    /// The devices separated before the reader had the card's link.
    Moved,
}

/// The phone showing its invite (HCE on Android): it answers each
/// command APDU and, once an exchange completes, knows the reader's
/// fingerprint. One exchange per card: after that it refuses everything.
pub struct Card {
    own: Vec<u8>,
    own_fp: Fingerprint,
    /// Fixed encapsulation randomness (`with_seed`), or None for fresh
    /// randomness on every exchange.
    seed: Option<Zeroizing<[u8; 64]>>,
    state: CardState,
}

enum CardState {
    Idle,
    Selected,
    Offer {
        ver: u8,
        ek: Vec<u8>,
    },
    Reply {
        reply: Vec<u8>,
        k_r: Zeroizing<[u8; 32]>,
        h: [u8; 32],
    },
    Done(Fingerprint),
    Failed,
}

impl Card {
    /// A card handing out `own`. Errors if its text is longer than
    /// [`MAX_LINK`] bytes.
    pub fn new(own: &InviteLink) -> Result<Self> {
        Self::build(own, None)
    }

    /// Deterministic encapsulation, as X-Wing `EncapsulateDerand`: every
    /// exchange uses `eseed`. For tests and fuzzing only.
    pub fn with_seed(own: &InviteLink, eseed: [u8; 64]) -> Result<Self> {
        Self::build(own, Some(Zeroizing::new(eseed)))
    }

    fn build(own: &InviteLink, seed: Option<Zeroizing<[u8; 64]>>) -> Result<Self> {
        let own_text = own.to_string().into_bytes();
        if own_text.len() > MAX_LINK {
            return Err(Error::Malformed("invite link too long to tap"));
        }
        Ok(Self {
            own: own_text,
            own_fp: own.fingerprint,
            seed,
            state: CardState::Idle,
        })
    }

    /// The tapping device's fingerprint, once an exchange completed.
    pub fn finished(&self) -> Option<Fingerprint> {
        match self.state {
            CardState::Done(fp) => Some(fp),
            _ => None,
        }
    }

    /// One command APDU in, one response APDU (with status word) out.
    /// Never panics; any response is 2 to 243 bytes.
    pub fn process(&mut self, apdu: &[u8]) -> Vec<u8> {
        if matches!(self.state, CardState::Done(_)) {
            return SW_STATE.to_vec();
        }
        let Some(cmd) = parse(apdu) else {
            return self.fail(SW_DATA);
        };
        match (cmd.cla, cmd.ins) {
            (CLA_ISO, INS_SELECT) => self.select(&cmd),
            (CLA_PROPRIETARY, INS_OFFER) => self.offer(&cmd),
            (CLA_PROPRIETARY, INS_FETCH) => self.fetch(&cmd),
            (CLA_PROPRIETARY, INS_GIVE) => self.give(&cmd),
            _ => self.fail(SW_INS),
        }
    }

    fn fail(&mut self, sw: [u8; 2]) -> Vec<u8> {
        self.state = CardState::Failed;
        sw.to_vec()
    }

    fn select(&mut self, cmd: &Apdu) -> Vec<u8> {
        if cmd.p1 != 0x04 || cmd.data != AID {
            return self.fail(SW_NOT_FOUND);
        }
        self.state = CardState::Selected;
        let mut r = b"TN".to_vec();
        r.extend_from_slice(&[VERSION, 0]);
        r.extend_from_slice(&SW_OK);
        r
    }

    fn offer(&mut self, cmd: &Apdu) -> Vec<u8> {
        let (ver, last) = (cmd.p2 >> 1, cmd.p2 & 1 == 1);
        if matches!(self.state, CardState::Selected) && cmd.p1 == 0 {
            if ver == 0 || ver > VERSION {
                return self.fail(SW_P1P2);
            }
            self.state = CardState::Offer {
                ver,
                ek: Vec::with_capacity(PUBLIC_LEN),
            };
        }
        let (have, offered) = match &self.state {
            CardState::Offer { ver, ek } => (ek.len(), *ver),
            _ => return self.fail(SW_STATE),
        };
        if usize::from(cmd.p1) * CHUNK != have {
            return self.fail(SW_STATE);
        }
        if offered != ver {
            return self.fail(SW_P1P2);
        }
        // Whole chunks, then a final one that brings ek to exactly its length.
        let len = have + cmd.data.len();
        let fits = if last {
            !cmd.data.is_empty() && len == PUBLIC_LEN
        } else {
            cmd.data.len() == CHUNK && len < PUBLIC_LEN
        };
        if !fits {
            return self.fail(SW_DATA);
        }
        let CardState::Offer { ek, .. } = &mut self.state else {
            return self.fail(SW_STATE);
        };
        ek.extend_from_slice(cmd.data);
        if !last {
            return SW_OK.to_vec();
        }
        let ek = std::mem::take(ek);
        match self.encapsulate(ver, &ek) {
            Some(state) => {
                self.state = state;
                SW_OK.to_vec()
            }
            None => self.fail(SW_DATA),
        }
    }

    fn encapsulate(&self, ver: u8, ek: &[u8]) -> Option<CardState> {
        let public = HybridPublic::from_bytes(ek).ok()?;
        let eseed = match &self.seed {
            Some(s) => s.clone(),
            None => Zeroizing::new(random_bytes::<64>()),
        };
        let (ct, ss) = public.encapsulate_derand(&eseed).ok()?;
        let h = transcript(ver, ek, &ct);
        let (k_c, k_r) = keys(&ss, &h);
        let mut reply = ct;
        reply.extend(Suite::ChaCha20Poly1305.seal(&k_c, &NONCE, &h, &pad(&self.own)?));
        Some(CardState::Reply { reply, k_r, h })
    }

    fn fetch(&mut self, cmd: &Apdu) -> Vec<u8> {
        if !matches!(self.state, CardState::Reply { .. }) {
            return self.fail(SW_STATE);
        }
        if !cmd.data.is_empty() || cmd.p2 != 0 {
            return self.fail(SW_DATA);
        }
        let i = usize::from(cmd.p1);
        let chunk = match &self.state {
            CardState::Reply { reply, .. } if i < REPLY_CHUNKS => {
                let end = ((i + 1) * CHUNK).min(reply.len());
                reply
                    .get(i * CHUNK..end)
                    .map(|c| (end < reply.len(), c.to_vec()))
            }
            _ => None,
        };
        let Some((more, chunk)) = chunk else {
            return self.fail(SW_P1P2);
        };
        let mut r = Vec::with_capacity(1 + chunk.len() + 2);
        r.push(u8::from(more));
        r.extend_from_slice(&chunk);
        r.extend_from_slice(&SW_OK);
        r
    }

    fn give(&mut self, cmd: &Apdu) -> Vec<u8> {
        let CardState::Reply { k_r, h, .. } = &self.state else {
            return self.fail(SW_STATE);
        };
        let fp = (cmd.data.len() == BOX_R && cmd.p1 == 0 && cmd.p2 == 0)
            .then(|| Suite::ChaCha20Poly1305.open(k_r, &NONCE, h, cmd.data).ok())
            .flatten()
            .and_then(|b| <[u8; 20]>::try_from(b.as_slice()).ok())
            .map(Fingerprint);
        match fp {
            Some(fp) if fp != self.own_fp => {
                self.state = CardState::Done(fp);
                SW_OK.to_vec()
            }
            _ => self.fail(SW_DATA),
        }
    }
}

/// What a reader does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Transmit this command APDU and pass the response to `step`.
    Send(Vec<u8>),
    /// The card has our fingerprint and we have its link.
    Done(InviteLink),
    Failed(Failure),
}

/// How a whole tap ended (`run`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `mutual`: the card got our fingerprint too (false: the tag went
    /// away after we had its link but before it had ours).
    Tapped {
        link: InviteLink,
        mutual: bool,
    },
    Failed(Failure),
}

/// The device that taps (reader mode on Android, Core NFC on iOS, PC/SC
/// on Linux). It reveals only `own`, its fingerprint.
pub struct Reader {
    own: Fingerprint,
    eph: HybridSecret,
    state: ReaderState,
    link_c: Option<InviteLink>,
}

enum ReaderState {
    Idle,
    Selecting,
    Offering { ver: u8, next: usize },
    Fetching { ver: u8, got: Vec<u8> },
    Giving,
    Over,
}

impl Reader {
    /// A reader with a fresh ephemeral key.
    pub fn new(own: Fingerprint) -> Self {
        Self::with_secret(own, HybridSecret::generate())
    }

    /// A reader with a given ephemeral key (`HybridSecret::from_seed` in
    /// tests and fuzzing).
    pub fn with_secret(own: Fingerprint, eph: HybridSecret) -> Self {
        Self {
            own,
            eph,
            state: ReaderState::Idle,
            link_c: None,
        }
    }

    /// The SELECT APDU that begins an exchange. Resets any earlier one.
    pub fn start(&mut self) -> Vec<u8> {
        self.state = ReaderState::Selecting;
        self.link_c = None;
        let mut a = vec![CLA_ISO, INS_SELECT, 0x04, 0x00, AID.len() as u8];
        a.extend_from_slice(&AID);
        a.push(0x00);
        a
    }

    /// The card's response (data and status word) to the last command.
    pub fn step(&mut self, response: &[u8]) -> Step {
        let state = std::mem::replace(&mut self.state, ReaderState::Over);
        let (data, ok) = match response.split_last_chunk::<2>() {
            Some((data, sw)) => (data, *sw == SW_OK),
            None => (&[][..], false),
        };
        match state {
            ReaderState::Selecting => {
                let Some((b"TN", [max, _flags, ..])) = data.split_first_chunk::<2>() else {
                    return Step::Failed(Failure::NotAnInvite);
                };
                if !ok {
                    return Step::Failed(Failure::NotAnInvite);
                }
                if *max == 0 {
                    return Step::Failed(Failure::Unsupported);
                }
                self.offer((*max).min(VERSION), 0)
            }
            ReaderState::Offering { ver, next } if ok && data.is_empty() => {
                if next < OFFER_CHUNKS {
                    self.offer(ver, next)
                } else {
                    self.state = ReaderState::Fetching {
                        ver,
                        got: Vec::with_capacity(REPLY),
                    };
                    Step::Send(fetch(0))
                }
            }
            // A card that has dropped our version still names its own
            // highest in SELECT; it refuses the version on the first chunk.
            ReaderState::Offering { next: 1, .. } if response == SW_P1P2 => {
                Step::Failed(Failure::Unsupported)
            }
            ReaderState::Fetching { ver, mut got } if ok => {
                let Some((&more, chunk)) = data.split_first() else {
                    return Step::Failed(Failure::Garbled);
                };
                if more > 1 || chunk.len() > CHUNK || got.len() + chunk.len() > REPLY {
                    return Step::Failed(Failure::Garbled);
                }
                got.extend_from_slice(chunk);
                if more == 1 {
                    let i = got.len() / CHUNK;
                    if chunk.len() != CHUNK || i >= REPLY_CHUNKS {
                        return Step::Failed(Failure::Garbled);
                    }
                    self.state = ReaderState::Fetching { ver, got };
                    return Step::Send(fetch(i as u8));
                }
                match self.open(ver, &got) {
                    Some(give) => {
                        self.state = ReaderState::Giving;
                        Step::Send(give)
                    }
                    None => Step::Failed(Failure::Garbled),
                }
            }
            ReaderState::Giving if ok => match &self.link_c {
                Some(l) => Step::Done(l.clone()),
                None => Step::Failed(Failure::Garbled),
            },
            _ => Step::Failed(Failure::Garbled),
        }
    }

    /// The tag went away: the card's link if its reply had opened, else None.
    pub fn lost(&self) -> Option<InviteLink> {
        self.link_c.clone()
    }

    fn offer(&mut self, ver: u8, i: usize) -> Step {
        let ek = self.eph.public().as_bytes();
        let start = i * CHUNK;
        let end = (start + CHUNK).min(PUBLIC_LEN);
        let Some(chunk) = ek.get(start..end) else {
            return Step::Failed(Failure::Garbled);
        };
        let last = end == PUBLIC_LEN;
        let mut a = vec![
            CLA_PROPRIETARY,
            INS_OFFER,
            i as u8,
            (ver << 1) | u8::from(last),
            chunk.len() as u8,
        ];
        a.extend_from_slice(chunk);
        self.state = ReaderState::Offering { ver, next: i + 1 };
        Step::Send(a)
    }

    /// Opens the card's reply; returns the GIVE APDU.
    fn open(&mut self, ver: u8, reply: &[u8]) -> Option<Vec<u8>> {
        if reply.len() != REPLY {
            return None;
        }
        let (ct, box_c) = reply.split_at(CIPHERTEXT_LEN);
        let ss = self.eph.decapsulate(ct).ok()?;
        let h = transcript(ver, self.eph.public().as_bytes(), ct);
        let (k_c, k_r) = keys(&ss, &h);
        let padded = Zeroizing::new(Suite::ChaCha20Poly1305.open(&k_c, &NONCE, &h, box_c).ok()?);
        let link: InviteLink = unpad(&padded)?.parse().ok()?;
        if link.fingerprint == self.own {
            return None;
        }
        self.link_c = Some(link);
        let box_r = Suite::ChaCha20Poly1305.seal(&k_r, &NONCE, &h, &self.own.0);
        let mut a = vec![CLA_PROPRIETARY, INS_GIVE, 0, 0, box_r.len() as u8];
        a.extend_from_slice(&box_r);
        Some(a)
    }
}

/// Drives `reader` through a whole exchange. `transceive` sends one
/// command APDU and returns the response, or None when the tag is gone.
/// Gives up after a bounded number of round trips.
pub fn run(reader: &mut Reader, mut transceive: impl FnMut(&[u8]) -> Option<Vec<u8>>) -> Outcome {
    let mut apdu = reader.start();
    for _ in 0..MAX_ROUND_TRIPS {
        let Some(response) = transceive(&apdu) else {
            return match reader.lost() {
                Some(link) => Outcome::Tapped {
                    link,
                    mutual: false,
                },
                None => Outcome::Failed(Failure::Moved),
            };
        };
        match reader.step(&response) {
            Step::Send(next) => apdu = next,
            Step::Done(link) => return Outcome::Tapped { link, mutual: true },
            Step::Failed(f) => return Outcome::Failed(f),
        }
    }
    Outcome::Failed(Failure::Garbled)
}

fn fetch(i: u8) -> Vec<u8> {
    vec![CLA_PROPRIETARY, INS_FETCH, i, 0, 0]
}

fn transcript(ver: u8, ek: &[u8], ct: &[u8]) -> [u8; 32] {
    derive(label::TAP_TRANSCRIPT, &[&AID, &[ver], ek, ct])
}

fn keys(ss: &[u8; 32], h: &[u8; 32]) -> (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>) {
    let k = Zeroizing::new(derive::<64>(label::TAP_KEYS, &[ss, h]));
    let (mut k_c, mut k_r) = (Zeroizing::new([0; 32]), Zeroizing::new([0; 32]));
    k_c.copy_from_slice(&k[..32]);
    k_r.copy_from_slice(&k[32..]);
    (k_c, k_r)
}

fn pad(link: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let len = u16::try_from(link.len())
        .ok()
        .filter(|&l| usize::from(l) <= MAX_LINK)?;
    let mut p = Zeroizing::new(Vec::with_capacity(PADDED));
    p.extend_from_slice(&len.to_be_bytes());
    p.extend_from_slice(link);
    p.resize(PADDED, 0);
    Some(p)
}

fn unpad(p: &[u8]) -> Option<&str> {
    let ([hi, lo], rest) = p.split_first_chunk::<2>().map(|(l, r)| (*l, r))?;
    let len = usize::from(u16::from_be_bytes([hi, lo]));
    if p.len() != PADDED || len > MAX_LINK {
        return None;
    }
    let (link, zeros) = rest.split_at_checked(len)?;
    if zeros.iter().any(|&b| b != 0) {
        return None;
    }
    std::str::from_utf8(link).ok()
}

/// A short command APDU.
struct Apdu<'a> {
    cla: u8,
    ins: u8,
    p1: u8,
    p2: u8,
    data: &'a [u8],
}

/// Short APDUs only (cases 1 to 4: Lc up to 255, optional Le).
fn parse(apdu: &[u8]) -> Option<Apdu<'_>> {
    let (&[cla, ins, p1, p2], body) = apdu.split_first_chunk::<4>()?;
    let data = match body {
        // Case 1 (no body) and case 2 (Le only).
        [] | [_] => &[][..],
        [lc, rest @ ..] => {
            let lc = usize::from(*lc);
            // Lc = 0 here would start an extended APDU.
            if lc == 0 || !(rest.len() == lc || rest.len() == lc + 1) {
                return None;
            }
            rest.get(..lc)?
        }
    };
    Some(Apdu {
        cla,
        ins,
        p1,
        p2,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::invite::InviteKey;

    type Transcript = Vec<(Vec<u8>, Vec<u8>)>;

    fn link_of(fp: Fingerprint, addrs: &[&str]) -> InviteLink {
        InviteLink {
            fingerprint: fp,
            addrs: addrs.iter().map(|a| (*a).to_owned()).collect(),
            key: Some(InviteKey::from_bytes(&[0x42; 16]).unwrap()),
        }
    }

    fn fp(seed: u8) -> Fingerprint {
        Identity::from_seed(&[seed; 32]).public().fingerprint()
    }

    fn card_link() -> InviteLink {
        link_of(fp(1), &["[2001:db8::7]:7450", "198.51.100.4:7450"])
    }

    /// Runs `reader` against `card`; the tag goes away after `drop_after`
    /// commands, if given.
    fn run_pair(
        card: &mut Card,
        reader: &mut Reader,
        drop_after: Option<usize>,
    ) -> (Transcript, Outcome) {
        let mut t = Vec::new();
        let outcome = run(reader, |cmd| {
            if drop_after.is_some_and(|n| t.len() >= n) {
                return None;
            }
            let resp = card.process(cmd);
            t.push((cmd.to_vec(), resp.clone()));
            Some(resp)
        });
        (t, outcome)
    }

    fn sw(r: &[u8]) -> [u8; 2] {
        *r.last_chunk::<2>().unwrap()
    }

    fn seeded() -> (Card, Reader, InviteLink) {
        let link = card_link();
        let card = Card::with_seed(&link, [5; 64]).unwrap();
        let reader = Reader::with_secret(fp(2), HybridSecret::from_seed(&[3; 32]));
        (card, reader, link)
    }

    fn select() -> Vec<u8> {
        Reader::new(fp(9)).start()
    }

    /// A card that has been sent SELECT and the whole OFFER, with the
    /// commands that got it there.
    fn offered() -> (Card, Vec<Vec<u8>>) {
        let (mut card, mut reader, _) = seeded();
        let mut cmds = vec![reader.start()];
        let mut resp = card.process(&cmds[0]);
        for _ in 0..OFFER_CHUNKS {
            let Step::Send(c) = reader.step(&resp) else {
                panic!("expected an OFFER")
            };
            resp = card.process(&c);
            cmds.push(c);
        }
        assert_eq!(sw(&resp), SW_OK);
        (card, cmds)
    }

    #[test]
    fn tap_gives_link_and_fingerprint() {
        let link = card_link();
        let mut card = Card::new(&link).unwrap();
        let mut reader = Reader::new(fp(2));
        let (t, outcome) = run_pair(&mut card, &mut reader, None);
        assert_eq!(outcome, Outcome::Tapped { link, mutual: true });
        assert_eq!(card.finished(), Some(fp(2)));
        assert_eq!(t.len(), 15);
        assert_eq!(1 + OFFER_CHUNKS + REPLY_CHUNKS + 1, 15);
    }

    #[test]
    fn eavesdropper_sees_neither() {
        let short = link_of(fp(1), &["192.0.2.1:7450"]);
        let long = link_of(
            fp(1),
            &[
                "[2001:db8::7]:7450",
                "198.51.100.4:7450",
                "192.168.1.20:7450",
                "a-rather-long-host-name.example.org:7450",
            ],
        );
        let mut sizes = Vec::new();
        for link in [&short, &long] {
            let mut card = Card::new(link).unwrap();
            let mut reader = Reader::new(fp(2));
            let (t, outcome) = run_pair(&mut card, &mut reader, None);
            assert!(matches!(outcome, Outcome::Tapped { mutual: true, .. }));
            let all: Vec<u8> = t
                .iter()
                .flat_map(|(c, r)| c.iter().chain(r))
                .copied()
                .collect();
            let has = |needle: &[u8]| all.windows(needle.len()).any(|w| w == needle);
            assert!(!has(link.fingerprint.compact().as_bytes()));
            assert!(!has(&link.fingerprint.0));
            assert!(!has(link.key.unwrap().to_string().as_bytes()));
            for a in &link.addrs {
                assert!(!has(a.as_bytes()), "{a}");
            }
            assert!(!has(b"threnody://"));
            assert!(!has(&fp(2).0));
            assert!(!has(fp(2).compact().as_bytes()));
            sizes.push(t.iter().map(|(c, r)| c.len() + r.len()).collect::<Vec<_>>());
        }
        assert_eq!(sizes[0], sizes[1], "padding hides the link's length");
    }

    #[test]
    fn card_needs_select_first() {
        let (_, cmds) = offered();
        for c in [cmds[1].clone(), fetch(0), vec![0x80, INS_GIVE, 0, 0, 1, 0]] {
            let mut card = Card::new(&card_link()).unwrap();
            assert_eq!(card.process(&c), SW_STATE);
        }
        let mut card = Card::new(&card_link()).unwrap();
        let mut other = select();
        other[5] ^= 1;
        assert_eq!(card.process(&other), SW_NOT_FOUND);
        assert_eq!(card.process(&[0x80, 0x10, 0, 0]), SW_INS);
        assert_eq!(card.process(&select()), [b'T', b'N', 1, 0, 0x90, 0]);
        assert_eq!(card.process(&[0x80, 0x10, 0, 0]), SW_INS);
    }

    #[test]
    fn out_of_order_chunks_fail_until_reselected() {
        let (_, cmds) = offered();
        let mut card = Card::new(&card_link()).unwrap();
        card.process(&cmds[0]);
        assert_eq!(card.process(&cmds[1]), SW_OK);
        assert_eq!(card.process(&cmds[3]), SW_STATE, "skipped chunk 1");
        assert_eq!(card.process(&cmds[2]), SW_STATE, "failed until SELECT");
        assert_eq!(card.process(&fetch(0)), SW_STATE);
        let mut reader = Reader::new(fp(2));
        let (_, outcome) = run_pair(&mut card, &mut reader, None);
        assert!(matches!(outcome, Outcome::Tapped { mutual: true, .. }));
    }

    #[test]
    fn wrong_key_length_is_refused() {
        let (_, cmds) = offered();
        let last = cmds.last().unwrap();
        for extra in [-1i32, 1] {
            let mut card = Card::new(&card_link()).unwrap();
            for c in &cmds[..cmds.len() - 1] {
                assert_eq!(sw(&card.process(c)), SW_OK);
            }
            let mut c = last.clone();
            if extra < 0 {
                c.pop();
            } else {
                c.push(0);
            }
            c[4] = (c.len() - 5) as u8;
            assert_eq!(card.process(&c), SW_DATA);
        }
        // A short middle chunk is refused too.
        let mut card = Card::new(&card_link()).unwrap();
        card.process(&cmds[0]);
        let mut c = cmds[1].clone();
        c.pop();
        c[4] -= 1;
        assert_eq!(card.process(&c), SW_DATA);
    }

    fn replied() -> (Card, Reader) {
        let (mut card, mut reader, _) = seeded();
        let (t, outcome) = run_pair(&mut card, &mut reader, Some(14));
        assert_eq!(t.len(), 14);
        assert!(matches!(outcome, Outcome::Tapped { mutual: false, .. }));
        (card, reader)
    }

    #[test]
    fn wrong_give_length_is_refused() {
        for len in [35u8, 37] {
            let (mut card, _) = replied();
            let mut c = vec![0x80, INS_GIVE, 0, 0, len];
            c.extend(std::iter::repeat_n(7, usize::from(len)));
            assert_eq!(card.process(&c), SW_DATA);
            assert_eq!(card.finished(), None);
        }
    }

    fn give_of(reader: &mut Reader, card: &mut Card) -> Vec<u8> {
        // Replay the FETCHes through the reader to get its GIVE.
        let mut resp = card.process(&reader.start());
        loop {
            match reader.step(&resp) {
                Step::Send(c) if c[1] == INS_GIVE => return c,
                Step::Send(c) => resp = card.process(&c),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn tampered_give_is_refused() {
        let (mut card, mut reader, _) = seeded();
        let mut give = give_of(&mut reader, &mut card);
        give[10] ^= 0x01;
        assert_eq!(card.process(&give), SW_DATA);
        assert_eq!(card.finished(), None);
    }

    #[test]
    fn tampered_reply_is_refused_by_reader() {
        let (mut card, mut reader, _) = seeded();
        let outcome = run(&mut reader, |cmd| {
            let mut r = card.process(cmd);
            if cmd[1] == INS_FETCH && cmd[2] == 6 {
                r[3] ^= 0x10;
            }
            Some(r)
        });
        assert_eq!(outcome, Outcome::Failed(Failure::Garbled));
        assert_eq!(card.finished(), None);
    }

    #[test]
    fn give_from_another_tap_is_refused() {
        let link = card_link();
        let mut card1 = Card::new(&link).unwrap();
        let mut reader1 = Reader::new(fp(2));
        let give = give_of(&mut reader1, &mut card1);
        let mut card2 = Card::new(&link).unwrap();
        let mut reader2 = Reader::new(fp(2));
        let _ = give_of(&mut reader2, &mut card2);
        assert_eq!(card2.process(&give), SW_DATA);
        assert_eq!(card2.finished(), None);
    }

    #[test]
    fn card_serves_one_exchange() {
        let (mut card, mut reader, _) = seeded();
        let (_, outcome) = run_pair(&mut card, &mut reader, None);
        assert!(matches!(outcome, Outcome::Tapped { mutual: true, .. }));
        assert_eq!(card.process(&select()), SW_STATE);
        let mut second = Reader::new(fp(3));
        let (_, outcome) = run_pair(&mut card, &mut second, None);
        assert_eq!(outcome, Outcome::Failed(Failure::NotAnInvite));
        assert_eq!(card.finished(), Some(fp(2)));
    }

    #[test]
    fn oversized_links_are_refused() {
        let host = "h".repeat(120);
        let addrs: Vec<String> = (0..4).map(|i| format!("{host}{i}:7450")).collect();
        let refs: Vec<&str> = addrs.iter().map(String::as_str).collect();
        let link = link_of(fp(1), &refs);
        assert!(link.to_string().len() > MAX_LINK);
        assert!(Card::new(&link).is_err());

        let ok = pad(b"threnody://x").unwrap();
        assert_eq!(unpad(&ok), Some("threnody://x"));
        let mut long = ok.to_vec();
        long[..2].copy_from_slice(&513u16.to_be_bytes());
        assert_eq!(unpad(&long), None);
        let mut dirty = ok.to_vec();
        dirty[PADDED - 1] = 1;
        assert_eq!(unpad(&dirty), None);
        let mut not_utf8 = ok.to_vec();
        not_utf8[2] = 0xFF;
        assert_eq!(unpad(&not_utf8), None);
        assert_eq!(unpad(&ok[..PADDED - 1]), None);
        assert!(pad(&[0; MAX_LINK + 1]).is_none());
    }

    #[test]
    fn own_fingerprint_is_refused() {
        // The card's link names the reader itself.
        let mut card = Card::new(&link_of(fp(2), &["192.0.2.1:7450"])).unwrap();
        let mut reader = Reader::new(fp(2));
        let (_, outcome) = run_pair(&mut card, &mut reader, None);
        assert_eq!(outcome, Outcome::Failed(Failure::Garbled));
        assert_eq!(reader.lost(), None);
        // The reader's fingerprint is the card's own (a card whose link
        // names someone else, so that the reader gets as far as GIVE).
        let mut card = Card::new(&card_link()).unwrap();
        card.own_fp = fp(2);
        let mut reader = Reader::new(fp(2));
        let outcome = run(&mut reader, |c| {
            let r = card.process(c);
            if c[1] == INS_GIVE {
                assert_eq!(r, SW_DATA);
            }
            Some(r)
        });
        assert_eq!(outcome, Outcome::Failed(Failure::Garbled));
        assert_eq!(card.finished(), None);
    }

    #[test]
    fn lost_keeps_link_only_after_m2() {
        let (mut card, mut reader, link) = seeded();
        let (_, outcome) = run_pair(&mut card, &mut reader, Some(13));
        assert_eq!(outcome, Outcome::Failed(Failure::Moved));
        let (mut card, mut reader, link2) = seeded();
        assert_eq!(link.fingerprint, link2.fingerprint);
        let (_, outcome) = run_pair(&mut card, &mut reader, Some(14));
        assert_eq!(
            outcome,
            Outcome::Tapped {
                link: link2,
                mutual: false
            }
        );
        assert_eq!(card.finished(), None);
    }

    #[test]
    fn fetch_index_out_of_range() {
        let (mut card, _) = offered();
        assert_eq!(sw(&card.process(&fetch(6))), SW_OK);
        assert_eq!(card.process(&fetch(7)), SW_P1P2);
    }

    #[test]
    fn apdus_fit_short_length() {
        let (mut card, mut reader, _) = seeded();
        let (t, _) = run_pair(&mut card, &mut reader, None);
        for (c, r) in &t {
            assert!(c.len() <= 5 + CHUNK, "{}", c.len());
            assert!(r.len() <= 1 + CHUNK + 2, "{}", r.len());
        }
    }

    #[test]
    fn version_negotiation() {
        let mut card = Card::new(&card_link()).unwrap();
        assert_eq!(
            card.process(&select()),
            [0x54, 0x4E, 0x01, 0x00, 0x90, 0x00]
        );

        let mut reader = Reader::new(fp(2));
        reader.start();
        let Step::Send(offer) = reader.step(&[0x54, 0x4E, 0x02, 0x00, 0x90, 0x00]) else {
            panic!("a v2 card that speaks v1 is fine")
        };
        assert_eq!(offer[1], INS_OFFER);
        assert_eq!(offer[3] >> 1, 1);
        // ...but a card that no longer does refuses the version at once.
        assert_eq!(reader.step(&SW_P1P2), Step::Failed(Failure::Unsupported));
        // Later in the offer, the same answer is just a broken exchange.
        reader.start();
        reader.step(&[0x54, 0x4E, 0x01, 0x00, 0x90, 0x00]);
        assert!(matches!(reader.step(&SW_OK), Step::Send(_)));
        assert_eq!(reader.step(&SW_P1P2), Step::Failed(Failure::Garbled));
        reader.start();
        assert_eq!(
            reader.step(&[0x54, 0x4E, 0x00, 0x00, 0x90, 0x00]),
            Step::Failed(Failure::Unsupported)
        );
        reader.start();
        assert_eq!(
            reader.step(&SW_NOT_FOUND),
            Step::Failed(Failure::NotAnInvite)
        );
        reader.start();
        assert_eq!(reader.step(&SW_OK), Step::Failed(Failure::NotAnInvite));

        let mut v2 = offer.clone();
        v2[3] = (2 << 1) | (v2[3] & 1);
        assert_eq!(card.process(&v2), SW_P1P2);
        // Every chunk must name the same version.
        card.process(&select());
        let (_, cmds) = offered();
        assert_eq!(card.process(&cmds[1]), SW_OK);
        let mut c = cmds[2].clone();
        c[3] = 2 << 1;
        assert_eq!(card.process(&c), SW_P1P2);
    }

    #[test]
    fn run_is_bounded() {
        let mut calls = 0;
        let mut reader = Reader::new(fp(2));
        let outcome = run(&mut reader, |_| {
            calls += 1;
            Some(SW_OK.to_vec())
        });
        assert!(matches!(outcome, Outcome::Failed(_)));
        assert!(calls <= MAX_ROUND_TRIPS);
        // A card that answers every FETCH with "more" never ends the reply.
        let mut calls = 0;
        let mut reader = Reader::new(fp(2));
        let outcome = run(&mut reader, |cmd| {
            calls += 1;
            Some(match cmd[1] {
                INS_SELECT => vec![b'T', b'N', 1, 0, 0x90, 0],
                INS_FETCH => {
                    let mut r = vec![1];
                    r.extend([0; CHUNK]);
                    r.extend(SW_OK);
                    r
                }
                _ => SW_OK.to_vec(),
            })
        });
        assert_eq!(outcome, Outcome::Failed(Failure::Garbled));
        assert!(calls <= MAX_ROUND_TRIPS);
    }

    #[test]
    fn seeded_runs_are_reproducible() {
        let (mut c1, mut r1, _) = seeded();
        let (t1, o1) = run_pair(&mut c1, &mut r1, None);
        let (mut c2, mut r2, _) = seeded();
        let (t2, o2) = run_pair(&mut c2, &mut r2, None);
        assert!(matches!(o1, Outcome::Tapped { mutual: true, .. }));
        assert_eq!(o1, o2);
        assert_eq!(t1, t2);
    }

    #[test]
    fn parse_takes_short_apdus_only() {
        assert!(parse(&[0, 1, 2]).is_none());
        assert_eq!(parse(&[0x80, 0xD2, 3, 0]).unwrap().p1, 3);
        assert!(parse(&[0x80, 0xD2, 3, 0, 0]).unwrap().data.is_empty());
        assert_eq!(parse(&[0x80, 0xD4, 0, 0, 2, 7, 8]).unwrap().data, [7, 8]);
        assert_eq!(parse(&[0x80, 0xD4, 0, 0, 2, 7, 8, 0]).unwrap().data, [7, 8]);
        assert!(parse(&[0x80, 0xD4, 0, 0, 2, 7]).is_none());
        assert!(parse(&[0x80, 0xD4, 0, 0, 2, 7, 8, 9, 10]).is_none());
        assert!(parse(&[0x80, 0xD4, 0, 0, 0, 1, 0, 7]).is_none(), "extended");
    }
}
