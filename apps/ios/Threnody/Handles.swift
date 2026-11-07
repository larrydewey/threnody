import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// A handle (Appendix S): `name-XXXX-XXXX`, which people can type into
/// "Add contact" to find this device without an invite. None unless
/// chosen: anyone who has it can reach this device and learn its IP
/// address. The name and version are kept in `Prefs`, so the handle is
/// published again after a restart (main identity only).
enum Handles {
    /// Publishes the chosen handle again (off the main thread: Argon2id).
    static func resume(_ n: ThrenodyNode) {
        guard let name = Prefs.text(Prefs.handleName) else { return }
        let version = Prefs.number(Prefs.handleVersion) ?? 0
        Task.detached(priority: .utility) { _ = try? n.setHandle(name: name, version: version) }
    }

    /// Publishes `name` at `version`; returns the handle.
    /// Nothing is kept if an erase came meanwhile.
    static func publish(_ n: ThrenodyNode, name: String, version: UInt64) async throws -> String? {
        let gen = NodeStore.currentGeneration
        let h = try await offMain { try n.setHandle(name: name, version: version) }
        if h != nil {
            Prefs.setText(Prefs.handleName, name, generation: gen)
            Prefs.setNumber(Prefs.handleVersion, version, generation: gen)
        }
        return h
    }

    /// Stops publishing it; it stops working within the hour.
    static func remove(_ n: ThrenodyNode) async {
        let gen = NodeStore.currentGeneration
        _ = try? await offMain { try n.setHandle(name: nil, version: 0) }
        Prefs.setText(Prefs.handleName, nil, generation: gen)
    }
}

/// Settings → Your handle: shows it, with Share, Copy and Change; or
/// offers to make one.
@MainActor
struct HandleView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var handle: String?
    @State private var name = ""
    @State private var busy = false
    @State private var failure: String?
    @State private var choosing = false
    @State private var copied = false

    var body: some View {
        Form {
            if let handle, !choosing {
                Section {
                    Text(handle).font(.title3.monospaced()).textSelection(.enabled)
                    ShareLink("Share", item: handle)
                    Button(copied ? "Copied" : "Copy") {
                        UIPasteboard.general.setItems([[UTType.plainText.identifier: handle]], options: [.localOnly: true])
                        copied = true
                    }
                } footer: {
                    Text("Give this to people you want to find you. They type it into “Add contact”. A new code stops the old one from working within the hour.")
                }
                Section("Change") {
                    Button("New code (same name)") { change(version: (Prefs.number(Prefs.handleVersion) ?? 0) + 1) }
                    Button("Different name") {
                        name = Prefs.text(Prefs.handleName) ?? ""
                        choosing = true
                    }
                    Button("Remove my handle", role: .destructive) { remove() }
                }
            } else {
                Section {
                    TextField("A name, e.g. larry", text: $name)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    Button {
                        change(version: Prefs.number(Prefs.handleVersion) ?? 0)
                    } label: {
                        if busy { ProgressView() } else { Text("Create").bold() }
                    }
                    .disabled(busy || name.trimmingCharacters(in: .whitespaces).isEmpty)
                } header: {
                    Text("Get a handle")
                } footer: {
                    Text("People who have your handle can find you by typing it, without an invite. A code made from your key is added to the name, so no one else can take it. Anyone with the handle can write to you (as a message request) and learn this device's IP address. You can change or remove it at any time."
                        + (store.node?.reachInternet() == false ? "\n\nNeeds “Reach contacts over the internet”, which is off." : ""))
                }
            }
            if busy { Section { ProgressView("Publishing…") } }
        }
        .navigationTitle("Your handle")
        .navigationBarTitleDisplayMode(.inline)
        .alert("Couldn't make that handle", isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failure ?? "")
        }
        .onAppear { handle = store.node?.ownHandle() }
    }

    private func change(version: UInt64) {
        guard let n = store.node else { return }
        let chosen = choosing || handle == nil ? name.trimmingCharacters(in: .whitespaces) : (Prefs.text(Prefs.handleName) ?? "")
        guard !chosen.isEmpty else { return }
        busy = true
        Task {
            do {
                if let h = try await Handles.publish(n, name: chosen, version: version) { handle = h }
                choosing = false
            } catch {
                failure = error.localizedDescription
            }
            busy = false
        }
    }

    private func remove() {
        guard let n = store.node else { return }
        Task {
            await Handles.remove(n)
            handle = nil
            name = ""
        }
    }
}
