//! Tunnel peers kept in step with mutual approval (Appendix D).

use threnody_core::PublicIdentity;

use crate::node::{Shared, lock};

impl Shared {
    /// Takes down every tunnel peer that is no longer mutually approved
    /// (blocked, deleted, revoked on another of our devices, or a device
    /// that left its account): `tunnel_down` for each. Paths that end
    /// approval without an `Approval` message call this.
    pub(crate) fn prune_tunnels(&self) {
        let peers: Vec<PublicIdentity> = lock(&self.tunnel_peers).keys().copied().collect();
        for p in peers.iter().filter(|p| !self.mutual(p)) {
            self.tunnel_down(p);
        }
    }
}
