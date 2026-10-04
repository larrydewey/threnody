//! Bluetooth LE transport via BlueZ: advertise our L2CAP channel inside a
//! private beacon (Appendix E, K), find other Threnody devices, and connect
//! to them, automatically for approved contacts. The session over the
//! channel is the ordinary Threnody handshake and ratchet.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bluer::adv::{Advertisement, SecondaryChannel};
use bluer::l2cap::{SocketAddr, Stream, StreamListener};
use bluer::{AdapterEvent, Address, AddressType};
use futures_util::StreamExt;
use threnody_core::PublicIdentity;
use threnody_core::discovery::{EPOCH_SECS, beacon_port};
use threnody_net::Node;
use threnody_net::discovery::BLE_SERVICE_UUID;

/// A Threnody device seen over Bluetooth LE.
#[derive(Clone, Debug)]
pub struct Found {
    pub addr: Address,
    pub addr_type: AddressType,
    pub psm: u16,
    pub rssi: Option<i16>,
    pub name: Option<String>,
}

/// Scans for `secs` seconds and returns every advertising Threnody device.
pub async fn scan(secs: u64) -> Result<Vec<Found>> {
    let uuid: bluer::Uuid = BLE_SERVICE_UUID.parse()?;
    let session = bluer::Session::new().await.context("connecting to BlueZ")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter")?;
    adapter.set_powered(true).await.ok();
    let mut found: HashMap<Address, Found> = HashMap::new();
    let events = adapter.discover_devices().await?;
    let mut events = Box::pin(events);
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            ev = events.next() => {
                let Some(AdapterEvent::DeviceAdded(addr)) = ev else { continue };
                let Ok(dev) = adapter.device(addr) else { continue };
                let Ok(Some(data)) = dev.service_data().await else { continue };
                let Some(psm) = data.get(&uuid).and_then(|d| advert_psm(d)) else { continue };
                found.insert(addr, Found {
                    addr,
                    addr_type: dev.address_type().await.unwrap_or(AddressType::LeRandom),
                    psm,
                    rssi: dev.rssi().await.ok().flatten(),
                    name: dev.name().await.ok().flatten(),
                });
            }
        }
    }
    // BlueZ also reports devices it cached earlier (possibly with a stale
    // PSM); only those heard during this scan have a signal reading.
    let mut list: Vec<Found> = found.into_values().filter(|f| f.rssi.is_some()).collect();
    list.sort_by_key(|f| std::cmp::Reverse(f.rssi));
    Ok(list)
}

/// Largest L2CAP SDU we send. Android delivers an SDU to the app only once
/// it is complete and stalls on large ones (a 33 KB SDU never arrived on a
/// Pixel 8a; 10 KB did), so writes are cut to this size.
pub const MAX_SDU: usize = 4096;
/// The receive MTU we offer, so peers can send us SDUs as large as ours
/// (the default, 672 bytes, slows phone-to-laptop transfers).
const RECV_MTU: u16 = MAX_SDU as u16;

/// An L2CAP stream whose writes never exceed [`MAX_SDU`].
pub struct SduLimit<S>(S);

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for SduLimit<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for SduLimit<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let n = buf.len().min(MAX_SDU);
        std::pin::Pin::new(&mut self.0).poll_write(cx, &buf[..n])
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

type Ours = std::sync::Mutex<HashMap<Address, std::time::Instant>>;

/// Devices we have opened Threnody channels with over Bluetooth, and when.
fn ours() -> &'static Ours {
    static OURS: std::sync::OnceLock<Ours> = std::sync::OnceLock::new();
    OURS.get_or_init(Default::default)
}

/// A fresh channel may still be in its handshake; leave it alone this long.
const LINK_GRACE: Duration = Duration::from_secs(30);

fn remember(addr: Address) {
    ours()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(addr, std::time::Instant::now());
}

