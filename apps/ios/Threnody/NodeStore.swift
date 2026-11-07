import BackgroundTasks
import Combine
import os
import SwiftUI
import UIKit
import UserNotifications

/// Runs blocking node calls off the main thread.
func offMain<T>(_ work: @escaping @Sendable () throws -> T) async throws -> T {
    try await withCheckedThrowingContinuation { c in
        DispatchQueue.global(qos: .userInitiated).async { c.resume(with: Result(catching: work)) }
    }
}

enum Paths {
    /// Tests point these at a temporary directory.
    static var supportOverride: URL?
    static var homeOverride: URL?

    static var support: URL { supportOverride ?? URL.applicationSupportDirectory }
    static var home: URL { homeOverride ?? support.appendingPathComponent("threnody", isDirectory: true) }
    /// The main identity's files (`Media`), kept out of backups like the node's directory.
    static var media: URL { Media.dir(nil) }
    static let refreshTask = "org.threnody.app.refresh"
    /// The reminder that a scheduled message is due.
    static let scheduledReminder = "scheduled-next"
    /// The category of message notifications: "New message" when previews are hidden.
    static let messageCategory = "message"

    static var hasIdentity: Bool {
        FileManager.default.fileExists(atPath: home.appendingPathComponent("identity.cbor").path)
    }

    /// Keeps `dir` and everything in it out of iCloud and computer backups.
    static func excludeFromBackup(_ dir: URL) throws {
        var dir = dir
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try dir.setResourceValues(values)
    }
}

/// Stops an event pump: an anonymous identity's when it is burned.
final class PumpFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var on = true

    var alive: Bool {
        lock.lock()
        defer { lock.unlock() }
        return on
    }

    func stop() {
        lock.lock()
        on = false
        lock.unlock()
    }
}

extension NodeEvent {
    /// The contact device an event is about, if any.
    var peer: String? {
        switch self {
        case let .message(peer, _, _, _), let .delivered(peer, _), let .read(peer), let .messageEdited(peer),
             let .messagesDeleted(peer, _), let .approvalChanged(peer, _), let .connected(peer, _),
             let .disconnected(peer, _), let .messageRequest(peer, _, _), let .reacted(peer, _),
             let .pinned(peer, _, _), let .typing(peer, _), let .profileChanged(peer):
            return peer
        case let .file(peer, _, _, _, _, _, _, _, _, _):
            return peer
        case let .scheduledChanged(peer?, _):
            return peer
        case let .scheduledFailed(peer?, _, _, _):
            return peer
        default:
            return nil
        }
    }

    /// The group an event is about, if any.
    var group: String? {
        switch self {
        case let .groupMessage(group, _, _, _, _, _), let .groupMembersChanged(group, _, _, _),
             let .groupRoleChanged(group, _, _), let .groupLeft(group), let .groupJoined(group, _, _),
             let .groupChangeFailed(group, _), let .groupUpgradePending(group, _), let .groupOutOfStep(group):
            return group
        case let .groupFile(_, group, _, _, _, _, _, _, _, _, _, _):
            return group
        case let .delivered(_, group?), let .reacted(_, group?), let .pinned(_, group?, _),
             let .scheduledChanged(_, group?), let .scheduledFailed(_, group?, _, _):
            return group
        default:
            return nil
        }
    }
}

/// An event, and which identity's node it came from (nil = the main one).
struct StoreEvent {
    let persona: String?
    let event: NodeEvent
}

/// Owns the nodes for the whole process (the main identity's and each
/// anonymous identity's), their event pumps, and what the screens show.
/// Like Android's `Threnody` object, but iOS suspends the app soon after
/// it leaves the screen: see the README.
@MainActor
final class NodeStore: ObservableObject {
    static let shared = NodeStore()

    enum Phase: Equatable {
        case starting, onboarding, ready
        case failed(String)
    }

    @Published private(set) var phase: Phase = .starting
    /// Every identity's chats, archived ones left out.
    @Published private(set) var chats: [ChatRow] = []
    @Published private(set) var archived: [ChatRow] = []
    @Published private(set) var folders: [FolderInfo] = []
    /// The folder shown (nil = All), and the chat keys in it.
    @Published private(set) var folder: UInt64?
    @Published private(set) var folderChats: Set<String> = []
    /// Contact devices typing to us right now.
    @Published private(set) var typing: Set<String> = []
    /// A chat to open (from a notification, a search or a new contact).
    @Published var openChat: ChatRef?
    /// A message to scroll to when that chat opens, and the search that found it.
    var pendingJump: (ref: ChatRef, entry: HistoryEntry, query: String)?
    /// Onboarding chose to join an existing account: show that screen.
    @Published var pendingJoin = false
    /// A link code to join with, from a scanned QR code.
    @Published var joinCode: String?
    /// Another device removed this one from the account.
    @Published private(set) var deviceRemoved = false
    /// Anonymous identities' labels by id, and whether they're all open.
    @Published private(set) var personaLabels: [String: String] = [:]
    @Published private(set) var personasReady = false
    let events = PassthroughSubject<StoreEvent, Never>()

