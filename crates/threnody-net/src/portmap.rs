//! Ask the router to forward our TCP and UDP ports to us, so contacts
//! can often dial us directly instead of hole punching.
//!
//! Always on with reachability, and never without it (so never for an
//! anonymous identity): failure just means we keep relying on observed
//! addresses and hole punching. A mapping is released on shutdown and
//! when reachability is turned off. IPv4 only; IPv6 needs no mapping.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU16;
use std::time::Duration;

use port_control_client::{Config, Method, PortMapping, Protocol};

use crate::node::{Event, Node, lock};

/// How often a node with reachability off checks whether it came on.
const OFF_CHECK: Duration = Duration::from_secs(1);

/// UPnP first (most widely supported router feature), then PCP, then
/// NAT-PMP.
const METHODS: &[Method] = &[Method::Upnp, Method::Pcp, Method::NatPmp];

fn config(protocol: Protocol, port: NonZeroU16) -> Config {
    Config::new(protocol, port)
        .lifetime(Duration::from_secs(2 * 60 * 60))
        .methods(METHODS.to_vec())
}

/// Asks the router for mappings while reachability is on, and releases
/// them when it goes off. Returns when the node shuts down.
pub(crate) async fn run(node: Node, port: NonZeroU16) {
    loop {
        // Nothing goes to the router while reachability is off: the
        // mapping itself would open this device to the internet.
        while !node.reach_enabled() {
            tokio::select! {
                () = node.closed() => return,
                () = tokio::time::sleep(OFF_CHECK) => {}
            }
        }
        if !mapped(&node, port).await {
            return;
        }
    }
}

/// Watches the router mappings for the node's TCP and UDP ports. While
/// a mapping holds, its external address becomes a candidate; when it is
/// lost or changes, candidates are regathered. Returns, after releasing
/// both mappings, when reachability goes off (`true`) or the node shuts
/// down (`false`).
async fn mapped(node: &Node, port: NonZeroU16) -> bool {
    node.emit(Event::ReachNote {
        note: format!("asking the router to forward port {port}"),
    });
    let udp = PortMapping::start(config(Protocol::Udp, port));
    let tcp = PortMapping::start(config(Protocol::Tcp, port));
    let mut wu = udp.watch();
    let mut wt = tcp.watch();
    loop {
        // Wait for the router to grant us anything, but not forever.
        let granted = tokio::select! {
            () = node.closed() => false,
            _ = wu.wait_for(|m| m.is_some()) => true,
            _ = wt.wait_for(|m| m.is_some()) => true,
            () = tokio::time::sleep(Duration::from_secs(10)) => true,
        };
        if !granted || !node.reach_enabled() {
            release(node, udp, tcp).await;
            return granted;
        }
        publish(node, &udp, &tcp);
        // Steady state: re-check whenever a mapping changes, every 5 s
        // otherwise (the mappings renew and retry on their own).
        let open = tokio::select! {
            () = node.closed() => false,
            _ = wu.changed() => true,
            _ = wt.changed() => true,
            () = tokio::time::sleep(Duration::from_secs(5)) => true,
        };
        if !open || !node.reach_enabled() {
            release(node, udp, tcp).await;
            return open;
        }
        publish(node, &udp, &tcp);
        if udp.mapping().is_none() && tcp.mapping().is_none() {
            let mut inner = lock(&node.rdv().inner);
            if !inner.mapped.is_empty() {
                inner.mapped.clear();
                drop(inner);
                node.rdv().regather.notify_one();
            }
        }
    }
}

/// Stores the external addresses of the live mappings as candidates.
/// Returns true when the set changed, so the caller should regather.
fn publish(node: &Node, udp: &PortMapping, tcp: &PortMapping) -> bool {
    let mut mapped: Vec<SocketAddr> = Vec::new();
    for m in [udp.mapping(), tcp.mapping()].into_iter().flatten() {
        let addr = SocketAddr::new(IpAddr::V4(*m.external.ip()), m.external.port());
        if !mapped.contains(&addr) {
            mapped.push(addr);
        }
    }
    let mut inner = lock(&node.rdv().inner);
    if inner.mapped == mapped {
        return false;
    }
    inner.mapped = mapped;
    drop(inner);
    node.rdv().regather.notify_one();
    true
}

async fn release(node: &Node, udp: PortMapping, tcp: PortMapping) {
    let changed = {
        let mut inner = lock(&node.rdv().inner);
        let changed = !inner.mapped.is_empty();
        inner.mapped.clear();
        changed
    };
    if changed {
        node.rdv().regather.notify_one();
    }
    udp.stop().await;
    tcp.stop().await;
}
