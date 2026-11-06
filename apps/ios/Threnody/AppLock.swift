import LocalAuthentication
import SwiftUI
import UIKit

/// App lock (Appendix C, *App lock*): the gate in front of every screen.
///
/// The decisions are in Rust (`AppLockClock`, the lock file); this feeds
/// the clock what the app does and shows `LockView` when it says so. App
/// lock is on exactly while its file exists, which needs no Keychain or
/// Secure Enclave to check, so a cold start fails closed before the
/// settings have been read. It is a gate on the screen, not encryption:
/// the node keeps running behind it.
@MainActor
final class AppLock: ObservableObject {
    static let shared = AppLock()

    @Published private(set) var locked: Bool
    /// The settings once read (nil until then, or when unreadable).
    @Published private(set) var settings: LockSettings?
    /// The lock file is there but can't be read: stay locked, offer erasing.
    @Published private(set) var unreadable = false
    /// What the lock screen says under the field.
    @Published var status = ""
    /// Wall-clock ms before which entries are refused.
    @Published private(set) var retryAt: UInt64 = 0
    /// The one-time notice that device lock came on.
    @Published var notice = false
    private var clock: AppLockClock
    private var lastActivity: UInt64 = 0

    /// A monotonic clock that counts sleep (Darwin's CLOCK_MONOTONIC does).
    nonisolated static func now() -> UInt64 { clock_gettime_nsec_np(CLOCK_MONOTONIC) / 1_000_000 }
    nonisolated static var home: String { Paths.home.path }
    /// Whether app lock is on: its file exists. Synchronous; fails closed.
    nonisolated static var isEnabled: Bool { appLockEnabled(home: home) }

