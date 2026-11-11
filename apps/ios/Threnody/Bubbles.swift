import SwiftUI
import UIKit

/// A message in a chat: who sent it (in groups), what it replies to, its
/// text (mentions by name, search matches marked), photos and files,
/// polls and lists, reactions, and when it was sent with its ticks.
/// Links in the text take taps, which `onLink` confirms before opening.
struct Bubble: View {
    let item: ChatItem
    /// The sender's name, above incoming group messages.
    let sender: String?
    let fromThisDevice: Bool
    let quoteAuthor: String?
    /// What we call each mentioned device, in order.
    let names: [String]
    /// The in-chat search, whose matches are marked.
    let query: String
    let revealed: Set<String>
    let persona: String?
    let onReveal: (HistoryEntry) -> Void
    let onOpen: (URL) -> Void
    let onLink: (TextLink) -> Void
    let onQuote: (QuoteInfo) -> Void
    let onReact: (ReactionInfo) -> Void

    private var entry: HistoryEntry { item.first }

    var body: some View {
        let out = entry.outgoing
        HStack {
            if out { Spacer(minLength: 48) }
            VStack(alignment: out ? .trailing : .leading, spacing: 4) {
                VStack(alignment: .leading, spacing: 4) {
                    if let sender {
                        Text(sender).font(.caption.bold())
                            .foregroundStyle(out ? Color.white.opacity(0.9) : Color.accentColor)
                    }
                    if entry.forwarded {
                        Text("Forwarded").font(.caption2.italic()).opacity(0.8)
                    }
                    if let q = entry.quote { quote(q, out: out) }
                    content(out: out)
                    Text(meta)
                        .font(.caption2)
                        .foregroundStyle(out ? Color.white.opacity(0.8) : Color.secondary)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                        .accessibilityLabel(meta.replacingOccurrences(of: "✓✓", with: "delivered")
                            .replacingOccurrences(of: "✓", with: "sent").replacingOccurrences(of: "👁️", with: "read"))
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .foregroundStyle(out ? Color.white : Color.primary)
                .background(out ? Color.accentColor : Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 16))
                if !entry.reactions.isEmpty { reactions }
            }
            .fixedSize(horizontal: false, vertical: true)
            if !out { Spacer(minLength: 48) }
        }
    }

