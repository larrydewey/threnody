import SwiftUI

/// The folders above the chat list: All, each folder, and "+".
@MainActor
struct FolderBar: View {
    @EnvironmentObject private var store: NodeStore
    @State private var editing: FolderEditing?

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                chip("All", selected: store.folder == nil) { store.selectFolder(nil) }
                ForEach(store.folders, id: \.id) { f in
                    chip("\(f.emoji) \(f.name)".trimmingCharacters(in: .whitespaces), selected: store.folder == f.id) {
                        store.selectFolder(f.id)
                    }
                    .contextMenu {
                        Button("Edit", systemImage: "pencil") { editing = FolderEditing(folder: f) }
                        Button("Move left", systemImage: "arrow.left") { move(f, by: -1) }
                            .disabled(store.folders.first?.id == f.id)
                        Button("Move right", systemImage: "arrow.right") { move(f, by: 1) }
                            .disabled(store.folders.last?.id == f.id)
                        Button("Delete", systemImage: "trash", role: .destructive) { delete(f) }
                    }
                }
                Button { editing = FolderEditing(folder: nil) } label: {
                    Image(systemName: "plus").padding(.horizontal, 10).padding(.vertical, 6)
                }
                .accessibilityLabel("New folder")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
        }
        .sheet(item: $editing) { e in FolderEditorView(folder: e.folder) }
    }

    private func chip(_ title: String, selected: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title)
                .font(.subheadline)
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .foregroundStyle(selected ? Color.white : Color.primary)
                .background(selected ? Color.accentColor : Color(.secondarySystemBackground), in: Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    private func move(_ f: FolderInfo, by: Int) {
        guard let n = store.node else { return }
        var ids = store.folders.map(\.id)
        guard let i = ids.firstIndex(of: f.id), ids.indices.contains(i + by) else { return }
        ids.swapAt(i, i + by)
        let order = ids
        Task {
            _ = try? await offMain { n.orderFolders(ids: order) }
            store.refresh()
        }
    }

    private func delete(_ f: FolderInfo) {
        guard let n = store.node else { return }
        let id = f.id
        if store.folder == id { store.selectFolder(nil) }
        Task {
            _ = try? await offMain { n.deleteFolder(id: id) }
            store.refresh()
        }
    }
}

/// Which folder the editor edits (nil: a new one).
struct FolderEditing: Identifiable {
    let folder: FolderInfo?
    var id: UInt64 { folder?.id ?? 0 }
}

/// A quiet-hours slot as edited: days (bit 0 Monday) and times.
private struct QuietSlot: Identifiable {
    let id = UUID()
    var days: UInt8
    var start: Date
    var end: Date
}

/// Adds or changes a folder: its chats, the rules that add chats to it,
/// and when its chats notify.
@MainActor
struct FolderEditorView: View {
    let folder: FolderInfo?
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var emoji = ""
    @State private var chats: Set<String> = []
    @State private var rules: UInt32 = 0
    @State private var notify: NotifyLevel?
    @State private var quiet: [QuietSlot] = []
    @State private var breakthroughCount = 0
    @State private var breakthroughWindow = 10
    @State private var failure: String?

