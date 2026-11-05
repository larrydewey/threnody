//! Limits on connections that aren't sessions yet.
//!
//! Anyone who can reach a listener can open connections, and until the
//! handshake says who is on the other end each one holds a socket (and
//! memory) on trust. A [`Gate`] caps how many are in that state at once,
//! in all and from one source; a [`Pass`] holds a place until dropped.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::node::lock;

/// Where a connection comes from, for the per-source limit. IPv6 counts
/// by /64: one host usually has a whole one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Source {
    Ip(IpAddr),
    /// A radio link (Bluetooth, Wi-Fi Direct): no address to tell
    /// callers apart, so they share one allowance (in a gate of their
    /// own: load from the internet mustn't shut out the local fallback).
    Link,
}

impl Source {
    pub(crate) fn ip(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V6(v6) => {
                let s = v6.segments();
                Self::Ip(IpAddr::from([s[0], s[1], s[2], s[3], 0, 0, 0, 0]))
            }
            v4 => Self::Ip(v4),
        }
    }
}

#[derive(Default)]
struct Counts {
    all: usize,
    by: HashMap<Source, usize>,
}

pub(crate) struct Gate {
    total: usize,
    per_source: usize,
    counts: Arc<Mutex<Counts>>,
}

/// A place taken in a [`Gate`], given back on drop.
pub(crate) struct Pass {
    counts: Arc<Mutex<Counts>>,
    source: Source,
}

impl Gate {
    pub(crate) fn new(total: usize, per_source: usize) -> Self {
        Self {
            total,
            per_source,
            counts: Arc::default(),
        }
    }

    /// A place for a connection from `source`, unless the gate is full.
    pub(crate) fn enter(&self, source: Source) -> Option<Pass> {
        let mut c = lock(&self.counts);
        let here = c.by.get(&source).copied().unwrap_or(0);
        if c.all >= self.total || here >= self.per_source {
            return None;
        }
        c.all += 1;
        c.by.insert(source, here + 1);
        Some(Pass {
            counts: self.counts.clone(),
            source,
        })
    }

    /// Places taken.
    pub(crate) fn len(&self) -> usize {
        lock(&self.counts).all
    }

    /// Places taken by `source`.
    pub(crate) fn held(&self, source: Source) -> usize {
        lock(&self.counts).by.get(&source).copied().unwrap_or(0)
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        let mut c = lock(&self.counts);
        c.all -= 1;
        if let Some(n) = c.by.get_mut(&self.source) {
            *n -= 1;
            if *n == 0 {
                c.by.remove(&self.source);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_are_capped_in_all_and_per_source_and_given_back() {
        let gate = Gate::new(3, 2);
        let a = Source::ip("192.0.2.1".parse().unwrap());
        let b = Source::ip("192.0.2.2".parse().unwrap());
        let a1 = gate.enter(a).unwrap();
        let _a2 = gate.enter(a).unwrap();
        assert!(gate.enter(a).is_none(), "per source");
        let _b1 = gate.enter(b).unwrap();
        assert!(gate.enter(Source::Link).is_none(), "in all");
        assert_eq!(gate.held(a), 2);
        drop(a1);
        assert_eq!(gate.len(), 2);
        assert_eq!(gate.held(a), 1);
        assert!(gate.enter(a).is_some());
    }

    #[test]
    fn ipv6_counts_by_prefix() {
        let x = Source::ip("2001:db8:1:2::1".parse().unwrap());
        let y = Source::ip("2001:db8:1:2:ffff::9".parse().unwrap());
        let z = Source::ip("2001:db8:1:3::1".parse().unwrap());
        assert_eq!(x, y);
        assert_ne!(x, z);
        let mapped = Source::ip("::ffff:192.0.2.1".parse().unwrap());
        assert_eq!(mapped, Source::ip("192.0.2.1".parse().unwrap()));
    }
}
