import Foundation

/// The app's settings, as a property list beside the identity (the node's
/// home): out of backups, readable after the first unlock so background
/// launches can apply them, and erased with everything else. Not
/// `UserDefaults`, which is in backups: an anonymous identity's port there
/// would show that it exists.
///
/// Protective switches read as on unless turned off, as on Android and
/// Linux, whose keys these are. Safe from any thread.
enum Prefs {
    static let cover = "cover_traffic"
    static let onion = "onion_first"
    static let strip = "strip_metadata"
    static let reach = "reach_internet"
    static let volunteers = "use_volunteers"
    static let sendRead = "send_read_receipts"
    static let sendTyping = "send_typing"
    /// Seconds; absent means off.
    static let timer = "default_timer"
    static let handleName = "handle_name"
    static let handleVersion = "handle_version"

    /// The switches that are on unless turned off.
    static let protective = [cover, onion, strip, reach, volunteers, sendRead, sendTyping]

    /// The port an anonymous identity listens on, kept so its invites
    /// keep working across launches.
    static func portKey(_ persona: String) -> String { "port_\(persona)" }

    static var file: URL { Paths.home.appendingPathComponent("ios-settings.plist") }

    private static let lock = NSLock()
    private static var cache: [String: String]?

    /// On unless set to off.
    static func flag(_ key: String) -> Bool { text(key) != "false" }

    static func setFlag(_ key: String, _ on: Bool) { setText(key, on ? "true" : "false") }

    static func number(_ key: String) -> UInt64? { text(key).flatMap { UInt64($0) } }

    static func setNumber(_ key: String, _ n: UInt64?) { setText(key, n.map { String($0) }) }

    static func text(_ key: String) -> String? {
        lock.lock()
        defer { lock.unlock() }
        return load()[key]
    }

    static func setText(_ key: String, _ value: String?) {
        lock.lock()
        defer { lock.unlock() }
        var all = load()
        all[key] = value
        cache = all
        write(all)
    }

    /// Forgets what was read (after an erase, and in tests).
    static func reset() {
        lock.lock()
        cache = nil
        lock.unlock()
    }

    /// With the lock held.
    private static func load() -> [String: String] {
        if let cache { return cache }
        var read: [String: String] = [:]
        if let data = try? Data(contentsOf: file),
           let plist = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: String] {
            read = plist
        }
        cache = read
        return read
    }

    /// With the lock held. The home directory may not exist yet (a setting
    /// changed during onboarding): it is made, and kept out of backups.
    private static func write(_ all: [String: String]) {
        do {
            try FileManager.default.createDirectory(at: Paths.home, withIntermediateDirectories: true)
            try Paths.excludeFromBackup(Paths.home)
            let data = try PropertyListSerialization.data(fromPropertyList: all, format: .binary, options: 0)
            try data.write(to: file, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
        } catch {}
    }
}