    private static let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]

    /// Main-identity chats that can be put in a folder.
    private var candidates: [ChatRow] {
        (store.chats + store.archived).filter { $0.ref.persona == nil && $0.invite == nil }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Emoji", text: $emoji)
                    TextField("Name", text: $name)
                }
                Section("Chats") {
                    ForEach(candidates) { row in
                        let key = row.ref.chatKey
                        Toggle(row.title, isOn: Binding(get: { chats.contains(key) }, set: { on in
                            if on { chats.insert(key) } else { chats.remove(key) }
                        }))
                    }
                }
                Section {
                    rule("All contacts", 1)
                    rule("All groups", 2)
                    rule("Verified contacts", 4)
                    rule("Only chats with unread messages", 8)
                    rule("Only chats that mention me", 16)
                } header: {
                    Text("Rules")
                } footer: {
                    Text("The first three add chats; the last two keep only some.")
                }
                Section {
                    Picker("Notifications", selection: $notify) {
                        Text("As each chat says").tag(NotifyLevel?.none)
                        Text("All messages").tag(NotifyLevel?.some(.all))
                        Text("Mentions only").tag(NotifyLevel?.some(.mentions))
                        Text("Off").tag(NotifyLevel?.some(.off))
                    }
                }
                Section {
                    ForEach($quiet) { $slot in
                        VStack(alignment: .leading) {
                            HStack(spacing: 4) {
                                ForEach(0..<7, id: \.self) { d in
                                    let bit = UInt8(1) << UInt8(d)
                                    Button(Self.days[d]) { slot.days ^= bit }
                                        .font(.caption)
                                        .buttonStyle(.bordered)
                                        .tint(slot.days & bit != 0 ? Color.accentColor : Color.gray)
                                }
                            }
                            DatePicker("From", selection: $slot.start, displayedComponents: .hourAndMinute)
                            DatePicker("Until", selection: $slot.end, displayedComponents: .hourAndMinute)
                        }
                    }
                    .onDelete { quiet.remove(atOffsets: $0) }
                    Button("Add quiet hours") {
                        quiet.append(QuietSlot(days: 0x7F, start: Self.date(22 * 60), end: Self.date(7 * 60)))
                    }
                } header: {
                    Text("Quiet hours")
                } footer: {
                    Text("Its chats don't notify during these hours.")
                }
                Section {
                    Stepper(breakthroughCount == 0 ? "Off" : "After \(breakthroughCount) messages", value: $breakthroughCount, in: 0...10)
                    if breakthroughCount > 0 {
                        Stepper("Within \(breakthroughWindow) minutes", value: $breakthroughWindow, in: 1...120)
                    }
                } header: {
                    Text("Let someone through")
                } footer: {
                    Text("When one person writes to you directly this many times in a short while, you're notified even during quiet hours.")
                }
            }
            .navigationTitle(folder == nil ? "New folder" : "Edit folder")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) { Button("Save") { save() } }
            }
            .alert("Couldn't save", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(failure ?? "")
            }
            .onAppear(perform: fill)
        }
    }

    private func rule(_ title: String, _ bit: UInt32) -> some View {
        Toggle(title, isOn: Binding(get: { rules & bit != 0 }, set: { on in
            if on { rules |= bit } else { rules &= ~bit }
        }))
    }

    /// Today at `minutes` after midnight, for the time pickers.
    private static func date(_ minutes: UInt16) -> Date {
        Calendar.current.startOfDay(for: Date()).addingTimeInterval(TimeInterval(minutes) * 60)
    }

    private static func minutes(_ d: Date) -> UInt16 {
        let c = Calendar.current.dateComponents([.hour, .minute], from: d)
        return UInt16((c.hour ?? 0) * 60 + (c.minute ?? 0))
    }

    private func fill() {
        guard let f = folder else { return }
        name = f.name
        emoji = f.emoji
        chats = Set(f.chats)
        rules = f.rules
        notify = f.notify
        quiet = f.quiet.map { QuietSlot(days: $0.days, start: Self.date($0.start), end: Self.date($0.end)) }
        breakthroughCount = Int(f.breakthroughCount)
        breakthroughWindow = max(1, Int(f.breakthroughWindowMin))
    }

    private func save() {
        guard let n = store.node else { return }
        let info = FolderInfo(
            id: folder?.id ?? 0,
            name: name.trimmingCharacters(in: .whitespaces),
            emoji: emoji.trimmingCharacters(in: .whitespaces),
            chats: Array(chats),
            rules: rules,
            notify: notify,
            quiet: quiet.map { QuietSlotInfo(days: $0.days, start: Self.minutes($0.start), end: Self.minutes($0.end)) },
            breakthroughCount: UInt8(breakthroughCount),
            breakthroughWindowMin: UInt16(breakthroughWindow)
        )
        Task {
            do {
                _ = try await offMain { try n.saveFolder(folder: info) }
                store.refresh()
                dismiss()
            } catch {
                failure = error.localizedDescription
            }
        }
    }
}
