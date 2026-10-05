//! Mutation testing of every parser that touches untrusted bytes.
//!
//! Stable-toolchain stand-in for coverage-guided fuzzing (the `fuzz/`
//! directory has cargo-fuzz targets for nightly). Valid frames are mutated
//! with a seeded RNG; every decoder must return `Err` or `Ok`, never panic,
//! and a mutated frame must never be accepted as authentic.

use rand_core::{RngCore, SeedableRng};

use crate::channel::SecureChannel;
use crate::handshake::{Initiator, Responder};
use crate::identity::Identity;
use crate::message::AppMessage;
use crate::store::Contacts;
use crate::wire;

const ITERATIONS: usize = 3000;

fn mutate(rng: &mut impl RngCore, input: &[u8]) -> Vec<u8> {
    let mut v = input.to_vec();
    let edits = 1 + rng.next_u32() % 4;
    for _ in 0..edits {
        let len = v.len().max(1);
        let pos = rng.next_u32() as usize % len;
        match rng.next_u32() % 6 {
            0 if !v.is_empty() => v[pos] ^= 1 << (rng.next_u32() % 8),
            1 if !v.is_empty() => v[pos] = rng.next_u32() as u8,
            2 => v.truncate(pos),
            3 => v.insert(pos.min(v.len()), rng.next_u32() as u8),
            // Interesting CBOR heads: huge lengths, indefinite, break, tags.
            4 if !v.is_empty() => {
                const HEADS: [u8; 10] =
                    [0x1b, 0x5b, 0x7b, 0x9b, 0xbb, 0x5f, 0x9f, 0xff, 0xc6, 0xf9];
                v[pos] = HEADS[rng.next_u32() as usize % HEADS.len()];
            }
            _ => {
                let n = (rng.next_u32() % 16) as usize;
                for _ in 0..n {
                    v.push(rng.next_u32() as u8);
                }
            }
        }
    }
    v
}

#[test]
fn parsers_survive_mutated_input() {
    let mut rng = rand_chacha::ChaCha20Rng::from_seed([7; 32]);
    let a = Identity::generate();
    let b = Identity::generate();

    let (ini, hs1) = Initiator::start(&a).unwrap();
    let (resp, hs2) = Responder::respond(&b, &hs1).unwrap();
    let (hs3, ea) = ini.finish(&hs2).unwrap();
    let eb = resp.finish(&hs3).unwrap();
    let mut ca = SecureChannel::from(ea);
    let mut cb = SecureChannel::from(eb);
    let hello = ca.seal(&AppMessage::Hello { features: 0 }).unwrap();
    cb.open(&hello).unwrap();
    let text = ca
        .seal(&AppMessage::Text {
            sent_ms: 1,
            body: "x".into(),
            expires_in_s: None,
            id: 0,
        })
        .unwrap();
    let app = AppMessage::File {
        sent_ms: 9,
        name: "n".into(),
        data: vec![1, 2, 3],
        id: 0,
        sensitive: false,
        caption: String::new(),
        album: 0,
    }
    .encode()
    .unwrap();
    let mut contacts = Contacts::default();
    contacts.observe(a.public(), Some("h:1".into()), 1);
    let contacts_bytes = contacts.encode().unwrap();

    for _ in 0..ITERATIONS {
        let _ = wire::decode_envelope(&mutate(&mut rng, &hs1));
        let _ = Responder::respond(&b, &mutate(&mut rng, &hs1));

        let (ini, hs1) = Initiator::start(&a).unwrap();
        let (resp, hs2) = Responder::respond(&b, &hs1).unwrap();
        let bad2 = mutate(&mut rng, &hs2);
        if bad2 != hs2 {
            assert!(ini.finish(&bad2).is_err(), "mutated HS2 accepted");
        }
        let bad3 = mutate(&mut rng, &hs3);
        assert!(
            resp.finish(&bad3).is_err(),
            "foreign or mutated HS3 accepted"
        );

        let bad = mutate(&mut rng, &text);
        if bad != text {
            assert!(cb.open(&bad).is_err(), "mutated ratchet frame accepted");
        }
        let _ = AppMessage::decode(&mutate(&mut rng, &app));
        let _ = Contacts::decode(&mutate(&mut rng, &contacts_bytes));
    }
    // The channel still works after thousands of rejected frames.
    assert_eq!(
        cb.open(&text).unwrap(),
        AppMessage::Text {
            sent_ms: 1,
            body: "x".into(),
            expires_in_s: None,
            id: 0,
        }
    );
}
