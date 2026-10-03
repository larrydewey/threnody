#![no_main]
//! Feeds arbitrary HS2 frames to a live initiator. Nothing but the genuine
//! responder's HS2 may ever be accepted.
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use threnody_core::Identity;
use threnody_core::handshake::Initiator;

static ID: LazyLock<Identity> = LazyLock::new(|| Identity::from_seed(&[2; 32]));

fuzz_target!(|data: &[u8]| {
    let (ini, _hs1) = Initiator::start(&ID).unwrap();
    assert!(ini.finish(data).is_err());
});
