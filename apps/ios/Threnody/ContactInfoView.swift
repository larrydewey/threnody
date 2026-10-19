import SwiftUI

@MainActor
struct ContactInfoView: View {
    let key: String
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var safety: (device: String, number: String)?
    @State private var timer: UInt32?
    @State private var failure: String?
    @State private var confirmBlock = false
    @State private var confirmRevoke = false

    private static let timers: [(String, UInt32?)] = [
        ("Off", nil), ("5 minutes", 300), ("1 hour", 3600), ("1 day", 86400), ("1 week", 604_800),
    ]

    var body: some View {
        Form {
            if let c = store.conversation(key) {
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
                        ForEach(Self.timers.indices, id: \.self) { i in
                            Text(Self.timers[i].0).tag(Self.timers[i].1)
                        }
                    }
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
        .navigationTitle(store.conversation(key)?.title ?? "Contact")
        .navigationBarTitleDisplayMode(.inline)
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .task(id: store.conversation(key)?.unverified) { await load() }
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
        guard let n = store.node, let c = store.conversation(key) else { return }
        if name.isEmpty { name = c.name ?? "" }
        // An unverified device first.
        let target = c.unverified.first ?? c.device
        if let number = try? await offMain({ try n.safetyNumber(peer: target) }) {
            safety = (target, number)
        }
        let d = c.device
        timer = (try? await offMain { try n.disappearing(peer: d) }) ?? nil
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
