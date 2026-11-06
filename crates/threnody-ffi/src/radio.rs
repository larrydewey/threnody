//! Bluetooth LE and Wi-Fi Direct on Linux (Appendix K, L), through
//! `threnody-radio`, for desktop apps. Android and iOS bridge their own
//! platform radios (`ByteLink`); this is the BlueZ and NetworkManager
//! side, run on the node's own runtime.
//!
//! Anonymous identities never use radios: a beacon or a Wi-Fi Direct
//! group would show nearby devices who they are.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use threnody_net::DirectOffer;
use threnody_radio::ble::{Address, AddressType, Found, Listening};
use threnody_radio::wifidirect::Groups;

use crate::{NodeEvent, Result, ThrenodyNode, fail, fp};

/// The radio state of one node.
pub(crate) struct Radio {
    /// Private directory for the short-lived Wi-Fi Direct password file.
    scratch: PathBuf,
    ble: Mutex<Option<Listening>>,
    /// Serialises Bluetooth start and stop across their `block_on`s, so
    /// two quick toggles can't both advertise.
    ble_op: Mutex<()>,
    ble_error: Mutex<Option<String>>,
    groups: Groups,
}

impl Radio {
    pub(crate) fn new(scratch: PathBuf) -> Self {
        Self {
            scratch,
            ble: Mutex::new(None),
            ble_op: Mutex::new(()),
            ble_error: Mutex::new(None),
            groups: Groups::new(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A Threnody device heard over Bluetooth LE.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BleDeviceRecord {
    pub address: String,
    /// A random (not public) LE address: pass it back to connect.
    pub random: bool,
    pub psm: u16,
    pub rssi: Option<i16>,
    pub name: Option<String>,
}

const NOT_FOR_PERSONAS: &str = "anonymous identities don't use Bluetooth or Wi-Fi Direct: they would show nearby devices who they are";

#[uniffi::export]
impl ThrenodyNode {
    /// Advertises a private beacon, accepts sessions and dials approved
    /// contacts heard nearby over Bluetooth LE (main identity only).
    /// Idempotent; returns the L2CAP PSM.
    pub fn start_bluetooth(&self) -> Result<u16> {
        self.no_persona()?;
        let _op = lock(&self.radio.ble_op);
        if let Some(l) = lock(&self.radio.ble).as_ref() {
            return Ok(l.psm);
        }
        let _g = self.rt.enter();
        match self
            .rt
            .block_on(threnody_radio::ble::listen(&self.node, self.radio_log()))
        {
            Ok(l) => {
                let psm = l.psm;
                *lock(&self.radio.ble) = Some(l);
                *lock(&self.radio.ble_error) = None;
                Ok(psm)
            }
            Err(e) => {
                let e = format!("{e:#}");
                *lock(&self.radio.ble_error) = Some(e.clone());
                Err(fail(e))
            }
        }
    }

    /// Stops Bluetooth LE and waits (up to 3 s) for the controller to go
    /// idle.
    pub fn stop_bluetooth(&self) {
        let _op = lock(&self.radio.ble_op);
        let l = lock(&self.radio.ble).take();
        if let Some(l) = l {
            let _g = self.rt.enter();
            self.rt.block_on(l.shutdown());
        }
    }

    pub fn bluetooth_running(&self) -> bool {
        lock(&self.radio.ble).is_some()
    }

    /// Why the last start failed (no adapter, BlueZ not running), if it
    /// did; cleared by a start that works.
    pub fn bluetooth_error(&self) -> Option<String> {
        lock(&self.radio.ble_error).clone()
    }

    /// The Threnody devices heard in `secs` seconds (1 to 30). Main
    /// identity only.
    pub fn bluetooth_scan(&self, secs: u32) -> Result<Vec<BleDeviceRecord>> {
        self.no_persona()?;
        let _g = self.rt.enter();
        let found = self
            .rt
            .block_on(threnody_radio::ble::scan(u64::from(secs.clamp(1, 30))))
            .map_err(|e| fail(format!("{e:#}")))?;
        Ok(found
            .into_iter()
            .map(|f| BleDeviceRecord {
                address: f.addr.to_string(),
                random: f.addr_type == AddressType::LeRandom,
                psm: f.psm,
                rssi: f.rssi,
                name: f.name,
            })
            .collect())
    }

    /// Opens a session over a device's L2CAP channel (from
    /// `bluetooth_scan`); returns the peer's fingerprint.
    pub fn bluetooth_connect(&self, address: String, random: bool, psm: u16) -> Result<String> {
        self.no_persona()?;
        let addr: Address = address.parse().map_err(fail)?;
        let f = Found {
            addr,
            addr_type: if random {
                AddressType::LeRandom
            } else {
                AddressType::LePublic
            },
            psm,
            rssi: None,
            name: None,
        };
        let _g = self.rt.enter();
        let peer = self
            .rt
            .block_on(threnody_radio::ble::connect(&self.node, &f, None))
            .map_err(|e| fail(format!("{e:#}")))?;
        Ok(fp(&peer))
    }

    /// Joins the Wi-Fi Direct group `peer` offered (`WifiDirectOffer`)
    /// through NetworkManager and dials it, pinned. One group at a time;
    /// a repeated offer for the group already joined is ignored. Blocks
    /// until joined (up to about a minute). Main identity only.
    pub fn join_wifi_direct(
        &self,
        peer: String,
        ssid: String,
        passphrase: String,
        addr: String,
    ) -> Result<()> {
        self.no_persona()?;
        let p = self.resolve(&peer)?;
        let offer = DirectOffer {
            ssid,
            passphrase,
            addr,
        };
        let _g = self.rt.enter();
        self.rt
            .block_on(self.radio.groups.join(
                &self.node,
                p,
                &offer,
                &self.radio.scratch,
                self.radio_log(),
            ))
            .map_err(|e| fail(format!("{e:#}")))
    }

    /// Leaves every joined group, and deletes profiles a crash left
    /// behind; returns how many.
    pub fn leave_wifi_direct(&self) -> u32 {
        let _g = self.rt.enter();
        let n = self
            .rt
            .block_on(self.radio.groups.leave_all(&self.radio_log()));
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// The contacts whose Wi-Fi Direct groups are joined.
    pub fn wifi_direct_joined(&self) -> Vec<String> {
        self.radio.groups.peers().iter().map(fp).collect()
    }
}

impl ThrenodyNode {
    fn no_persona(&self) -> Result<()> {
        if self.node.is_persona() {
            return Err(fail(NOT_FOR_PERSONAS));
        }
        Ok(())
    }

    /// Radio trouble, as `NodeEvent::Other` lines for the app's log.
    fn radio_log(&self) -> threnody_radio::Log {
        let queued = self.queued.clone();
        std::sync::Arc::new(move |line| {
            lock(&queued).push_back(NodeEvent::Other {
                description: format!("radio: {line}"),
            });
        })
    }

    /// Stops Bluetooth and leaves joined groups (bounded, so quitting
    /// never hangs on NetworkManager).
    pub(crate) fn radio_shutdown(&self) {
        self.stop_bluetooth();
        if self.radio.groups.peers().is_empty() {
            return;
        }
        let log = self.radio_log();
        let _g = self.rt.enter();
        self.rt.block_on(async {
            let _ =
                tokio::time::timeout(Duration::from_secs(10), self.radio.groups.leave_all(&log))
                    .await;
        });
    }

    /// Releases radio state inside the runtime (bluer's D-Bus objects
    /// need it), before the runtime itself goes.
    pub(crate) fn radio_drop(&mut self) {
        let _g = self.rt.enter();
        drop(lock(&self.radio.ble).take());
        drop(std::mem::take(&mut self.radio.groups));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_node(dir: &tempfile::TempDir, name: &str) -> std::sync::Arc<ThrenodyNode> {
        ThrenodyNode::open(dir.path().join(name).display().to_string(), None, None).unwrap()
    }

    #[test]
    fn anonymous_identities_never_use_radios() {
        let dir = tempfile::tempdir().unwrap();
        let me = main_node(&dir, "me");
        let rec = me
            .create_persona("market".into(), None, Some("pw".into()))
            .unwrap();
        let p = ThrenodyNode::open(rec.home, Some("pw".into()), None).unwrap();
        let anon = |r: Result<()>| {
            let e = r.unwrap_err().to_string();
            assert!(e.contains("anonymous"), "{e}");
        };
        anon(p.start_bluetooth().map(|_| ()));
        anon(p.bluetooth_scan(1).map(|_| ()));
        anon(
            p.bluetooth_connect("00:11:22:33:44:55".into(), false, 129)
                .map(|_| ()),
        );
        anon(p.join_wifi_direct(
            me.device_fingerprint(),
            "DIRECT-th-abcd".into(),
            "abcdefgh2345".into(),
            "192.168.49.1:7450".into(),
        ));
        assert!(!p.bluetooth_running());
    }

    #[test]
    fn wifi_direct_join_checks_peer_and_offer_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = main_node(&dir, "a");
        let b = main_node(&dir, "b");
        let join = |peer: String, ssid: &str, pass: &str| {
            a.join_wifi_direct(peer, ssid.into(), pass.into(), "192.168.49.1:7450".into())
                .unwrap_err()
                .to_string()
        };
        let e = join("UNKNOWN".into(), "DIRECT-th-abcd", "abcdefgh2345");
        assert!(e.contains("no contact"), "{e}");
        let addr = b.listen("127.0.0.1:0".into()).unwrap();
        let b_fp = a.connect(b.invite_link(addr)).unwrap();
        let e = join(b_fp.clone(), "DIRECT-th-abcd", "short12");
        assert!(e.contains("passphrase"), "{e}");
        let e = join(b_fp, "HomeWifi", "abcdefgh2345");
        assert!(e.contains("DIRECT-"), "{e}");
        assert!(a.wifi_direct_joined().is_empty());
    }

    #[test]
    fn dropping_a_node_without_shutdown_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let a = main_node(&dir, "a");
        assert!(!a.bluetooth_running());
        assert!(a.bluetooth_error().is_none());
        assert!(a.wifi_direct_joined().is_empty());
        drop(a);
    }
}
