import BackgroundTasks
import Combine
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
    static let support = URL.applicationSupportDirectory
    static let home = support.appendingPathComponent("threnody", isDirectory: true)
    static let refreshTask = "org.threnody.app.refresh"

    static var hasIdentity: Bool {
        FileManager.default.fileExists(atPath: home.appendingPathComponent("identity.cbor").path)
    }

    /// Keeps a received file; returns where, relative to Application Support
    /// (the container's absolute path changes between app updates).
    static func save(name: String, data: Data) -> String? {
        let base = (name as NSString).lastPathComponent
        let rel = "media/\(UUID().uuidString)-\(base.isEmpty ? "file" : base)"
        let url = support.appendingPathComponent(rel)
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try data.write(to: url, options: .completeFileProtectionUntilFirstUserAuthentication)
            return rel
        } catch {
            return nil
        }
    }

    static func resolve(_ location: String?) -> URL? {
        guard let location else { return nil }
        let url = support.appendingPathComponent(location)
        return FileManager.default.fileExists(atPath: url.path) ? url : nil
    }
}

/// A chat partner: one account (or lone device) and how to reach it.
struct Conversation: Identifiable, Hashable {
    let key: String
    /// The device to address; sends reach every device of the account.
    let device: String
    let name: String?
    let connected: Bool
    let approved: Bool
    let mutual: Bool
    let verified: Bool
    /// Some device was verified: an unverified one is new (a safety alert).
    let anyVerified: Bool
    /// We want their messages; otherwise this is a message request.
    let accepted: Bool
    let blocked: Bool
    let unverified: [String]
    let devices: [String]

    var id: String { key }
    var title: String { name ?? NodeStore.short(device) }

    static func from(_ contacts: [ContactInfo]) -> [Conversation] {
        var order: [String] = []
        var byKey: [String: [ContactInfo]] = [:]
        for c in contacts {
            let k = c.account ?? c.fingerprint
            if byKey[k] == nil { order.append(k) }
            byKey[k, default: []].append(c)
        }
        return order.map { k in
            let d = byKey[k]!
            return Conversation(
                key: k,
                device: (d.first(where: \.connected) ?? d[0]).fingerprint,
                name: d.lazy.compactMap(\.name).first,
                connected: d.contains(where: \.connected),
                approved: d.contains(where: \.localApproved),
                mutual: d.contains(where: \.mutuallyApproved),
                verified: d.allSatisfy(\.verified),
                anyVerified: d.contains(where: \.verified),
                accepted: d.contains(where: \.accepted),
                blocked: d.contains(where: \.blocked),
                unverified: d.filter { !$0.verified }.map(\.fingerprint),
                devices: d.map(\.fingerprint)
            )
        }
    }
}

struct ChatRow: Identifiable {
    let conversation: Conversation
    let preview: String
    let at: UInt64
    let unread: UInt32
    var id: String { conversation.key }
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
        default:
            return nil
        }
    }
}

/// Owns the node for the whole process, its event pump, and what the
/// screens show. Like Android's `Threnody` object, but iOS suspends the
/// app soon after it leaves the screen: see the README.
@MainActor
final class NodeStore: ObservableObject {
    static let shared = NodeStore()

    enum Phase: Equatable {
        case starting, onboarding, ready
        case failed(String)
    }

    @Published private(set) var phase: Phase = .starting
    @Published private(set) var chats: [ChatRow] = []
    /// Contact devices typing to us right now.
    @Published private(set) var typing: Set<String> = []
    /// A conversation to open (from a notification or a new contact).
    @Published var openChat: String?
    let events = PassthroughSubject<NodeEvent, Never>()

    private(set) var node: ThrenodyNode?
    private(set) var listenAddr = ""
    private(set) var myDevice = ""
    /// The devices of the conversation on screen; their messages don't notify.
    var visibleChat: Set<String> = []
    private var active = true
    private var launched = false
    private let path = PathWatcher()
    private var typingTimeouts: [String: Task<Void, Never>] = [:]
    private var refreshing = false
    private var refreshAgain = false

    nonisolated static func short(_ fp: String) -> String { String(fp.prefix(9)) }

    func launch() {
        guard !launched else { return }
        launched = true
        if Paths.hasIdentity { open(deviceName: nil) } else { phase = .onboarding }
    }

    /// Opens the node, creating a sealed identity on first use.
    func open(deviceName: String?) {
        launched = true
        phase = .starting
        Task {
            do {
                let (n, addr) = try await offMain { try Self.openNode(deviceName: deviceName) }
                started(n, addr)
            } catch {
                phase = .failed(error.localizedDescription)
            }
        }
    }

    private nonisolated static func openNode(deviceName: String?) throws -> (ThrenodyNode, String) {
        var home = Paths.home
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        // Sealed to this device's keys: a copy elsewhere is useless.
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try home.setResourceValues(values)
        let n = try ThrenodyNode.open(home: home.path, passphrase: KeyVault.passphrase(home: home), tunnelPort: nil)
        let addr = (try? n.listen(addr: "0.0.0.0:7450")) ?? (try n.listen(addr: "0.0.0.0:0"))
        if let deviceName, !deviceName.isEmpty, let me = n.devices().first(where: \.thisDevice) {
            try? n.renameDevice(device: me.fingerprint, name: deviceName)
        }
        return (n, addr)
    }

