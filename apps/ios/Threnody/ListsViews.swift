import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// Renders a list item's markdown: bold, italics, code and strikethrough.
/// Links are dropped, so an item can't show text that goes somewhere else
/// when tapped.
enum ListMarkdown {
    static func render(_ text: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        guard var a = try? AttributedString(markdown: text, options: options) else { return AttributedString(text) }
        a.link = nil
        return a
    }
}

/// Settings → Lists: shared checklists.
@MainActor
struct ListsView: View {
    var persona: String?
    @EnvironmentObject private var store: NodeStore
    @State private var lists: [ListInfo] = []
    @State private var naming = false
    @State private var title = ""
    @State private var opened: String?
    @State private var failure: String?

    private var node: ThrenodyNode? { store.node(for: persona) }

    var body: some View {
        List {
            ForEach(lists, id: \.id) { l in
                NavigationLink {
                    ListEditorView(id: l.id, persona: persona)
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(l.title.isEmpty ? "List" : l.title)
                        let done = l.items.filter { $0.done && !$0.heading }.count
                        let all = l.items.filter { !$0.heading }.count
                        Text("\(done) of \(all) done · \(Date(ms: l.updatedMs).formatted(.relative(presentation: .named)))")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                .swipeActions {
                    Button("Delete", role: .destructive) {
                        let id = l.id
                        act { try $0.deleteList(id: id) }
                    }
                }
            }
        }
        .overlay {
            if lists.isEmpty { ContentUnavailableView("No lists yet", systemImage: "checklist") }
        }
        .navigationTitle("Lists")
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    title = ""
                    naming = true
                } label: {
                    Image(systemName: "plus")
                }
                .accessibilityLabel("New list")
            }
        }
        .navigationDestination(item: $opened) { id in ListEditorView(id: id, persona: persona) }
        .alert("New list", isPresented: $naming) {
            TextField("Title", text: $title)
            Button("Create") { create() }
            Button("Cancel", role: .cancel) {}
        }
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .task { load() }
        .onReceive(store.events) { se in
            if se.persona == persona, case .listChanged = se.event { load() }
        }
    }

    private func load() {
        lists = node?.lists() ?? []
    }

    private func create() {
        guard let n = node else { return }
        let t = title.trimmingCharacters(in: .whitespaces)
        Task {
            do {
                opened = try await offMain { try n.createList(title: t) }
            } catch {
                failure = error.localizedDescription
            }
            load()
        }
    }

    private func act(_ f: @escaping @Sendable (ThrenodyNode) throws -> Void) {
        guard let n = node else { return }
        Task {
            do {
                try await offMain { try f(n) }
            } catch {
                failure = error.localizedDescription
            }
            load()
        }
    }
}

/// One list: tick, edit, reorder, indent; share it with a chat.
@MainActor
struct ListEditorView: View {
    let id: String
    var persona: String?
    @EnvironmentObject private var store: NodeStore
    @State private var list: ListInfo?
    @State private var newItem = ""
    @State private var editing: ListItemInfo?
    @State private var editText = ""
    @State private var renaming = false
    @State private var newTitle = ""
    @State private var pasting = false
    @State private var pasted = ""
    @State private var sharing = false
    @State private var failure: String?
    @FocusState private var adding: Bool

    private var node: ThrenodyNode? { store.node(for: persona) }

