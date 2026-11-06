//! Linux radio transports (Appendix K, L), shared by the CLI and the
//! desktop app (through `threnody-ffi`): Bluetooth LE through BlueZ, and
//! joining Wi-Fi Direct groups through NetworkManager.

#[cfg(all(feature = "ble", target_os = "linux"))]
pub mod ble;
pub mod wifidirect;

/// Where background tasks report trouble: one line, no "! " prefix.
pub type Log = std::sync::Arc<dyn Fn(String) + Send + Sync>;
