//! Joining a peer's Wi-Fi Direct group through NetworkManager (Appendix L).
//!
//! Linux joins as a client of the group the peer (typically a phone)
//! created. NetworkManager connects a Wi-Fi interface to the group's
//! network with the passphrase the peer sent over the encrypted session;
//! hosting a group would need root access to wpa_supplicant. The profile
//! is temporary: no autoconnect, no default route, removed on leave.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use threnody_net::DirectOffer;
use tokio::process::Command;

/// NetworkManager connection-profile name for a group.
pub fn profile(offer: &DirectOffer) -> String {
    format!("threnody-{}", offer.ssid)
}

async fn nmcli(args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new("nmcli")
            .args(args)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow!("nmcli timed out"))?
    .context("running nmcli (is NetworkManager installed?)")?;
    if !out.status.success() {
        bail!(
            "nmcli {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Joins the group; returns the profile name to pass to [`leave`].
/// `scratch` is a private directory for the short-lived password file
/// (the passphrase never appears on a command line).
pub async fn join(offer: &DirectOffer, scratch: &Path) -> Result<String> {
    let name = profile(offer);
    let _ = nmcli(&["connection", "delete", &name]).await;
    nmcli(&[
        "connection",
        "add",
        "type",
        "wifi",
        "con-name",
        &name,
        "ssid",
        &offer.ssid,
        "wifi-sec.key-mgmt",
        "wpa-psk",
        "wifi-sec.psk-flags",
        "2", // not saved: supplied at activation
        "connection.autoconnect",
        "no",
        "ipv4.method",
        "auto",
        "ipv4.never-default",
        "yes",
        "ipv4.ignore-auto-dns",
        "yes",
        "ipv6.method",
        "disabled",
    ])
    .await?;
    let pw = scratch.join(format!(".{name}.pw"));
    write_private(
        &pw,
        &format!("802-11-wireless-security.psk:{}\n", offer.passphrase),
    )?;
    let up = nmcli(&[
        "--wait",
        "45",
        "connection",
        "up",
        "id",
        &name,
        "passwd-file",
        &pw.to_string_lossy(),
    ])
    .await;
    let _ = std::fs::remove_file(&pw);
    if let Err(e) = up {
        let _ = nmcli(&["connection", "delete", &name]).await;
        return Err(e);
    }
    Ok(name)
}

/// Leaves the group and forgets the profile; NetworkManager then returns
/// the interface to its usual network.
pub async fn leave(name: &str) -> Result<()> {
    nmcli(&["connection", "delete", name]).await.map(|_| ())
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}
