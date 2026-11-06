import CoreNFC
import Foundation

/// Tapping an Android phone that shows its invite (Appendix T). An iPhone
/// can only be the reader: card emulation needs a restricted Apple
/// entitlement. The exchange itself is all in Rust (`NfcReader`); this
/// only moves APDUs between it and Core NFC.
final class NfcTap: NSObject, NFCTagReaderSessionDelegate {
    private var session: NFCTagReaderSession?
    private var reader: NfcReader?
    private var done: (@MainActor (NfcOutcome) -> Void)?

    static var available: Bool { NFCTagReaderSession.readingAvailable }

    /// Opens the system NFC sheet; `done` gets the outcome, unless the
    /// user cancels the sheet.
    func begin(reader: NfcReader, done: @escaping @MainActor (NfcOutcome) -> Void) {
        guard let s = NFCTagReaderSession(pollingOption: .iso14443, delegate: self, queue: nil) else { return }
        self.reader = reader
        self.done = done
        s.alertMessage = "Hold the top of your iPhone to the back of their Android phone while it shows their invite."
        session = s
        s.begin()
    }

    func tagReaderSessionDidBecomeActive(_ session: NFCTagReaderSession) {}

    func tagReaderSession(_ session: NFCTagReaderSession, didInvalidateWithError error: Error) {
        self.session = nil
        reader = nil
        done = nil
    }

    /// Runs on the delegate queue (main). The pending callback is taken
    /// here, before anything can invalidate the session, so neither the
    /// exchange's task nor didInvalidate touches `done` or `reader` again.
    func tagReaderSession(_ session: NFCTagReaderSession, didDetect tags: [NFCTag]) {
        guard let reader, let d = done else { return }
        self.reader = nil
        done = nil
        guard case let .iso7816(tag)? = tags.first else {
            session.invalidate(errorMessage: NfcFailure.notAnInvite.text)
            return Self.deliver(.failed(why: .notAnInvite), to: d)
        }
        Task {
            let outcome = await Self.exchange(session: session, tag: tag, reader: reader)
            switch outcome {
            case .tapped:
                session.alertMessage = "Got their invite"
                session.invalidate()
            case let .failed(why):
                session.invalidate(errorMessage: why.text)
            }
            Self.deliver(outcome, to: d)
        }
    }

    /// The whole exchange. iOS has already selected the AID listed in
    /// Info.plist; the reader's own SELECT simply selects it again.
    private static func exchange(session: NFCTagReaderSession, tag: NFCISO7816Tag, reader: NfcReader) async -> NfcOutcome {
        do {
            try await session.connect(to: .iso7816(tag))
        } catch {
            return .failed(why: .moved)
        }
        var apdu = reader.start()
        // Bounded by the reader, which fails a run that goes on too long.
        while true {
            guard let command = NFCISO7816APDU(data: apdu) else { return .failed(why: .garbled) }
            let response: Data
            do {
                let (data, sw1, sw2) = try await tag.sendCommand(apdu: command)
                response = data + Data([sw1, sw2])
            } catch {
                // The tag went away: a half tap if we already had their invite.
                return reader.lost().map { .tapped(peerLink: $0, mutual: false) } ?? .failed(why: .moved)
            }
            switch reader.step(response: response) {
            case let .send(next): apdu = next
            case let .done(link): return .tapped(peerLink: link, mutual: true)
            case let .failed(why): return .failed(why: why)
            }
        }
    }

    private static func deliver(_ outcome: NfcOutcome, to d: @escaping @MainActor (NfcOutcome) -> Void) {
        Task { @MainActor in d(outcome) }
    }
}

extension NfcFailure {
    /// What to tell the user (the Android app's wording).
    var text: String {
        switch self {
        case .moved: "Hold the phones together until it buzzes"
        case .notAnInvite: "That isn't a phone showing a Threnody invite."
        case .unsupported: "Their Threnody is too new for this one: update, or scan their QR code."
        case .garbled: "Something went wrong: try again, or scan their QR code."
        }
    }
}
