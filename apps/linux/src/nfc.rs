//! Tapping a phone that shows its invite on a PC/SC NFC reader
//! (Appendix T). A computer can only be the reader: an Android phone
//! showing "My invite" looks like an ISO-DEP card to it. The exchange
//! itself is `threnody_core::tap`, through the FFI; this is the pipe.

use std::ffi::CString;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pcsc::{Context, Protocols, ReaderState, Scope, ShareMode, State};
use threnody_ffi::{NfcFailure, NfcOutcome, NfcTransport, ThrenodyNode};

/// Room for any response: the tap protocol's largest is 243 bytes, and
/// short APDUs answer at most 256 bytes and a status word.
const RECV: usize = 258;

/// Whether `pcscd` answers and at least one reader is attached.
pub fn available() -> bool {
    Context::establish(Scope::User)
        .and_then(|c| c.list_readers_owned())
        .is_ok_and(|r| !r.is_empty())
}

/// A wait for a phone on the reader, which the GTK thread can cancel.
pub struct Waiter(Context);

impl Waiter {
    pub fn new() -> Option<Self> {
        Context::establish(Scope::User).ok().map(Self)
    }

    /// Ends a `tap` under way (it reports `Moved`).
    pub fn cancel(&self) {
        let _ = self.0.cancel();
    }
}

/// Waits up to `timeout` for a phone on any reader, then runs the whole
/// exchange with it. Blocking: run it off the GTK thread.
pub fn tap(node: &ThrenodyNode, waiter: &Waiter, timeout: Duration) -> NfcOutcome {
    let moved = NfcOutcome::Failed {
        why: NfcFailure::Moved,
    };
    let Some(reader) = wait_for_card(&waiter.0, timeout) else {
        return moved;
    };
    let Ok(card) = waiter.0.connect(&reader, ShareMode::Shared, Protocols::ANY) else {
        return moved;
    };
    node.nfc_reader()
        .run(Arc::new(PcscTransport(Mutex::new(card))))
}

/// The first reader with a card (a phone) on it.
fn wait_for_card(ctx: &Context, timeout: Duration) -> Option<CString> {
    let until = Instant::now() + timeout;
    let readers = ctx.list_readers_owned().ok()?;
    let mut states: Vec<ReaderState> = readers
        .into_iter()
        .map(|r| ReaderState::new(r, State::UNAWARE))
        .collect();
    loop {
        let left = until.checked_duration_since(Instant::now())?;
        // Times out, is cancelled, or a reader went away: give up.
        ctx.get_status_change(Some(left), &mut states).ok()?;
        for s in &mut states {
            let now = s.event_state();
            if now.contains(State::PRESENT) && !now.contains(State::MUTE) {
                return Some(s.name().to_owned());
            }
            s.sync_current_state();
        }
    }
}

/// An ISO-DEP pipe to the phone on the reader.
struct PcscTransport(Mutex<pcsc::Card>);

impl NfcTransport for PcscTransport {
    fn transceive(&self, apdu: Vec<u8>) -> Option<Vec<u8>> {
        let card = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut buf = [0u8; RECV];
        card.transmit(&apdu, &mut buf).ok().map(<[u8]>::to_vec)
    }
}

/// What to tell the user when a tap failed (the Android app's wording).
pub fn text(why: NfcFailure) -> &'static str {
    match why {
        NfcFailure::Moved => "Hold their phone on the reader until it buzzes",
        NfcFailure::NotAnInvite => "That isn't a phone showing a Threnody invite.",
        NfcFailure::Unsupported => {
            "Their Threnody is too new for this one: update, or scan their QR code."
        }
        NfcFailure::Garbled => "Something went wrong: try again, or scan their QR code.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_fits_the_largest_response() {
        // more || chunk || SW1 SW2
        const { assert!(RECV >= 1 + threnody_core::tap::CHUNK + 2) };
    }
}
