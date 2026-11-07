import SwiftUI

/// Search across every chat of every identity: conversations whose names
/// have every word, then messages, newest first. Tapping a message opens
/// its chat at it, with the search carried over.
@MainActor
struct SearchResultsView: View {
    let query: String
    let open: (ChatRef) -> Void
    @EnvironmentObject private var store: NodeStore
    @State private var found: [Found] = []
    @State private var searching = false

    struct Found: Identifiable {
        let ref: ChatRef
        let entry: HistoryEntry
        var id: String { ref.encoded + "|" + ChatItem.key(entry) }
    }

    private var q: String { query.trimmingCharacters(in: .whitespaces) }

    private var named: [ChatRow] {
        (store.chats + store.archived).filter { $0.invite == nil && searchMatches(text: $0.title, query: q) }
    }

    var body: some View {
        Group {
            if !named.isEmpty {
                Section("Conversations") {
                    ForEach(named) { row in
                        Button { open(row.ref) } label: { ChatRowView(row: row, typing: false) }
                            .buttonStyle(.plain)
                    }
                }
            }
            Section("Messages") {
                if found.isEmpty {
                    Text(searching ? "Searching…" : "No messages found").foregroundStyle(.secondary)
                }
                ForEach(found) { f in
                    Button {
                        store.pendingJump = (f.ref, f.entry, q)
                        open(f.ref)
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            HStack {
                                Text(store.row(f.ref)?.title ?? "Chat").font(.subheadline.bold()).lineLimit(1)
                                Spacer()
                                Text(Date(ms: f.entry.atMs), format: .dateTime.day().month().year())
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            Text(snippet(f)).font(.subheadline).lineLimit(2)
                        }
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .task(id: q) { await search() }
    }

    /// "who: text", the matches marked; a file found by its name shows it.
    private func snippet(_ f: Found) -> AttributedString {
        let who = f.entry.outgoing ? "You" : store.name(f.entry.device, persona: f.ref.persona)
        let text = f.entry.text.split(whereSeparator: \.isNewline).joined(separator: " ")
        let body = searchRanges(text: text, query: q).isEmpty
            ? "📎 \(f.entry.file?.name ?? text)" : text
        var a = AttributedString("\(who): ")
        a.font = .subheadline.bold()
        var b = AttributedString(body)
        for r in searchRanges(text: body, query: q) {
            guard let range = Range(NSRange(location: Int(r.start), length: Int(r.len)), in: b) else { continue }
            b[range].backgroundColor = Color.yellow.opacity(0.6)
        }
        return a + b
    }

    private func search() async {
        let query = q
        guard !query.isEmpty, let main = store.node else {
            found = []
            return
        }
        try? await Task.sleep(for: .milliseconds(300))
        guard !Task.isCancelled else { return }
        searching = true
        let nodes: [(String?, ThrenodyNode)] = [(nil, main)] + store.personas.map { (Optional($0.key), $0.value) }
        let hits = (try? await offMain { () -> [(String?, SearchHit, Bool)] in
            var all: [(String?, SearchHit, Bool)] = []
            for (persona, n) in nodes {
                let own = Set(n.devices().map(\.fingerprint)).union([n.accountFingerprint(), n.deviceFingerprint()])
                for h in (try? n.searchAll(query: query, limit: 50)) ?? [] {
                    all.append((persona, h, h.group == nil && h.peer.map { own.contains($0) } == true))
                }
            }
            return all
        }) ?? []
        guard !Task.isCancelled else { return }
        var out: [Found] = []
        for (persona, h, notes) in hits {
            let ref: ChatRef
            if let g = h.group {
                ref = ChatRef(persona: persona, target: .group(g))
            } else if notes {
                // Notes to self are named by one of our devices or the account.
                guard persona == nil else { continue }
                ref = ChatRef(persona: nil, target: .notes)
            } else if let peer = h.peer, let c = store.conversation(peer, persona: persona), !c.blocked {
                ref = ChatRef(persona: persona, target: .contact(c.key))
            } else {
                continue
            }
            out.append(Found(ref: ref, entry: h.entry))
        }
        found = Array(out.sorted { $0.entry.atMs > $1.entry.atMs }.prefix(50))
        searching = false
    }
}
