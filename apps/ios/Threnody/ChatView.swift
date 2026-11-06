import ImageIO
import SwiftUI
import UIKit

@MainActor
struct ChatView: View {
    /// The conversation's key, or a device of it (before its account is known).
    let key: String
    @EnvironmentObject private var store: NodeStore
    @State private var entries: [HistoryEntry] = []
    @State private var pending: [String] = []
    @State private var text = ""
    @State private var sentRead: Set<UInt64> = []
    @State private var typingSent = false
    @State private var typingSentAt = Date.distantPast
    @State private var failure: String?
    /// This chat's scheduled messages, soonest first.
    @State private var scheduled: [ScheduledInfo] = []
    @State private var showLater = false
    @State private var showScheduled = false
    @FocusState private var focused: Bool
    @Environment(\.scenePhase) private var scenePhase

    private var convo: Conversation? { store.conversation(key) }
    private var device: String { convo?.device ?? key }
    private var devices: Set<String> { Set(convo?.devices ?? [key]).union([key]) }

    var body: some View {
        VStack(spacing: 0) {
            if let c = convo { banner(c) }
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 6) {
                        ForEach(Array(entries.enumerated()), id: \.offset) { _, e in
                            Bubble(entry: e, fromThisDevice: e.device == store.myDevice)
                        }
                        ForEach(Array(pending.enumerated()), id: \.offset) { _, body in
                            PendingBubble(text: body)
                        }
                        if !store.typing.isDisjoint(with: devices) {
                            TypingBubble()
                        }
                        Color.clear.frame(height: 1).id("bottom")
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                }
                .defaultScrollAnchor(.bottom)
                .onChange(of: entries.count + pending.count) { proxy.scrollTo("bottom") }
            }
            if !scheduled.isEmpty { scheduledBar }
            compose
        }
        .sheet(isPresented: $showLater) {
            LaterSheet(initial: nil, text: nil) { date, _ in if let date { sendLater(date) } }
        }
        .sheet(isPresented: $showScheduled) {
            ScheduledList(items: scheduled) { item, action in scheduledAction(item, action) }
        }
        .navigationTitle(convo?.title ?? NodeStore.short(key))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                NavigationLink {
                    ContactInfoView(key: key)
                } label: {
                    Image(systemName: "info.circle")
                }
            }
        }
        .alert("Couldn't send", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .task { await appear() }
        .onDisappear(perform: close)
        .onReceive(store.events) { e in
            if let peer = e.peer, devices.contains(peer) { Task { await load() } }
            if case .historySynced = e { Task { await load() } }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { Task { await load() } }
        }
        .onChange(of: text) { _, t in sendTyping(focused && !t.isEmpty) }
    }

    @ViewBuilder private func banner(_ c: Conversation) -> some View {
        if !c.accepted {
            Banner(text: "\(c.title) wants to message you. They can't tell whether you've read this. Accept to reply; block to stop them for good.", warning: true) {
                Button("Accept") { act { try $0.acceptContact(peer: c.device) } }
                Button("Block", role: .destructive) { act { try $0.blockContact(peer: c.device) } }
            }
        } else if !c.verified && c.anyVerified {
            Banner(text: "⚠ \(c.title) added a device you haven't verified. Until you compare its safety number, someone else could be reading as them.", warning: true) {
                NavigationLink("Compare") { ContactInfoView(key: key) }
            }
        } else if !c.approved {
            Banner(text: "Approve \(c.title) once you trust them, and compare safety numbers to make sure no one is in the middle. Mutually approved contacts relay for each other and hold each other's messages while offline.") {
                Button("Approve") { act { try $0.setApproval(peer: c.device, approved: true) } }
                NavigationLink("Compare") { ContactInfoView(key: key) }
            }
        } else if !c.verified {
            Banner(text: "⚠ Not verified. Compare safety numbers with \(c.title) to make sure no one is in the middle.", warning: true) {
                NavigationLink("Compare") { ContactInfoView(key: key) }
            }
        }
    }

    private var compose: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField("Message", text: $text, axis: .vertical)
                .lineLimit(1...6)
                .focused($focused)
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 18))
            Button(action: send) {
                Image(systemName: "arrow.up.circle.fill").font(.system(size: 32))
            }
            .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            .accessibilityHint("Touch and hold to send later")
            .contextMenu {
                Button("Send Later…", systemImage: "clock") { showLater = true }
                    .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
    }

    /// "🕓 2 scheduled · next Tue 08:00"; opens the list.
    private var scheduledBar: some View {
        let failed = scheduled.filter { $0.failed != nil }.count
        var label = "\(scheduled.count) scheduled"
        if let next = scheduled.first(where: { $0.failed == nil }) {
            label += " · next " + Date(ms: next.dueMs).formatted(date: .abbreviated, time: .shortened)
        }
        if failed > 0 { label += " · \(failed) not sent" }
        return Button { showScheduled = true } label: {
            Label(label, systemImage: failed > 0 ? "exclamationmark.triangle" : "clock")
                .font(.footnote)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .foregroundStyle(failed > 0 ? Color.orange : Color.accentColor)
        .background(.bar)
    }

    private func appear() async {
        store.visibleChat = devices
        // Not connected: look for them across the internet now.
        if let n = store.node {
            let d = device
            Task.detached { try? n.seek(peer: d) }
        }
        if let n = store.node, text.isEmpty {
            let d = device
            text = (try? await offMain { try n.draft(peer: d) }) ?? ""
        }
        await load()
    }

    private func close() {
        store.visibleChat = []
        sendTyping(false)
        guard let n = store.node else { return }
        let (d, t) = (device, text)
        Task.detached { try? n.setDraft(peer: d, text: t) }
    }

    private func load() async {
        guard let n = store.node else { return }
        let d = device
        let k = convo?.key ?? key
        guard let h = try? await offMain({ try n.history(peer: d, limit: 300) }) else { return }
        entries = h
        scheduled = (try? await offMain { try n.scheduled(peer: d) }) ?? []
        guard scenePhase == .active else { return }
        _ = try? await offMain { try n.markSeen(chat: k) }
        // Incoming messages now on screen are read.
        let ids = h.filter { !$0.outgoing && $0.id != 0 && !sentRead.contains($0.id) }.map(\.id)
        guard !ids.isEmpty, convo?.accepted != false else { return }
        // Without a session now, they're reported on a later load.
        if (try? await offMain { try n.reportRead(peer: d, ids: ids) }) != nil {
            sentRead.formUnion(ids)
        }
    }

    private func send() {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty, let n = store.node else { return }
        text = ""
        pending.append(body)
        sendTyping(false)
        let d = device
        Task {
            do {
                _ = try await offMain { try n.sendText(peer: d, text: body) }
            } catch {
                failure = error.localizedDescription
            }
            if let i = pending.firstIndex(of: body) { pending.remove(at: i) }
            await load()
            store.refresh()
        }
    }

    /// Schedules what's in the message box for `date`, as `send` sends it now.
    private func sendLater(_ date: Date) {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty, let n = store.node else { return }
        let due = UInt64(max(0, date.timeIntervalSince1970 * 1000))
        // Clearing it also saves an empty draft on leaving, as after a send.
        text = ""
        sendTyping(false)
        let d = device
        Task {
            do {
                _ = try await offMain { try n.scheduleText(peer: d, text: body, dueMs: due, quoteAtMs: nil, quoteDevice: nil) }
            } catch {
                if text.isEmpty { text = body }
                failure = error.localizedDescription
            }
            await load()
        }
    }

    private func scheduledAction(_ item: ScheduledInfo, _ action: ScheduledList.Action) {
        let id = item.id
        switch action {
        case .sendNow:
            act { if !$0.sendScheduledNow(id: id) { throw BeingSent() } }
        case .delete:
            act { if !$0.cancelScheduled(id: id) { throw BeingSent() } }
        case let .edit(newText, date):
            // A time only when the user picked one: a missed message keeps
            // its (past) time and waits for "Send Now".
            let due = date.map { UInt64(max(0, $0.timeIntervalSince1970 * 1000)) }
            let (oldText, oldDue) = (item.text, item.dueMs)
            act { n in
                if newText != oldText {
                    let edited = try n.editScheduled(id: id, text: newText)
                    if !edited { throw BeingSent() }
                }
                if let due, due != oldDue {
                    let moved = try n.reschedule(id: id, dueMs: due)
                    if !moved { throw BeingSent() }
                }
            }
        }
        Task { await load() }
    }

    /// Tells the contact we started or stopped typing: on a change, and
    /// again now and then while typing goes on.
    private func sendTyping(_ on: Bool) {
        guard let n = store.node, convo?.accepted != false else { return }
        if typingSent == on && !(on && Date().timeIntervalSince(typingSentAt) > 4) { return }
        typingSent = on
        typingSentAt = Date()
        let d = device
        Task.detached { try? n.setTyping(peer: d, active: on) }
    }

    private func act(_ f: @escaping @Sendable (ThrenodyNode) throws -> Void) {
        guard let n = store.node else { return }
        Task {
            do {
                try await offMain { try f(n) }
            } catch {
                failure = error.localizedDescription
            }
            store.refresh()
        }
    }
}

