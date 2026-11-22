import PhotosUI
import SwiftUI

/// Chat themes (Appendix W): a chat's shared look drawn behind its
/// messages, and the sheet that chooses one. The same shapes and colours
/// as on every other device; in dark mode, light backgrounds deepen.
enum Themes {
    /// Accent colours on offer (the same on every app).
    static let accents: [UInt32] = [
        0xFF4F46E5, 0xFF1E6FB8, 0xFF00897B, 0xFF2E7D32, 0xFFE65100, 0xFFC62828, 0xFFD81B60, 0xFF6A1B9A,
    ]

    static func color(_ c: UInt32) -> Color {
        Color(
            .sRGB,
            red: Double((c >> 16) & 0xFF) / 255,
            green: Double((c >> 8) & 0xFF) / 255,
            blue: Double(c & 0xFF) / 255,
            opacity: Double((c >> 24) & 0xFF) / 255
        )
    }

    /// `c` for the colour scheme: deepened in dark mode.
    static func shade(_ c: UInt32, dark: Bool) -> Color {
        color(dark ? nightColor(color: c) : c)
    }

    /// A pattern's ink in dark mode: lighter, to show on deep paper.
    static func ink(_ c: UInt32, dark: Bool) -> Color {
        guard dark else { return color(c) }
        let mix = { (s: UInt32) -> UInt32 in
            let v = (c >> s) & 0xFF
            return (v + (255 - v) / 2) << s
        }
        return color((c & 0xFF00_0000) | mix(16) | mix(8) | mix(0))
    }

    /// The accent, unless we keep our own look.
    static func accent(_ theme: ThemeInfo?, own: Bool) -> Color? {
        guard !own, let a = theme?.accent else { return nil }
        return color(a)
    }
}

/// A pattern's tile, `tile` points across, repeated over the whole view.
struct PatternView: View {
    let pattern: PatternInfo
    let ink: Color
    let paper: Color
    var tile: CGFloat = 132

    var body: some View {
        let shapes = patternShapes(pattern: pattern)
        Canvas { ctx, size in
            ctx.fill(Path(CGRect(origin: .zero, size: size)), with: .color(paper))
            let cols = Int(size.width / tile) + 2
            let rows = Int(size.height / tile) + 2
            for col in -1..<cols {
                for row in -1..<rows {
                    let ox = CGFloat(col) * tile
                    let oy = CGFloat(row) * tile
                    for s in shapes {
                        switch s {
                        case let .circle(x, y, r):
                            let c = CGPoint(x: ox + CGFloat(x) * tile, y: oy + CGFloat(y) * tile)
                            let rr = CGFloat(r) * tile
                            ctx.fill(Path(ellipseIn: CGRect(x: c.x - rr, y: c.y - rr, width: rr * 2, height: rr * 2)), with: .color(ink))
                        case let .line(x1, y1, x2, y2, width):
                            var p = Path()
                            p.move(to: CGPoint(x: ox + CGFloat(x1) * tile, y: oy + CGFloat(y1) * tile))
                            p.addLine(to: CGPoint(x: ox + CGFloat(x2) * tile, y: oy + CGFloat(y2) * tile))
                            ctx.stroke(p, with: .color(ink), style: StrokeStyle(lineWidth: CGFloat(width) * tile, lineCap: .round))
                        case let .polygon(xs, ys):
                            var p = Path()
                            for (i, (x, y)) in zip(xs, ys).enumerated() {
                                let pt = CGPoint(x: ox + CGFloat(x) * tile, y: oy + CGFloat(y) * tile)
                                if i == 0 { p.move(to: pt) } else { p.addLine(to: pt) }
                            }
                            p.closeSubpath()
                            ctx.fill(p, with: .color(ink))
                        }
                    }
                }
            }
        }
    }
}