    private func started(_ n: ThrenodyNode, _ addr: String) {
        node = n
        listenAddr = addr
        myDevice = n.deviceFingerprint()
        n.setForeground(on: active)
        n.reconnect()
        // The store lives as long as the process.
        Self.pump(n) { e in Task { @MainActor in self.handle(e) } }
        path.start { cellular in
            n.networkChanged(toStandby: false, mobile: cellular)
            n.reconnect()
        }
        phase = .ready
        refresh()
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
    }

    /// Drains the node's events on a thread of its own, for as long as the process lives.
    private nonisolated static func pump(_ n: ThrenodyNode, _ deliver: @escaping @Sendable (NodeEvent) -> Void) {
        let thread = Thread {
            while true {
                guard let e = n.nextEvent(timeoutMs: 1000) else { continue }
                // Received files are kept and recorded before the screens reload.
                if case let .file(peer, name, data, id, sensitive, caption, album, clip, _, forwarded) = e {
                    let location = Paths.save(name: name, data: data)
                    try? n.recordReceivedFile(
                        peer: peer, name: name, size: UInt64(data.count), location: location, id: id,
                        options: FileOptions(sensitive: sensitive, caption: caption, album: album, clip: clip, forwarded: forwarded)
                    )
                }
                deliver(e)
            }
        }
        thread.name = "threnody-events"
        thread.qualityOfService = .userInitiated
        thread.start()
    }

    private func handle(_ e: NodeEvent) {
        switch e {
        case let .typing(peer, on):
            setTyping(peer, on)
        case let .message(peer, text, _, quiet):
            setTyping(peer, false)
            notify(peer, text, quiet: quiet)
        case let .file(peer, name, _, _, sensitive, caption, _, _, quiet, _):
            notify(peer, sensitive ? "📎 Sensitive file" : "📎 " + (caption.isEmpty ? name : caption), quiet: quiet)
        case let .messageRequest(peer, _, _):
            // Not even who: a stranger's name can be a message in itself.
            notify(peer, "New message request", quiet: false, anonymous: true)
        case let .disconnected(peer, _):
            if conversation(peer)?.mutual == true { node?.reconnect() }
        default:
            break
        }
        events.send(e)
        refresh()
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

    private func notify(_ peer: String, _ body: String, quiet: Bool, anonymous: Bool = false) {
        guard !quiet, !(active && visibleChat.contains(peer)) else { return }
        let c = conversation(peer)
        let content = UNMutableNotificationContent()
        content.title = anonymous ? "Threnody" : (c?.title ?? Self.short(peer))
        content.body = body
        content.sound = .default
        content.threadIdentifier = c?.key ?? peer
        content.userInfo = ["chat": c?.key ?? peer]
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil))
    }

    /// Reloads the chat list; calls while one runs coalesce into one more.
    func refresh() {
        guard let n = node else { return }
        if refreshing {
            refreshAgain = true
            return
        }
        refreshing = true
        Task {
            if let rows = try? await offMain({ Self.rows(n) }) { chats = rows }
            refreshing = false
            if refreshAgain {
                refreshAgain = false
                refresh()
            }
        }
    }

    private nonisolated static func rows(_ n: ThrenodyNode) -> [ChatRow] {
        let unread = Dictionary(n.unreadCounts().map { ($0.chat, $0.unread) }, uniquingKeysWith: { a, _ in a })
        return Conversation.from(n.contacts()).filter { !$0.blocked }.map { c in
            let last = (try? n.history(peer: c.device, limit: 1))?.last
            return ChatRow(conversation: c, preview: last.map(preview) ?? "", at: last?.atMs ?? 0, unread: unread[c.key] ?? 0)
        }.sorted { $0.at > $1.at }
    }

    nonisolated static func preview(_ e: HistoryEntry) -> String {
        let body = e.file.map { "📎 " + (e.text.isEmpty ? $0.name : e.text) } ?? e.text
        return e.outgoing ? "You: " + body : body
    }

    /// The conversation a key or device belongs to.
    func conversation(_ keyOrDevice: String) -> Conversation? {
        chats.lazy.map(\.conversation).first { $0.key == keyOrDevice || $0.devices.contains(keyOrDevice) }
    }

    /// Our invite on this device's current address, if it has one.
    func invite() -> String? {
        guard let n = node else { return nil }
        let port = listenAddr.split(separator: ":").last.map(String.init) ?? "7450"
        if let ip = LocalAddress.ipv4() { return n.inviteLink(addr: "\(ip):\(port)") }
        return n.reachability().addresses.first.map { n.inviteLink(addr: $0) }
    }

    // MARK: Lifecycle

    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid

    func scenePhase(_ phase: ScenePhase) {
        switch phase {
        case .active:
            active = true
            endBackgroundTask()
            UNUserNotificationCenter.current().removeAllDeliveredNotifications()
            node?.setForeground(on: true)
            node?.reconnect()
            refresh()
        case .background:
            active = false
            node?.setForeground(on: false)
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

    func scheduleRefresh() {
        let request = BGAppRefreshTaskRequest(identifier: Paths.refreshTask)
        request.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(request)
    }

    /// A background refresh: reconnects and gives mailboxes a short while
    /// to deliver. iOS decides when (and whether) this runs.
    func backgroundRefresh(_ task: BGAppRefreshTask) {
        scheduleRefresh()
        launch()
        node?.reconnect()
        let work = Task {
            do {
                try await Task.sleep(for: .seconds(20))
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
        [.banner, .sound]
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard let chat = response.notification.request.content.userInfo["chat"] as? String else { return }
        await MainActor.run { NodeStore.shared.openChat = chat }
    }
}