    private(set) var node: ThrenodyNode?
    private(set) var personas: [String: ThrenodyNode] = [:]
    private var pumps: [String: PumpFlag] = [:]
    private(set) var listenAddr = ""
    private(set) var myDevice = ""
    /// The chat on screen, and its contact's devices: their messages don't notify.
    var visibleChat: ChatRef?
    var visibleDevices: Set<String> = []
    private var active = true
    private var launched = false
    /// Started by a background refresh: anonymous identities and the
    /// handle wait for the app to come to the front.
    private var launchedInBackground = false
    /// The app came to the front while the node was still opening.
    private var wantsForeground = false
    private var foregroundDone = false
    private var sweeper: Task<Void, Never>?
    private let path = PathWatcher()
    private var typingTimeouts: [String: Task<Void, Never>] = [:]
    private var refreshing = false
    private var refreshAgain = false

    nonisolated static func short(_ fp: String) -> String { String(fp.prefix(9)) }

    /// Bumped by an erase: threads of the node before it stop, and write
    /// nothing more (received files are saved outside the node's home).
    private nonisolated static let generation = OSAllocatedUnfairLock(initialState: 0)
    nonisolated static var currentGeneration: Int { generation.withLock { $0 } }

    var personaNodes: [ThrenodyNode] { Array(personas.values) }

    /// The node of `persona` (nil = the main identity), if open.
    func node(for persona: String?) -> ThrenodyNode? {
        guard let persona else { return node }
        return personas[persona]
    }

    func launch() {
        guard !launched else { return }
        launched = true
        if Paths.hasIdentity { open(deviceName: nil) } else { phase = .onboarding }
    }