struct Banner<Actions: View>: View {
    let text: String
    var warning = false
    @ViewBuilder let actions: () -> Actions

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(text).font(.footnote)
            HStack(spacing: 16) { actions() }.font(.footnote.bold())
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(warning ? Color.orange.opacity(0.15) : Color.accentColor.opacity(0.12))
    }
}

private struct Bubble: View {
    let entry: HistoryEntry
    let fromThisDevice: Bool

    var body: some View {
        let out = entry.outgoing
        HStack {
            if out { Spacer(minLength: 48) }
            VStack(alignment: .trailing, spacing: 2) {
                if let file = entry.file {
                    FileContent(file: file, caption: entry.text)
                } else {
                    Text(entry.text).frame(maxWidth: .infinity, alignment: .leading).fixedSize(horizontal: false, vertical: true)
                }
                Text(meta)
                    .font(.caption2)
                    .foregroundStyle(out ? Color.white.opacity(0.8) : Color.secondary)
                    .accessibilityLabel(meta.replacingOccurrences(of: "✓✓", with: "delivered").replacingOccurrences(of: "✓", with: "sent").replacingOccurrences(of: "👁️", with: "read"))
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .foregroundStyle(out ? Color.white : Color.primary)
            .background(out ? Color.accentColor : Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 16))
            .fixedSize(horizontal: false, vertical: true)
            if !out { Spacer(minLength: 48) }
        }
    }

