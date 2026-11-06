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

/// Whether `pcscd` answers and at least one reader could be an NFC
/// reader: one that is empty or holds a contactless card. A YubiKey or a
/// contact card reader with a card in it always holds a contact card, so
/// it alone doesn't show "Tap a phone".
pub fn available() -> bool {
    let Ok(ctx) = Context::establish(Scope::User) else {
        return false;
    };
    let Ok(readers) = ctx.list_readers_owned() else {
        return false;
    };
    let mut states: Vec<ReaderState> = readers
        .into_iter()
        .map(|r| ReaderState::new(r, State::UNAWARE))
        .collect();
    if states.is_empty() || ctx.get_status_change(Duration::ZERO, &mut states).is_err() {
        return false;
    }
    states.iter().any(|s| {
        let now = s.event_state();
        !now.contains(State::PRESENT) || contactless(s.atr())
    })
}

/// Whether `atr` is the one a PC/SC reader makes up for a contactless
/// card (PC/SC Part 3: `3B 8n 80 01 ...`), as an Android phone emulating
/// a card gets. Contact cards and tokens (a YubiKey's `3B FD ...`) never
/// match, so a tap never sends them anything.
fn contactless(atr: &[u8]) -> bool {
    matches!(atr, [0x3B, t0, 0x80, 0x01, ..] if t0 & 0xF0 == 0x80)
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
/// exchange with it. A contactless card that isn't a phone showing an
/// invite (a transit card left on the reader) is passed over until it is
/// lifted, and the wait goes on. Blocking: run it off the GTK thread.
pub fn tap(node: &ThrenodyNode, waiter: &Waiter, timeout: Duration) -> NfcOutcome {
    let until = Instant::now() + timeout;
    let mut last = NfcFailure::Moved;
    let Some(mut watch) = Watch::new(&waiter.0) else {
        return NfcOutcome::Failed { why: last };
    };
    while let Some(reader) = watch.next(&waiter.0, until) {
        let Ok(card) = waiter.0.connect(&reader, ShareMode::Shared, Protocols::ANY) else {
            continue;
        };
        match node
            .nfc_reader()
            .run(Arc::new(PcscTransport(Mutex::new(card))))
        {
            NfcOutcome::Failed {
                why: NfcFailure::NotAnInvite,
            } => last = NfcFailure::NotAnInvite,
            outcome => return outcome,
        }
    }
    NfcOutcome::Failed { why: last }
}

/// The readers, and which hold a card already tried.
struct Watch {
    states: Vec<ReaderState>,
    tried: Vec<bool>,
}

impl Watch {
    fn new(ctx: &Context) -> Option<Self> {
        let states: Vec<ReaderState> = ctx
            .list_readers_owned()
            .ok()?
            .into_iter()
            .map(|r| ReaderState::new(r, State::UNAWARE))
            .collect();
        let tried = vec![false; states.len()];
        Some(Self { states, tried })
    }

    /// The next reader with a contactless card on it not yet tried; a
    /// card counts as new again once it has been lifted.
    fn next(&mut self, ctx: &Context, until: Instant) -> Option<CString> {
        loop {
            let left = until.checked_duration_since(Instant::now())?;
            // Times out, is cancelled, or a reader went away: give up.
            ctx.get_status_change(Some(left), &mut self.states).ok()?;
            for (s, tried) in self.states.iter_mut().zip(&mut self.tried) {
                let now = s.event_state();
                s.sync_current_state();
                if !now.contains(State::PRESENT) {
                    *tried = false;
                } else if !*tried && !now.contains(State::MUTE) && contactless(s.atr()) {
                    *tried = true;
                    return Some(s.name().to_owned());
                }
            }
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
    fn only_contactless_cards_are_tapped() {
        // An Android phone emulating a card, on an ACR122U.
        assert!(contactless(&[0x3B, 0x80, 0x80, 0x01, 0x01]));
        // ISO 14443-4 with historical bytes, and a type B card.
        assert!(contactless(&[
            0x3B, 0x8C, 0x80, 0x01, 0x50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x2C
        ]));
        // A YubiKey over CCID, and a contact smartcard.
        let yubikey = [
            0x3B, 0xFD, 0x13, 0x00, 0x00, 0x81, 0x31, 0xFE, 0x15, 0x80, 0x73, 0xC0, 0x21, 0xC0,
            0x57, 0x59, 0x75, 0x62, 0x69, 0x4B, 0x65, 0x79, 0x40,
        ];
        assert!(!contactless(&yubikey));
        assert!(!contactless(&[0x3B, 0x9F, 0x95, 0x81, 0x31, 0xFE, 0x9F]));
        assert!(!contactless(&[]));
    }

    #[test]
    fn the_buffer_fits_the_largest_response() {
        // more || chunk || SW1 SW2
        const { assert!(RECV >= 1 + threnody_core::tap::CHUNK + 2) };
    }
}
