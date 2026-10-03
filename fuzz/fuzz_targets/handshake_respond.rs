#![no_main]
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use threnody_core::Identity;
use threnody_core::handshake::Responder;

static ID: LazyLock<Identity> = LazyLock::new(|| Identity::from_seed(&[1; 32]));

fuzz_target!(|data: &[u8]| {
    let _ = Responder::respond(&ID, data);
});
