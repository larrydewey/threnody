import BackgroundTasks
import SwiftUI
import UIKit
import UserNotifications

@main
@MainActor
struct ThrenodyApp: App {
    @StateObject private var store = NodeStore.shared
    @Environment(\.scenePhase) private var scenePhase

    init() {
        UNUserNotificationCenter.current().delegate = NotificationDelegate.shared
        BGTaskScheduler.shared.register(forTaskWithIdentifier: Paths.refreshTask, using: nil) { @Sendable task in
            guard let task = task as? BGAppRefreshTask else { return task.setTaskCompleted(success: false) }
            Task { @MainActor in NodeStore.shared.backgroundRefresh(task) }
        }
    }

    var body: some Scene {
        WindowGroup {
            RootView().environmentObject(store)
        }
        .onChange(of: scenePhase) { _, phase in
            store.scenePhase(phase)
            AppLock.shared.scene(phase)
        }
    }
}

@MainActor
struct RootView: View {
    @EnvironmentObject private var store: NodeStore
    @ObservedObject private var lock = AppLock.shared
    @Environment(\.scenePhase) private var scenePhase
    /// Stored negatively, so unset means hidden (the protected default).
    @AppStorage("showInAppSwitcher") private var showInAppSwitcher = false

    /// The app-switcher cover: always while app lock is on, else unless
    /// the user chose to show Threnody there.
    private var cover: Bool { scenePhase != .active && (!showInAppSwitcher || AppLock.isEnabled) }

    var body: some View {
        ZStack {
            Group {
                switch store.phase {
                case .starting:
                    ProgressView("Starting…")
                case .onboarding:
                    OnboardingView()
                case .ready:
                    ChatListView()
                case .failed(let reason):
                    ContentUnavailableView("Couldn't start", systemImage: "exclamationmark.triangle", description: Text(reason))
                }
            }
            // Under the lock screen: not read out by VoiceOver, not touchable.
            .accessibilityHidden(lock.locked)
            .allowsHitTesting(!lock.locked)
        }
        // The lock screen and the cover are in a window of their own, above
        // every sheet and alert (`LockWindow`).
        .onChange(of: cover, initial: true) { _, on in lock.covered = on }
        .onChange(of: lock.locked || cover, initial: true) { _, on in LockWindow.shared.show(on) }
        // Any touch postpones the idle lock.
        .simultaneousGesture(DragGesture(minimumDistance: 0).onChanged { _ in lock.activity() })
        .task { store.launch() }
        .alert("App lock", isPresented: $lock.notice) {
            Button("OK", role: .cancel) {}
            Button("Turn off", role: .destructive) { Task { await lock.disableDevice() } }
        } message: {
            Text("Threnody now locks with Face ID or your passcode. Change this in Settings → App lock.")
        }
    }
}

@MainActor
struct OnboardingView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var deviceName = UIDevice.current.name
    /// On by default where the phone has a passcode.
    @State private var lock = AppLock.deviceSecure

    var body: some View {
        VStack(spacing: 24) {
            Spacer()
            Image(systemName: "bubble.left.and.bubble.right.fill")
                .font(.system(size: 64))
                .foregroundStyle(.tint)
            Text("Threnody").font(.largeTitle.bold())
            Text("Messages go straight to your contacts' devices, end to end encrypted. There is no account and no server: your identity is a key made on this phone and sealed by its Secure Enclave.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            TextField("This device's name", text: $deviceName)
                .textFieldStyle(.roundedBorder)
            Text("Your contacts and your other devices see this name.")
                .font(.footnote)
                .foregroundStyle(.secondary)
            if AppLock.deviceSecure {
                Toggle("Lock Threnody with Face ID / passcode", isOn: $lock)
            }
            Spacer()
            Button {
                store.open(deviceName: deviceName.trimmingCharacters(in: .whitespaces), lock: lock)
            } label: {
                Text("Create identity").frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
        }
        .padding(24)
    }
}
