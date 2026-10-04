//! Bluetooth LE transport via BlueZ: find Threnody devices advertising
//! their L2CAP channel, and connect to them. The session over the channel
//! is the ordinary Threnody handshake and ratchet.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bluer::l2cap::{SocketAddr, Stream};
use bluer::{AdapterEvent, Address, AddressType};
use futures_util::StreamExt;
use threnody_core::PublicIdentity;
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
                let Some(psm) = data.get(&uuid).and_then(|d| d.get(..2)).map(|b| u16::from_le_bytes([b[0], b[1]])) else { continue };
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

/// Opens the device's L2CAP channel and runs a Threnody session over it.
pub async fn connect(node: &Node, f: &Found) -> Result<PublicIdentity> {
    let target = SocketAddr::new(f.addr, f.addr_type, f.psm);
    let stream = tokio::time::timeout(Duration::from_secs(20), Stream::connect(target))
        .await
        .map_err(|_| anyhow!("Bluetooth connection timed out"))?
        .with_context(|| format!("L2CAP connect to {} psm {}", f.addr, f.psm))?;
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
    Ok(node
        .connect_stream(stream, "ble", f.addr.to_string(), None)
        .await?)
}
