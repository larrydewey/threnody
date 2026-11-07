import SwiftUI

/// Settings → Devices: this account's devices; rename or remove them.
@MainActor
struct DevicesView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var devices: [DeviceInfo] = []
    @State private var renaming: DeviceInfo?
    @State private var newName = ""
    @State private var removing: DeviceInfo?
    @State private var failure: String?

    var body: some View {
        List {
            Section {
                ForEach(devices, id: \.fingerprint) { d in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(d.name.isEmpty ? NodeStore.short(d.fingerprint) : d.name)
                        Text(d.thisDevice ? "This device" : NodeStore.short(d.fingerprint))
                            .font(.caption.monospaced()).foregroundStyle(.secondary)
                    }
                    .swipeActions {
                        if !d.thisDevice && devices.count > 1 {
                            Button("Remove", role: .destructive) { removing = d }
                        }
                        Button("Rename") {
                            newName = d.name
                            renaming = d
                        }
                    }
                }
            } footer: {
                Text("Swipe a device to rename it or remove it from your account.")
            }
            Section {
                NavigationLink("Link a new device") { LinkDeviceView() }
            }
        }
        .navigationTitle("Devices")
        .navigationBarTitleDisplayMode(.inline)
        .task { load() }
        .alert("Rename device", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } }),
               presenting: renaming) { d in
            TextField("Name", text: $newName)
            Button("Save") {
                let (device, name) = (d.fingerprint, newName.trimmingCharacters(in: .whitespaces))
                act { try $0.renameDevice(device: device, name: name) }
            }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Your other devices and your contacts see this name.")
        }
        .confirmationDialog("Remove \(removing?.name ?? "")?", isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
                            titleVisibility: .visible, presenting: removing) { d in
            Button("Remove", role: .destructive) {
                let device = d.fingerprint
                act { try $0.removeDevice(device: device) }
            }
        } message: { _ in
            Text("It leaves your account: your other devices and your contacts stop trusting it, and it gets no more of your messages. Do this for a lost or retired device. To use it again, link it anew.")
        }
        .alert("Failed", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
    }

    private func load() {
        devices = store.node?.devices() ?? []
    }

    private func act(_ f: @escaping @Sendable (ThrenodyNode) throws -> Void) {
        guard let n = store.node else { return }
        Task {
            do {
                try await offMain { try f(n) }
            } catch {
                failure = error.localizedDescription
            }
            load()
            store.refresh()
        }
    }
}

/// Settings → Link a new device: a one-time code for this account.
@MainActor
struct LinkDeviceView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var code: String?

    var body: some View {
        Form {
            if let code {
                Section {
                    QRCodeView(text: code)
                        .frame(width: 240, height: 240)
                        .frame(maxWidth: .infinity)
                    ShareLink("Share code", item: code)
                } footer: {
                    Text("On the new device, choose “Join another device's account” and scan or paste this. It works once. Only show it to your own devices.")
                }
                if let n = store.node {
                    Section("Account") {
                        Text(n.accountFingerprint()).font(.caption.monospaced()).textSelection(.enabled)
                    }
                }
            } else {
                Text("No network: both devices need to be on the same network.")
            }
        }
        .navigationTitle("Link a new device")
        .navigationBarTitleDisplayMode(.inline)
        .onAppear {
            guard let n = store.node, let ip = LocalAddress.ipv4() else { return }
            let port = store.listenAddr.split(separator: ":").last.map(String.init) ?? "7450"
            code = n.createLinkCode(addr: "\(ip):\(port)")
        }
    }
}

/// Settings → Join another device's account (and onboarding's "I already
/// use Threnody"): this device joins the account of the one that made the
/// code.
@MainActor
struct JoinAccountView: View {
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    @State private var code = ""
    @State private var scanning = false
    @State private var busy = false
    @State private var joined: String?
    @State private var failure: String?

    var body: some View {
        Form {
            if store.node == nil {
                ProgressView("Starting…")
            } else if let joined {
                Section {
                    Label("Joined account \(NodeStore.short(joined))", systemImage: "checkmark.circle")
                    Button("Done") {
                        store.pendingJoin = false
                        dismiss()
                    }
                }
            } else {
                Section {
                    TextField("threnody-link://…", text: $code, axis: .vertical)
                        .font(.footnote.monospaced())
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    PasteButton(payloadType: String.self) { strings in
                        if let s = strings.first { code = s.trimmingCharacters(in: .whitespacesAndNewlines) }
                    }
                    Button("Scan QR code", systemImage: "qrcode.viewfinder") { scanning = true }
                    Button {
                        join()
                    } label: {
                        if busy { ProgressView() } else { Text("Join").bold() }
                    }
                    .disabled(busy || classifyLink(text: code)?.kind != .deviceLink)
                } footer: {
                    Text("This device becomes part of that account: your contacts see both as you. Get the code from “Link a new device” on the other device. Only use a code you made yourself: whoever made it gets this device's contacts and history.")
                }
            }
        }
        .navigationTitle("Join another device's account")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if store.pendingJoin {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Not now") {
                        store.pendingJoin = false
                        dismiss()
                    }
                }
            }
        }
        .sheet(isPresented: $scanning) {
            QRScannerView { scanned in
                scanning = false
                if classifyLink(text: scanned)?.kind == .deviceLink {
                    code = scanned
                } else {
                    failure = "That QR code isn't a link code."
                }
            }
            .ignoresSafeArea()
        }
        .alert("Couldn't join", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .onAppear {
            if let c = store.joinCode {
                code = c
                store.joinCode = nil
            }
        }
    }

    private func join() {
        guard let n = store.node else { return }
        let c = code.trimmingCharacters(in: .whitespacesAndNewlines)
        busy = true
        Task {
            do {
                joined = try await offMain { try n.linkWith(code: c) }
                store.refresh()
            } catch {
                failure = error.localizedDescription
            }
            busy = false
        }
    }
}
