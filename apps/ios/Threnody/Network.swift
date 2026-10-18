import Foundation
import Network

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

/// Tells the node when the network path changes, like Android's network callbacks.
final class PathWatcher {
    private let monitor = NWPathMonitor()
    private var last: NWInterface.InterfaceType?

    func start(_ changed: @escaping @Sendable (_ cellular: Bool) -> Void) {
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self, path.status == .satisfied else { return }
            let type: NWInterface.InterfaceType = path.usesInterfaceType(.wifi) ? .wifi
                : path.usesInterfaceType(.cellular) ? .cellular : .other
            if type == self.last { return }
            let first = self.last == nil
            self.last = type
            if !first { changed(type == .cellular) }
        }
        monitor.start(queue: DispatchQueue(label: "threnody.path"))
    }
}