    var body: some View {
        List {
            if let l = list {
                ForEach(l.items, id: \.id) { item in
                    row(item)
                }
                .onMove { from, to in move(from: from, to: to) }
                TextField("Add an item", text: $newItem)
                    .focused($adding)
                    .onSubmit { add() }
                if !l.sharedWith.isEmpty {
                    Section("Shared with") {
                        ForEach(l.sharedWith, id: \.self) { chat in
                            HStack {
                                Text(chatName(chat))
                                Spacer()
                                Button("Stop sharing") {
                                    act { try $0.unshareList(id: id, chat: chat) }
                                }
                                .font(.caption)
                            }
                        }
                    }
                }
            } else {
                Text("This list was deleted.").foregroundStyle(.secondary)
            }
        }
        .navigationTitle(list.map { $0.title.isEmpty ? "List" : $0.title } ?? "List")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Menu {
                    Button("Rename", systemImage: "pencil") {
                        newTitle = list?.title ?? ""
                        renaming = true
                    }
                    Button("Clear done items", systemImage: "checkmark.circle") {
                        act { _ = try $0.clearDoneItems(id: id) }
                    }
                    Button("Paste markdown", systemImage: "doc.on.clipboard") {
                        pasted = ""
                        pasting = true
                    }
                    Button("Copy as markdown", systemImage: "doc.on.doc") { copy() }
                    Button("Share with…", systemImage: "square.and.arrow.up") { sharing = true }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
            }
            ToolbarItem(placement: .topBarTrailing) { EditButton() }
        }
        .alert("Edit item", isPresented: Binding(get: { editing != nil }, set: { if !$0 { editing = nil } }),
               presenting: editing) { item in
            TextField("Item", text: $editText)
            Button("Save") {
                let (itemId, text) = (item.id, editText)
                act { try $0.editListItem(id: id, item: itemId, text: text) }
            }
            Button("Cancel", role: .cancel) {}
        }
        .alert("Rename list", isPresented: $renaming) {
            TextField("Title", text: $newTitle)
            Button("Save") {
                let t = newTitle.trimmingCharacters(in: .whitespaces)
                act { try $0.renameList(id: id, title: t) }
            }
            Button("Cancel", role: .cancel) {}
        }
        .sheet(isPresented: $pasting) {
            NavigationStack {
                Form {
                    TextField("- [ ] item\n- [x] done\n# heading", text: $pasted, axis: .vertical).lineLimit(4...12)
                    PasteButton(payloadType: String.self) { strings in pasted = strings.first ?? "" }
                }
                .navigationTitle("Paste markdown")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) { Button("Cancel") { pasting = false } }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Add") {
                            let md = pasted
                            pasting = false
                            act { _ = try $0.addListMarkdown(id: id, markdown: md) }
                        }
                    }
                }
            }
        }
        .sheet(isPresented: $sharing) {
            ShareListView(chats: shareTargets) { chat in
                sharing = false
                act { try $0.shareList(id: id, chat: chat) }
            }
        }
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .task { load() }
        .onReceive(store.events) { se in
            if se.persona == persona, case let .listChanged(changed) = se.event, changed == id { load() }
        }
    }

    private func row(_ item: ListItemInfo) -> some View {
        HStack(spacing: 8) {
            if item.heading {
                Text(ListMarkdown.render(item.text)).font(.headline)
            } else {
                Button {
                    let (itemId, done) = (item.id, !item.done)
                    act { try $0.setListItemDone(id: id, item: itemId, done: done) }
                } label: {
                    Image(systemName: item.done ? "checkmark.circle.fill" : "circle")
                }
                .buttonStyle(.plain)
                .accessibilityLabel(item.done ? "Done" : "Not done")
                Text(ListMarkdown.render(item.text))
                    .strikethrough(item.done)
                    .foregroundStyle(item.done ? Color.secondary : Color.primary)
            }
            Spacer()
        }
        .padding(.leading, CGFloat(item.indent) * 20)
        .contentShape(Rectangle())
        .onTapGesture {
            editText = item.text
            editing = item
        }
        .swipeActions(edge: .trailing) {
            Button("Delete", role: .destructive) {
                let itemId = item.id
                act { try $0.deleteListItem(id: id, item: itemId) }
            }
        }
        .swipeActions(edge: .leading) {
            Button("Indent") {
                let (itemId, indent) = (item.id, min(item.indent + 1, 3))
                act { try $0.indentListItem(id: id, item: itemId, indent: indent) }
            }
            .disabled(item.indent >= 3)
            Button("Outdent") {
                let (itemId, indent) = (item.id, item.indent > 0 ? item.indent - 1 : 0)
                act { try $0.indentListItem(id: id, item: itemId, indent: indent) }
            }
            .disabled(item.indent == 0)
        }
    }

    /// The chats a list can be shared with: contacts and groups.
    private var shareTargets: [ChatRow] {
        store.chats.filter { row in
            guard row.ref.persona == persona else { return false }
            switch row.ref.target {
            case .contact, .group: return row.accepted
            default: return false
            }
        }
    }

    private func chatName(_ chat: String) -> String {
        store.chats.first { $0.ref.persona == persona && $0.ref.chatKey == chat }?.title ?? NodeStore.short(chat)
    }

    private func load() {
        list = node?.list(id: id)
    }

    private func add() {
        let text = newItem.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return }
        newItem = ""
        let last = list?.items.last?.id
        act { _ = try $0.addListItem(id: id, after: last, text: text, heading: false) }
        adding = true
    }

    private func move(from: IndexSet, to: Int) {
        guard var items = list?.items, let i = from.first else { return }
        let item = items[i].id
        items.move(fromOffsets: from, toOffset: to)
        guard let at = items.firstIndex(where: { $0.id == item }) else { return }
        let after = at == 0 ? nil : items[at - 1].id
        list?.items = items
        act { try $0.moveListItem(id: id, item: item, after: after) }
    }

    private func copy() {
        guard let n = node else { return }
        Task {
            if let md = try? await offMain({ try n.listMarkdown(id: id) }) {
                UIPasteboard.general.setItems([[UTType.plainText.identifier: md]], options: [.localOnly: true])
            }
        }
    }

    private func act(_ f: @escaping @Sendable (ThrenodyNode) throws -> Void) {
        guard let n = node else { return }
        Task {
            do {
                try await offMain { try f(n) }
            } catch {
                failure = error.localizedDescription
            }
            load()
        }
    }
}

/// Picks a chat to share a list with.
struct ShareListView: View {
    let chats: [ChatRow]
    let pick: (String) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List(chats) { row in
                Button(row.title) { pick(row.ref.chatKey) }
            }
            .overlay {
                if chats.isEmpty { ContentUnavailableView("No chats to share with", systemImage: "person.2") }
            }
            .navigationTitle("Share with")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
        }
    }
}