    private var meta: String {
        var s = Date(ms: entry.atMs).formatted(date: .omitted, time: .shortened)
        if entry.edited { s += " · edited" }
        if entry.disappearing { s += " · ⏱" }
        guard entry.outgoing else { return s }
        // Sent from another of our devices: that device sees its ticks.
        if !fromThisDevice { return s + " · from your other device" }
        if entry.read { return s + " 👁️" }
        return s + (entry.delivered ? " ✓✓" : " ✓")
    }
}

private struct FileContent: View {
    let file: FileInfo
    let caption: String
    @State private var image: UIImage?

    var body: some View {
        let url = Paths.resolve(file.location)
        let label = file.sensitive ? "Sensitive file" : (caption.isEmpty ? file.name : caption)
        VStack(alignment: .leading, spacing: 6) {
            if let image {
                Image(uiImage: image)
                    .resizable()
                    .scaledToFit()
                    .frame(maxWidth: 240, maxHeight: 320)
                    .clipShape(RoundedRectangle(cornerRadius: 10))
            }
            if let url {
                ShareLink(item: url) { Label(label, systemImage: "paperclip") }
            } else {
                Label(label, systemImage: "paperclip")
            }
        }
        .task(id: file.location) {
            guard !file.sensitive, let url else { return }
            image = try? await offMain { Thumbnail.load(url, maxSide: 960) }
        }
    }
}

/// Pictures for chat bubbles: decoded small, off the main thread, and
/// refused when too large, as on Android.
enum Thumbnail {
    /// Larger images are refused rather than decoded (decompression bombs).
    static let maxPixels = 100_000_000.0

    /// The image at `url`, at most `maxSide` pixels on its longer side, or
    /// nil if it isn't one, can't be read, or is too large to decode.
    static func load(_ url: URL, maxSide: Int) -> UIImage? {
        guard let source = CGImageSourceCreateWithURL(url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary),
              let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let w = props[kCGImagePropertyPixelWidth] as? Int,
              let h = props[kCGImagePropertyPixelHeight] as? Int,
              w > 0, h > 0, Double(w) * Double(h) <= maxPixels
        else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: maxSide,
        ]
        guard let cg = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
        return UIImage(cgImage: cg)
    }
}

private struct PendingBubble: View {
    let text: String