    private init() {
        let on = Self.isEnabled
        locked = on
        clock = AppLockClock(locked: on, biometric: true, awaySecs: 0, idleSecs: 300, nowMs: Self.now())
        // No lock file, no Keychain: a new identity rotates the secret.
        if on { reload() }
        Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(15))
                guard let self else { return }
                if Self.isEnabled && self.clock.tick(nowMs: Self.now()) { self.locked = true }
            }
        }
    }

    /// Reads the settings (off the main thread) and applies their times.
    func reload() {
        Task {
            do {
                let s = try await offMain { try appLock(home: AppLock.home, pepper: KeyVault.secret()) }
                apply(s)
            } catch {
                settings = nil
                unreadable = Self.isEnabled
                if unreadable { status = "App lock settings can't be read." }
            }
        }
    }

    func apply(_ s: LockSettings) {
        settings = s
        unreadable = false
        retryAt = s.retryAtMs
        clock.setPolicy(biometric: s.biometric, awaySecs: s.awaySecs, idleSecs: s.idleSecs)
    }

    /// The scene left (`.background`) or came back (`.active`). Face ID and
    /// system alerts only make it `.inactive`: that isn't leaving.
    func scene(_ phase: ScenePhase) {
        guard Self.isEnabled else { return }
        switch phase {
        case .background: clock.left(nowMs: Self.now())
        case .active: _ = clock.returned(nowMs: Self.now())
        default: break
        }
        locked = clock.isLocked()
    }

    /// Input in front: postpones the idle lock.
    func activity() {
        let t = Self.now()
        guard t &- lastActivity >= 1000 else { return }
        lastActivity = t
        clock.activity(nowMs: t)
    }

    func lockNow() {
        guard Self.isEnabled else { return }
        clock.lockNow()
        locked = true
    }

    private func unlocked() {
        clock.unlocked(nowMs: Self.now())
        locked = false
        status = ""
    }

    /// After an erase: as on a fresh install, nothing is locked.
    func reset() {
        clock = AppLockClock(locked: false, biometric: true, awaySecs: 0, idleSecs: 300, nowMs: Self.now())
        locked = false
        settings = nil
        unreadable = false
        status = ""
        retryAt = 0
        notice = false
    }

    /// Checks an entry on the lock screen. The duress PIN looks exactly
    /// like the right one until the app starts afresh.
    func submit(_ text: String) async {
        guard !text.isEmpty else { return }
        status = "Unlocking…"
        do {
            let outcome = try await offMain { try unlockApp(home: AppLock.home, pepper: KeyVault.secret(), entered: text) }
            switch outcome {
            case .unlocked:
                unlocked()
            case let .wrong(retryAtMs):
                status = "Wrong PIN"
                retryAt = retryAtMs
            case let .wait(retryAtMs):
                status = ""
                retryAt = retryAtMs
            case .erased:
                await NodeStore.shared.wipe(alreadyErased: true)
            }
        } catch {
            status = error.localizedDescription
        }
    }

    /// Face ID, Touch ID or the passcode. In device-lock mode the passcode
    /// may stand in; with an app PIN only biometrics count, so the
    /// passcode can't get around the app PIN or the duress PIN.
    func authenticate() async {
        guard let s = settings, s.kind == nil || s.biometric else { return }
        let device = s.kind == nil
        if await Self.deviceAuth(biometricOnly: !device, reason: "Unlock Threnody") {
            Task.detached { try? biometricUnlocked(home: AppLock.home, pepper: KeyVault.secret()) }
            unlocked()
        } else if device && !LAContext().canEvaluatePolicy(.deviceOwnerAuthentication, error: nil) {
            // Only a passcode that is gone opens the app: removing it took the passcode.
            unlocked()
            status = "Your iPhone has no passcode any more; set an app PIN to keep Threnody locked."
        }
    }

    nonisolated static func deviceAuth(biometricOnly: Bool, reason: String) async -> Bool {
        let ctx = LAContext()
        let policy: LAPolicy = biometricOnly ? .deviceOwnerAuthenticationWithBiometrics : .deviceOwnerAuthentication
        guard ctx.canEvaluatePolicy(policy, error: nil) else { return false }
        return (try? await ctx.evaluatePolicy(policy, localizedReason: reason)) ?? false
    }

    /// Whether the phone has a passcode (so device lock can work).
    nonisolated static var deviceSecure: Bool {
        LAContext().canEvaluatePolicy(.deviceOwnerAuthentication, error: nil)
    }

    /// Before turning device lock off or loosening it: the lock file can't
    /// check the phone's own authentication.
    static func confirmDevice() async -> Bool {
        guard deviceSecure else { return true }
        return await deviceAuth(biometricOnly: false, reason: "Confirm it's you")
    }

    /// Runs a change to the lock file off the main thread and applies the
    /// settings it leaves; the error's text if it failed.
    func change(_ work: @escaping @Sendable (_ home: String, _ pepper: String) throws -> Void) async -> String? {
        do {
            let s = try await offMain { () throws -> LockSettings in
                let p = try KeyVault.secret()
                try work(AppLock.home, p)
                return try appLock(home: AppLock.home, pepper: p)
            }
            apply(s)
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    /// Turns device lock on (new identities, and once on installs from
    /// before app lock).
    func enableDevice() async {
        _ = await change { h, p in
            try setAppLock(home: h, pepper: p, current: nil, secret: nil, kind: nil, biometric: true,
                           awaySecs: 0, idleSecs: 300, eraseAfterFailures: false)
        }
    }

    /// Turns device lock off, after the phone's own authentication.
    func disableDevice() async {
        guard await Self.confirmDevice() else { return }
        _ = await change { h, p in try clearAppLock(home: h, pepper: p, current: nil) }
    }
}

/// The lock screen: the app's name, one PIN or passphrase field, and a
/// Face ID button when that's on. Nothing else.
struct LockView: View {
    @ObservedObject private var lock = AppLock.shared
    @State private var text = ""
    @State private var now = Date()

    private var waitLeft: Int {
        let left = Int64(lock.retryAt) - Int64(now.timeIntervalSince1970 * 1000)
        return left > 0 ? Int((left + 999) / 1000) : 0
    }

    var body: some View {
        VStack(spacing: 20) {
            Spacer()
            Image(systemName: "lock.fill").font(.system(size: 48)).foregroundStyle(.tint)
            Text("Threnody").font(.largeTitle.bold())
            if lock.unreadable {
                Text(lock.status).foregroundStyle(.secondary).multilineTextAlignment(.center)
                EraseAllButton()
            } else if let s = lock.settings {
                if let kind = s.kind {
                    SecureField(kind == .passphrase ? "Passphrase" : "PIN", text: $text)
                        .keyboardType(kind == .digits ? .numberPad : .default)
                        .textFieldStyle(.roundedBorder)
                        .multilineTextAlignment(.center)
                        .disabled(waitLeft > 0)
                        .onSubmit(submit)
                    Button("Unlock", action: submit)
                        .buttonStyle(.borderedProminent)
                        .disabled(waitLeft > 0 || text.isEmpty)
                }
                if s.kind == nil || s.biometric {
                    Button(s.kind == nil ? "Unlock with Face ID or passcode" : "Use Face ID") {
                        Task { await lock.authenticate() }
                    }
                }
                Text(waitLeft > 0 ? "Try again in \(waitLeft) s" : lock.status)
                    .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
            } else {
                ProgressView()
            }
            Spacer()
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(.systemBackground))
        .onReceive(Timer.publish(every: 1, on: .main, in: .common).autoconnect()) { now = $0 }
        // Once the settings are in: prompt at once where that unlocks.
        .task(id: lock.settings == nil) { await lock.authenticate() }
    }

    private func submit() {
        let t = text
        text = ""
        Task { await lock.submit(t) }
    }
}

/// What the app switcher shows instead of the app.
struct PrivacyCover: View {
    var body: some View {
        ZStack {
            Color(.systemBackground).ignoresSafeArea()
            Image(systemName: "bubble.left.and.bubble.right.fill")
                .font(.system(size: 64))
                .foregroundStyle(.tint)
        }
    }
}

/// "Erase all Threnody data…" with its typed confirmation.
struct EraseAllButton: View {
    @State private var asking = false
    @State private var typed = ""

    var body: some View {
        Button("Erase all Threnody data…", role: .destructive) {
            typed = ""
            asking = true
        }
        .alert("Erase all Threnody data?", isPresented: $asking) {
            TextField("Type erase", text: $typed)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
            Button("Erase", role: .destructive) {
                if typed.trimmingCharacters(in: .whitespaces) == "erase" {
                    Task { await NodeStore.shared.wipe() }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This erases your identity, contacts, history and received files, then starts Threnody afresh. Your linked devices and your contacts are not told. Type erase to confirm.")
        }
    }
}

/// What a secret sheet asks for.
enum LockAsk: Identifiable {
    case turnOn(LockKind), change(LockKind), duress(LockKind, set: Bool), turnOff(LockKind), toDevice(LockKind)

    var id: String {
        switch self {
        case .turnOn: "on"
        case .change: "change"
        case .duress: "duress"
        case .turnOff: "off"
        case .toDevice: "device"
        }
    }
}

private func noun(_ kind: LockKind?) -> String { kind == .passphrase ? "passphrase" : "PIN" }

private let duressHelp = "If someone makes you unlock Threnody, enter this instead of your PIN. Everything on this device is erased without warning, and Threnody opens as if newly installed. Your other devices and your contacts are not told. Face ID always opens your real data: if someone could force your face, turn it off."

private let awayChoices: [(String, UInt32)] = [
    ("Immediately", 0), ("After 1 minute", 60), ("After 5 minutes", 300), ("After 15 minutes", 900), ("After 1 hour", 3600),
]
private let idleChoices: [(String, UInt32?)] = [
    ("Never", nil), ("After 1 minute", 60), ("After 5 minutes", 300), ("After 15 minutes", 900),
]

/// Settings → App lock.
struct AppLockSection: View {
    @ObservedObject private var lock = AppLock.shared
    @State private var asking: LockAsk?
    @State private var error: String?
    /// Stored negatively, so unset means hidden (the protected default).
    @AppStorage("showInAppSwitcher") private var showInAppSwitcher = false

    var body: some View {
        Section {
            if let s = lock.settings, AppLock.isEnabled {
                enabled(s)
            } else {
                if AppLock.deviceSecure {
                    Button("Lock with Face ID or passcode") { Task { await lock.enableDevice() } }
                }
                Button("Lock with an app PIN…") { asking = .turnOn(.digits) }
                Button("Lock with an app passphrase…") { asking = .turnOn(.passphrase) }
            }
            Toggle("Hide in app switcher", isOn: Binding(get: { !showInAppSwitcher }, set: { showInAppSwitcher = !$0 }))
                .disabled(AppLock.isEnabled)
            if let error { Text(error).foregroundStyle(.red).font(.footnote) }
        } header: {
            Text("App lock")
        } footer: {
            Text("App lock keeps people who pick up your phone out of Threnody. Messages still arrive while it is locked, and notifications say only “New message”. While it is on, the app switcher never shows Threnody.")
        }
        .sheet(item: $asking) { ask in SecretSheet(ask: ask) }
        .onAppear { if AppLock.isEnabled && lock.settings == nil { lock.reload() } }
    }

    @ViewBuilder private func enabled(_ s: LockSettings) -> some View {
        let device = s.kind == nil
        Text(device ? "Locks with Face ID or your passcode." : "Locks with an app \(noun(s.kind)).")
            .foregroundStyle(.secondary)
        Picker("Lock after leaving", selection: Binding(get: { s.awaySecs }, set: { new in
            policy(s, looser: new > s.awaySecs) { h, p in
                try setLockPolicy(home: h, pepper: p, biometric: s.biometric, awaySecs: new, idleSecs: s.idleSecs,
                                  eraseAfterFailures: s.eraseAfterFailures)
            }
        })) {
            ForEach(awayChoices.indices, id: \.self) { i in Text(awayChoices[i].0).tag(awayChoices[i].1) }
        }
        Picker("Lock when idle", selection: Binding(get: { s.idleSecs }, set: { new in
            let looser = s.idleSecs.map { now in new.map { $0 > now } ?? true } ?? false
            policy(s, looser: looser) { h, p in
                try setLockPolicy(home: h, pepper: p, biometric: s.biometric, awaySecs: s.awaySecs, idleSecs: new,
                                  eraseAfterFailures: s.eraseAfterFailures)
            }
        })) {
            ForEach(idleChoices.indices, id: \.self) { i in Text(idleChoices[i].0).tag(idleChoices[i].1) }
        }
        if let kind = s.kind {
            Toggle("Also unlock with Face ID", isOn: Binding(get: { s.biometric }, set: { on in
                policy(s, looser: false) { h, p in
                    try setLockPolicy(home: h, pepper: p, biometric: on, awaySecs: s.awaySecs, idleSecs: s.idleSecs,
                                      eraseAfterFailures: s.eraseAfterFailures)
                }
            }))
            Button("Change \(noun(kind))…") { asking = .change(kind) }
            Button("Duress \(noun(kind)): \(s.duress ? "set" : "not set")…") { asking = .duress(kind, set: s.duress) }
            Toggle("Erase after 10 wrong \(noun(kind))s", isOn: Binding(get: { s.eraseAfterFailures }, set: { on in
                policy(s, looser: false) { h, p in
                    try setLockPolicy(home: h, pepper: p, biometric: s.biometric, awaySecs: s.awaySecs, idleSecs: s.idleSecs,
                                      eraseAfterFailures: on)
                }
            }))
            if AppLock.deviceSecure { Button("Use Face ID or passcode instead…") { asking = .toDevice(kind) } }
        } else {
            Button("Use an app PIN instead…") { asking = .turnOn(.digits) }
            Button("Use an app passphrase instead…") { asking = .turnOn(.passphrase) }
        }
        Button("Lock now") { lock.lockNow() }
        Button("Turn app lock off…", role: .destructive) {
            if let kind = s.kind { asking = .turnOff(kind) } else { Task { await lock.disableDevice() } }
        }
    }

    /// Changes the policy; loosening device lock needs the phone's own authentication first.
    private func policy(_ s: LockSettings, looser: Bool,
                        _ work: @escaping @Sendable (String, String) throws -> Void) {
        Task {
            if s.kind == nil && looser {
                guard await AppLock.confirmDevice() else { return }
            }
            error = await lock.change(work)
        }
    }
}

/// Asks for the current secret, a new one (twice), or both.
struct SecretSheet: View {
    let ask: LockAsk
    @Environment(\.dismiss) private var dismiss
    @ObservedObject private var lock = AppLock.shared
    @State private var current = ""
    @State private var first = ""
    @State private var second = ""
    @State private var error: String?

    private var kind: LockKind {
        switch ask {
        case let .turnOn(k), let .change(k), let .duress(k, _), let .turnOff(k), let .toDevice(k): k
        }
    }
    private var needsCurrent: Bool {
        if case .turnOn = ask { return lock.settings?.kind != nil }
        return true
    }
    private var needsNew: Bool {
        switch ask {
        case .turnOn, .change, .duress: true
        case .turnOff, .toDevice: false
        }
    }
    private var title: String {
        switch ask {
        case .turnOn: "App \(noun(kind))"
        case .change: "Change \(noun(kind))"
        case .duress: "Duress \(noun(kind))"
        case .turnOff: "Turn app lock off"
        case .toDevice: "Use Face ID or passcode"
        }
    }
    private var keyboard: UIKeyboardType { kind == .digits ? .numberPad : .default }

    var body: some View {
        NavigationStack {
            Form {
                if case .duress = ask { Text(duressHelp).font(.footnote).foregroundStyle(.secondary) }
                if needsCurrent {
                    SecureField("Current \(noun(lock.settings?.kind ?? kind))", text: $current).keyboardType(keyboard)
                }
                if needsNew {
                    SecureField(kind == .passphrase ? "At least 6 characters" : "4 to 16 digits", text: $first)
                        .keyboardType(keyboard)
                    SecureField("Again", text: $second).keyboardType(keyboard)
                }
                if case let .duress(_, set) = ask, set {
                    Button("Remove the duress \(noun(kind))", role: .destructive) {
                        run { h, p, cur, _ in try setDuressPin(home: h, pepper: p, current: cur, duress: nil) }
                    }
                }
                if let error { Text(error).foregroundStyle(.red) }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) { Button("Save", action: save) }
            }
        }
    }

    private func save() {
        if needsNew {
            guard lockSecretOk(kind: kind, secret: first) else {
                error = kind == .passphrase ? "A passphrase needs at least 6 characters." : "A PIN is 4 to 16 digits."
                return
            }
            guard first == second else {
                error = "They differ."
                return
            }
        }
        let s = lock.settings
        let bio = s?.biometric ?? false
        let away = s?.awaySecs ?? 0
        let idle: UInt32? = s != nil ? s!.idleSecs : 300
        let erase = s?.eraseAfterFailures ?? false
        let k = kind
        switch ask {
        case .turnOn:
            run { h, p, cur, new in
                try setAppLock(home: h, pepper: p, current: cur.isEmpty ? nil : cur, secret: new, kind: k, biometric: false,
                               awaySecs: away, idleSecs: idle, eraseAfterFailures: false)
            }
        case .change:
            run { h, p, cur, new in
                try setAppLock(home: h, pepper: p, current: cur, secret: new, kind: k, biometric: bio,
                               awaySecs: away, idleSecs: idle, eraseAfterFailures: erase)
            }
        case .duress:
            run { h, p, cur, new in try setDuressPin(home: h, pepper: p, current: cur, duress: new) }
        case .turnOff:
            run { h, p, cur, _ in try clearAppLock(home: h, pepper: p, current: cur) }
        case .toDevice:
            run { h, p, cur, _ in
                try setAppLock(home: h, pepper: p, current: cur, secret: nil, kind: nil, biometric: true,
                               awaySecs: away, idleSecs: idle, eraseAfterFailures: false)
            }
        }
    }

    private func run(_ work: @escaping @Sendable (String, String, String, String) throws -> Void) {
        let (cur, new) = (current, first)
        Task {
            if let e = await lock.change({ h, p in try work(h, p, cur, new) }) {
                error = e
            } else {
                dismiss()
            }
        }
    }
}
