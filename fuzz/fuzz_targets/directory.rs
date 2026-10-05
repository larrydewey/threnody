#![no_main]
//! Directory messages, documents, relay descriptors and EXTEND cells
//! (Appendix P): what directories, relays and anonymous peers send.
use libfuzzer_sys::fuzz_target;
use threnody_core::directory::{DirMsg, DirectoryDoc, DirectoryLink, RelayDescriptor};
use threnody_core::onion::ExtendReq;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = DirMsg::decode(data) {
        assert_eq!(DirMsg::decode(&m.encode().unwrap()).unwrap(), m);
    }
    if let Ok(d) = RelayDescriptor::decode(data) {
        assert!(d.verify().is_ok());
    }
    let _ = DirectoryDoc::decode(data);
    if let Ok(x) = ExtendReq::decode(data) {
        assert_eq!(ExtendReq::decode(&x.encode().unwrap()).unwrap(), x);
    }
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = s.parse::<DirectoryLink>();
    }
});
