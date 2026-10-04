//! Private local discovery beacons (spec §7.1, §10).
//!
//! A beacon tells mutually approved peers on the same network where to
//! reach us, and tells nobody else anything linkable:
//!
//! ```text
//! beacon = nonce (16) || port (u16 BE) || tag_1 || ... || tag_k     k = multiple of 8
//! tag_i  = BLAKE3-keyed(D_i, label || sender_id || epoch (u64 LE) || nonce || port)[0..16]
//! epoch  = unix_seconds / 300
//! ```
//!
//! `D_i` is the discovery key the sender shares with approved peer `i`,
//! exported from their latest session (`DISCOVERY_CONTEXT`). Unused tag
//! slots are random. Receivers try each approved contact for the current
//! and adjacent epochs. Binding `sender_id` stops a node from matching its
//! own beacons; dialing pins the fingerprint, so a replayed beacon can at
//! worst trigger a failed connection.

use rand_core::{OsRng, RngCore};

use crate::identity::PublicIdentity;

/// Exporter context for the pairwise discovery key.
pub const DISCOVERY_CONTEXT: &[u8] = b"lan discovery key";
pub const EPOCH_SECS: u64 = 300;
const NONCE_LEN: usize = 16;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = NONCE_LEN + 2;
const TAG_LABEL: &[u8] = b"threnody v1 2026-10-03 discovery beacon";
/// Tag slots come in blocks of this many, hiding the exact peer count.
pub const TAG_BLOCK: usize = 8;
/// Upper bound on tags per beacon (keeps datagrams under ~1.1 KB).
pub const MAX_TAGS: usize = 64;
/// Tags per Bluetooth LE beacon: one block, so the beacon (146 bytes) fits
/// a single extended advertising PDU. With more approved peers, each beacon
/// carries a fresh random subset of them.
pub const BLE_TAGS: usize = TAG_BLOCK;

fn tag(
    key: &[u8; 32],
    sender: &PublicIdentity,
    epoch: u64,
    nonce: &[u8],
    port: u16,
) -> [u8; TAG_LEN] {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(TAG_LABEL);
    h.update(sender.as_bytes());
    h.update(&epoch.to_le_bytes());
    h.update(nonce);
    h.update(&port.to_be_bytes());
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&h.finalize().as_bytes()[..TAG_LEN]);
    out
}

/// Builds a beacon for `keys` (one discovery key per approved peer; at most
/// [`MAX_TAGS`] are used).
pub fn beacon(me: &PublicIdentity, keys: &[[u8; 32]], port: u16, now_secs: u64) -> Vec<u8> {
    beacon_with(me, keys, port, now_secs, MAX_TAGS)
}

/// Builds a beacon with at most `max_tags` tags. If there are more keys
/// than that, a random subset is used, so over successive beacons every
/// peer is eventually included.
pub fn beacon_with(
    me: &PublicIdentity,
    keys: &[[u8; 32]],
    port: u16,
    now_secs: u64,
    max_tags: usize,
) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let max_tags = max_tags.clamp(1, MAX_TAGS);
    let mut keys: Vec<&[u8; 32]> = keys.iter().collect();
    if keys.len() > max_tags {
        shuffle(&mut keys);
        keys.truncate(max_tags);
    }
    let slots = keys.len().div_ceil(TAG_BLOCK).max(1) * TAG_BLOCK;
    let epoch = now_secs / EPOCH_SECS;
    let mut out = Vec::with_capacity(HEADER_LEN + slots * TAG_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&port.to_be_bytes());
    let mut tags: Vec<[u8; TAG_LEN]> = keys
        .iter()
        .map(|k| tag(k, me, epoch, &nonce, port))
        .collect();
    while tags.len() < slots {
        let mut r = [0u8; TAG_LEN];
        OsRng.fill_bytes(&mut r);
        tags.push(r);
    }
    // Shuffle so slot position does not reveal contact-book order.
    shuffle(&mut tags);
    for t in tags {
        out.extend_from_slice(&t);
    }
    out
}

fn shuffle<T>(v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let j = (OsRng.next_u32() as usize) % (i + 1);
        v.swap(i, j);
    }
}

/// The port (or Bluetooth PSM) a beacon advertises, readable by anyone.
pub fn beacon_port(data: &[u8]) -> Option<u16> {
    let p = data.get(NONCE_LEN..HEADER_LEN)?;
    Some(u16::from_be_bytes([p[0], p[1]]))
}

