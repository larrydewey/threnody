import Foundation

/// Which chat: of which identity (nil = the main one), and what.
struct ChatRef: Hashable, Codable, Sendable {
    enum Target: Hashable, Codable, Sendable {
        /// A contact: its conversation key (account), or a device of it.
        case contact(String)
        /// A group, by its hex id.
        case group(String)
        /// The notes to self (main identity only).
        case notes
        /// An invitation to a group (by its hex id) not yet answered.
        case groupInvite(String)
    }

    let persona: String?
    let target: Target

    /// The node's name for the chat (folders, pins, unread counts): the
    /// account fingerprint, the group's hex id, or "notes".
    var chatKey: String {
        switch target {
        case let .contact(k): k
        case let .group(g), let .groupInvite(g): g
        case .notes: "notes"
        }
    }

    var isGroup: Bool {
        if case .group = target { return true }
        return false
    }

    /// For notification userInfo: "persona|kind|key".
    var encoded: String {
        let p = persona ?? ""
        switch target {
        case let .contact(k): return "\(p)|c|\(k)"
        case let .group(g): return "\(p)|g|\(g)"
        case .notes: return "\(p)|n|"
        case let .groupInvite(g): return "\(p)|i|\(g)"
        }
    }

    init(persona: String?, target: Target) {
        self.persona = persona
        self.target = target
    }

    init?(encoded: String) {
        let parts = encoded.split(separator: "|", omittingEmptySubsequences: false).map(String.init)
        guard parts.count == 3 else { return nil }
        let persona = parts[0].isEmpty ? nil : parts[0]
        let key = parts[2]
        switch parts[1] {
        case "c" where !key.isEmpty: target = .contact(key)
        case "g" where !key.isEmpty: target = .group(key)
        case "i" where !key.isEmpty: target = .groupInvite(key)
        case "n": target = .notes
        default: return nil
        }
        self.persona = persona
    }
}

/// A chat partner: one account (or lone device) and how to reach it.
struct Conversation: Identifiable, Hashable, Sendable {
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

