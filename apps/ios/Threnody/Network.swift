import Foundation
import Network
import os

enum LocalAddress {
    /// This device's IPv4 address on Wi-Fi (en0), else on any other up interface.
    static func ipv4() -> String? {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return nil }
        defer { freeifaddrs(head) }
        var found: [String: String] = [:]
        for ptr in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let ifa = ptr.pointee
            guard let addr = ifa.ifa_addr, addr.pointee.sa_family == UInt8(AF_INET),
                  ifa.ifa_flags & UInt32(IFF_UP) != 0, ifa.ifa_flags & UInt32(IFF_LOOPBACK) == 0
            else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(addr, socklen_t(addr.pointee.sa_len), &host, socklen_t(host.count),
                              nil, 0, NI_NUMERICHOST) == 0 else { continue }
            found[String(cString: ifa.ifa_name)] = String(cString: host)
        }
        return found["en0"] ?? found.first { !$0.key.hasPrefix("utun") && !$0.key.hasPrefix("pdp_ip") }?.value
            ?? found.first?.value
    }
}

/// What the network path is: on cellular, and metered (expensive, or
/// Low Data Mode).
struct NetworkPath: Sendable, Equatable {
    let cellular: Bool
    let metered: Bool
}

/// Tells the node when the network path changes, like Android's network
/// callbacks: the first path too (its metering sets the cover-traffic
/// interval), then each change of interface type or metering.
final class PathWatcher {
    typealias Callback = @Sendable (_ network: NetworkPath, _ typeChanged: Bool) -> Void

    private let monitor = NWPathMonitor()
    private var last: (type: NWInterface.InterfaceType, metered: Bool)?
    /// Set on the main actor, read on the monitor's queue.
    private let changed = OSAllocatedUnfairLock<Callback?>(initialState: nil)
    private var started = false

    /// Starts watching, or (an NWPathMonitor starts only once) swaps what
    /// it calls, after an erase and a new identity in the same process.
    func start(_ changed: @escaping Callback) {
        self.changed.withLock { $0 = changed }
        guard !started else { return }
        started = true
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self, path.status == .satisfied else { return }
            let type: NWInterface.InterfaceType = path.usesInterfaceType(.wifi) ? .wifi
                : path.usesInterfaceType(.cellular) ? .cellular : .other
            let metered = path.isExpensive || path.isConstrained
            let before = self.last
            if let before, before.type == type && before.metered == metered { return }
            self.last = (type, metered)
            // The first path isn't a change of network: nothing to redial.
            let typeChanged = before.map { $0.type != type } ?? false
            if let call = self.changed.withLock({ $0 }) {
                call(NetworkPath(cellular: type == .cellular, metered: metered), typeChanged)
            }
        }
        monitor.start(queue: DispatchQueue(label: "threnody.path"))
    }
}
