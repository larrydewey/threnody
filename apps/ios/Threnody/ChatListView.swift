import SwiftUI

@MainActor
struct ChatListView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var path: [ChatRef] = []
    @State private var adding = false
    @State private var settings = false
    @State private var newGroup = false
    @State private var lists = false
    @State private var newPersona = false
    @State private var query = ""
    @State private var failure: String?

    /// The rows the selected folder shows (persona chats only under All).
    private var rows: [ChatRow] {
        guard store.folder != nil else { return store.chats }
        return store.chats.filter { $0.ref.persona == nil && store.folderChats.contains($0.ref.chatKey) }
    }

    var body: some View {
        NavigationStack(path: $path) {
            List {
                if query.trimmingCharacters(in: .whitespaces).isEmpty {
                    if store.deviceRemoved {
                        Label("This device was removed from your account.", systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange).font(.footnote)
                    }
                    ForEach(rows) { row in
                        NavigationLink(value: row.ref) {
                            ChatRowView(row: row, typing: !store.typing.isDisjoint(with: row.devices))
                        }
                        .swipeActions(edge: .leading) {
                            if row.invite == nil {
                                Button(row.pinned == nil ? "Pin" : "Unpin", systemImage: row.pinned == nil ? "pin" : "pin.slash") {
                                    pin(row, row.pinned == nil)
                                }
                                .tint(.orange)
                            }
                        }
                        .swipeActions(edge: .trailing) {
                            if row.invite == nil {
                                Button("Archive", systemImage: "archivebox") { archive(row, true) }.tint(.gray)
                            }
                        }
                    }
                    if !store.archived.isEmpty && store.folder == nil {
                        NavigationLink {
                            ArchivedView()
                        } label: {
                            Label("Archived (\(store.archived.count))", systemImage: "archivebox")
                                .foregroundStyle(.secondary)
                        }
                    }
                } else {
                    SearchResultsView(query: query) { ref in path.append(ref) }
                }
            }
            .listStyle(.plain)
            .safeAreaInset(edge: .top) {
                if query.isEmpty { FolderBar().background(.bar) }
            }
            .searchable(text: $query, prompt: "Search")
            .autocorrectionDisabled()
            .textInputAutocapitalization(.never)
            .overlay {
                if store.chats.isEmpty && query.isEmpty {
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
                        .accessibilityLabel("Settings")
                }
                ToolbarItem(placement: .primaryAction) {
                    Menu {
                        Button("Add contact", systemImage: "person.badge.plus") { adding = true }
                        Button("New group", systemImage: "person.3") { newGroup = true }
                        Button("Note to self", systemImage: "note.text") {
                            path.append(ChatRef(persona: nil, target: .notes))
                        }
                        Button("Lists", systemImage: "checklist") { lists = true }
                        Button("New anonymous invite", systemImage: "theatermasks") { newPersona = true }
                    } label: {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("New")
                }
            }
            .navigationDestination(for: ChatRef.self) { ref in
                if case let .groupInvite(g) = ref.target {
                    GroupInviteView(ref: ref, group: g)
                } else {
                    ChatView(ref: ref)
                }
            }
            .navigationDestination(isPresented: $lists) { ListsView() }
            .sheet(isPresented: $adding) { AddContactView() }
            .sheet(isPresented: $settings) { SettingsView() }
            .sheet(isPresented: $newGroup) {
                NewGroupView { ref in
                    newGroup = false
                    path.append(ref)
                }
            }
            .sheet(isPresented: $newPersona) { NavigationStack { NewPersonaView() } }
            .refreshable { store.refresh() }
            .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(failure ?? "")
            }
            .onChange(of: store.openChat) { _, ref in
                guard let ref else { return }
                adding = false
                settings = false
                path = [ref]
                store.openChat = nil
            }
        }
    }

    private func pin(_ row: ChatRow, _ on: Bool) {
        guard let n = store.node(for: row.ref.persona) else { return }
        let key = row.ref.chatKey
        Task {
            do {
                try await offMain {
                    try n.setChatPinned(chat: key, pinned: on)
                    // The node keeps at most ten, quietly.
                    if on && !n.pinnedChats().contains(key) { throw AppError("At most ten chats can be pinned.") }
                }
            } catch {
                failure = error.localizedDescription
            }
            store.refresh()
        }
    }

    private func archive(_ row: ChatRow, _ on: Bool) {
        guard let n = store.node(for: row.ref.persona) else { return }
        let key = row.ref.chatKey
        Task {
            do {
                try await offMain { try n.setChatArchived(chat: key, archived: on) }
            } catch {
                failure = error.localizedDescription
            }
            store.refresh()
        }
    }
}

