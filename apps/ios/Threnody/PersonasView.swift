import SwiftUI

/// Settings → Anonymous identities: each has its own keys, contacts and
/// chats, and nothing links it to you unless you reveal it. On iPhone one
/// is reachable only while Threnody is open and on the same network as
/// whoever has its invite: iOS suspends the app, and anonymous identities
/// never use the DHT.
@MainActor
struct PersonasView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var records: [PersonaRecord] = []
    @State private var creating = false
    @State private var showing: PersonaRecord?
    @State private var renaming: PersonaRecord?
    @State private var newLabel = ""
    @State private var burning: PersonaRecord?
    @State private var failure: String?

    var body: some View {
        List {
            Section {
                ForEach(records, id: \.id) { r in
                    Button { showing = r } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text("🎭 \(store.personaLabels[r.id] ?? r.label)").foregroundStyle(.primary)
                            Text(r.expiresMs.map { "Burns \(Date(ms: $0).formatted(.relative(presentation: .named)))" } ?? "Kept until you burn it")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .swipeActions {
                        Button("Burn", role: .destructive) { burning = r }
                        Button("Rename") {
                            newLabel = store.personaLabels[r.id] ?? r.label
                            renaming = r
                        }
                    }
                }
            } footer: {
                Text("Tap one to show its invite; swipe to rename or burn it. On iPhone an anonymous identity can be reached only while Threnody is open and on the same network as whoever has its invite.")
            }
            Section {
                Button("New anonymous invite", systemImage: "plus") { creating = true }
            }
        }
        .navigationTitle("Anonymous identities")
        .navigationBarTitleDisplayMode(.inline)
        .task { await load() }
        .onChange(of: store.personaLabels) { Task { await load() } }
        .sheet(isPresented: $creating) { NavigationStack { NewPersonaView() } }
        .sheet(item: Binding(get: { showing.map(PersonaItem.init) }, set: { showing = $0?.record })) { item in
            NavigationStack { PersonaInviteView(record: item.record) }
        }
        .alert("Rename", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } }),
               presenting: renaming) { r in
            TextField("Label", text: $newLabel)
            Button("Save") {
                let label = newLabel.trimmingCharacters(in: .whitespaces)
                Task {
                    do { try await store.renamePersona(r.id, label: label) } catch { failure = error.localizedDescription }
                    await load()
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Only you see this label.")
        }
        .confirmationDialog("Burn this anonymous identity?", isPresented: Binding(get: { burning != nil }, set: { if !$0 { burning = nil } }),
                            titleVisibility: .visible, presenting: burning) { r in
            Button("Burn", role: .destructive) {
                Task {
                    do { try await store.burnPersona(r.id) } catch { failure = error.localizedDescription }
                    await load()
                }
            }
        } message: { _ in
            Text("Its keys, contacts, messages and files are deleted for good. Nobody can reach it again.")
        }
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
    }

    private func load() async {
        guard let n = store.node else { return }
        records = (try? await offMain { try n.personas() }) ?? []
    }
}

private struct PersonaItem: Identifiable {
    let record: PersonaRecord
    var id: String { record.id }
}

/// A new anonymous identity: its label and when it burns itself.
@MainActor
struct NewPersonaView: View {
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var label = ""
    @State private var burn = 2
    @State private var busy = false
    @State private var failure: String?

    private static let burnChoices: [(String, UInt64?)] = [
        ("Keep it until I burn it", nil), ("Burn after 1 day", 86_400_000),
        ("Burn after 1 week", 604_800_000), ("Burn after 4 weeks", 2_419_200_000),
    ]

    var body: some View {
        Form {
            Section {
                TextField("Your label for it (only you see this)", text: $label)
                Picker("Burn", selection: $burn) {
                    ForEach(Self.burnChoices.indices, id: \.self) { i in Text(Self.burnChoices[i].0).tag(i) }
                }
                .pickerStyle(.inline)
            } footer: {
                Text("A new identity with its own keys and conversations. Nothing links it to you unless you reveal it. Anyone you reach directly can still see your network address.")
            }
        }
        .navigationTitle("Anonymous invite")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button("Create") { create() }.disabled(busy || store.node == nil)
            }
        }
        .alert("Couldn't create it", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
    }

    private func create() {
        let name = label.trimmingCharacters(in: .whitespaces)
        let expires = Self.burnChoices[burn].1.map { UInt64(Date().timeIntervalSince1970 * 1000) + $0 }
        busy = true
        Task {
            do {
                try await store.createPersona(label: name.isEmpty ? "Anonymous" : name, expiresMs: expires)
                dismiss()
            } catch {
                failure = error.localizedDescription
            }
            busy = false
        }
    }
}

/// An anonymous identity's invite, on its own port and this network's
/// address (it never uses the DHT).
@MainActor
struct PersonaInviteView: View {
    let record: PersonaRecord
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss

    private var link: String? {
        guard let n = store.personas[record.id], let ip = LocalAddress.ipv4(),
              let port = store.personaPort(record.id) else { return nil }
        return n.inviteLink(addr: "\(ip):\(port)")
    }

    var body: some View {
        Form {
            if store.personas[record.id] == nil {
                ProgressView("Opening anonymous identity…")
            } else if let link {
                Section {
                    QRCodeView(text: link)
                        .frame(width: 240, height: 240)
                        .frame(maxWidth: .infinity)
                    ShareLink("Share invite", item: link)
                } footer: {
                    Text("Whoever uses this reaches your anonymous identity, not you. It works while this iPhone is on this network and Threnody is open.")
                }
            } else {
                Text("No network: connect to Wi-Fi to make an invite.")
            }
        }
        .navigationTitle("🎭 \(store.personaLabels[record.id] ?? record.label)")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
    }
}
