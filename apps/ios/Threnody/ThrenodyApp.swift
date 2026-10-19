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
        .onChange(of: scenePhase) { _, phase in store.scenePhase(phase) }
    }
}

@MainActor
struct RootView: View {
    @EnvironmentObject private var store: NodeStore

    var body: some View {
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
        .task { store.launch() }
    }
}

@MainActor
struct OnboardingView: View {
    @EnvironmentObject private var store: NodeStore
    @State private var deviceName = UIDevice.current.name

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
            Spacer()
            Button {
                store.open(deviceName: deviceName.trimmingCharacters(in: .whitespaces))
            } label: {
                Text("Create identity").frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
        }
        .padding(24)
    }
}
