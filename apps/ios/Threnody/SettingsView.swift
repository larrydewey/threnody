import SwiftUI

/// A privacy switch: on unless turned off, with what that means below it.
@MainActor
private struct PrefToggle: View {
    @EnvironmentObject private var store: NodeStore
    let title: String
    let key: String
    let on: String
    let off: String
    @State private var value = true

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Toggle(title, isOn: Binding(get: { value }, set: { new in
                value = new
                Prefs.setFlag(key, new)
                Privacy.applyAll(store)
            }))
            Text(value ? on : off).font(.footnote).foregroundStyle(.secondary)
        }
        .onAppear { value = Prefs.flag(key) }
    }
}

@MainActor
struct SettingsView: View {
    @EnvironmentObject private var store: NodeStore
    @Environment(\.dismiss) private var dismiss
    /// Read by NodeStore.privateNotifications.
    @AppStorage("privateNotifications") private var privateNotifications = true
    /// Screen security, stored negatively so unset means on.
    @AppStorage("showInAppSwitcher") private var showInAppSwitcher = false
    @State private var timer: UInt32?
    @State private var confirmReset = false

    var body: some View {
        NavigationStack {
            Form {
                if store.deviceRemoved {
                    Section {
                        Label("This device was removed from your account. Your contacts no longer accept it as you.",
                              systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                    }
                }
                Section("Account & identity") {
                    NavigationLink("Devices") { DevicesView() }
                    NavigationLink("Link a new device") { LinkDeviceView() }
                    NavigationLink("Join another device's account") { JoinAccountView() }
                    NavigationLink("Anonymous identities") { PersonasView() }
                }
                Section {
                    PrefToggle(title: "Cover traffic", key: Prefs.cover,
                               on: "Traffic no longer shows when you send. On mobile data and Low Data Mode it slows to every ten seconds.",
                               off: "An observer can see when messages are sent.")
                    PrefToggle(title: "Onion routing first", key: Prefs.onion,
                               on: "Contacts are reached through onion circuits when possible.",
                               off: "Contacts are dialed directly: the network sees who you talk to.")
                    PrefToggle(title: "Remove photo metadata", key: Prefs.strip,
                               on: "Photos are sent without location, camera or time details.",
                               off: "Photos are sent with their metadata, which can include where they were taken.")
                    VStack(alignment: .leading, spacing: 4) {
                        Toggle("Private notifications", isOn: $privateNotifications)
                        Text(privateNotifications
                            ? "Notifications say only “New message”, without sender or text."
                            : "Notifications show who wrote and what they said, except for disappearing messages.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    VStack(alignment: .leading, spacing: 4) {
                        Toggle("Screen security", isOn: Binding(get: { !showInAppSwitcher }, set: { showInAppSwitcher = !$0 }))
                            .disabled(AppLock.isEnabled)
                        Text(AppLock.isEnabled
                            ? "While app lock is on, Threnody is always hidden in the app switcher and from screen recording."
                            : !showInAppSwitcher
                            ? "Threnody is hidden in the app switcher, and while the screen is recorded or mirrored. iOS doesn't let apps block screenshots."
                            : "The app switcher and screen recordings show Threnody.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                } header: {
                    Text("Privacy & security")
                }
                AppLockSection()
                Section("Network") {
                    PrefToggle(title: "Reach contacts over the internet", key: Prefs.reach,
                               on: "Contacts can be reached on mobile data and other networks. Strangers in the public DHT see this device's IP address, not who you talk to.",
                               off: "Contacts are reached only nearby, through relays, or at addresses you dial.")
                    PrefToggle(title: "Route through volunteer relays", key: Prefs.volunteers,
                               on: "When contacts can't relay for you, circuits go through volunteers from your directories.",
                               off: "Only your own contacts relay for you.")
                    NavigationLink("Your handle") { HandleView() }
                    Button("Reset invite links", role: .destructive) { confirmReset = true }
                }
                Section {
                    PrefToggle(title: "Send read receipts", key: Prefs.sendRead,
                               on: "When you open a chat, the sender learns you've displayed their messages.",
                               off: "The sender won't know when you've read their messages.")
                    PrefToggle(title: "Send typing indicators", key: Prefs.sendTyping,
                               on: "Contacts see “…” while you write to them.",
                               off: "Contacts don't see when you're typing.")
                    Picker("Default disappearing timer", selection: Binding(get: { timer }, set: { t in
                        timer = t
                        Prefs.setNumber(Prefs.timer, t.map { UInt64($0) })
                        Privacy.applyAll(store)
                    })) {
                        ForEach(Privacy.timers.indices, id: \.self) { i in
                            Text(Privacy.timers[i].0).tag(Privacy.timers[i].1)
                        }
                    }
                    NavigationLink("Lists") { ListsView() }
                } header: {
                    Text("Messaging")
                } footer: {
                    Text("The timer applies to chats that don't have one of their own.")
                }
                if let n = store.node {
                    Section("This device") {
                        LabeledContent("Device", value: NodeStore.short(n.deviceFingerprint()))
                        LabeledContent("Account", value: NodeStore.short(n.accountFingerprint()))
                        LabeledContent("Listening on", value: store.listenAddr)
                        Text("Identity \(KeyVault.describe). Its privacy settings, handle and anonymous identities are kept with it, out of backups. Screen security and private notifications stay with the app, so they apply before the identity opens.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                }
                Section("Background") {
                    Text("iOS suspends Threnody shortly after you leave it. Messages sent to you meanwhile wait with mutually approved contacts (mailboxes) and arrive when you open the app again, or during the occasional background refresh iOS allows. Threnody doesn't use push notifications: they would need a server that sees every wake-up.")
                        .font(.footnote).foregroundStyle(.secondary)
                }
                Section {
                    EraseAllButton()
                } footer: {
                    Text("Erases everything Threnody keeps on this iPhone and starts afresh. Nothing is sent: your contacts and other devices aren't told.")
                }
            }
            .navigationTitle("Settings")
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .confirmationDialog("Reset invite links?", isPresented: $confirmReset, titleVisibility: .visible) {
                // A new key (Appendix S): old links stop finding this device in the DHT.
                Button("Reset", role: .destructive) { store.node?.resetInvites() }
            } message: {
                Text("Invite links you've shared stop finding this device across the internet. They still work at the addresses they carry while this device is there, so someone on the same network could still use one. Contacts you've already added aren't affected.")
            }
            .onAppear { timer = Prefs.number(Prefs.timer).map { UInt32(clamping: $0) } }
        }
    }
}
