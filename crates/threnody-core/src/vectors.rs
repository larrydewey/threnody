//! Reproducible test vectors (spec §15).
//!
//! The vectors live in `docs/test-vectors/v1.txt`. This test regenerates
//! every value from fixed seeds and requires an exact match, so any change
//! to the wire format or key schedule shows up as a diff. To regenerate
//! after an intentional change:
//!
//! ```sh
//! THRENODY_REGEN_VECTORS=1 cargo test -p threnody-core vectors
//! ```

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::channel::SecureChannel;
use crate::crypto::hybrid::{HybridPublic, HybridSecret};
use crate::crypto::kdf::{self, label};
use crate::crypto::rng::Rng;
use crate::handshake::{Initiator, Responder};
use crate::identity::{Identity, safety_number};
use crate::message::AppMessage;

const HEADER: &str = "\
# Threnody protocol v1 test vectors.
#
# Every random value comes from ChaCha20 (rand_chacha 0.3, RFC 8439 block
# function, 20 rounds) seeded with the listed 32-byte seed. The protocol
# draws bytes from a party's stream in this order:
#   hybrid keygen : X-Wing seed (32)
#   hybrid encap  : X-Wing eseed (64)
#   handshake I   : keygen(e_I) ; after HS2, ratchet uses the same stream:
#                   keygen(A1), encap(rk_R)
#   handshake R   : encap(e_I), keygen(rk_R) ; ratchet continues the stream
#   ratchet send  : header nonce (12) per message
#   ratchet step  : keygen(own'), encap(peer key), on receiving a new chain
# Ed25519 identities come from the listed 32-byte seeds (RFC 8032).
# Values are lowercase hex unless noted.
";

fn seed(tag: u8) -> [u8; 32] {
    let mut s = [0u8; 32];
    for (i, b) in s.iter_mut().enumerate() {
        *b = tag.wrapping_add(i as u8);
    }
    s
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

fn generate() -> String {
    let mut out = String::from(HEADER);
    // An empty value marks a section heading.
    let mut put = |k: &str, v: String| {
        let _ = if v.is_empty() {
            writeln!(out, "{k}")
        } else {
            writeln!(out, "{k} = {v}")
        };
    };

    // KDF
    let kdf_out: [u8; 32] = kdf::derive(label::ROOT, &[b"input one", b""]);
    put("\n# kdf", String::new());
    put("kdf.label", format!("{:?}", label::ROOT));
    put("kdf.parts", "\"input one\", \"\"".into());
    put("kdf.output", hex(&kdf_out));

    // Identities, fingerprints, safety number
    let a = Identity::from_seed(&seed(0xa0));
    let b = Identity::from_seed(&seed(0xb0));
    put("\n# identity", String::new());
    put("identity.a.seed", hex(&seed(0xa0)));
    put("identity.a.public", hex(a.public().as_bytes()));
    put(
        "identity.a.fingerprint (text)",
        a.public().fingerprint().to_string(),
    );
    put("identity.b.seed", hex(&seed(0xb0)));
    put("identity.b.public", hex(b.public().as_bytes()));
    put(
        "identity.b.fingerprint (text)",
        b.public().fingerprint().to_string(),
    );
    put(
        "safety_number(a, b) (text)",
        safety_number(&a.public(), &b.public()),
    );

    // Hybrid KEM
    let mut rng = Rng::seeded(seed(0x10));
    let sk = HybridSecret::generate_with(&mut rng);
    let pk = HybridPublic::from_bytes(sk.public().as_bytes()).unwrap();
    let (ct, ss) = pk.encapsulate_with(&mut rng).unwrap();
    assert_eq!(*sk.decapsulate(&ct).unwrap(), *ss);
    put(
        "\n# hybrid kem (keygen then encap on one stream)",
        String::new(),
    );
    put("kem.rng_seed", hex(&seed(0x10)));
    put("kem.public", hex(pk.as_bytes()));
    put("kem.ciphertext", hex(&ct));
    put("kem.shared_secret", hex(&ss[..]));

    // Handshake + ratchet
    let (ini, hs1) = Initiator::start_with(&a, Rng::seeded(seed(0x20))).unwrap();
    let (resp, hs2) = Responder::respond_with(&b, &hs1, Rng::seeded(seed(0x30))).unwrap();
    let (hs3, ea) = ini.finish(&hs2).unwrap();
    let eb = resp.finish(&hs3).unwrap();
    assert_eq!(ea.session_id, eb.session_id);
    put(
        "\n# handshake (initiator = a, responder = b)",
        String::new(),
    );
    put("handshake.initiator.rng_seed", hex(&seed(0x20)));
    put("handshake.responder.rng_seed", hex(&seed(0x30)));
    put("handshake.hs1", hex(&hs1));
    put("handshake.hs2", hex(&hs2));
    put("handshake.hs3", hex(&hs3));
    put("handshake.session_id", hex(&ea.session_id));

    let mut ca = SecureChannel::from(ea);
    let mut cb = SecureChannel::from(eb);
    let m0 = ca.seal(&AppMessage::Hello { features: 0 }).unwrap();
    assert_eq!(cb.open(&m0).unwrap(), AppMessage::Hello { features: 0 });
    let text = AppMessage::Text {
        sent_ms: 1_791_000_000_000,
        body: "threnody".into(),
        expires_in_s: None,
    };
    let m1 = cb.seal(&text).unwrap();
    assert_eq!(ca.open(&m1).unwrap(), text);
    let m2 = ca.seal(&AppMessage::Approval { approved: true }).unwrap();
    assert_eq!(
        cb.open(&m2).unwrap(),
        AppMessage::Approval { approved: true }
    );
    put("\n# ratchet frames, in send order", String::new());
    put("ratchet.0 (a -> b, Hello)", hex(&m0));
    put(
        "ratchet.1 (b -> a, Text 1791000000000 \"threnody\")",
        hex(&m1),
    );
    put("ratchet.2 (a -> b, Approval true)", hex(&m2));
    out
}

#[test]
fn vectors_match_published_file() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/test-vectors/v1.txt");
    let fresh = generate();
    assert_eq!(fresh, generate(), "vector generation is not deterministic");
    if std::env::var_os("THRENODY_REGEN_VECTORS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &fresh).unwrap();
        return;
    }
    let published = std::fs::read_to_string(&path)
        .expect("docs/test-vectors/v1.txt missing; run with THRENODY_REGEN_VECTORS=1");
    assert!(
        published == fresh,
        "wire format or key schedule changed; diff against {}",
        path.display()
    );
}