    /// Opens the node, creating a sealed identity on first use. `lock`:
    /// what onboarding chose for app lock (nil for an existing identity).
    func open(deviceName: String?, lock: Bool? = nil) {
        launched = true
        phase = .starting
        Task {
            do {
                let (n, addr) = try await offMain { try Self.openNode(deviceName: deviceName) }
                started(n, addr, lock: lock)
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }

    private nonisolated static func openNode(deviceName: String?) throws -> (ThrenodyNode, String) {
        let home = Paths.home
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        // Sealed to this device's keys: a copy elsewhere is useless.
        try Paths.excludeFromBackup(home)
        // Received files too; ones kept before this was done included.
        if FileManager.default.fileExists(atPath: Paths.media.path) { try? Paths.excludeFromBackup(Paths.media) }
        // A new identity gets a new sealing secret, so nothing sealed under
        // an earlier one (an erase that died part way) can be opened.
        if !Paths.hasIdentity { KeyVault.forget() }
        let n = try ThrenodyNode.open(home: home.path, passphrase: KeyVault.passphrase(home: home), tunnelPort: nil)
        // The user's settings before it listens: with "Reach contacts over
        // the internet" off, it never joins the DHT, even for a moment.
        Privacy.apply(to: n, persona: false)
        // The usual port, or any free one if it's taken.
        let addr: String
        if let a = try? n.listen(addr: "0.0.0.0:7450") {
            addr = a
        } else {
            addr = try n.listen(addr: "0.0.0.0:0")
        }
        if let deviceName, !deviceName.isEmpty, let me = n.devices().first(where: \.thisDevice) {
            try? n.renameDevice(device: me.fingerprint, name: deviceName)
        }
        return (n, addr)
    }

    private func started(_ n: ThrenodyNode, _ addr: String, lock: Bool?) {
        node = n
        listenAddr = addr
        myDevice = n.deviceFingerprint()
        n.setForeground(on: active)
        n.reconnect()
        // The settings are applied (before listening), so scheduled
        // messages may go out now.
        n.startScheduledSending()
        setUtcOffset()
        // Until an erase: then a new node gets its own.
        let gen = Self.currentGeneration
        Self.pump(n, gen, persona: nil, flag: PumpFlag()) { e in
            Task { @MainActor in if Self.currentGeneration == gen { self.handle(e, persona: nil, node: n) } }
        }
        startSweeper()
        path.start { network, typeChanged in
            // Cover traffic slows down on expensive and Low Data networks.
            Privacy.metered = network.metered
            Task { @MainActor in
                let store = NodeStore.shared
                guard let main = store.node else { return }
                let nodes = [main] + store.personaNodes
                Task.detached {
                    let cover = Privacy.coverMs
                    for n in nodes { n.setCoverTraffic(intervalMs: cover) }
                    if typeChanged {
                        main.networkChanged(toStandby: false, mobile: network.cellular)
                        main.reconnect()
                    }
                }
            }
        }
        UNUserNotificationCenter.current().setNotificationCategories([
            UNNotificationCategory(
                identifier: Paths.messageCategory, actions: [], intentIdentifiers: [],
                hiddenPreviewsBodyPlaceholder: "New message", options: []
            ),
        ])
        phase = .ready
        defaultLock(onboarding: lock)
        refresh()
        armScheduled()
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
        if !launchedInBackground || wantsForeground { foregroundSetup() }
    }

    /// Once per process, the first time the app is in front with a node:
    /// publishes the handle (Argon2id) and opens anonymous identities.
    /// Never during a background refresh.
    private func foregroundSetup() {
        guard !foregroundDone, let n = node else { return }
        foregroundDone = true
        Handles.resume(n)
        openPersonas(n)
    }

    /// Minutes east of UTC for quiet hours; again on every return to the
    /// app, as a daylight-saving change sends no notification.
    func setUtcOffset() {
        node?.setUtcOffset(minutes: Int32(TimeZone.current.secondsFromGMT() / 60))
    }

    /// App lock comes on by itself (device lock) where the phone has a
    /// passcode: as onboarding chose for a new identity, and once, with a
    /// notice and a way to turn it off, on installs from before it.
    private func defaultLock(onboarding: Bool?) {
        let key = "lockOffered"
        if UserDefaults.standard.bool(forKey: key) { return }
        UserDefaults.standard.set(true, forKey: key)
        guard !AppLock.isEnabled, AppLock.deviceSecure else { return }
        if let onboarding {
            if onboarding { Task { await AppLock.shared.enableDevice() } }
        } else {
            Task {
                await AppLock.shared.enableDevice()
                AppLock.shared.notice = AppLock.isEnabled
            }
        }
    }

    /// Drains a node's events on a thread of its own, until an erase bumps
    /// the generation or `flag` stops (an anonymous identity burned).
    private nonisolated static func pump(
        _ n: ThrenodyNode, _ gen: Int, persona: String?, flag: PumpFlag,
        _ deliver: @escaping @Sendable (NodeEvent) -> Void
    ) {
        let thread = Thread {
            while NodeStore.currentGeneration == gen && flag.alive {
                guard let e = n.nextEvent(timeoutMs: 1000) else { continue }
                // Erased or burned meanwhile: nothing of the old node is kept.
                if NodeStore.currentGeneration != gen || !flag.alive { return }
                // Received files are kept and recorded before the screens reload.
                switch e {
                case let .file(peer, name, data, id, sensitive, caption, album, clip, _, forwarded):
                    guard let location = NodeStore.keep(name, data, gen, persona) else { return }
                    try? n.recordReceivedFile(
                        peer: peer, name: name, size: UInt64(data.count), location: location, id: id,
                        options: FileOptions(sensitive: sensitive, caption: caption, album: album, clip: clip, forwarded: forwarded)
                    )
                case let .groupFile(id, group, from, name, data, _, sensitive, caption, album, clip, _, forwarded):
                    guard let location = NodeStore.keep(name, data, gen, persona) else { return }
                    try? n.recordReceivedGroupFile(
                        group: group, from: from, name: name, size: UInt64(data.count), location: location, id: id,
                        options: FileOptions(sensitive: sensitive, caption: caption, album: album, clip: clip, forwarded: forwarded)
                    )
                default:
                    break
                }
                deliver(e)
            }
        }
        thread.name = "threnody-events"
        thread.qualityOfService = .userInitiated
        thread.start()
    }

    /// Saves a received file holding the generation, so an erase can't
    /// remove the media directory between the check and the write. Nil
    /// once erased; `.some(nil)` if it couldn't be saved.
    private nonisolated static func keep(_ name: String, _ data: Data, _ gen: Int, _ persona: String?) -> String?? {
        generation.withLock { now -> String?? in
            now == gen ? .some(Media.save(name: name, data: data, persona: persona)) : .none
        }
    }

    private func handle(_ e: NodeEvent, persona: String?, node n: ThrenodyNode) {
        switch e {
        case let .typing(peer, on):
            setTyping(peer, on)
        case let .message(peer, text, _, quiet):
            setTyping(peer, false)
            notifyContact(peer, persona: persona, node: n, body: text, quiet: quiet)
        case let .file(peer, name, _, _, sensitive, caption, _, clip, quiet, _):
            let label = fileLabel(name: name, sensitive: sensitive, caption: caption, clip: clip)
            notifyContact(peer, persona: persona, node: n, body: label, quiet: quiet)
        case let .messageRequest(peer, _, _):
            // Not even who: a stranger's name can be a message in itself.
            notify(ChatRef(persona: persona, target: .contact(peer)), title: nil, body: "New message request",
                   quiet: false, anonymous: true, disappears: false)
        case let .groupMessage(group, from, text, ours, _, quiet):
            if !ours { notifyGroup(group, from: from, persona: persona, node: n, body: text, quiet: quiet) }
        case let .groupFile(_, group, from, name, _, ours, sensitive, caption, _, clip, quiet, _):
            if !ours {
                let label = fileLabel(name: name, sensitive: sensitive, caption: caption, clip: clip)
                notifyGroup(group, from: from, persona: persona, node: n, body: label, quiet: quiet)
            }
        case let .groupInvited(group, name, from, quiet):
            groupInvited(group, name: name, from: from, persona: persona, node: n, quiet: quiet)
        case let .disconnected(peer, _):
            if conversation(peer, persona: persona)?.mutual == true { n.reconnect() }
        case .scheduledChanged:
            if persona == nil { armScheduled() }
        case let .scheduledFailed(peer?, _, _, reason):
            // Why, never the text.
            notifyContact(peer, persona: persona, node: n, body: "Scheduled message not sent: \(Self.reasonText(reason))", quiet: false)
        case let .accountChanged(account, added, _):
            accountChanged(account, added: added, persona: persona, node: n)
        case .thisDeviceRemoved:
            if persona == nil {
                deviceRemoved = true
                notify(nil, title: "Threnody", body: "This device was removed from your account. Your contacts no longer accept it as you.",
                       quiet: false, disappears: false)
            }
        case .messagesDeleted:
            // Their files go too.
            Task.detached(priority: .background) { Media.sweep(n, persona: persona) }
        default:
            break
        }
        events.send(StoreEvent(persona: persona, event: e))
        refresh()
    }

    /// A device joined an account: ours (say so), or a verified contact's,
    /// whose new device we haven't verified (a safety alert). Worked out
    /// afresh from the contacts, which already have the new device.
    private func accountChanged(_ account: String, added: [String], persona: String?, node n: ThrenodyNode) {
        guard !added.isEmpty else { return }
        if account == n.accountFingerprint() {
            guard persona == nil else { return }
            notify(nil, title: "Threnody", body: "A new device joined your account. If you didn't link it, remove it in Settings → Devices.",
                   quiet: false, disappears: false)
            return
        }
        let new = Set(added)
        Task {
            let found = try? await offMain {
                Conversation.from(n.contacts(), me: n.deviceFingerprint(), account: n.accountFingerprint())
                    .first { $0.key == account || !new.isDisjoint(with: $0.devices) }
            }
            guard let c = found ?? nil, c.anyVerified else { return }
            notify(ChatRef(persona: persona, target: .contact(c.key)), title: "Safety alert: \(c.title)",
                   body: "\(c.title) added a device you haven't verified. Compare safety numbers.",
                   quiet: false, disappears: false, safety: true)
        }
    }

    /// From a contact we accepted: the group's name and who invites. From
    /// anyone else, not even the group's name: a stranger's can be a
    /// message in itself (as on Android).
    private func groupInvited(_ group: String, name: String, from: String, persona: String?, node n: ThrenodyNode, quiet: Bool) {
        guard !quiet else { return }
        let ref = ChatRef(persona: persona, target: .groupInvite(group))
        let gen = Self.currentGeneration
        Task {
            let accepted = (try? await offMain { n.contacts().contains { $0.fingerprint == from && $0.accepted } }) ?? false
            guard Self.currentGeneration == gen else { return }
            if accepted {
                let who = conversation(from, persona: persona)?.title ?? Self.short(from)
                notify(ref, title: name, body: "\(who) invites you to join", quiet: false, disappears: false)
            } else {
                notify(ref, title: nil, body: "New group invitation", quiet: false, anonymous: true, disappears: false)
            }
        }
    }

    private func setTyping(_ peer: String, _ on: Bool) {
        typingTimeouts.removeValue(forKey: peer)?.cancel()
        if on {
            typing.insert(peer)
            // A stop can be lost; the bubble goes by itself.
            typingTimeouts[peer] = Task {
                try? await Task.sleep(for: .seconds(8))
                if !Task.isCancelled { typing.remove(peer) }
            }
        } else {
            typing.remove(peer)
        }
    }

    /// Private notifications, on unless turned off in Settings: a
    /// notification says only "New message", not who wrote or what.
    static var privateNotifications: Bool {
        UserDefaults.standard.object(forKey: "privateNotifications") as? Bool ?? true
    }

    private func notifyContact(_ peer: String, persona: String?, node n: ThrenodyNode, body: String, quiet: Bool) {
        if active && visibleChat?.persona == persona && visibleDevices.contains(peer) { return }
        let c = conversation(peer, persona: persona)
        // A disappearing message's text would outlive it in the notification.
        // If it can't be told, it is taken to disappear.
        let disappears = (try? n.history(peer: peer, limit: 1))?.last?.disappearing ?? true
        notify(ChatRef(persona: persona, target: .contact(c?.key ?? peer)), title: c?.title ?? Self.short(peer),
               body: body, quiet: quiet, disappears: disappears)
    }

    private func notifyGroup(_ group: String, from: String, persona: String?, node n: ThrenodyNode, body: String, quiet: Bool) {
        let ref = ChatRef(persona: persona, target: .group(group))
        let who = conversation(from, persona: persona)?.title ?? Self.short(from)
        let name = n.groups().first { $0.id == group }?.name ?? "Group"
        let disappears = (try? n.groupHistory(group: group, limit: 1))?.last?.disappearing ?? true
        notify(ref, title: name, body: "\(who): \(body)", quiet: quiet, disappears: disappears)
    }

    /// Posts a notification for `ref` (nil: about the app itself), unless
    /// it's `quiet` or that chat is on screen. With app lock on, it says
    /// nothing and names no chat; with private notifications, only "New
    /// message"; a disappearing message's text is never shown.
    private func notify(_ ref: ChatRef?, title: String?, body: String, quiet: Bool, anonymous: Bool = false,
                        disappears: Bool, safety: Bool = false) {
        guard !quiet else { return }
        if let ref, active, visibleChat == ref { return }
        let content = UNMutableNotificationContent()
        content.sound = .default
        content.categoryIdentifier = Paths.messageCategory
        if AppLock.isEnabled {
            content.title = "Threnody"
            content.body = "New message"
            content.threadIdentifier = "message"
            UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "message", content: content, trigger: nil))
            return
        }
        if anonymous || ref == nil {
            content.title = title ?? "Threnody"
            content.body = body
        } else if Self.privateNotifications {
            content.title = "Threnody"
            content.body = safety ? "Safety alert: a contact added a device you haven't verified." : "New message"
        } else {
            content.title = title ?? "Threnody"
            content.body = disappears ? "New message" : body
            if let p = ref?.persona { content.subtitle = "🎭 \(personaLabels[p] ?? "Anonymous identity")" }
        }
        if let ref {
            content.threadIdentifier = ref.encoded
            content.userInfo = ["chat": ref.encoded]
        }
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil))
    }

    /// Why a scheduled message wasn't sent.
    nonisolated static func reasonText(_ r: ScheduleFailure) -> String {
        switch r {
        case .missed: return "missed its time"
        case .contact: return "the contact was blocked or removed"
        case .group: return "you're no longer in the group"
        case .error: return "couldn't send"
        case .removed: return "this device was removed from your account"
        }
    }

    /// iOS suspends the app, so a scheduled message goes only while it
    /// runs: at its time, a reminder (saying nothing of who or what) asks
    /// to open the app, and a background refresh is asked for then too.
    func armScheduled() {
        let center = UNUserNotificationCenter.current()
        center.removePendingNotificationRequests(withIdentifiers: [Paths.scheduledReminder])
        guard let next = node?.nextScheduledMs() else { return }
        let wait = max(1, Date(ms: next).timeIntervalSinceNow)
        let content = UNMutableNotificationContent()
        content.title = "Threnody"
        content.body = "A scheduled message is due. Open Threnody to send it."
        content.sound = .default
        center.add(UNNotificationRequest(
            identifier: Paths.scheduledReminder,
            content: content,
            trigger: UNTimeIntervalNotificationTrigger(timeInterval: wait, repeats: false)
        ))
        scheduleRefresh(at: Date(ms: next))
    }

    /// Reloads the chat list; calls while one runs coalesce into one more.
    func refresh() {
        guard let n = node else { return }
        if refreshing {
            refreshAgain = true
            return
        }
        refreshing = true
        let others = personas.map { (id: $0.key, node: $0.value, label: personaLabels[$0.key] ?? "Anonymous identity") }
        let selected = folder
        Task {
            let loaded = try? await offMain { () -> ([ChatRow], [FolderInfo], [String]) in
                var rows = ChatRow.rows(n, persona: nil, label: nil)
                for p in others { rows += ChatRow.rows(p.node, persona: p.id, label: p.label) }
                let inFolder = selected.map { n.folderChats(id: $0) } ?? []
                return (ChatRow.sorted(rows), n.folders(), inFolder)
            }
            if let loaded {
                let (rows, newFolders, inFolder) = loaded
                chats = rows.filter { !$0.archived }
                archived = rows.filter(\.archived)
                folders = newFolders
                if folder == selected { folderChats = Set(inFolder) }
                if let f = folder, !newFolders.contains(where: { $0.id == f }) { folder = nil }
            }
            refreshing = false
            if refreshAgain {
                refreshAgain = false
                refresh()
            }
        }
    }

    /// Shows folder `id` (nil = All).
    func selectFolder(_ id: UInt64?) {
        folder = id
        folderChats = []
        refresh()
    }

    /// The conversation a key or device belongs to, in `persona`'s chats.
    func conversation(_ keyOrDevice: String, persona: String? = nil) -> Conversation? {
        (chats + archived).lazy.filter { $0.ref.persona == persona }.compactMap(\.conversation)
            .first { $0.key == keyOrDevice || $0.devices.contains(keyOrDevice) }
    }

    /// The row for `ref`, if it is in the list.
    func row(_ ref: ChatRef) -> ChatRow? {
        (chats + archived).first { $0.ref == ref }
    }

    /// What we call a device: "You", a contact's name, or the start of
    /// its fingerprint.
    func name(_ device: String, persona: String? = nil) -> String {
        if device == myDevice { return "You" }
        if let n = node(for: persona), device == n.deviceFingerprint() { return "You" }
        return conversation(device, persona: persona)?.title ?? Self.short(device)
    }

    /// Our invite on this device's current address, if it has one.
    func invite() -> String? {
        guard let n = node else { return nil }
        let port = listenAddr.split(separator: ":").last.map(String.init) ?? "7450"
        // The node adds its IPv6 and public addresses, and the key that
        // finds this device on mobile data; with neither, there's no invite.
        let addr = LocalAddress.ipv4().map { "\($0):\(port)" } ?? ""
        if addr.isEmpty && !n.reachInternet() && n.reachability().addresses.isEmpty { return nil }
        return n.inviteLink(addr: addr)
    }

    // MARK: Anonymous identities

    /// Burns the ones whose time is up, then opens the rest, one after
    /// another off the main thread (each unlock runs Argon2id).
    private func openPersonas(_ main: ThrenodyNode) {
        let gen = Self.currentGeneration
        Task {
            let burned = (try? await offMain { try main.burnExpiredPersonas() }) ?? []
            for id in burned { forgetPersona(id, gen: gen) }
            let records = (try? await offMain { try main.personas() }) ?? []
            for r in records where Self.currentGeneration == gen {
                personaLabels[r.id] = r.label
                await openPersona(r, gen: gen)
            }
            personasReady = true
            refresh()
        }
    }

    private func openPersona(_ r: PersonaRecord, gen: Int) async {
        guard personas[r.id] == nil else { return }
        let id = r.id
        let home = r.home
        let opened = try? await offMain { () -> ThrenodyNode in
            let n = try ThrenodyNode.open(home: home, passphrase: KeyVault.passphrase(home: URL(fileURLWithPath: home)), tunnelPort: nil)
            Privacy.apply(to: n, persona: true)
            // The same port as before, so its invites keep working.
            let addr: String
            if let port = Prefs.number(Prefs.portKey(id)), let a = try? n.listen(addr: "0.0.0.0:\(port)") {
                addr = a
            } else {
                addr = try n.listen(addr: "0.0.0.0:0")
            }
            // Not if an erase came meanwhile: the port would say it existed.
            if let port = addr.split(separator: ":").last.flatMap({ UInt64($0) }) {
                Prefs.setNumber(Prefs.portKey(id), port, generation: gen)
            }
            return n
        }
        guard let n = opened else { return }
        guard Self.currentGeneration == gen, personas[id] == nil else {
            n.shutdown()
            return
        }
        personas[id] = n
        let flag = PumpFlag()
        pumps[id] = flag
        n.setForeground(on: active)
        n.reconnect()
        Self.pump(n, gen, persona: id, flag: flag) { e in
            Task { @MainActor in
                if Self.currentGeneration == gen && flag.alive { self.handle(e, persona: id, node: n) }
            }
        }
    }

    /// The port an anonymous identity listens on, for its invite.
    func personaPort(_ id: String) -> UInt64? { Prefs.number(Prefs.portKey(id)) }

    func createPersona(label: String, expiresMs: UInt64?) async throws {
        guard let main = node else { return }
        let r = try await offMain { try main.createPersona(label: label, expiresMs: expiresMs, passphrase: KeyVault.secret()) }
        personaLabels[r.id] = r.label
        await openPersona(r, gen: Self.currentGeneration)
        refresh()
    }

    func renamePersona(_ id: String, label: String) async throws {
        guard let main = node else { return }
        try await offMain { try main.renamePersona(id: id, label: label) }
        personaLabels[id] = label
        refresh()
    }

    /// Deletes an anonymous identity and everything it kept.
    func burnPersona(_ id: String) async throws {
        guard let main = node else { return }
        let gen = Self.currentGeneration
        pumps.removeValue(forKey: id)?.stop()
        let n = personas.removeValue(forKey: id)
        if visibleChat?.persona == id { visibleChat = nil }
        try await offMain {
            n?.shutdown()
            try main.burnPersona(id: id)
        }
        forgetPersona(id, gen: gen)
        refresh()
    }

    /// `gen`: when the burn began. After an erase there is nothing to forget.
    private func forgetPersona(_ id: String, gen: Int) {
        guard Self.currentGeneration == gen else { return }
        Media.remove(persona: id)
        Prefs.setNumber(Prefs.portKey(id), nil, generation: gen)
        personaLabels.removeValue(forKey: id)
    }

    // MARK: Files

    /// Deletes files no message refers to any more, every five minutes
    /// while the app is in front; then burns anonymous identities whose
    /// time is up.
    private func startSweeper() {
        sweeper?.cancel()
        let gen = Self.currentGeneration
        sweeper = Task { [weak self] in
            while !Task.isCancelled && Self.currentGeneration == gen {
                guard let self, let main = self.node else { return }
                let nodes: [(String?, ThrenodyNode)] = [(nil, main)] + self.personas.map { (Optional($0.key), $0.value) }
                _ = try? await offMain {
                    for (persona, n) in nodes { Media.sweep(n, persona: persona) }
                }
                let now = UInt64(Date().timeIntervalSince1970 * 1000)
                if let records = try? await offMain({ try main.personas() }) {
                    for r in records where (r.expiresMs ?? .max) <= now && self.personas[r.id] != nil {
                        try? await self.burnPersona(r.id)
                    }
                }
                try? await Task.sleep(for: .seconds(300))
            }
        }
    }

    // MARK: Erasing

    /// Erases everything ("Erase all Threnody data", the duress PIN): the
    /// node's data directory (unless `alreadyErased`) with the settings and
    /// anonymous identities in it, every identity's files, the Secure
    /// Enclave key and Keychain item, settings, notifications and
    /// background tasks, then shows onboarding as a first launch does.
    /// Nothing is sent: contacts and other devices aren't told.
    func wipe(alreadyErased: Bool = false) async {
        let gen = Self.generation.withLock { g -> Int in
            g += 1
            return g
        }
        sweeper?.cancel()
        sweeper = nil
        for flag in pumps.values { flag.stop() }
        let all = [node].compactMap { $0 } + personaNodes
        node = nil
        personas = [:]
        pumps = [:]
        _ = try? await offMain {
            for n in all { n.shutdown() }
            if !alreadyErased { _ = try? wipeHome(home: Paths.home.path) }
            Media.removeAll()
            KeyVault.forget()
        }
        // After the node's erase: a settings write that landed since goes too.
        Prefs.erased(generation: gen)
        if let id = Bundle.main.bundleIdentifier { UserDefaults.standard.removePersistentDomain(forName: id) }
        let center = UNUserNotificationCenter.current()
        center.removeAllDeliveredNotifications()
        center.removeAllPendingNotificationRequests()
        BGTaskScheduler.shared.cancelAllTaskRequests()
        for t in typingTimeouts.values { t.cancel() }
        typingTimeouts = [:]
        chats = []
        archived = []
        folders = []
        folder = nil
        folderChats = []
        typing = []
        openChat = nil
        pendingJump = nil
        pendingJoin = false
        joinCode = nil
        deviceRemoved = false
        personaLabels = [:]
        personasReady = false
        visibleChat = nil
        visibleDevices = []
        myDevice = ""
        listenAddr = ""
        launched = false
        launchedInBackground = false
        wantsForeground = false
        foregroundDone = false
        AppLock.shared.reset()
        phase = .onboarding
    }

    // MARK: Lifecycle

    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid

    func scenePhase(_ phase: ScenePhase) {
        switch phase {
        case .active:
            active = true
            endBackgroundTask()
            UNUserNotificationCenter.current().removeAllDeliveredNotifications()
            for n in [node].compactMap({ $0 }) + personaNodes {
                n.setForeground(on: true)
                n.reconnect()
            }
            // The network may have become metered while away.
            Privacy.applyAll(self)
            setUtcOffset()
            if node != nil {
                foregroundSetup()
                startSweeper()
            } else {
                // Opened during a background refresh's cold start: once the node is up.
                wantsForeground = true
            }
            refresh()
            // Sends what's due, and replaces a stale reminder.
            node?.startScheduledSending()
            armScheduled()
        case .background:
            active = false
            sweeper?.cancel()
            sweeper = nil
            for n in [node].compactMap({ $0 }) + personaNodes { n.setForeground(on: false) }
            // A little time to finish sends; then iOS suspends us.
            if backgroundTask == .invalid {
                backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "threnody-flush") { [weak self] in
                    self?.endBackgroundTask()
                }
            }
            scheduleRefresh()
        default:
            break
        }
    }

    private func endBackgroundTask() {
        guard backgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTask)
        backgroundTask = .invalid
    }

    /// Asks iOS for a background refresh in 15 minutes, or sooner when a
    /// scheduled message is due (`at`).
    func scheduleRefresh(at: Date? = nil) {
        var earliest = Date(timeIntervalSinceNow: 15 * 60)
        if let due = at ?? node?.nextScheduledMs().map({ Date(ms: $0) }) { earliest = min(earliest, due) }
        let request = BGAppRefreshTaskRequest(identifier: Paths.refreshTask)
        request.earliestBeginDate = earliest
        try? BGTaskScheduler.shared.submit(request)
    }

    /// A background refresh: reconnects and gives mailboxes a short while
    /// to deliver. iOS decides when (and whether) this runs. Anonymous
    /// identities and the handle wait until the app is opened.
    func backgroundRefresh(_ task: BGAppRefreshTask) {
        scheduleRefresh()
        if !launched { launchedInBackground = true }
        launch()
        node?.reconnect()
        let work = Task {
            do {
                // The node opens asynchronously on a cold start.
                let started = Date()
                while node == nil && Date().timeIntervalSince(started) < 5 {
                    try await Task.sleep(for: .milliseconds(250))
                }
                // Scheduled messages that are due go now (at once if none are).
                if let n = node {
                    _ = try? await offMain { n.runDueScheduled(timeoutMs: 20_000) }
                    armScheduled()
                }
                // At least the usual while for mailboxes to deliver.
                let left = 20 - Date().timeIntervalSince(started)
                if left > 0 { try await Task.sleep(for: .seconds(left)) }
                task.setTaskCompleted(success: true)
            } catch {}
        }
        task.expirationHandler = { @Sendable in
            work.cancel()
            task.setTaskCompleted(success: false)
        }
    }
}

/// Shows notifications while the app is open, and opens their chat when tapped.
final class NotificationDelegate: NSObject, UNUserNotificationCenterDelegate {
    static let shared = NotificationDelegate()

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification)
        async -> UNNotificationPresentationOptions {
        // The app is open: the scheduled message is going out already.
        if notification.request.identifier == Paths.scheduledReminder { return [] }
        return [.banner, .sound]
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard let chat = response.notification.request.content.userInfo["chat"] as? String else { return }
        // Notifications from earlier versions carry a bare contact key.
        let ref = ChatRef(encoded: chat) ?? ChatRef(persona: nil, target: .contact(chat))
        await MainActor.run { NodeStore.shared.openChat = ref }
    }
}