/// A chat's background, or nothing for the app's own.
struct ThemeBackdrop: View {
    let background: BackgroundInfo?
    var tile: CGFloat = 132
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let dark = scheme == .dark
        switch background {
        case nil, .plain?:
            Color.clear
        case let .color(c)?:
            Themes.shade(c, dark: dark)
        case let .gradient(top, bottom)?:
            LinearGradient(colors: [Themes.shade(top, dark: dark), Themes.shade(bottom, dark: dark)], startPoint: .top, endPoint: .bottom)
        case let .pattern(pattern, ink, paper)?:
            PatternView(pattern: pattern, ink: Themes.ink(ink, dark: dark), paper: Themes.shade(paper, dark: dark), tile: tile)
        case let .photo(data)?:
            if let image = UIImage(data: data) {
                // Dimmed, so the messages read.
                Image(uiImage: image).resizable().scaledToFill()
                    .overlay(dark ? Color.black.opacity(0.6) : Color.white.opacity(0.4))
                    .clipped()
            } else {
                Color.clear
            }
        }
    }
}

/// A group's picture: an emoji on its colour, or the photo, in a circle.
struct GroupPicture: View {
    let picture: PictureInfo
    var size: CGFloat = 40

    var body: some View {
        switch picture {
        case let .emoji(emoji, color):
            Text(emoji).font(.system(size: size * 0.5))
                .frame(width: size, height: size)
                .background(Themes.color(color), in: Circle())
        case let .photo(data):
            if let image = UIImage(data: data) {
                Image(uiImage: image).resizable().scaledToFill()
                    .frame(width: size, height: size).clipShape(Circle())
            } else {
                Circle().fill(.quaternary).frame(width: size, height: size)
            }
        }
    }
}

/// The sheet that chooses a chat's look, with a preview of it on top.
struct ThemeSheet: View {
    let current: ThemeInfo?
    let group: Bool
    let canSet: Bool
    let own: Bool
    let onOwn: (Bool) -> Void
    let onSet: (ThemeInfo?) -> Void

