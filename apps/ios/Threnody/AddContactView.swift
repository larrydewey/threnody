import AVFoundation
import CoreImage.CIFilterBuiltins
import SwiftUI
import UIKit

@MainActor
struct AddContactView: View {
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var link = ""
    @State private var name = ""
    @State private var invite: String?
    @State private var scanning = false
    @State private var busy = false
    @State private var failure: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("threnody://…", text: $link, axis: .vertical)
                        .font(.footnote.monospaced())
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Name (only you see it)", text: $name)
                    Button("Scan QR code", systemImage: "qrcode.viewfinder") { scanning = true }
                    PasteButton(payloadType: String.self) { strings in
                        if let s = strings.first { link = s.trimmingCharacters(in: .whitespacesAndNewlines) }
                    }
                    Button {
                        connect()
                    } label: {
                        if busy { ProgressView() } else { Text("Connect").bold() }
                    }
                    .disabled(!link.hasPrefix("threnody://") || busy)
                } header: {
                    Text("Their invite")
                } footer: {
                    Text("Scan their QR code or paste the link they sent you.")
                }
                Section {
                    if let invite {
                        QRCodeView(text: invite)
                            .frame(width: 240, height: 240)
                            .frame(maxWidth: .infinity)
                        Text(invite).font(.caption.monospaced()).textSelection(.enabled)
                        ShareLink("Share invite", item: invite)
                    } else {
                        Text("No network: connect to Wi-Fi to make an invite.")
                    }
                } header: {
                    Text("Your invite")
                } footer: {
                    Text("Let the other person scan this, or send them the link. Anyone with it can contact this device.")
                }
            }
            .navigationTitle("Add contact")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Close") { dismiss() } }
            }
            .sheet(isPresented: $scanning) {
                QRScannerView { code in
                    scanning = false
                    link = code
                }
                .ignoresSafeArea()
            }
            .alert("Couldn't connect", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(failure ?? "")
            }
            .onAppear { invite = store.invite() }
        }
    }

    private func connect() {
        guard let n = store.node else { return }
        let (target, petname) = (link.trimmingCharacters(in: .whitespacesAndNewlines), name.trimmingCharacters(in: .whitespaces))
        busy = true
        Task {
            do {
                let peer = try await offMain {
                    let peer = try n.connect(target: target)
                    if !petname.isEmpty { try? n.setName(peer: peer, name: petname) }
                    return peer
                }
                store.refresh()
                store.openChat = peer
                dismiss()
            } catch {
                failure = error.localizedDescription
            }
            busy = false
        }
    }
}

/// A QR code drawn with Core Image.
struct QRCodeView: View {
    let text: String

    var body: some View {
        if let image = Self.render(text) {
            Image(uiImage: image)
                .interpolation(.none)
                .resizable()
                .scaledToFit()
                .accessibilityLabel("Invite QR code")
        }
    }

    static func render(_ text: String) -> UIImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage?.transformed(by: CGAffineTransform(scaleX: 8, y: 8)),
              let cg = CIContext().createCGImage(output, from: output.extent)
        else { return nil }
        return UIImage(cgImage: cg)
    }
}

/// Scans a QR code with the camera; calls `found` with the first one.
struct QRScannerView: UIViewControllerRepresentable {
    let found: (String) -> Void

    func makeUIViewController(context: Context) -> ScannerController {
        let c = ScannerController()
        c.found = found
        return c
    }

    func updateUIViewController(_ controller: ScannerController, context: Context) {}
}

final class ScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    var found: ((String) -> Void)?
    private let session = AVCaptureSession()
    private var preview: AVCaptureVideoPreviewLayer?
    private var done = false

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        Task {
            if await AVCaptureDevice.requestAccess(for: .video) {
                setUp()
            } else {
                say("Allow camera access in Settings to scan invites.")
            }
        }
    }

    private func setUp() {
        guard let camera = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: camera),
              session.canAddInput(input)
        else { return say("No camera available.") }
        session.addInput(input)
        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else { return say("No camera available.") }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        output.metadataObjectTypes = [.qr]
        let layer = AVCaptureVideoPreviewLayer(session: session)
        layer.videoGravity = .resizeAspectFill
        layer.frame = view.layer.bounds
        view.layer.addSublayer(layer)
        preview = layer
        let s = self.session
        DispatchQueue.global(qos: .userInitiated).async { s.startRunning() }
    }

    private func say(_ text: String) {
        let label = UILabel()
        label.text = text
        label.textColor = .white
        label.numberOfLines = 0
        label.textAlignment = .center
        label.frame = view.bounds.insetBy(dx: 24, dy: 24)
        label.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(label)
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        preview?.frame = view.layer.bounds
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        let s = self.session
        DispatchQueue.global(qos: .userInitiated).async { s.stopRunning() }
    }

    nonisolated func metadataOutput(
        _ output: AVCaptureMetadataOutput, didOutput objects: [AVMetadataObject], from connection: AVCaptureConnection
    ) {
        guard let code = (objects.first as? AVMetadataMachineReadableCodeObject)?.stringValue else { return }
        // The delegate queue is the main queue.
        MainActor.assumeIsolated {
            guard !done else { return }
            done = true
            UINotificationFeedbackGenerator().notificationOccurred(.success)
            found?(code)
        }
    }
}