/// Archived chats: swipe to bring one back.
@MainActor
struct ArchivedView: View {
    @EnvironmentObject private var store: NodeStore

    var body: some View {
        List(store.archived) { row in
            NavigationLink(value: row.ref) {
                ChatRowView(row: row, typing: false)
            }
            .swipeActions(edge: .trailing) {
                Button("Unarchive", systemImage: "tray.and.arrow.up") {
                    guard let n = store.node(for: row.ref.persona) else { return }
                    let key = row.ref.chatKey
                    Task {
                        _ = try? await offMain { try n.setChatArchived(chat: key, archived: false) }
                        store.refresh()
                    }
                }
                .tint(.accentColor)
            }
        }
        .listStyle(.plain)
        .overlay {
            if store.archived.isEmpty { ContentUnavailableView("No archived chats", systemImage: "archivebox") }
        }
        .navigationTitle("Archived")
        .navigationBarTitleDisplayMode(.inline)
    }
}

struct ChatRowView: View {
    let row: ChatRow
    let typing: Bool

    private var icon: String? {
        switch row.ref.target {
        case .group, .groupInvite: "person.3.fill"
        case .notes: "note.text"
        case .contact: nil
        }
    }

    var body: some View {
        HStack(spacing: 12) {
            Circle()
                .fill(Color.accentColor.gradient)
                .frame(width: 44, height: 44)
                .overlay {
                    if let icon {
                        Image(systemName: icon).foregroundStyle(.white)
                    } else {
                        Text(row.title.prefix(1).uppercased()).font(.headline).foregroundStyle(.white)
                    }
                }
                .overlay(alignment: .bottomTrailing) {
                    if row.connected {
                        Circle().fill(.green).frame(width: 12, height: 12)
                            .overlay(Circle().stroke(Color(.systemBackground), lineWidth: 2))
                    }
                }
            VStack(alignment: .leading, spacing: 2) {
                HStack {
                    if row.pinned != nil { Image(systemName: "pin.fill").font(.caption2).foregroundStyle(.secondary) }
                    Text(row.title).font(.headline).lineLimit(1)
                    if row.verified { Image(systemName: "checkmark.seal.fill").foregroundStyle(.tint).font(.caption) }
                    Spacer()
                    if row.at > 0 {
                        Text(Date(ms: row.at), format: .relative(presentation: .numeric))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                if let tag = row.tag {
                    Text(tag).font(.caption).foregroundStyle(.purple).lineLimit(1)
                }
                HStack {
                    Group {
                        if row.invite != nil {
                            Text("Invitation").italic()
                        } else if !row.accepted {
                            Text("Message request").italic()
                        } else if typing {
                            Text("typing…").italic()
                        } else {
                            Text(row.preview)
                        }
                    }
                    .font(.subheadline).foregroundStyle(.secondary).lineLimit(1)
                    Spacer()
                    if row.mentions > 0 {
                        Text("@")
                            .font(.caption.bold()).foregroundStyle(.white)
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(Color.orange, in: Capsule())
                            .accessibilityLabel("Mentions you")
                    }
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
