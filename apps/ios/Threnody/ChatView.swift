import PhotosUI
import QuickLook
import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// One row of a chat: a message, or photos and files sent together.
struct ChatItem: Identifiable {
    let entries: [HistoryEntry]
    var first: HistoryEntry { entries[0] }
    var id: String { ChatItem.key(first) }

    /// A stable id for an entry, to scroll to it.
    static func key(_ e: HistoryEntry) -> String { "\(e.atMs)/\(e.device)" }

    /// Groups consecutive entries of one album.
    static func group(_ entries: [HistoryEntry]) -> [ChatItem] {
        var items: [ChatItem] = []
        var run: [HistoryEntry] = []
        func flush() {
            if !run.isEmpty { items.append(ChatItem(entries: run)) }
            run = []
        }
        for e in entries {
            let album = e.file?.album ?? 0
            if album != 0, let last = run.last, last.file?.album == album, last.device == e.device {
                run.append(e)
            } else {
                flush()
                run = [e]
                if album == 0 { flush() }
            }
        }
        flush()
        return items
    }
}

/// Which messages to delete, and for whom.
private struct Deleting: Identifiable {
    let entries: [HistoryEntry]
    let everyone: Bool
    var id: String { (everyone ? "e" : "m") + ChatItem.key(entries[0]) }
}

/// Files picked to send, before the send sheet.
struct PickedFile: Identifiable, Sendable {
    let id = UUID()
    let name: String
    let data: Data
}

@MainActor
struct ChatView: View {
    let ref: ChatRef
    @EnvironmentObject private var store: NodeStore
    @State private var entries: [HistoryEntry] = []
    @State private var pending: [String] = []
    @State private var text = ""
    @State private var replyTo: HistoryEntry?
    @State private var editing: HistoryEntry?
    @State private var editText = ""
    @State private var deleting: Deleting?
    /// A tapped link, waiting for the user to say whether to open it.
    @State private var openingLink: TextLink?
    /// The link in the message box on offer for a preview, its card once
    /// fetched (only when asked: the site sees this device's address), and
    /// links not to offer again.
    @State private var linkOffer: String?
    @State private var linkCard: LinkPreviewInfo?
    @State private var linkFetching = false
    @State private var linkDropped: Set<String> = []
    @State private var reactingTo: Reacting?
    /// Sensitive photos the user uncovered (by "atMs/device").
    @State private var revealed: Set<String> = []
    @State private var sentRead: Set<UInt64> = []
    @State private var typingSent = false
    @State private var typingSentAt = Date.distantPast
    @State private var failure: String?
    @State private var notice: String?
    /// This chat's scheduled messages, soonest first.
    @State private var scheduled: [ScheduledInfo] = []
    @State private var showLater = false
    @State private var showScheduled = false
    // Search in this chat.
    @State private var searching = false
    @State private var query = ""
    @State private var hits: [HistoryEntry] = []
    @State private var hitIndex = 0
    /// A message to bring into view (a quoted one, a search hit).
    @State private var jump: HistoryEntry?
    @State private var scrollTarget: String?
    // Sending files.
    @State private var pickingPhotos = false
    @State private var photoItems: [PhotosPickerItem] = []
    @State private var pickingFiles = false
    @State private var picked: [PickedFile] = []
    @State private var sending = false
    @State private var previewURL: URL?
    @FocusState private var focused: Bool
    @Environment(\.scenePhase) private var scenePhase

    private var node: ThrenodyNode? { store.node(for: ref.persona) }
    private var convo: Conversation? {
        guard case let .contact(k) = ref.target else { return nil }
        return store.conversation(k, persona: ref.persona)
    }
    private var group: GroupInfo? { store.row(ref)?.group }
    private var contactKey: String? {
        if case let .contact(k) = ref.target { return k }
        return nil
    }
    private var device: String { convo?.device ?? contactKey ?? "" }
    private var devices: Set<String> {
        guard let k = contactKey else { return [] }
        return Set(convo?.devices ?? [k]).union([k])
    }
    private var target: ChatTarget? { node.map { ChatTarget(node: $0, ref: ref, device: device) } }
    private var title: String {
        switch ref.target {
        case .contact: convo?.title ?? NodeStore.short(contactKey ?? "")
        case .group: group?.name ?? "Group"
        case .notes: "Note to self"
        case .groupInvite: store.row(ref)?.title ?? "Invitation"
        }
    }