/// Drops LE links to our peers that no longer carry a session. A lingering
/// link (left by a peer whose app was killed) can stop the controller from
/// advertising, so nobody could reconnect. Only touches devices we ran
/// Threnody sessions with; other Bluetooth devices are left alone.
async fn sweep_links(node: &Node, adapter: &bluer::Adapter) {
    let addrs: Vec<Address> = ours()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(_, t)| t.elapsed() > LINK_GRACE)
        .map(|(a, _)| *a)
        .collect();
    let live: Vec<String> = node
        .sessions()
        .into_iter()
        .filter(|s| s.transport == "ble")
        .map(|s| s.remote)
        .collect();
    for addr in addrs {
        if live.contains(&addr.to_string()) {
            continue;
        }
        let Ok(dev) = adapter.device(addr) else {
            continue;
        };
        if dev.is_connected().await.unwrap_or(false) {
            let _ = dev.disconnect().await;
        }
        ours()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&addr);
    }
}

/// Aborts a pending LE connection to `addr` (BlueZ `Device.Disconnect`
/// also cancels one that is still being made).
async fn cancel_connect(addr: Address) {
    let Ok(session) = bluer::Session::new().await else {
        return;
    };
    let Ok(adapter) = session.default_adapter().await else {
        return;
    };
    if let Ok(dev) = adapter.device(addr) {
        let _ = dev.disconnect().await;
    }
}

/// How often the advertised beacon is replaced: well inside one discovery
/// epoch, so peers always see a current tag and a fresh nonce.
const REFRESH: Duration = Duration::from_secs(EPOCH_SECS / 5);

/// The PSM in a Threnody advert: a beacon's port field.
fn advert_psm(data: &[u8]) -> Option<u16> {
    beacon_port(data)
}