/// Returns every candidate `(peer, port)` whose tag appears in `data`.
pub fn recognise<'a>(
    data: &[u8],
    candidates: impl IntoIterator<Item = (&'a PublicIdentity, &'a [u8; 32])>,
    now_secs: u64,
) -> Vec<(PublicIdentity, u16)> {
    if data.len() < HEADER_LEN + TAG_LEN
        || !(data.len() - HEADER_LEN).is_multiple_of(TAG_LEN)
        || (data.len() - HEADER_LEN) / TAG_LEN > MAX_TAGS.div_ceil(TAG_BLOCK) * TAG_BLOCK
    {
        return Vec::new();
    }
    let (nonce, rest) = data.split_at(NONCE_LEN);
    let (port, tags) = rest.split_at(2);
    let port = u16::from_be_bytes([port[0], port[1]]);
    let epoch = now_secs / EPOCH_SECS;
    let mut found = Vec::new();
    for (peer, key) in candidates {
        let hit = [epoch.saturating_sub(1), epoch, epoch + 1]
            .iter()
            .any(|&e| {
                let want = tag(key, peer, e, nonce, port);
                tags.as_chunks::<TAG_LEN>().0.contains(&want)
            });
        // A peer may be listed once per key it is known by.
        if hit && !found.iter().any(|(p, _)| p == peer) {
            found.push((*peer, port));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    #[test]
    fn approved_peers_recognise_and_others_do_not() {
        let alice = Identity::generate().public();
        let bob = Identity::generate().public();
        let carol = Identity::generate().public();
        let (k_ab, k_ac) = ([1u8; 32], [2u8; 32]);
        let now = 1_791_000_000;
        let b = beacon(&alice, &[k_ab, k_ac], 7450, now);
        assert_eq!(b.len(), HEADER_LEN + TAG_BLOCK * TAG_LEN);

        // Bob knows alice with k_ab.
        assert_eq!(recognise(&b, [(&alice, &k_ab)], now), vec![(alice, 7450)]);
        // Adjacent epoch still matches; far epochs do not.
        assert_eq!(recognise(&b, [(&alice, &k_ab)], now + EPOCH_SECS).len(), 1);
        assert!(recognise(&b, [(&alice, &k_ab)], now + 3 * EPOCH_SECS).is_empty());
        // A stranger's key, or the wrong sender identity, finds nothing.
        assert!(recognise(&b, [(&alice, &[9u8; 32])], now).is_empty());
        assert!(recognise(&b, [(&bob, &k_ab)], now).is_empty());
        // Alice does not recognise her own beacon as coming from bob/carol.
        assert!(recognise(&b, [(&bob, &k_ab), (&carol, &k_ac)], now).is_empty());
    }

    #[test]
    fn ble_beacons_rotate_through_peers() {
        let alice = Identity::generate().public();
        let keys: Vec<[u8; 32]> = (0..20u8).map(|i| [i; 32]).collect();
        let now = 1_791_000_000;
        let mut seen = [false; 20];
        for _ in 0..64 {
            let b = beacon_with(&alice, &keys, 128, now, BLE_TAGS);
            assert_eq!(b.len(), HEADER_LEN + BLE_TAGS * TAG_LEN);
            let hits: Vec<usize> = (0..20)
                .filter(|&i| !recognise(&b, [(&alice, &keys[i])], now).is_empty())
                .collect();
            assert_eq!(hits.len(), BLE_TAGS);
            for i in hits {
                seen[i] = true;
            }
        }
        assert!(seen.iter().all(|&s| s), "every peer gets a turn");
    }

    #[test]
    fn beacons_are_unlinkable_and_reject_garbage() {
        let alice = Identity::generate().public();
        let b1 = beacon(&alice, &[[1u8; 32]], 7450, 0);
        let b2 = beacon(&alice, &[[1u8; 32]], 7450, 0);
        assert_ne!(b1[..NONCE_LEN], b2[..NONCE_LEN]);
        assert!(
            !b1[HEADER_LEN..]
                .chunks(TAG_LEN)
                .any(|t| b2[HEADER_LEN..].chunks(TAG_LEN).any(|u| u == t))
        );
        assert_eq!(
            beacon(&alice, &[], 1, 0).len(),
            HEADER_LEN + TAG_BLOCK * TAG_LEN
        );
        assert_eq!(beacon_port(&b1), Some(7450));
        assert_eq!(beacon_port(&[0u8; 17]), None);
        for bad in [&[][..], &[0u8; 20][..], &b1[..b1.len() - 1]] {
            assert!(recognise(bad, [(&alice, &[1u8; 32])], 0).is_empty());
        }
    }
}