    /// The conversations in `contacts`, one per account. Our own devices
    /// (`me`, and others of our `account`) are contacts once the account
    /// has more than one device; they aren't chats.
    static func from(_ contacts: [ContactInfo], me: String, account: String) -> [Conversation] {
        var order: [String] = []
        var byKey: [String: [ContactInfo]] = [:]
        for c in contacts where c.fingerprint != me && c.account != account && c.fingerprint != account {
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

/// One row of the chat list.
struct ChatRow: Identifiable, Sendable {
    let ref: ChatRef
    let title: String
    /// "🎭 label" for an anonymous identity's chats.
    let tag: String?
    let preview: String
    let at: UInt64
    let unread: UInt32
    let mentions: UInt32
    /// Its place among pinned chats.
    let pinned: Int?
    let archived: Bool
    let conversation: Conversation?
    let group: GroupInfo?
    let invite: GroupInvite?

    var id: String { ref.encoded }
    var connected: Bool { conversation?.connected ?? false }
    var verified: Bool { conversation?.verified ?? false }
    var accepted: Bool { conversation?.accepted ?? true }
    var devices: [String] { conversation?.devices ?? [] }

    /// The chat list of one identity: contacts (not blocked, not our own
    /// devices), groups, the notes to self (main identity only) and group
    /// invitations. Pinned first in pinned order, then the newest first.
    static func rows(_ n: ThrenodyNode, persona: String?, label: String?) -> [ChatRow] {
        let counts = Dictionary(n.unreadCounts().map { ($0.chat, $0) }, uniquingKeysWith: { a, _ in a })
        let pins = n.pinnedChats()
        let archived = Set(n.archivedChats())
        let tag = label.map { "🎭 \($0)" }
        func row(_ ref: ChatRef, _ title: String, last: HistoryEntry?, conversation: Conversation? = nil,
                 group: GroupInfo? = nil, invite: GroupInvite? = nil) -> ChatRow {
            let key = ref.chatKey
            let preview = last.map { ($0.outgoing ? "You: " : "") + entryPreview(entry: $0) } ?? ""
            return ChatRow(
                ref: ref, title: title, tag: tag, preview: preview, at: last?.atMs ?? 0,
                unread: counts[key]?.unread ?? 0, mentions: counts[key]?.mentions ?? 0,
                pinned: pins.firstIndex(of: key), archived: archived.contains(key),
                conversation: conversation, group: group, invite: invite
            )
        }
        var rows: [ChatRow] = []
        let conversations = Conversation.from(n.contacts(), me: n.deviceFingerprint(), account: n.accountFingerprint())
        for c in conversations where !c.blocked {
            let last = (try? n.history(peer: c.device, limit: 1))?.last
            rows.append(row(ChatRef(persona: persona, target: .contact(c.key)), c.title, last: last, conversation: c))
        }
        for g in n.groups() {
            let last = (try? n.groupHistory(group: g.id, limit: 1))?.last
            rows.append(row(ChatRef(persona: persona, target: .group(g.id)), g.name, last: last, group: g))
        }
        if persona == nil {
            let last = (try? n.noteHistory(limit: 1))?.last
            rows.append(row(ChatRef(persona: nil, target: .notes), "Note to self", last: last))
        }
        for i in n.groupInvites() {
            rows.append(row(ChatRef(persona: persona, target: .groupInvite(i.group)), i.name, last: nil, invite: i))
        }
        return sorted(rows)
    }

    /// Pinned rows first, in pinned order; then the newest first.
    static func sorted(_ rows: [ChatRow]) -> [ChatRow] {
        rows.sorted { a, b in
            switch (a.pinned, b.pinned) {
            case let (x?, y?): return x < y
            case (.some, nil): return true
            case (nil, .some): return false
            case (nil, nil): return a.at > b.at
            }
        }
    }
}

/// Sends, loads and deletes in whichever kind of chat it is.
struct ChatTarget: Sendable {
    let node: ThrenodyNode
    let ref: ChatRef
    /// The contact's device to address (sends reach all its devices).
    let device: String

    static let notDelivered = "Not delivered: they're offline and no mutual contact can hold it."

    var isContact: Bool {
        if case .contact = ref.target { return true }
        return false
    }

    func history(limit: UInt32) throws -> [HistoryEntry] {
        switch ref.target {
        case .contact: return try node.history(peer: device, limit: limit)
        case let .group(g), let .groupInvite(g): return try node.groupHistory(group: g, limit: limit)
        case .notes: return try node.noteHistory(limit: limit)
        }
    }

    /// Sends `text`, quoting `replyTo`; a warning to show, if any.
    func send(_ text: String, replyTo: HistoryEntry?) throws -> String? {
        switch ref.target {
        case .contact:
            let reached: UInt32
            if let q = replyTo {
                reached = try node.sendReply(peer: device, text: text, atMs: q.atMs, device: q.device)
            } else {
                reached = try node.sendText(peer: device, text: text)
            }
            return reached == 0 ? Self.notDelivered : nil
        case let .group(g), let .groupInvite(g):
            if let q = replyTo {
                try node.sendGroupReply(group: g, text: text, atMs: q.atMs, device: q.device)
            } else {
                try node.sendGroupText(group: g, text: text)
            }
            return nil
        case .notes:
            try node.writeNote(text: text, quoteAtMs: replyTo?.atMs, quoteDevice: replyTo?.device)
            return nil
        }
    }

    /// Sends a file, kept by us at `location`.
    func sendFile(name: String, data: Data, location: String?, options: FileOptions) throws {
        switch ref.target {
        case .contact: try node.sendFile(peer: device, name: name, data: data, location: location, options: options)
        case let .group(g), let .groupInvite(g):
            try node.sendGroupFile(group: g, name: name, data: data, location: location, options: options)
        case .notes: try node.addNoteFile(name: name, size: UInt64(data.count), location: location, options: options)
        }
    }

    /// Whether reactions are offered: not in notes, not without an id.
    func canReact(_ e: HistoryEntry) -> Bool {
        if case .notes = ref.target { return false }
        return e.id != 0
    }

    func react(_ e: HistoryEntry, _ emoji: String, add: Bool) throws -> Bool {
        switch ref.target {
        case .contact: return try node.react(peer: device, id: e.id, emoji: emoji, add: add)
        case let .group(g), let .groupInvite(g): return try node.reactInGroup(group: g, id: e.id, emoji: emoji, add: add)
        case .notes: return false
        }
    }

    /// Deletes `entries` here (and, with `everyone`, for the contact too),
    /// and our copies of their files.
    func delete(_ entries: [HistoryEntry], everyone: Bool) throws {
        switch ref.target {
        case .contact:
            if entries.allSatisfy({ $0.id != 0 }) {
                _ = try node.deleteMessages(peer: device, ids: entries.map(\.id), everyone: everyone)
            } else {
                for e in entries { _ = try node.deleteEntry(peer: device, atMs: e.atMs, device: e.device) }
            }
        case let .group(g), let .groupInvite(g):
            for e in entries { _ = try node.deleteGroupEntry(group: g, atMs: e.atMs, device: e.device) }
        case .notes:
            for e in entries { _ = try node.deleteNote(atMs: e.atMs, device: e.device) }
        }
        for e in entries { Media.forget(e.file?.location) }
    }

    /// Edits one of our messages; false if it can't be edited any more.
    func edit(_ e: HistoryEntry, _ body: String) throws -> Bool {
        guard isContact else { return false }
        return try node.editMessage(peer: device, id: e.id, body: body)
    }

    func search(_ query: String, limit: UInt32) throws -> [HistoryEntry] {
        switch ref.target {
        case .contact: return try node.searchMessages(peer: device, query: query, limit: limit)
        case let .group(g), let .groupInvite(g): return try node.searchGroupMessages(group: g, query: query, limit: limit)
        case .notes: return try noteSearch(query, limit: limit)
        }
    }

    /// The notes matching `query`: the notes' hits of a search of everything.
    private func noteSearch(_ query: String, limit: UInt32) throws -> [HistoryEntry] {
        let own = Set(node.devices().map(\.fingerprint)).union([node.accountFingerprint()])
        return try node.searchAll(query: query, limit: limit)
            .filter { hit in hit.group == nil && hit.peer.map { own.contains($0) } == true }
            .map(\.entry)
    }

    func draft() -> String {
        switch ref.target {
        case .contact: (try? node.draft(peer: device)) ?? ""
        case let .group(g), let .groupInvite(g): (try? node.groupDraft(group: g)) ?? ""
        case .notes: node.noteDraft()
        }
    }

    func setDraft(_ text: String) {
        switch ref.target {
        case .contact: _ = try? node.setDraft(peer: device, text: text)
        case let .group(g), let .groupInvite(g): _ = try? node.setGroupDraft(group: g, text: text)
        case .notes: _ = try? node.setNoteDraft(text: text)
        }
    }

    func markSeen() {
        _ = try? node.markSeen(chat: ref.chatKey)
    }

    func timer() -> UInt32? {
        switch ref.target {
        case .contact: (try? node.disappearing(peer: device)) ?? nil
        case let .group(g), let .groupInvite(g): (try? node.groupDisappearing(group: g)) ?? nil
        case .notes: nil
        }
    }

    func setTimer(_ seconds: UInt32?) throws {
        switch ref.target {
        case .contact: try node.setDisappearing(peer: device, seconds: seconds)
        case let .group(g), let .groupInvite(g): try node.setGroupDisappearing(group: g, seconds: seconds)
        case .notes: break
        }
    }

    func notifyLevel() -> NotifyLevel {
        switch ref.target {
        case .contact: (try? node.notifyLevel(peer: device)) ?? .all
        case let .group(g), let .groupInvite(g): (try? node.groupNotifyLevel(group: g)) ?? .all
        case .notes: .all
        }
    }

    func setNotifyLevel(_ level: NotifyLevel) throws {
        switch ref.target {
        case .contact: try node.setNotifyLevel(peer: device, level: level)
        case let .group(g), let .groupInvite(g): try node.setGroupNotifyLevel(group: g, level: level)
        case .notes: break
        }
    }

    /// Scheduled messages, soonest first (none in notes).
    func scheduled() throws -> [ScheduledInfo] {
        switch ref.target {
        case .contact: return try node.scheduled(peer: device)
        case let .group(g), let .groupInvite(g): return try node.groupScheduled(group: g)
        case .notes: return []
        }
    }

    func schedule(_ text: String, dueMs: UInt64) throws {
        switch ref.target {
        case .contact:
            _ = try node.scheduleText(peer: device, text: text, dueMs: dueMs, quoteAtMs: nil, quoteDevice: nil)
        case let .group(g), let .groupInvite(g):
            _ = try node.scheduleGroupText(group: g, text: text, dueMs: dueMs, quoteAtMs: nil, quoteDevice: nil, mentions: [])
        case .notes:
            throw AppError("Notes can't be scheduled.")
        }
    }
}