    @ViewBuilder private func content(out: Bool) -> some View {
        if item.entries.count > 1 {
            LazyVGrid(columns: [GridItem(.flexible(), spacing: 4), GridItem(.flexible(), spacing: 4)], spacing: 4) {
                ForEach(item.entries, id: \.self) { e in
                    FileContent(entry: e, caption: "", compact: true, revealed: revealed.contains(ChatItem.key(e)),
                                onReveal: { onReveal(e) }, onOpen: onOpen)
                }
            }
            .frame(maxWidth: 260)
            if !entry.text.isEmpty { Text(entry.text) }
        } else if entry.file != nil {
            FileContent(entry: entry, caption: entry.text, compact: false, revealed: revealed.contains(ChatItem.key(entry)),
                        onReveal: { onReveal(entry) }, onOpen: onOpen)
        } else if let poll = entry.poll {
            pollView(poll)
        } else if let list = entry.list {
            NavigationLink {
                ListEditorView(id: list, persona: persona)
            } label: {
                Label(entry.text.isEmpty ? "List" : entry.text, systemImage: "checklist").bold()
            }
            .buttonStyle(.plain)
        } else {
            Text(styled(out: out))
                .tint(out ? Color.white : Color.accentColor)
                .environment(\.openURL, OpenURLAction { url in
                    // Never straight to the browser: the chat asks first.
                    if let l = textLinks(text: url.absoluteString).first { onLink(l) }
                    return .handled
                })
                .frame(maxWidth: .infinity, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The text with mentions by name (ours marked), links underlined and
    /// tappable, and search matches highlighted. Mentions and links are
    /// placed in Rust (`showMentions`, `textLinks`); the ranges are UTF-16,
    /// as NSRange is.
    private func styled(out: Bool) -> AttributedString {
        let shown = showMentions(text: entry.text, mentions: entry.mentions, names: names)
        var a = AttributedString(shown.text)
        for m in shown.mentions {
            guard let r = Range(NSRange(location: Int(m.start), length: Int(m.len)), in: a) else { continue }
            a[r].font = .body.bold()
            if m.mine { a[r].backgroundColor = (out ? Color.white : Color.accentColor).opacity(0.25) }
        }
        for l in textLinks(text: shown.text) {
            guard let r = Range(NSRange(location: Int(l.start), length: Int(l.len)), in: a),
                  let url = URL(string: l.url) else { continue }
            a[r].link = url
            a[r].underlineStyle = .single
        }
        if !query.isEmpty {
            for h in searchRanges(text: shown.text, query: query) {
                guard let r = Range(NSRange(location: Int(h.start), length: Int(h.len)), in: a) else { continue }
                a[r].backgroundColor = Color.yellow.opacity(0.7)
                a[r].foregroundColor = Color.black
            }
        }
        return a
    }

    private func quote(_ q: QuoteInfo, out: Bool) -> some View {
        Button { onQuote(q) } label: {
            HStack(spacing: 6) {
                Rectangle().fill(out ? Color.white.opacity(0.7) : Color.accentColor).frame(width: 3)
                VStack(alignment: .leading, spacing: 1) {
                    Text(quoteAuthor ?? "").font(.caption.bold())
                    Text(q.text.isEmpty ? "Deleted message" : q.text).font(.caption).lineLimit(2)
                }
                Spacer(minLength: 0)
            }
            .fixedSize(horizontal: false, vertical: true)
            .padding(6)
            .background((out ? Color.white : Color.primary).opacity(0.1), in: RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .accessibilityHint("Shows the message it replies to")
    }

    /// Polls are shown, not voted in, on iPhone.
    private func pollView(_ poll: PollInfo) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("📊 \(entry.text)").bold()
            ForEach(poll.options.indices, id: \.self) { i in
                HStack {
                    Text(poll.options[i].mine ? "✓ \(poll.options[i].text)" : poll.options[i].text)
                    Spacer()
                    Text("\(poll.options[i].votes)")
                }
                .font(.subheadline)
            }
            Text(poll.closed ? "Poll ended · \(poll.voters) voted" : "\(poll.voters) voted · vote from another device")
                .font(.caption2).opacity(0.8)
        }
    }

    private var reactions: some View {
        HStack(spacing: 4) {
            ForEach(entry.reactions, id: \.emoji) { r in
                Button { onReact(r) } label: {
                    Text(r.count > 1 ? "\(r.emoji) \(r.count)" : r.emoji)
                        .font(.caption)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(r.mine ? Color.accentColor.opacity(0.25) : Color(.tertiarySystemFill), in: Capsule())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("\(r.emoji), \(r.count)\(r.mine ? ", yours" : "")")
            }
        }
    }

    private var meta: String {
        var s = Date(ms: entry.atMs).formatted(date: .omitted, time: .shortened)
        if entry.edited { s += " · edited" }
        if entry.disappearing { s += " · ⏱" }
        guard entry.outgoing else { return s }
        // Sent from another of our devices: its ticks as that device
        // passed them on (the node decides; never a lone "sent").
        if !fromThisDevice { s += " · from your other device" }
        switch entry.tick {
        case .read: return s + " 👁️"
        case .delivered: return s + " ✓✓"
        case let .partial(delivered, recipients): return s + " ✓ \(delivered)/\(recipients)"
        case .sent: return s + " ✓"
        case .none: return s
        }
    }
}

/// A photo or file in a bubble. Photos are decoded small and off the main
/// thread, and refused when too large (they'd crash the app); sensitive
/// ones are covered until tapped. Tapping opens QuickLook, which plays
/// voice and video messages and offers Share and Save.
struct FileContent: View {
    let entry: HistoryEntry
    let caption: String
    /// In an album grid.
    let compact: Bool
    let revealed: Bool
    let onReveal: () -> Void
    let onOpen: (URL) -> Void
    @State private var image: UIImage?
    @State private var unshowable = false
    /// A file that turned out to be an image too large for QuickLook.
    @State private var refused = false

    var body: some View {
        if let file = entry.file {
            let url = Media.resolve(file.location)
            let photo = file.clip == nil && isImageName(name: file.name)
            let label = fileLabel(name: file.name, sensitive: file.sensitive, caption: compact ? "" : caption, clip: file.clip)
            VStack(alignment: .leading, spacing: 6) {
                if url == nil {
                    Label("\(label) · on another device", systemImage: "paperclip")
                } else if photo && file.sensitive && !revealed {
                    Button(action: onReveal) {
                        RoundedRectangle(cornerRadius: 10)
                            .fill(Color.black.opacity(0.85))
                            .frame(width: compact ? 120 : 220, height: compact ? 120 : 160)
                            .overlay(
                                Text("Sensitive photo. Tap to view")
                                    .font(.footnote).foregroundStyle(.white)
                                    .multilineTextAlignment(.center).padding(8)
                            )
                    }
                    .buttonStyle(.plain)
                } else if photo, let image, let url {
                    Button { onOpen(url) } label: {
                        Image(uiImage: image)
                            .resizable()
                            .scaledToFit()
                            .frame(maxWidth: compact ? 128 : 240, maxHeight: compact ? 128 : 320)
                            .clipShape(RoundedRectangle(cornerRadius: 10))
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(label)
                    if !compact && !caption.isEmpty && !file.sensitive { Text(caption) }
                } else if photo && unshowable, let url {
                    // Never handed to QuickLook, which would decode it whole.
                    Label("\(label) · Too large to preview", systemImage: "photo")
                    ShareLink(item: url) { Label("Share", systemImage: "square.and.arrow.up") }
                } else if photo {
                    RoundedRectangle(cornerRadius: 10)
                        .fill(Color.secondary.opacity(0.2))
                        .frame(width: compact ? 120 : 220, height: compact ? 120 : 160)
                        .overlay(ProgressView())
                } else if refused, let url {
                    Label("\(label) · Too large to preview", systemImage: "paperclip")
                    ShareLink(item: url) { Label("Share", systemImage: "square.and.arrow.up") }
                } else if let url {
                    Button { open(url, clip: file.clip != nil) } label: {
                        Label(label, systemImage: file.clip == nil ? "paperclip" : (file.clip?.video == true ? "video" : "waveform"))
                    }
                    .buttonStyle(.plain)
                }
            }
            .task(id: "\(file.location ?? "")|\(revealed)") {
                guard photo, let url, !file.sensitive || revealed else { return }
                let side = compact ? 360 : 720
                let cg = try? await offMain { Media.thumbnail(url, maxSide: side) }
                if let cg = cg ?? nil {
                    image = UIImage(cgImage: cg)
                } else {
                    unshowable = true
                }
            }
        }
    }

    /// Opens a file in QuickLook, unless its bytes are an image too large
    /// to decode, whatever its name says.
    private func open(_ url: URL, clip: Bool) {
        if clip {
            onOpen(url)
            return
        }
        Task {
            let safe = (try? await offMain { Media.previewable(url) }) ?? false
            if safe { onOpen(url) } else { refused = true }
        }
    }
}
