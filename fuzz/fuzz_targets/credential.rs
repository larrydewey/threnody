#![no_main]
//! Credential messages, presentations and relay tokens (Appendix O): what
//! peers and strangers on anonymous links send.
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use threnody_core::Identity;
use threnody_core::credential::{
    CredMsg, Issued, Issuer, IssuerKey, Presentation, RelayToken, peer_context,
};

static ISSUER: LazyLock<IssuerKey> =
    LazyLock::new(|| Issuer::new(&Identity::from_seed(&[1; 32]), 0).unwrap().key().clone());
static ME: LazyLock<Identity> = LazyLock::new(|| Identity::from_seed(&[2; 32]));

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = CredMsg::decode(data) {
        assert_eq!(CredMsg::decode(&m.encode().unwrap()).unwrap(), m);
    }
    let _ = Issued::decode(data);
    let _ = IssuerKey::decode(data);
    if let Ok(p) = Presentation::decode(data) {
        // Never accepted: nothing here was issued by this key.
        assert!(p.verify(&ISSUER, &peer_context(&ME.public()), &[0; 32], 0).is_err());
    }
    if let Ok(t) = RelayToken::decode(data) {
        assert!(t.verify(std::slice::from_ref(&*ISSUER), &ME.public(), &[0; 32], 0).is_err());
    }
});
