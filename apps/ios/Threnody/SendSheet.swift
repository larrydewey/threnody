import SwiftUI
import UIKit

/// Photos and files about to be sent: a caption, whether they're
/// sensitive (covered until the recipient taps them), and what happens to
/// photo metadata.
struct SendSheet: View {
    let files: [PickedFile]
    let strip: Bool
    let onSend: (_ caption: String, _ sensitive: Bool) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var caption: String
    @State private var sensitive = false
    @State private var thumbs: [UUID: UIImage] = [:]

    init(files: [PickedFile], caption: String, strip: Bool, onSend: @escaping (_ caption: String, _ sensitive: Bool) -> Void) {
        self.files = files
        self.strip = strip
        self.onSend = onSend
        _caption = State(initialValue: caption.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private var photos: Bool { files.allSatisfy { isImageName(name: $0.name) } }

    private var title: String {
        let what = photos ? (files.count == 1 ? "photo" : "photos") : (files.count == 1 ? "file" : "files")
        return "Send \(files.count) \(what)"
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    ScrollView(.horizontal) {
                        HStack(spacing: 8) {
                            ForEach(files) { f in
                                if let image = thumbs[f.id] {
                                    Image(uiImage: image)
                                        .resizable()
                                        .scaledToFill()
                                        .frame(width: 72, height: 72)
                                        .clipShape(RoundedRectangle(cornerRadius: 8))
                                } else {
                                    Label(f.name, systemImage: "doc")
                                        .font(.caption)
                                        .lineLimit(2)
                                        .frame(width: 72, height: 72)
                                        .background(Color.secondary.opacity(0.15), in: RoundedRectangle(cornerRadius: 8))
                                }
                            }
                        }
                    }
                }
                Section {
                    TextField("Caption", text: $caption, axis: .vertical).lineLimit(1...4)
                    Toggle("Sensitive", isOn: $sensitive)
                } footer: {
                    if strip && files.contains(where: { isImageName(name: $0.name) }) {
                        Text("Location and camera details are removed before sending.")
                    } else if sensitive {
                        Text("They see it covered until they tap it.")
                    }
                }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Send") {
                        let (c, s) = (caption.trimmingCharacters(in: .whitespacesAndNewlines), sensitive)
                        dismiss()
                        onSend(c, s)
                    }
                }
            }
            .task { await loadThumbs() }
        }
    }

    /// Small previews of the picked photos, decoded off the main thread
    /// and refused when too large, as received ones are.
    private func loadThumbs() async {
        for f in files where isImageName(name: f.name) {
            let data = f.data
            let cg = try? await offMain { Media.thumbnail(data: data, maxSide: 144) }
            if let cg = cg ?? nil { thumbs[f.id] = UIImage(cgImage: cg) }
        }
    }
}
