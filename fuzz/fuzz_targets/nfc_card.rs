#![no_main]
//! The NFC tap protocol (Appendix T): command APDUs a phone showing its
//! invite receives, and responses a tapping reader receives. `data` is a
//! run of `len: u8 || bytes` APDUs.
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use threnody_core::crypto::hybrid::HybridSecret;
use threnody_core::invite::InviteLink;
use threnody_core::tap::{self, Card, Reader};
use threnody_core::{Fingerprint, Identity};

static LINK: LazyLock<InviteLink> = LazyLock::new(|| {
    let fp = Identity::from_seed(&[1; 32]).public().fingerprint();
    format!("threnody://{}@198.51.100.4:7450", fp.compact())
        .parse()
        .unwrap()
});
static FP: LazyLock<Fingerprint> =
    LazyLock::new(|| Identity::from_seed(&[2; 32]).public().fingerprint());

fuzz_target!(|data: &[u8]| {
    let mut apdus = Vec::new();
    let mut rest = data;
    while let Some((&len, tail)) = rest.split_first() {
        let n = usize::from(len).min(tail.len());
        apdus.push(&tail[..n]);
        rest = &tail[n..];
    }
    let mut card = Card::with_seed(&LINK, [1; 64]).unwrap();
    for a in &apdus {
        let r = card.process(a);
        assert!((2..=243).contains(&r.len()));
    }
    let mut reader = Reader::with_secret(*FP, HybridSecret::from_seed(&[2; 32]));
    let mut responses = apdus.iter();
    let _ = tap::run(&mut reader, |_| responses.next().map(|r| r.to_vec()));
    let _ = reader.lost();
});
