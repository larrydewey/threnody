//! An established, ratcheted, padded channel to one authenticated peer.

use zeroize::Zeroizing;

use crate::crypto::aead::Suite;
use crate::crypto::kdf::{self, label};
use crate::error::Result;
use crate::handshake::Established;
use crate::identity::PublicIdentity;
use crate::message::{AppMessage, pad, unpad};
use crate::ratchet::Ratchet;

pub struct SecureChannel {
    peer: PublicIdentity,
    suite: Suite,
    session_id: [u8; 32],
    exporter: Zeroizing<[u8; 32]>,
    ratchet: Ratchet,
}

impl From<Established> for SecureChannel {
    fn from(e: Established) -> Self {
        Self {
            peer: e.peer,
            suite: e.suite,
            session_id: e.session_id,
            exporter: e.exporter,
            ratchet: e.ratchet,
        }
    }
}

impl SecureChannel {
    pub fn peer(&self) -> &PublicIdentity {
        &self.peer
    }

    pub fn suite(&self) -> Suite {
        self.suite
    }

    pub fn session_id(&self) -> &[u8; 32] {
        &self.session_id
    }

    /// Derives a session-bound secret for `context`, identical on both
    /// ends (in the spirit of the TLS exporter, RFC 8446 §7.5).
    pub fn export(&self, context: &[u8]) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(kdf::derive(label::EXPORT, &[&self.exporter[..], context]))
    }

    /// False on the responder until the initiator's first message arrives.
    pub fn can_send(&self) -> bool {
        self.ratchet.can_send()
    }

    /// Encodes, pads and encrypts one message into a wire frame.
    pub fn seal(&mut self, msg: &AppMessage) -> Result<Vec<u8>> {
        self.ratchet.encrypt(&pad(msg.encode()?))
    }

    /// Decrypts, unpads and decodes one wire frame.
    pub fn open(&mut self, frame: &[u8]) -> Result<AppMessage> {
        AppMessage::decode(&unpad(self.ratchet.decrypt(frame)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::{Initiator, Responder};
    use crate::identity::Identity;

    #[test]
    fn end_to_end_conversation() {
        let a = Identity::generate();
        let b = Identity::generate();
        let (ini, m1) = Initiator::start(&a).unwrap();
        let (resp, m2) = Responder::respond(&b, &m1).unwrap();
        let (m3, ea) = ini.finish(&m2).unwrap();
        let mut ca = SecureChannel::from(ea);
        let mut cb = SecureChannel::from(resp.finish(&m3).unwrap());
        assert_eq!(*ca.export(b"x"), *cb.export(b"x"));
        assert_ne!(*ca.export(b"x"), *ca.export(b"y"));

        let hello = ca.seal(&AppMessage::Hello { features: 0 }).unwrap();
        assert_eq!(cb.open(&hello).unwrap(), AppMessage::Hello { features: 0 });
        assert!(cb.can_send());

        let short = ca
            .seal(&AppMessage::Text {
                sent_ms: 1,
                body: "a".into(),
                expires_in_s: None,
                id: 0,
            })
            .unwrap();
        let longer = ca
            .seal(&AppMessage::Text {
                sent_ms: 1,
                body: "a".repeat(150),
                expires_in_s: None,
                id: 0,
            })
            .unwrap();
        assert_eq!(
            short.len(),
            longer.len(),
            "padding must hide small length differences"
        );
        cb.open(&short).unwrap();
        cb.open(&longer).unwrap();

        let reply = cb.seal(&AppMessage::Approval { approved: true }).unwrap();
        assert_eq!(
            ca.open(&reply).unwrap(),
            AppMessage::Approval { approved: true }
        );
    }
}
