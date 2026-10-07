//! Mutation testing of every parser that touches untrusted bytes.
//!
//! Stable-toolchain stand-in for coverage-guided fuzzing (the `fuzz/`
//! directory has cargo-fuzz targets for nightly). Valid frames are mutated
//! with a seeded RNG; every decoder must return `Err` or `Ok`, never panic,
//! and a mutated frame must never be accepted as authentic.

use rand_core::{Rng, SeedableRng};

use crate::channel::SecureChannel;
use crate::handshake::{Initiator, Responder};
use crate::identity::Identity;
use crate::message::AppMessage;
use crate::store::Contacts;
use crate::wire;

const ITERATIONS: usize = 3000;

fn mutate(rng: &mut impl Rng, input: &[u8]) -> Vec<u8> {
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

/// Credentials, relay tokens and directories (Appendices O and P): no
/// mutation may panic a parser, and none may make a forged key, document,
/// issuance or proof verify.
#[test]
fn credential_and_directory_parsers_survive_mutated_input() {
    use crate::credential::{
        self, CredMsg, Issued, Issuer, IssuerKey, Presentation, RELAY_SCHEMA, RelayToken, Request,
    };
    use crate::directory::{DirMsg, DirectoryDoc, RelayDescriptor};
    use crate::onion::{CreateState, ExtendReq};

    let mut rng = rand_chacha::ChaCha20Rng::from_seed([11; 32]);
    let now = crate::now_ms();
    let issuer = Issuer::new(&Identity::generate(), 0).unwrap();
    let key = issuer.key().clone();
    let key_bytes = key.encode().unwrap();
    let attrs = vec![
        ("a".to_owned(), "1".to_owned()),
        ("b".to_owned(), "2".to_owned()),
    ];
    let req = Request::new().unwrap();
    let issued = issuer
        .issue(&req.commitment, "s", &attrs, credential::day(now) + 5)
        .unwrap();
    let issued_bytes = issued.encode().unwrap();
    let commitment = req.commitment.clone();
    let cred = req.finish(&key, &issued).unwrap();
    let ctx = credential::peer_context(&Identity::generate().public());
    let pres = cred.present(&["b"], &ctx, &[1; 32]).unwrap();
    let pres_bytes = pres.encode().unwrap();

    let relay = Identity::generate();
    let tok_req = Request::new().unwrap();
    let tok_issued = issuer
        .issue(
            &tok_req.commitment,
            RELAY_SCHEMA,
            &Vec::new(),
            credential::day(now),
        )
        .unwrap();
    let tok_cred = tok_req.finish(&key, &tok_issued).unwrap();
    let token = RelayToken::new(
        &tok_cred,
        &relay.public(),
        credential::day(now),
        3,
        &[4; 32],
    )
    .unwrap();
    let token_bytes = token.encode().unwrap();

    let desc = RelayDescriptor::new(&relay, vec!["192.0.2.1:7450".into()], now, 3_600_000).unwrap();
    let desc_bytes = desc.encode().unwrap();
    let doc = DirectoryDoc::sign(&issuer, std::slice::from_ref(&desc), now, 3_600_000).unwrap();
    let doc_bytes = doc.encode().unwrap();
    let cred_msg = CredMsg::Ask {
        id: 1,
        schema: "s".into(),
        keys: vec!["a".into()],
        nonce: [2; 32],
    }
    .encode()
    .unwrap();
    let dir_msg = DirMsg::TokenRequest {
        id: 1,
        epoch: 2,
        commitment: commitment.clone(),
    }
    .encode()
    .unwrap();
    let (_, e_pub) = CreateState::new(relay.public().fingerprint());
    let extend = ExtendReq {
        to: relay.public().fingerprint(),
        e_pub,
        addr: Some("192.0.2.1:7450".into()),
        token: Some(token_bytes.clone()),
    }
    .encode()
    .unwrap();

    for _ in 0..1000 {
        if let Ok(k) = IssuerKey::decode(&mutate(&mut rng, &key_bytes)) {
            assert_eq!(k, key, "mutated issuer key verified");
        }
        if let Ok(d) = RelayDescriptor::decode(&mutate(&mut rng, &desc_bytes)) {
            assert_eq!(d, desc, "mutated relay descriptor verified");
        }
        if let Ok(d) = DirectoryDoc::decode(&mutate(&mut rng, &doc_bytes))
            && d.verify(&key, now).is_ok()
        {
            assert_eq!(
                (d.relays, d.published_ms, d.valid_until_ms),
                (doc.relays.clone(), doc.published_ms, doc.valid_until_ms)
            );
        }
        let _ = CredMsg::decode(&mutate(&mut rng, &cred_msg));
        let _ = DirMsg::decode(&mutate(&mut rng, &dir_msg));
        let _ = Issued::decode(&mutate(&mut rng, &issued_bytes));
        let _ = crate::credential::Credential::decode(&mutate(&mut rng, &issued_bytes));
        if let Ok(x) = ExtendReq::decode(&mutate(&mut rng, &extend)) {
            assert!(x.encode().is_ok());
        }
    }
    // Proof checks are slower: fewer rounds.
    for _ in 0..150 {
        if let Ok(p) = Presentation::decode(&mutate(&mut rng, &pres_bytes))
            && let Ok(v) = p.verify(&key, &ctx, &[1; 32], now)
        {
            assert_eq!(
                v.attributes,
                vec![("b".to_owned(), "2".to_owned())],
                "mutated proof verified"
            );
        }
        if let Ok(t) = RelayToken::decode(&mutate(&mut rng, &token_bytes))
            && t.verify(std::slice::from_ref(&key), &relay.public(), &[4; 32], now)
                .is_ok()
        {
            assert_eq!(
                (t.epoch, t.slot),
                (token.epoch, token.slot),
                "mutated token verified"
            );
        }
    }
    for _ in 0..40 {
        let r = Request::new().unwrap();
        let good = issuer
            .issue(&r.commitment, "s", &attrs, credential::day(now) + 5)
            .unwrap()
            .encode()
            .unwrap();
        let bad = mutate(&mut rng, &good);
        if bad != good
            && let Ok(i) = Issued::decode(&bad)
            && let Ok(c) = r.finish(&key, &i)
        {
            assert_eq!(c.attributes, attrs, "mutated issuance accepted");
        }
    }
}