    @State private var chosen = ThemeInfo(background: .plain, accent: nil, picture: nil)
    @State private var keepOwn = false
    @State private var photo: PhotosPickerItem?
    @State private var picturePhoto: PhotosPickerItem?
    @State private var emoji = ""
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    preview
                    if canSet { choices } else {
                        Text("Only the group's owners and admins can change its theme.")
                            .foregroundStyle(.secondary)
                    }
                    Toggle(isOn: $keepOwn) {
                        VStack(alignment: .leading) {
                            Text("Keep My Own Look Here")
                            Text("The plain background on your devices, whatever the theme. No one else is told.")
                                .font(.footnote).foregroundStyle(.secondary)
                        }
                    }
                    .onChange(of: keepOwn) { _, on in onOwn(on) }
                }
                .padding()
            }
            .navigationTitle(group ? "Group Theme" : "Chat Theme")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                if canSet {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Set") { dismiss(); onSet(chosen) }
                    }
                    ToolbarItem(placement: .bottomBar) {
                        Button("Plain", role: .destructive) { dismiss(); onSet(nil) }
                    }
                }
            }
        }
        .onAppear {
            if let current { chosen = current }
            keepOwn = own
        }
        .onChange(of: photo) { _, item in load(item, picture: false) }
        .onChange(of: picturePhoto) { _, item in load(item, picture: true) }
    }

    private var preview: some View {
        VStack(alignment: .leading, spacing: 6) {
            if group, let p = chosen.picture { GroupPicture(picture: p) }
            Text("Merry Christmas! 🎄")
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 16))
            HStack {
                Spacer()
                Text("Same to you!").foregroundStyle(.white)
                    .padding(.horizontal, 12).padding(.vertical, 8)
                    .background(chosen.accent.map(Themes.color) ?? Color.accentColor, in: RoundedRectangle(cornerRadius: 16))
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: group ? 170 : 120, alignment: .top)
        .background(ThemeBackdrop(background: chosen.background, tile: 84))
        .clipShape(RoundedRectangle(cornerRadius: 20))
    }

    @ViewBuilder private var choices: some View {
        Text("Theme Packs").font(.headline)
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 10) {
                ForEach(themePacks(), id: \.name) { p in
                    Button {
                        var t = p.theme
                        if !group { t.picture = nil }
                        chosen = t
                    } label: {
                        VStack(spacing: 4) {
                            ThemeBackdrop(background: p.theme.background, tile: 44)
                                .frame(width: 72, height: 72)
                                .overlay(Text(p.emoji).font(.title))
                                .clipShape(RoundedRectangle(cornerRadius: 16))
                            Text(p.name).font(.caption).foregroundStyle(.primary)
                        }
                    }
                    .accessibilityLabel(p.name)
                }
            }
        }
        Text("Patterns").font(.headline)
        swatches(themePatterns().map { pat in
            let paper: UInt32 = 0xFFF3_F4F8
            return (BackgroundInfo.pattern(pattern: pat, ink: 0x404F_46E5, paper: paper), patternName(pattern: pat), {
                let acc = chosen.accent ?? Themes.accents[0]
                chosen.background = .pattern(pattern: pat, ink: (acc & 0x00FF_FFFF) | 0x4000_0000, paper: paper)
            })
        })
        Text("Colours").font(.headline)
        let g = themeGradients()
        swatches(themeColors().map { c in (BackgroundInfo.color(color: c), "Colour", { chosen.background = .color(color: c) }) }
            + stride(from: 0, to: g.count - 1, by: 2).map { i in
                (BackgroundInfo.gradient(top: g[i], bottom: g[i + 1]), "Gradient", { chosen.background = .gradient(top: g[i], bottom: g[i + 1]) })
            })
        PhotosPicker("Choose a Photo…", selection: $photo, matching: .images)
        Text("Accent").font(.headline)
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 10) {
                Button { chosen.accent = nil } label: {
                    Circle().strokeBorder(.secondary, lineWidth: 2).frame(width: 44, height: 44)
                }
                .accessibilityLabel("The app's own")
                ForEach(Themes.accents, id: \.self) { a in
                    Button { chosen.accent = a } label: {
                        Circle().fill(Themes.color(a)).frame(width: 44, height: 44)
                    }
                    .accessibilityLabel("Accent")
                }
            }
        }
        if group {
            Text("Picture").font(.headline)
            HStack {
                TextField("Emoji", text: $emoji)
                    .frame(width: 80)
                    .textFieldStyle(.roundedBorder)
                    .onChange(of: emoji) { _, e in
                        guard let first = e.first else { return }
                        let field: UInt32 = 0xFFE8_E8FF
                        chosen.picture = .emoji(emoji: String(first), color: field)
                    }
                PhotosPicker("Photo…", selection: $picturePhoto, matching: .images)
                Button("None") { chosen.picture = nil }
            }
        }
    }

    private func swatches(_ items: [(BackgroundInfo, String, () -> Void)]) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 10) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    Button(action: item.2) {
                        ThemeBackdrop(background: item.0, tile: 40)
                            .frame(width: 44, height: 44)
                            .background(Color(.secondarySystemBackground))
                            .clipShape(Circle())
                    }
                    .accessibilityLabel(item.1)
                }
            }
        }
    }

    /// A picked photo shrunk to fit: at most 1600 points across and the
    /// background limit, or 512 and the picture limit.
    private func load(_ item: PhotosPickerItem?, picture: Bool) {
        guard let item else { return }
        Task {
            guard let data = try? await item.loadTransferable(type: Data.self), let image = UIImage(data: data) else { return }
            let side: CGFloat = picture ? 512 : 1600
            let limit = Int(picture ? maxPicturePhoto() : maxBackgroundPhoto())
            let scale = min(1, side / max(image.size.width, image.size.height))
            let size = CGSize(width: image.size.width * scale, height: image.size.height * scale)
            let small = UIGraphicsImageRenderer(size: size).image { _ in image.draw(in: CGRect(origin: .zero, size: size)) }
            var q: CGFloat = 0.85
            while q > 0.2 {
                if let jpeg = small.jpegData(compressionQuality: q), jpeg.count <= limit {
                    await MainActor.run {
                        if picture { chosen.picture = .photo(data: jpeg) } else { chosen.background = .photo(data: jpeg) }
                    }
                    return
                }
                q -= 0.1
            }
        }
    }
}
