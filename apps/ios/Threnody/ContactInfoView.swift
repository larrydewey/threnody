import SwiftUI

@MainActor
struct ContactInfoView: View {
    let ref: ChatRef
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var safety: (device: String, number: String)?
    @State private var timer: UInt32?
    @State private var failure: String?
    @State private var confirmBlock = false
    @State private var confirmRevoke = false
    @State private var confirmClear = false
    @State private var level: NotifyLevel = .all

    private var key: String { ref.chatKey }
    private var conversation: Conversation? { store.conversation(key, persona: ref.persona) }
    private var node: ThrenodyNode? { store.node(for: ref.persona) }

    var body: some View {
        Form {
            if let c = conversation {
                Section("Name") {
                    TextField("What you call them", text: $name)
                        .onSubmit {
                            let newName = name
                            act { try $0.setName(peer: c.device, name: newName) }
                        }
                    Text("Only you see this name.").font(.footnote).foregroundStyle(.secondary)
                }
                Section {
                    Toggle("Approved", isOn: Binding(get: { c.approved }, set: { on in
                        if on { act { try $0.setApproval(peer: c.device, approved: true) } } else { confirmRevoke = true }
                    }))
                    LabeledContent("Mutual", value: c.mutual ? "Yes" : "Waiting for them")
                    LabeledContent("Connected", value: c.connected ? "Yes" : "No")
                } header: {
                    Text("Trust")
                } footer: {
                    Text("Mutually approved contacts relay for each other and hold each other's messages while offline.")
                }
                Section {
                    if let safety {
                        Text(Self.pretty(safety.number))
                            .font(.title3.monospaced())
                            .frame(maxWidth: .infinity)
                            .multilineTextAlignment(.center)
                            .textSelection(.enabled)
                        if c.unverified.contains(safety.device) {
                            Button("They match") {
                                let target = safety.device
                                act { try $0.markVerified(peer: target) }
                            }
                        } else {
                            Label("Verified", systemImage: "checkmark.seal.fill").foregroundStyle(.tint)
                        }
                    }
                } header: {
                    Text(c.devices.count > 1 ? "Safety number · device \(NodeStore.short(safety?.device ?? c.device))" : "Safety number")
                } footer: {
                    Text("Compare this with the number on \(c.title)'s screen, in person or on a call. If they match, no one is intercepting your messages.")
                }
                Section("Devices") {
                    ForEach(c.devices, id: \.self) { d in
                        HStack {
                            Text(NodeStore.short(d)).font(.body.monospaced())
                            Spacer()
                            if !c.unverified.contains(d) { Image(systemName: "checkmark.seal.fill").foregroundStyle(.tint) }
                        }
                    }
                }
                Section("Disappearing messages") {
                    Picker("Timer", selection: Binding(get: { timer }, set: { t in
                        timer = t
                        act { try $0.setDisappearing(peer: c.device, seconds: t) }
                    })) {
                        ForEach(Privacy.timers.indices, id: \.self) { i in
                            Text(Privacy.timers[i].0).tag(Privacy.timers[i].1)
                        }
                    }
                }
                Section("Notifications") {
                    Picker("Notify", selection: Binding(get: { level }, set: { l in
                        level = l
                        act { try $0.setNotifyLevel(peer: c.device, level: l) }
                    })) {
                        Text("All messages").tag(NotifyLevel.all)
                        Text("Off").tag(NotifyLevel.off)
                    }
                }
                Section("Chat") {
                    let row = store.row(ChatRef(persona: ref.persona, target: .contact(c.key)))
                    Toggle("Pin chat", isOn: Binding(get: { row?.pinned != nil }, set: { on in
                        let k = c.key
                        act { n in
                            try n.setChatPinned(chat: k, pinned: on)
                            if on && !n.pinnedChats().contains(k) { throw AppError("At most ten chats can be pinned.") }
                        }
                    }))
                    Toggle("Archive chat", isOn: Binding(get: { row?.archived ?? false }, set: { on in
                        let k = c.key
                        act { try $0.setChatArchived(chat: k, archived: on) }
                    }))
                    Button("Clear chat on this device", role: .destructive) { confirmClear = true }
                }
                .confirmationDialog("Clear this chat?", isPresented: $confirmClear, titleVisibility: .visible) {
                    Button("Clear", role: .destructive) { act { try $0.clearConversation(peer: c.device) } }
                } message: {
                    Text("Its messages are deleted from this device. \(c.title) keeps theirs.")
                }
                Section {
                    Button("Block \(c.title)", role: .destructive) { confirmBlock = true }
                }
                .confirmationDialog("Block \(c.title)?", isPresented: $confirmBlock, titleVisibility: .visible) {
                    Button("Block", role: .destructive) {
                        act { try $0.blockContact(peer: c.device) }
                        dismiss()
                    }
                }
                .confirmationDialog("Revoke approval?", isPresented: $confirmRevoke, titleVisibility: .visible) {
                    Button("Revoke", role: .destructive) { act { try $0.setApproval(peer: c.device, approved: false) } }
                } message: {
                    Text("\(c.title) will no longer be able to relay for you or hold your messages.")
                }
            }
        }
        .navigationTitle(conversation?.title ?? "Contact")
        .navigationBarTitleDisplayMode(.inline)
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .task(id: conversation?.unverified) { await load() }
    }

    /// Twelve groups of five digits, three per line.
    static func pretty(_ number: String) -> String {
        let digits = Array(number.filter(\.isNumber))
        let groups = stride(from: 0, to: digits.count, by: 5).map { String(digits[$0..<min($0 + 5, digits.count)]) }
        return stride(from: 0, to: groups.count, by: 3)
            .map { groups[$0..<min($0 + 3, groups.count)].joined(separator: "  ") }
            .joined(separator: "\n")
    }

    private func load() async {
        guard let n = node, let c = conversation else { return }
        if name.isEmpty { name = c.name ?? "" }
        // An unverified device first.
        let target = c.unverified.first ?? c.device
        if let number = try? await offMain({ try n.safetyNumber(peer: target) }) {
            safety = (target, number)
        }
        let d = c.device
        timer = (try? await offMain { try n.disappearing(peer: d) }) ?? nil
        level = (try? await offMain { try n.notifyLevel(peer: d) }) ?? .all
    }

    private func act(_ f: @escaping @Sendable (ThrenodyNode) throws -> Void) {
        guard let n = node else { return }
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
