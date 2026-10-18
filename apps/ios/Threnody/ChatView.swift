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
            compose
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
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
    }

    private func appear() async {
        store.visibleChat = devices
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

    var body: some View {
        let url = Paths.resolve(file.location)
        let label = file.sensitive ? "Sensitive file" : (caption.isEmpty ? file.name : caption)
        VStack(alignment: .leading, spacing: 6) {
            if !file.sensitive, let url, let image = UIImage(contentsOfFile: url.path) {
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
