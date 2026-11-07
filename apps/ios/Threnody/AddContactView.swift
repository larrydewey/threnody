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
    @State private var failureTitle = "Couldn't connect"
    @State private var confirmReset = false
    /// The invite a tap gave us (Appendix T), while the field still holds it.
    @State private var tappedLink: String?
    @State private var nfc = NfcTap()

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Invite link or handle", text: $link, axis: .vertical)
                        .font(.footnote.monospaced())
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Name (only you see it)", text: $name)
                    Button("Scan QR code", systemImage: "qrcode.viewfinder") { scanning = true }
                    if NfcTap.available, let n = store.node {
                        Button("Tap an Android phone", systemImage: "wave.3.right") {
                            nfc.begin(reader: n.nfcReader()) { o in
                                switch o {
                                case let .tapped(l, _):
                                    link = l
                                    tappedLink = l
                                case let .failed(why):
                                    fail("Couldn't connect", why.text)
                                }
                            }
                        }
                    }
                    PasteButton(payloadType: String.self) { strings in
                        if let s = strings.first { link = s.trimmingCharacters(in: .whitespacesAndNewlines) }
                    }
                    Button {
                        connect()
                    } label: {
                        if busy {
                            HStack {
                                ProgressView()
                                if isHandle { Text("Looking for \(link.trimmingCharacters(in: .whitespacesAndNewlines))… (up to two minutes)") }
                            }
                        } else {
                            Text("Connect").bold()
                        }
                    }
                    // Only a well-formed invite (the parser all the apps
                    // share), or a handle when the DHT can be asked.
                    .disabled(busy || !(classifyLink(text: link)?.kind == .invite || (isHandle && reach)))
                    if isHandle && !reach {
                        Text("Looking for a handle needs “Reach contacts over the internet” (Settings).")
                            .font(.footnote).foregroundStyle(.orange)
                    }
                } header: {
                    Text("Their invite or handle")
                } footer: {
                    Text("Scan their QR code, paste the link they sent you, type their handle (name-XXXX-XXXX), or tap their Android phone.")
                }
                Section {
                    if let invite {
                        QRCodeView(text: invite)
                            .frame(width: 240, height: 240)
                            .frame(maxWidth: .infinity)
                        Text(invite).font(.caption.monospaced()).textSelection(.enabled)
                        ShareLink("Share invite", item: invite)
                        Button("Reset invite links", role: .destructive) { confirmReset = true }
                    } else {
                        Text("No network: connect to Wi-Fi to make an invite.")
                    }
                } header: {
                    Text("Your invite")
                } footer: {
                    Text("Let the other person scan this, or send them the link. Anyone with it can contact this device. To add by tapping, they show their invite on an Android phone and you choose Tap.")
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
                    let info = classifyLink(text: code)
                    if info?.kind == .deviceLink, let info {
                        // A link code: this device joins that account.
                        store.joinCode = info.link
                        dismiss()
                        // Once this sheet is gone: one sheet at a time.
                        Task {
                            try? await Task.sleep(for: .milliseconds(700))
                            store.pendingJoin = true
                        }
                    } else if info?.kind != .invite {
                        fail("Not an invite", "That QR code isn't a Threnody invite or link code.")
                    } else if isOwn(info) {
                        fail("Not an invite", "That's your own invite.")
                    } else if let info {
                        link = info.link
                    }
                }
                .ignoresSafeArea()
            }
            .alert(failureTitle, isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(failure ?? "")
            }
            .confirmationDialog("Reset invite links?", isPresented: $confirmReset, titleVisibility: .visible) {
                Button("Reset", role: .destructive) {
                    // A new key (Appendix S): old links stop finding this device in the DHT.
                    store.node?.resetInvites()
                    invite = store.invite()
                }
            } message: {
                Text("Invite links you've shared stop finding this device across the internet. They still work at the addresses they carry while this device is there, so someone on the same network could still use one. Contacts you've already added aren't affected.")
            }
            .onAppear { invite = store.invite() }
        }
    }

    /// The field holds a handle (Appendix S) rather than an invite.
    private var isHandle: Bool { looksLikeHandle(text: link) }

    /// Finding a handle asks the DHT, which needs this.
    private var reach: Bool { store.node?.reachInternet() ?? false }

    private func fail(_ title: String, _ text: String) {
        failureTitle = title
        failure = text
    }

    /// One of this device's own invites: dialling it would reach ourselves.
    private func isOwn(_ info: LinkInfo?) -> Bool {
        guard let fp = info?.fingerprint, let mine = store.node?.deviceFingerprint() else { return false }
        return fp == mine
    }

    private func connect() {
        guard let n = store.node else { return }
        if isOwn(classifyLink(text: link)) {
            return fail("Not an invite", "That's your own invite.")
        }
        failureTitle = "Couldn't connect"
        let (target, petname) = (link.trimmingCharacters(in: .whitespacesAndNewlines), name.trimmingCharacters(in: .whitespaces))
        // A hand-edited link isn't the phone we tapped.
        let tapped = tappedLink == target
        busy = true
        Task {
            do {
                let peer = try await offMain {
                    // We tapped phones: they're a contact even if they reached us first.
                    if tapped { _ = try n.expectContact(target: target, name: petname.isEmpty ? nil : petname) }
                    let peer = try n.connect(target: target)
                    if !petname.isEmpty { try? n.setName(peer: peer, name: petname) }
                    return peer
                }
                store.refresh()
                store.openChat = ChatRef(persona: nil, target: .contact(peer))
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