    var body: some View {
        HStack {
            Spacer(minLength: 48)
            VStack(alignment: .trailing, spacing: 2) {
                Text(text)
                Text("Sending…").font(.caption2).opacity(0.8)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .foregroundStyle(.white)
            .background(Color.accentColor.opacity(0.6), in: RoundedRectangle(cornerRadius: 16))
        }
    }
}

private struct TypingBubble: View {
    @State private var phase = 0.0

    var body: some View {
        HStack {
            HStack(spacing: 4) {
                ForEach(0..<3) { i in
                    Circle()
                        .frame(width: 7, height: 7)
                        .opacity(0.35 + 0.65 * abs(sin(phase + Double(i) * 0.8)))
                }
            }
            .foregroundStyle(.secondary)
            .padding(.horizontal, 14)
            .padding(.vertical, 12)
            .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 16))
            .accessibilityLabel("typing")
            Spacer()
        }
        .onAppear {
            withAnimation(.linear(duration: 1.2).repeatForever(autoreverses: false)) { phase = .pi }
        }
    }
}

/// A scheduled message changed under us: it's being sent (or went).
private struct BeingSent: LocalizedError {
    var errorDescription: String? { "It's being sent now." }
}

/// Picks when to send: a minute from now up to a year ahead. With `text`,
/// edits a scheduled message's text too, and hands back a date only if
/// the user picked one.
struct LaterSheet: View {
    let initial: Date?
    let text: String?
    let onPick: (Date?, String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var date = Date().addingTimeInterval(3600)
    @State private var dateTouched = false
    @State private var draft = ""

    var body: some View {
        NavigationStack {
            Form {
                if text != nil {
                    Section("Message") {
                        TextField("Message", text: $draft, axis: .vertical).lineLimit(1...8)
                    }
                }
                Section {
                    DatePicker(
                        "Send at",
                        selection: Binding(get: { date }, set: { date = $0; dateTouched = true }),
                        in: Date().addingTimeInterval(60)...Date().addingTimeInterval(365 * 86_400)
                    )
                    .datePickerStyle(.graphical)
                } footer: {
                    Text("Sends when Threnody is open on this iPhone.")
                }
            }
            .navigationTitle(text == nil ? "Send Later" : "Edit Scheduled Message")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button(text == nil ? "Schedule" : "Save") {
                        let t = draft.trimmingCharacters(in: .whitespacesAndNewlines)
                        dismiss()
                        onPick(text == nil || dateTouched ? date : nil, t)
                    }
                    .disabled(text != nil && draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            .onAppear {
                if let initial { date = max(initial, Date().addingTimeInterval(60)) }
                if let text { draft = text }
            }
        }
    }
}

/// A chat's scheduled messages: swipe to delete or send now, tap to edit.
struct ScheduledList: View {
    enum Action {
        case sendNow, delete
        /// The new text, and a new time if one was picked.
        case edit(String, Date?)
    }

    let items: [ScheduledInfo]
    let onAction: (ScheduledInfo, Action) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var editing: ScheduledInfo?

    var body: some View {
        NavigationStack {
            List(items, id: \.id) { item in
                Button { editing = item } label: {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(Date(ms: item.dueMs).formatted(date: .abbreviated, time: .shortened))
                            .font(.caption).foregroundStyle(.secondary)
                        Text(item.text).lineLimit(2).foregroundStyle(.primary)
                        if let f = item.failed {
                            Text("⚠ Not sent: \(NodeStore.reasonText(f))").font(.caption).foregroundStyle(.orange)
                        }
                        if item.sending {
                            Text("Sending now…").font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
                .disabled(item.sending)
                .swipeActions(edge: .trailing) {
                    Button("Delete", role: .destructive) { onAction(item, .delete) }
                }
                .swipeActions(edge: .leading) {
                    Button("Send Now") { onAction(item, .sendNow) }.tint(.accentColor)
                }
            }
            .navigationTitle("Scheduled")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .sheet(item: Binding(get: { editing.map(Editing.init) }, set: { editing = $0?.item })) { e in
                LaterSheet(initial: Date(ms: e.item.dueMs), text: e.item.text) { date, text in
                    onAction(e.item, .edit(text, date))
                }
            }
        }
    }

    private struct Editing: Identifiable {
        let item: ScheduledInfo
        var id: UInt64 { item.id }
    }
}
