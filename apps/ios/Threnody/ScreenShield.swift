import SwiftUI
import UIKit

/// Whether the screen is being recorded, mirrored or AirPlayed: then the
/// privacy cover goes up (with screen security on), as it does in the app
/// switcher. iOS has no public way to block screenshots; this is its share
/// of Android's FLAG_SECURE.
@MainActor
final class ScreenCapture: ObservableObject {
    static let shared = ScreenCapture()
    @Published private(set) var captured = false

    fileprivate func report(_ on: Bool) {
        if captured != on { captured = on }
    }
}

/// An invisible view in the app's window that reports its scene's capture
/// state, now and whenever it changes.
struct CaptureProbe: UIViewRepresentable {
    func makeUIView(context: Context) -> ProbeView { ProbeView() }
    func updateUIView(_ view: ProbeView, context: Context) {}

    final class ProbeView: UIView {
        override init(frame: CGRect) {
            super.init(frame: frame)
            isUserInteractionEnabled = false
            isAccessibilityElement = false
            registerForTraitChanges([UITraitSceneCaptureState.self]) { (view: ProbeView, _: UITraitCollection) in
                view.report()
            }
        }

        required init?(coder: NSCoder) { fatalError("not used") }

        override func didMoveToWindow() {
            super.didMoveToWindow()
            report()
        }

        private func report() {
            ScreenCapture.shared.report(traitCollection.sceneCaptureState == .active)
        }
    }
}