    var body: some View {
        Group {
            if node != nil {
                chat
            } else if ref.persona != nil && (!store.personasReady || store.personaLabels[ref.persona ?? ""] != nil) {
                ProgressView("Opening anonymous identity…")
            } else {
                ContentUnavailableView("This anonymous identity was burned.", systemImage: "flame")
            }
        }
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
    }

    /// The chat, in parts the type checker takes one at a time.
    private var chat: some View {
        withEvents(withDialogs(withSheets(layout)))
    }

    private var layout: some View {
        VStack(spacing: 0) {
            if let c = convo { banner(c) }
            if searching { searchBar }
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 6) {
                        ForEach(ChatItem.group(entries)) { item in
                            itemView(item).id(item.id)
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
                .onChange(of: entries.count + pending.count) { if jump == nil { proxy.scrollTo("bottom") } }
                .onChange(of: scrollTarget) { _, id in
                    guard let id else { return }
                    withAnimation { proxy.scrollTo(id, anchor: .center) }
                    scrollTarget = nil
                }
            }
            if !scheduled.isEmpty { scheduledBar }
            if let r = replyTo { replyStrip(r) }
            if let u = linkOffer { previewStrip(u) }
            compose
        }
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button {
                    searching.toggle()
                    if !searching { endSearch() }
                } label: {
                    Image(systemName: "magnifyingglass")
                }
                .accessibilityLabel("Search this chat")
                switch ref.target {
                case .contact:
                    NavigationLink { ContactInfoView(ref: ref) } label: { Image(systemName: "info.circle") }
                case .group:
                    NavigationLink { GroupInfoView(ref: ref) } label: { Image(systemName: "info.circle") }
                default:
                    EmptyView()
                }
            }
        }
    }

    private func withSheets(_ content: some View) -> some View {
        content
        .sheet(isPresented: $showLater) {
            LaterSheet(initial: nil, text: nil) { date, _ in if let date { sendLater(date) } }
        }
        .sheet(isPresented: $showScheduled) {
            ScheduledList(items: scheduled) { item, action in scheduledAction(item, action) }
        }
        .sheet(isPresented: Binding(get: { !picked.isEmpty }, set: { if !$0 { picked = [] } })) {
            SendSheet(files: picked, caption: text, strip: node?.stripMetadata() ?? true) { caption, sensitive in
                let files = picked
                picked = []
                sendFiles(files, caption: caption, sensitive: sensitive)
            }
        }
        .sheet(item: $reactingTo) { r in
            EmojiInputSheet { emoji in react(r.entry, emoji) }
        }
        .photosPicker(isPresented: $pickingPhotos, selection: $photoItems, maxSelectionCount: 10,
                      matching: .images, preferredItemEncoding: .current)
        .onChange(of: photoItems) { _, items in
            guard !items.isEmpty else { return }
            photoItems = []
            Task { await loadPhotos(items) }
        }
        .fileImporter(isPresented: $pickingFiles, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            if case let .success(urls) = result { loadFiles(urls) }
        }
        .quickLookPreview($previewURL)
    }

    private func withDialogs(_ content: some View) -> some View {
        content
        .alert("Couldn't send", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .alert("Threnody", isPresented: Binding(get: { notice != nil }, set: { if !$0 { notice = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(notice ?? "")
        }
        .alert("Edit message", isPresented: Binding(get: { editing != nil }, set: { if !$0 { editing = nil } }),
               presenting: editing) { e in
            TextField("Message", text: $editText)
            Button("Save") { edit(e, editText) }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("They see it marked as edited, if they're running a current version.")
        }
        // Links leave the app (and show your address to that site), so ask first.
        .confirmationDialog(
            "Open this link?",
            isPresented: Binding(get: { openingLink != nil }, set: { if !$0 { openingLink = nil } }),
            titleVisibility: .visible,
            presenting: openingLink
        ) { l in
            Button("Open") { if let url = URL(string: l.url) { UIApplication.shared.open(url) } }
            Button("Copy") {
                UIPasteboard.general.setItems([[UTType.plainText.identifier: l.url]], options: [.localOnly: true])
            }
            Button("Cancel", role: .cancel) {}
        } message: { l in
            Text("Goes to \(l.host)\n\n\(l.url)\n\nIt opens in your browser, outside Threnody. The site sees your network address.")
        }
        .confirmationDialog(
            deleting?.everyone == true ? "Delete for everyone?" : "Delete for you?",
            isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }),
            titleVisibility: .visible,
            presenting: deleting
        ) { d in
            Button("Delete", role: .destructive) { delete(d) }
        } message: { d in
            Text(d.everyone
                ? "Their devices delete it too, if they're running a current version."
                : "It's deleted from this device. Others keep their copies.")
        }
    }

    private func withEvents(_ content: some View) -> some View {
        content
        .task { await appear() }
        .onDisappear(perform: close)
        .onReceive(store.events) { se in
            if concerns(se) { Task { await load() } }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { Task { await load() } }
        }
        .onChange(of: text) { _, t in
            sendTyping(focused && !t.isEmpty)
            offerPreview(t)
        }
        .task(id: query) { await runSearch() }
        .onChange(of: jump) { _, e in
            guard let e else { return }
            Task {
                await load()
                scrollTarget = ChatItem.key(item(containing: e) ?? e)
            }
        }
    }

    /// Whether an event may change what this chat shows.
    private func concerns(_ se: StoreEvent) -> Bool {
        guard se.persona == ref.persona else { return false }
        if case .historySynced = se.event { return true }
        switch ref.target {
        case .contact: return se.event.peer.map { devices.contains($0) } ?? false
        case let .group(g): return se.event.group == g
        case .notes, .groupInvite: return false
        }
    }

    // MARK: Rows

    @ViewBuilder private func itemView(_ item: ChatItem) -> some View {
        let e = item.first
        let fromGroupMember = ref.isGroup && !e.outgoing
        Bubble(
            item: item,
            sender: fromGroupMember ? store.name(e.device, persona: ref.persona) : nil,
            fromThisDevice: e.device == node?.deviceFingerprint(),
            quoteAuthor: e.quote.map { store.name($0.author, persona: ref.persona) },
            names: e.mentions.map { store.name($0.device, persona: ref.persona) },
            query: searching ? query : "",
            revealed: revealed,
            persona: ref.persona,
            onReveal: { revealed.insert(ChatItem.key($0)) },
            onOpen: { previewURL = $0 },
            onLink: { openingLink = $0 },
            onQuote: { q in
                if let original = entries.first(where: { $0.id == q.id && $0.id != 0 }) { jump = original }
            },
            onReact: { reaction in
                guard let t = target, t.canReact(e) else { return }
                toggle(e, reaction.emoji, add: !reaction.mine)
            }
        )
        .contextMenu { menu(item) }
    }

    @ViewBuilder private func menu(_ item: ChatItem) -> some View {
        let e = item.first
        let canReact = target?.canReact(e) ?? false
        let mine = ref.isGroup == false && contactKey != nil && e.outgoing
            && e.device == node?.deviceFingerprint() && item.entries.allSatisfy { $0.id != 0 }
        if canReact {
            ControlGroup {
                ForEach(["👍", "❤️", "😂", "😮", "😢", "🙏"], id: \.self) { emoji in
                    Button(emoji) { toggle(e, emoji, add: !e.reactions.contains { $0.emoji == emoji && $0.mine }) }
                }
            }
            Button("More reactions…", systemImage: "face.smiling") { reactingTo = Reacting(entry: e) }
        }
        Button("Reply", systemImage: "arrowshape.turn.up.left") {
            replyTo = e
            focused = true
        }
        if !e.text.isEmpty {
            Button("Copy", systemImage: "doc.on.doc") {
                UIPasteboard.general.setItems([[UTType.plainText.identifier: e.text]], options: [.localOnly: true])
            }
        }
        if mine && e.file == nil && e.poll == nil {
            Button("Edit", systemImage: "pencil") {
                editText = e.text
                editing = e
            }
        }
        Button("Delete for me", systemImage: "trash", role: .destructive) {
            deleting = Deleting(entries: item.entries, everyone: false)
        }
        if mine {
            Button("Delete for everyone", systemImage: "trash.fill", role: .destructive) {
                deleting = Deleting(entries: item.entries, everyone: true)
            }
        }
    }

    /// The album row an entry is in (its first entry), if any.
    private func item(containing e: HistoryEntry) -> HistoryEntry? {
        ChatItem.group(entries).first { $0.entries.contains { $0.atMs == e.atMs && $0.device == e.device } }?.first
    }

    @ViewBuilder private func banner(_ c: Conversation) -> some View {
        if !c.accepted {
            Banner(text: "\(c.title) wants to message you. They can't tell whether you've read this. Accept to reply; block to stop them for good.", warning: true) {
                Button("Accept") { act { try $0.acceptContact(peer: c.device) } }
                Button("Block", role: .destructive) { act { try $0.blockContact(peer: c.device) } }
            }
        } else if !c.verified && c.anyVerified {
            Banner(text: "⚠ \(c.title) added a device you haven't verified. Until you compare its safety number, someone else could be reading as them.", warning: true) {
                NavigationLink("Compare") { ContactInfoView(ref: ref) }
            }
        } else if !c.approved {
            Banner(text: "Approve \(c.title) once you trust them, and compare safety numbers to make sure no one is in the middle. Mutually approved contacts relay for each other and hold each other's messages while offline.") {
                Button("Approve") { act { try $0.setApproval(peer: c.device, approved: true) } }
                NavigationLink("Compare") { ContactInfoView(ref: ref) }
            }
        } else if !c.verified {
            Banner(text: "⚠ Not verified. Compare safety numbers with \(c.title) to make sure no one is in the middle.", warning: true) {
                NavigationLink("Compare") { ContactInfoView(ref: ref) }
            }
        }
    }

    private var searchBar: some View {
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
            TextField("Search this chat", text: $query)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
            if !hits.isEmpty {
                Text("\(hitIndex + 1) of \(hits.count)").font(.caption).foregroundStyle(.secondary)
            } else if !query.trimmingCharacters(in: .whitespaces).isEmpty {
                Text("No results").font(.caption).foregroundStyle(.secondary)
            }
            Button { step(1) } label: { Image(systemName: "chevron.up") }
                .disabled(hitIndex + 1 >= hits.count)
                .accessibilityLabel("Older")
            Button { step(-1) } label: { Image(systemName: "chevron.down") }
                .disabled(hitIndex == 0 || hits.isEmpty)
                .accessibilityLabel("Newer")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
    }

    private func replyStrip(_ r: HistoryEntry) -> some View {
        HStack {
            Rectangle().fill(Color.accentColor).frame(width: 3)
            VStack(alignment: .leading, spacing: 2) {
                Text("Replying to \(store.name(r.device, persona: ref.persona))").font(.caption.bold())
                Text(entryPreview(entry: r)).font(.caption).lineLimit(1).foregroundStyle(.secondary)
            }
            Spacer()
            Button { replyTo = nil } label: { Image(systemName: "xmark.circle.fill") }
                .foregroundStyle(.secondary)
                .accessibilityLabel("Cancel reply")
        }
        .frame(height: 40)
        .padding(.horizontal, 12)
        .padding(.top, 6)
        .background(.bar)
    }

    /// Above the message box: "Add a preview of host", then the card.
    private func previewStrip(_ url: String) -> some View {
        let host = textLinks(text: url).first?.host ?? url
        return HStack {
            if let c = linkCard {
                Label(c.title.isEmpty ? (c.description.isEmpty ? host : c.description) : c.title, systemImage: "link")
                    .font(.callout).lineLimit(1)
            } else if linkFetching {
                ProgressView().controlSize(.small)
                Text("Getting a preview of \(host)…").font(.callout).foregroundStyle(.secondary).lineLimit(1)
            } else {
                Button("Add a preview of \(host)") { fetchPreview(url) }
                    .font(.callout).lineLimit(1)
                    .accessibilityHint("The site sees your network address.")
            }
            Spacer()
            Button {
                linkDropped.insert(url)
                linkOffer = nil
                linkCard = nil
                linkFetching = false
            } label: { Image(systemName: "xmark.circle.fill") }
                .foregroundStyle(.secondary)
                .accessibilityLabel(linkCard == nil ? "Don't add a preview" : "Remove the preview")
        }
        .frame(minHeight: 36)
        .padding(.horizontal, 12)
        .padding(.top, 4)
        .background(.bar)
    }

    /// Follows the message box: offers the first link not dropped.
    private func offerPreview(_ t: String) {
        let first = textLinks(text: t).map(\.url).first { !linkDropped.contains($0) }
        guard first != linkOffer else { return }
        linkOffer = first
        linkCard = nil
        linkFetching = false
    }

    private func fetchPreview(_ url: String) {
        guard let n = node else { return }
        linkFetching = true
        Task {
            let got = try? await offMain { () -> LinkPreviewInfo? in
                guard var p = n.fetchLinkPreview(url: url) else { return nil }
                p.image = LinkThumbnail.shrink(p.image)
                return p
            }
            guard linkOffer == url else { return }
            linkFetching = false
            if let p = got ?? nil {
                linkCard = p
            } else {
                notice = "No preview for \(textLinks(text: url).first?.host ?? url)"
                linkDropped.insert(url)
                linkOffer = nil
            }
        }
    }

    private var compose: some View {
        HStack(alignment: .bottom, spacing: 8) {
            Menu {
                Button("Photos", systemImage: "photo.on.rectangle") { pickingPhotos = true }
                Button("File", systemImage: "doc") { pickingFiles = true }
            } label: {
                Image(systemName: sending ? "hourglass" : "paperclip").font(.system(size: 22))
            }
            .disabled(sending)
            .accessibilityLabel("Attach")
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
                if !(ref.target == .notes) {
                    Button("Send Later…", systemImage: "clock") { showLater = true }
                        .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
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

    // MARK: Loading

    private func appear() async {
        store.visibleChat = ref
        store.visibleDevices = devices
        // Not connected: look for them across the internet now.
        if let n = node, contactKey != nil {
            let d = device
            Task.detached { try? n.seek(peer: d) }
        }
        if let t = target, text.isEmpty {
            text = (try? await offMain { t.draft() }) ?? ""
        }
        if let p = store.pendingJump, p.ref == ref {
            store.pendingJump = nil
            query = p.query
            searching = !p.query.isEmpty
            jump = p.entry
        }
        await load()
    }

    private func close() {
        if store.visibleChat == ref {
            store.visibleChat = nil
            store.visibleDevices = []
        }
        sendTyping(false)
        guard let t = target else { return }
        let draft = text
        Task.detached { t.setDraft(draft) }
    }

    private func load() async {
        guard let t = target else { return }
        let want = jump
        guard let h = try? await offMain({ () -> [HistoryEntry] in
            var limit: UInt32 = 300
            var h = try t.history(limit: limit)
            // An older message to show: load more until it's there.
            if let want {
                while !h.contains(where: { $0.atMs == want.atMs && $0.device == want.device })
                    && h.count >= Int(limit) && limit < 10_000 {
                    limit *= 2
                    h = try t.history(limit: limit)
                }
            }
            return h
        }) else { return }
        entries = h
        scheduled = (try? await offMain { try t.scheduled() }) ?? []
        guard scenePhase == .active else { return }
        _ = try? await offMain { t.markSeen() }
        // Incoming messages now on screen are read: reported to contacts
        // we accepted, unless read receipts are off.
        guard t.isContact, Prefs.flag(Prefs.sendRead), convo?.accepted != false, let n = node else { return }
        let ids = h.filter { !$0.outgoing && $0.id != 0 && !sentRead.contains($0.id) }.map(\.id)
        guard !ids.isEmpty else { return }
        let d = device
        // Without a session now, they're reported on a later load.
        if (try? await offMain { try n.reportRead(peer: d, ids: ids) }) != nil {
            sentRead.formUnion(ids)
        }
    }

    // MARK: Search

    private func runSearch() async {
        let q = query.trimmingCharacters(in: .whitespaces)
        guard searching, !q.isEmpty, let t = target else {
            hits = []
            hitIndex = 0
            return
        }
        try? await Task.sleep(for: .milliseconds(300))
        guard !Task.isCancelled else { return }
        let found = (try? await offMain { try t.search(q, limit: 500) }) ?? []
        guard !Task.isCancelled else { return }
        hits = found
        hitIndex = 0
        if let first = found.first { jump = first }
    }

    /// One hit older (+1) or newer (-1).
    private func step(_ by: Int) {
        let i = hitIndex + by
        guard hits.indices.contains(i) else { return }
        hitIndex = i
        jump = hits[i]
    }

    private func endSearch() {
        query = ""
        hits = []
        hitIndex = 0
        jump = nil
    }

    // MARK: Sending

    private func send() {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty, let t = target else { return }
        let quoted = replyTo
        // The card goes if its link is still in the text.
        let card = linkCard.flatMap { c in textLinks(text: body).contains { $0.url == c.url } ? c : nil }
        text = ""
        replyTo = nil
        linkOffer = nil
        linkCard = nil
        linkDropped = []
        pending.append(body)
        sendTyping(false)
        Task {
            do {
                if let warning = try await offMain({ try t.send(body, replyTo: quoted, preview: card) }) { failure = warning }
            } catch {
                // Back in the box, unless something else was typed meanwhile.
                if text.isEmpty { text = body }
                failure = error.localizedDescription
            }
            if let i = pending.firstIndex(of: body) { pending.remove(at: i) }
            jump = nil
            await load()
            store.refresh()
        }
    }

    /// Schedules what's in the message box for `date`, as `send` sends it now.
    private func sendLater(_ date: Date) {
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty, let t = target else { return }
        let due = UInt64(max(0, date.timeIntervalSince1970 * 1000))
        // Clearing it also saves an empty draft on leaving, as after a send.
        text = ""
        sendTyping(false)
        Task {
            do {
                try await offMain { try t.schedule(body, dueMs: due) }
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

    /// Picked photos: their bytes, under neutral names (a time in a name
    /// is metadata too).
    private func loadPhotos(_ items: [PhotosPickerItem]) async {
        var files: [PickedFile] = []
        for (i, item) in items.enumerated() {
            guard let data = try? await item.loadTransferable(type: Data.self) else { continue }
            let ext = item.supportedContentTypes.first?.preferredFilenameExtension ?? "jpg"
            files.append(PickedFile(name: items.count > 1 ? "photo-\(i + 1).\(ext)" : "photo.\(ext)", data: data))
        }
        if files.isEmpty { failure = "Couldn't read the photos." } else { picked = files }
    }

    private func loadFiles(_ urls: [URL]) {
        let limit = node?.maxFileSize() ?? UInt64.max
        var files: [PickedFile] = []
        var refused: [String] = []
        for url in urls {
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            let size = (try? url.resourceValues(forKeys: [.fileSizeKey]))?.fileSize ?? 0
            if UInt64(max(0, size)) > limit {
                refused.append("\(url.lastPathComponent) is too large to send")
            } else if let data = try? Data(contentsOf: url) {
                files.append(PickedFile(name: url.lastPathComponent, data: data))
            } else {
                refused.append("Couldn't read \(url.lastPathComponent)")
            }
        }
        if !refused.isEmpty { failure = refused.joined(separator: "\n") }
        if !files.isEmpty { picked = files }
    }

    /// Sends picked files: prepared (photo metadata removed), kept for
    /// ourselves, and sent; several go as an album, captioned once.
    private func sendFiles(_ files: [PickedFile], caption: String, sensitive: Bool) {
        guard let t = target, let n = node else { return }
        if !caption.isEmpty && caption == text.trimmingCharacters(in: .whitespacesAndNewlines) { text = "" }
        let persona = ref.persona
        sending = true
        Task {
            let errors = (try? await offMain { () -> [String] in
                let strip = n.stripMetadata()
                let limit = n.maxFileSize()
                let album = files.count > 1 ? albumId() : 0
                var errors: [String] = []
                for (i, f) in files.enumerated() {
                    do {
                        let (name, data) = try Media.prepare(name: f.name, data: f.data, strip: strip, max: limit)
                        let location = Media.save(name: name, data: data, persona: persona)
                        do {
                            let options = FileOptions(sensitive: sensitive, caption: i == 0 ? caption : "", album: album)
                            try t.sendFile(name: name, data: data, location: location, options: options)
                        } catch {
                            Media.forget(location)
                            throw error
                        }
                    } catch {
                        errors.append(error.localizedDescription)
                    }
                }
                return errors
            }) ?? []
            sending = false
            if !errors.isEmpty { failure = errors.joined(separator: "\n") }
            jump = nil
            await load()
            store.refresh()
        }
    }

    // MARK: Message actions

    private func toggle(_ e: HistoryEntry, _ emoji: String, add: Bool) {
        guard let t = target else { return }
        Task {
            do {
                _ = try await offMain { try t.react(e, emoji, add: add) }
            } catch {
                failure = error.localizedDescription
            }
            await load()
        }
    }

    private func react(_ e: HistoryEntry, _ emoji: String) {
        let add = !e.reactions.contains { $0.emoji == emoji && $0.mine }
        toggle(e, emoji, add: add)
    }

    private func edit(_ e: HistoryEntry, _ body: String) {
        let new = body.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let t = target, !new.isEmpty, new != e.text else { return }
        Task {
            do {
                let edited = try await offMain { try t.edit(e, new) }
                if !edited { failure = "It can't be edited any more." }
            } catch {
                failure = error.localizedDescription
            }
            await load()
        }
    }

    private func delete(_ d: Deleting) {
        guard let t = target else { return }
        Task {
            do {
                try await offMain { try t.delete(d.entries, everyone: d.everyone) }
                if d.everyone { notice = "Deleted. Their devices delete it too, if they're running a current version." }
            } catch {
                failure = error.localizedDescription
            }
            await load()
            store.refresh()
        }
    }

    /// Tells the contact we started or stopped typing: on a change, and
    /// again now and then while typing goes on. Starts only with typing
    /// indicators on; a stop always goes.
    private func sendTyping(_ on: Bool) {
        guard let n = node, contactKey != nil, convo?.accepted != false else { return }
        if on && !Prefs.flag(Prefs.sendTyping) { return }
        if typingSent == on && !(on && Date().timeIntervalSince(typingSentAt) > 4) { return }
        typingSent = on
        typingSentAt = Date()
        let d = device
        Task.detached { try? n.setTyping(peer: d, active: on) }
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
            await load()
        }
    }
}

/// "More reactions…": any one emoji from the keyboard.
struct EmojiInputSheet: View {
    let pick: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        NavigationStack {
            Form {
                TextField("Type one emoji", text: $text)
                    .focused($focused)
                    .font(.largeTitle)
                    .multilineTextAlignment(.center)
            }
            .navigationTitle("React")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("React") {
                        let e = text.trimmingCharacters(in: .whitespaces)
                        dismiss()
                        pick(e)
                    }
                    .disabled(!Emoji.isSingle(text.trimmingCharacters(in: .whitespaces)))
                }
            }
            .onAppear { focused = true }
        }
        .presentationDetents([.medium])
    }
}

enum Emoji {
    /// Whether `s` is exactly one emoji (a flag, a skin tone and a ZWJ
    /// sequence included), not a letter or digit.
    static func isSingle(_ s: String) -> Bool {
        guard s.count == 1, let c = s.first else { return false }
        let scalars = Array(c.unicodeScalars)
        if scalars.contains(where: { $0.properties.isEmojiPresentation }) { return true }
        return scalars.count > 1 && scalars[0].properties.isEmoji
    }
}

/// A message to react to with any emoji.
private struct Reacting: Identifiable {
    let entry: HistoryEntry
    var id: String { ChatItem.key(entry) }
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
