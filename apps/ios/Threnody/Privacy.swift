import Foundation

/// Applies the privacy settings (`Prefs`) to nodes, as Android's
/// `Threnody.applyPrivacyTo` and Linux's `apply_privacy_to` do. Safe from
/// any thread: nodes are opened off the main thread, and the settings
/// must hold before they listen.
enum Privacy {
    /// Disappearing-timer choices, in seconds (nil = off).
    static let timers: [(String, UInt32?)] = [
        ("Off", nil), ("30 seconds", 30), ("5 minutes", 300), ("1 hour", 3600),
        ("1 day", 86_400), ("1 week", 604_800), ("4 weeks", 2_419_200),
    ]

    private static let lock = NSLock()
    private static var meteredNow = false

    /// On an expensive or Low Data Mode network: cover traffic slows down.
    static var metered: Bool {
        get {
            lock.lock()
            defer { lock.unlock() }
            return meteredNow
        }
        set {
            lock.lock()
            meteredNow = newValue
            lock.unlock()
        }
    }

    /// The cover-traffic interval the settings and network call for.
    static var coverMs: UInt32? { coverIntervalMs(on: Prefs.flag(Prefs.cover), metered: metered) }

    /// Applies every setting to `n`. Call it before `listen`, so a node
    /// never joins the DHT for a moment when the user turned that off.
    /// Anonymous identities never reach across the internet.
    static func apply(to n: ThrenodyNode, persona: Bool) {
        n.setUseVolunteers(on: Prefs.flag(Prefs.volunteers))
        n.setCoverTraffic(intervalMs: coverMs)
        n.setPaceMessages(on: Prefs.flag(Prefs.pace))
        n.setOnionFirst(on: Prefs.flag(Prefs.onion))
        n.setStripMetadata(on: Prefs.flag(Prefs.strip))
        n.setSendReadReceipts(on: Prefs.flag(Prefs.sendRead))
        n.setReachInternet(on: Prefs.flag(Prefs.reach) && !persona)
        n.setDefaultDisappearing(seconds: Prefs.number(Prefs.timer).map { UInt32(clamping: $0) })
    }

    /// Applies the settings to the main node and every anonymous identity's.
    @MainActor static func applyAll(_ store: NodeStore) {
        if let n = store.node { apply(to: n, persona: false) }
        for n in store.personaNodes { apply(to: n, persona: true) }
    }
}
