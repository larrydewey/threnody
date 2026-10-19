import SwiftUI

@MainActor
struct ChatListView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var path: [String] = []
    @State private var adding = false
    @State private var settings = false

    var body: some View {
        NavigationStack(path: $path) {
            List(store.chats) { row in
                NavigationLink(value: row.id) {
                    ChatRowView(row: row, typing: !store.typing.isDisjoint(with: row.conversation.devices))
                }
            }
            .listStyle(.plain)
            .overlay {
                if store.chats.isEmpty {
                    ContentUnavailableView(
                        "No chats yet", systemImage: "bubble.left.and.bubble.right",
                        description: Text("Tap + to add a contact with their invite or QR code.")
                    )
                }
            }
            .navigationTitle("Threnody")
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { settings = true } label: { Image(systemName: "gearshape") }
                }
                ToolbarItem(placement: .primaryAction) {
                    Button { adding = true } label: { Image(systemName: "plus") }
                }
            }
            .navigationDestination(for: String.self) { ChatView(key: $0) }
            .sheet(isPresented: $adding) { AddContactView() }
            .sheet(isPresented: $settings) { SettingsView() }
            .refreshable { store.refresh() }
            .onChange(of: store.openChat) { _, key in
                guard let key else { return }
                adding = false
                path = [key]
                store.openChat = nil
            }
        }
    }
}

private struct ChatRowView: View {
    let row: ChatRow
    let typing: Bool

    var body: some View {
        let c = row.conversation
        HStack(spacing: 12) {
            Circle()
                .fill(Color.accentColor.gradient)
                .frame(width: 44, height: 44)
                .overlay(Text(c.title.prefix(1).uppercased()).font(.headline).foregroundStyle(.white))
                .overlay(alignment: .bottomTrailing) {
                    if c.connected {
                        Circle().fill(.green).frame(width: 12, height: 12)
                            .overlay(Circle().stroke(Color(.systemBackground), lineWidth: 2))
                    }
                }
            VStack(alignment: .leading, spacing: 2) {
                HStack {
                    Text(c.title).font(.headline).lineLimit(1)
                    if c.verified { Image(systemName: "checkmark.seal.fill").foregroundStyle(.tint).font(.caption) }
                    Spacer()
                    if row.at > 0 {
                        Text(Date(ms: row.at), format: .relative(presentation: .numeric))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                HStack {
                    Group {
                        if !c.accepted {
                            Text("Message request").italic()
                        } else if typing {
                            Text("typing…").italic()
                        } else {
                            Text(row.preview)
                        }
                    }
                    .font(.subheadline).foregroundStyle(.secondary).lineLimit(1)
                    Spacer()
                    if row.unread > 0 {
                        Text("\(row.unread)")
                            .font(.caption.bold()).foregroundStyle(.white)
                            .padding(.horizontal, 7).padding(.vertical, 2)
                            .background(Color.accentColor, in: Capsule())
                    }
                }
            }
        }
        .padding(.vertical, 4)
    }
}

extension Date {
    init(ms: UInt64) { self.init(timeIntervalSince1970: Double(ms) / 1000) }
}

@MainActor
struct SettingsView: View {
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                if let n = store.node {
                    Section("This device") {
                        LabeledContent("Device", value: NodeStore.short(n.deviceFingerprint()))
                        LabeledContent("Account", value: NodeStore.short(n.accountFingerprint()))
                        LabeledContent("Listening on", value: store.listenAddr)
                        Text("Identity \(KeyVault.describe).").font(.footnote).foregroundStyle(.secondary)
                    }
                }
                Section("Background") {
                    Text("iOS suspends Threnody shortly after you leave it. Messages sent to you meanwhile wait with mutually approved contacts (mailboxes) and arrive when you open the app again, or during the occasional background refresh iOS allows.")
                        .font(.footnote).foregroundStyle(.secondary)
                }
            }
            .navigationTitle("Settings")
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
    }
}