/// Opens the device's L2CAP channel and runs a Threnody session over it.
/// `expect` pins the peer's fingerprint.
pub async fn connect(
    node: &Node,
    f: &Found,
    expect: Option<threnody_core::Fingerprint>,
) -> Result<PublicIdentity> {
    let target = SocketAddr::new(f.addr, f.addr_type, f.psm);
    let socket = bluer::l2cap::Socket::<Stream>::new_stream()?;
    socket.set_recv_mtu(RECV_MTU).ok();
    socket.bind(SocketAddr::new(Address::any(), f.addr_type, 0))?;
    let stream = match tokio::time::timeout(Duration::from_secs(20), socket.connect(target)).await {
        Ok(r) => r.with_context(|| format!("L2CAP connect to {} psm {}", f.addr, f.psm))?,
        Err(_) => {
            // Dropping the socket should cancel the LE connection attempt,
            // but some controllers (seen on MediaTek) stay "initiating" and
            // then refuse to scan (EBUSY) until power-cycled. Cancel it
            // through BlueZ too.
            cancel_connect(f.addr).await;
            return Err(anyhow!("Bluetooth connection timed out"));
        }
    };
    // The kernel can report a non-blocking LE connect as done before the
    // channel exists (writes then fail with ENOTCONN). Wait until the
    // channel is really up: its send MTU is only known once connected.
    let started = std::time::Instant::now();
    while stream.as_ref().send_mtu().is_err() {
        if started.elapsed() > Duration::from_secs(20) {
            return Err(anyhow!("Bluetooth channel to {} never came up", f.addr));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    remember(f.addr);
    Ok(node
        .connect_stream(SduLimit(stream), "ble", f.addr.to_string(), expect)
        .await?)
}

/// Keeps advertising, accepting and auto-dialing for as long as it lives.
pub struct Listening {
    pub psm: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Listens on an LE L2CAP channel, advertises its PSM inside a private
/// beacon under the Threnody UUID, accepts sessions on it, and dials
/// approved contacts whose beacons we hear.
pub async fn listen(node: &Node) -> Result<Listening> {
    let session = bluer::Session::new().await.context("connecting to BlueZ")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter")?;
    adapter.set_powered(true).await.ok();
    let listener = StreamListener::bind(SocketAddr::new(Address::any(), AddressType::LePublic, 0))
        .await
        .context("binding an LE L2CAP channel")?;
    listener.as_ref().set_recv_mtu(RECV_MTU).ok();
    let psm = listener.as_ref().local_addr()?.psm;
    let uuid: bluer::Uuid = BLE_SERVICE_UUID.parse()?;
    let advert = move |node: &Node| Advertisement {
        advertisement_type: bluer::adv::Type::Peripheral,
        service_data: [(uuid, node.ble_beacon(psm))].into(),
        // The beacon is too big for a legacy advert; this makes BlueZ use
        // extended advertising.
        secondary_channel: Some(SecondaryChannel::OneM),
        ..Default::default()
    };
    // Fail early (and visibly) if the controller can't advertise.
    let first = adapter
        .advertise(advert(node))
        .await
        .context("starting the BLE advertisement")?;

    let mut tasks = Vec::new();
    let (n, a) = (node.clone(), adapter.clone());
    tasks.push(tokio::spawn(async move {
        let _session = session;
        let mut _advertising = first;
        loop {
            tokio::time::sleep(REFRESH).await;
            // Register the new beacon before dropping the old one.
            match a.advertise(advert(&n)).await {
                Ok(h) => _advertising = h,
                Err(e) => eprintln!("! Bluetooth advert refresh: {e}"),
            }
        }
    }));

    let n = node.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            let Ok((stream, sa)) = listener.accept().await else {
                break;
            };
            remember(sa.addr);
            let node = n.clone();
            tokio::spawn(async move {
                let _ = node
                    .accept_stream(SduLimit(stream), "ble", sa.addr.to_string())
                    .await;
            });
        }
    }));

    let (n, a) = (node.clone(), adapter.clone());
    tasks.push(tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            sweep_links(&n, &a).await;
        }
    }));

    // Scanning can fail to start (BlueZ may still be finishing a previous
    // scan, say after a restart) or stop; keep trying, backing off.
    let n = node.clone();
    tasks.push(tokio::spawn(async move {
        let mut wait = Duration::from_secs(2);
        let mut failing = false;
        let mut failures = 0u32;
        loop {
            let started = std::time::Instant::now();
            let r = auto_dial(&n, &adapter, uuid).await;
            if started.elapsed() > Duration::from_secs(60) {
                // It ran for a while: report a new failure, retry soon.
                wait = Duration::from_secs(2);
                failing = false;
            }
            match r {
                Err(e) if !failing => {
                    eprintln!("! Bluetooth scanning stopped ({e:#}); retrying");
                    failing = true;
                    failures = 1;
                }
                Err(_) => {
                    failures += 1;
                    // Seen with a MediaTek controller: BlueZ answers
                    // "InProgress" until the adapter is power-cycled. That
                    // would drop the user's other devices, so only say how.
                    if failures == 5 {
                        eprintln!(
                            "! Bluetooth still can't scan; contacts can still reach this laptop. \
                             If it persists: bluetoothctl power off && bluetoothctl power on"
                        );
                    }
                }
                Ok(()) => failing = false,
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    }));
    Ok(Listening { psm, tasks })
}

/// Scans continuously; dials approved contacts recognised from their
/// beacons (the smaller key dials, as on the LAN).
async fn auto_dial(node: &Node, adapter: &bluer::Adapter, uuid: bluer::Uuid) -> Result<()> {
    adapter
        .set_discovery_filter(bluer::DiscoveryFilter {
            transport: bluer::DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await
        .ok();
    let events = adapter.discover_devices_with_changes().await?;
    let mut events = Box::pin(events);
    // Devices are re-reported on every advert; look at each one at most
    // every few seconds.
    let mut last: HashMap<Address, std::time::Instant> = HashMap::new();
    while let Some(ev) = events.next().await {
        let AdapterEvent::DeviceAdded(addr) = ev else {
            continue;
        };
        if last
            .get(&addr)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            continue;
        }
        let Ok(dev) = adapter.device(addr) else {
            continue;
        };
        let Ok(Some(mut data)) = dev.service_data().await else {
            continue;
        };
        let Some(adv) = data.remove(&uuid) else {
            continue;
        };
        last.insert(addr, std::time::Instant::now());
        let Some((peer, psm)) = node.ble_heard(&adv) else {
            continue;
        };
        let f = Found {
            addr,
            addr_type: dev.address_type().await.unwrap_or(AddressType::LeRandom),
            psm,
            rssi: None,
            name: None,
        };
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(e) = connect(&node, &f, Some(peer.fingerprint())).await {
                eprintln!("! Bluetooth auto-connect to {}: {e:#}", peer.fingerprint());
            }
        });
    }
    Ok(())
}
