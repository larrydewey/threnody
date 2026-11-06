//! Adding a contact by tapping devices (Appendix T): thin objects over
//! `threnody_core::tap`. The apps only move APDUs between these and their
//! NFC stack; every decision is made here.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use threnody_core::invite::InviteLink;
use threnody_core::tap::{self, Card, Failure, Outcome, Reader, Step};

use crate::{Result, fail};

/// Why a tap didn't complete. Apps map each to a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NfcFailure {
    /// The tapped thing isn't a phone showing a Threnody invite.
    NotAnInvite,
    /// Their Threnody speaks only a newer tap protocol.
    Unsupported,
    /// Something went wrong in the exchange.
    Garbled,
    /// The devices separated too early: hold them together until it buzzes.
    Moved,
}

impl From<Failure> for NfcFailure {
    fn from(f: Failure) -> Self {
        match f {
            Failure::NotAnInvite => Self::NotAnInvite,
            Failure::Unsupported => Self::Unsupported,
            Failure::Garbled => Self::Garbled,
            Failure::Moved => Self::Moved,
        }
    }
}

/// What a reader does next (for platforms that drive the exchange
/// themselves, as iOS does with its async tag API).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NfcStep {
    /// Send this command APDU and pass the response to `step`.
    Send {
        apdu: Vec<u8>,
    },
    /// Done: the other phone has our fingerprint, and this is its invite.
    Done {
        peer_link: String,
    },
    Failed {
        why: NfcFailure,
    },
}

/// How a whole tap ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NfcOutcome {
    /// `mutual`: the other phone got our fingerprint too (false: the
    /// devices separated first, so treat the link as a scanned invite).
    Tapped {
        peer_link: String,
        mutual: bool,
    },
    Failed {
        why: NfcFailure,
    },
}

/// A platform's ISO-DEP pipe (Android `IsoDep`, a Linux PC/SC card),
/// called from the thread that runs [`NfcReader::run`].
#[uniffi::export(with_foreign)]
pub trait NfcTransport: Send + Sync {
    /// Sends one command APDU; returns the response (with its status
    /// word), or None when the tag is gone.
    fn transceive(&self, apdu: Vec<u8>) -> Option<Vec<u8>>;
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The phone showing its invite: feed it each command APDU (Android HCE).
/// It serves one exchange.
#[derive(uniffi::Object)]
pub struct NfcCard {
    inner: Mutex<Card>,
}

#[uniffi::export]
impl NfcCard {
    /// A card handing out `own_link`, this device's invite link.
    #[uniffi::constructor]
    pub fn new(own_link: String) -> Result<Arc<Self>> {
        let link: InviteLink = own_link.parse().map_err(fail)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Card::new(&link).map_err(fail)?),
        }))
    }

    /// One command APDU in, the response (with its status word) out.
    pub fn process(&self, apdu: Vec<u8>) -> Vec<u8> {
        locked(&self.inner).process(&apdu)
    }

    /// The tapping device's fingerprint, once the exchange completed.
    pub fn peer_fingerprint(&self) -> Option<String> {
        locked(&self.inner).finished().map(|f| f.to_string())
    }
}

/// The device that taps. Make a fresh one (`ThrenodyNode::nfc_reader`)
/// for every tag: each has its own one-time key.
#[derive(uniffi::Object)]
pub struct NfcReader {
    inner: Mutex<Reader>,
}

impl From<Reader> for NfcReader {
    fn from(r: Reader) -> Self {
        Self {
            inner: Mutex::new(r),
        }
    }
}

impl From<Outcome> for NfcOutcome {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Tapped { link, mutual } => Self::Tapped {
                peer_link: link.to_string(),
                mutual,
            },
            Outcome::Failed(f) => Self::Failed { why: f.into() },
        }
    }
}

#[uniffi::export]
impl NfcReader {
    /// The first command APDU (SELECT). Starts the exchange over.
    pub fn start(&self) -> Vec<u8> {
        locked(&self.inner).start()
    }

    /// The response (with its status word) to the last command.
    pub fn step(&self, response: Vec<u8>) -> NfcStep {
        match locked(&self.inner).step(&response) {
            Step::Send(apdu) => NfcStep::Send { apdu },
            Step::Done(link) => NfcStep::Done {
                peer_link: link.to_string(),
            },
            Step::Failed(f) => NfcStep::Failed { why: f.into() },
        }
    }

    /// The tag went away: the other phone's invite if we had it already
    /// (a half tap, to treat as a scanned invite), else None.
    pub fn lost(&self) -> Option<String> {
        locked(&self.inner).lost().map(|l| l.to_string())
    }

    /// The whole exchange over `transport`. Blocks until it ends (a
    /// bounded number of round trips).
    pub fn run(&self, transport: Arc<dyn NfcTransport>) -> NfcOutcome {
        let mut reader = locked(&self.inner);
        tap::run(&mut reader, |apdu| transport.transceive(apdu.to_vec())).into()
    }
}
