//! `threnody`: command-line client for the Threnody protocol.

#[cfg(all(feature = "ble", target_os = "linux"))]
mod ble;
mod chat;
mod groups;
mod target;
mod tunnel;

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use threnody_core::store::{Contact, Home, Lookup};
use threnody_core::{Identity, safety_number};
use threnody_net::AcceptPolicy;

#[derive(Parser)]
#[command(
    name = "threnody",
    version,
    about = "Encrypted, metadata-resistant messaging"
)]
struct Cli {
    /// Data directory (default: $THRENODY_HOME, else the platform data dir).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create this device's identity.
    Init {
        /// Replace an existing identity. Contacts will no longer recognise you.
        #[arg(long)]
        force: bool,
        /// Protect the identity key with a passphrase (Argon2id).
        #[arg(long)]
        passphrase: bool,
    },
    /// Add, change or remove the identity passphrase.
    Passphrase {
        /// Store the identity without a passphrase.
        #[arg(long)]
        remove: bool,
    },
    /// Show this device's fingerprint and its account.
    Id,
    /// Join an existing account using a link code from one of its devices.
    Link {
        /// The `threnody-link://…` code shown by `/device add`.
        code: String,
    },
    /// Print an invite link (and QR code) others can connect with.
    Invite {
        /// Address peers should dial, e.g. 192.0.2.7:7450.
        addr: String,
        /// Skip the QR code.
        #[arg(long)]
        no_qr: bool,
    },
    /// List contacts.
    Contacts,
    /// Give a contact a local name.
    Name { peer: String, name: String },
    /// Approve a contact for mesh / tunnel participation (spec §5.2).
    Approve { peer: String },
    /// Revoke a contact's approval.
    Revoke { peer: String },
    /// Compare safety numbers and mark a contact verified.
    Verify { peer: String },
    /// Delete a contact.
    Forget { peer: String },
    /// Run the node: listen, connect, and chat interactively.
    Run {
        /// Address to listen on.
        #[arg(long, default_value = "0.0.0.0:7450")]
        listen: String,
        /// Do not listen for inbound connections.
        #[arg(long)]
        no_listen: bool,
        /// Peers to dial at start: invite link, contact name/fingerprint, or host:port.
        #[arg(long = "connect", short = 'c')]
        connect: Vec<String>,
        /// Who may connect to us.
        #[arg(long, value_enum, default_value_t = Policy::Anyone)]
        policy: Policy,
        /// Send one padded frame per interval per session, with cover traffic
        /// when idle. Hides message timing at a bandwidth cost.
        #[arg(long, value_name = "MS")]
        constant_rate_ms: Option<u64>,
        /// Do not send or listen for LAN discovery beacons.
        #[arg(long)]
        no_discover: bool,
        /// UDP port for LAN discovery beacons (multicast 239.255.84.86).
        #[arg(long, default_value_t = threnody_net::discovery::DEFAULT_PORT)]
        discover_port: u16,
        /// Build WireGuard tunnels to mutually approved peers on this UDP port.
        #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "51820")]
        tunnel: Option<u16>,
        /// WireGuard interface name.
        #[arg(long, default_value = "thr0")]
        wg_iface: String,
        /// Push peer changes to the running interface with `wg set`
        /// (needs CAP_NET_ADMIN; bring the interface up with wg-quick first).
        #[arg(long, requires = "tunnel")]
        wg_apply: bool,
    },
    /// Show this device's WireGuard public key and overlay address.
    Tunnel,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Policy {
    /// Anyone who authenticates (trust on first use).
    Anyone,
    /// Only known contacts.
    Contacts,
    /// Only mutually approved contacts.
    Approved,
}

impl From<Policy> for AcceptPolicy {
    fn from(p: Policy) -> Self {
        match p {
            Policy::Anyone => Self::Anyone,
            Policy::Contacts => Self::ContactsOnly,
            Policy::Approved => Self::ApprovedOnly,
        }
    }
}

fn default_home() -> PathBuf {
    if let Some(h) = std::env::var_os("THRENODY_HOME") {
        return h.into();
    }
    if cfg!(windows)
        && let Some(a) = std::env::var_os("APPDATA")
    {
        return PathBuf::from(a).join("Threnody");
    }
    if cfg!(target_os = "macos")
        && let Some(h) = std::env::var_os("HOME")
    {
        return PathBuf::from(h).join("Library/Application Support/Threnody");
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("threnody");
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".local/share/threnody")
}

pub fn load_identity(home: &Home) -> Result<Identity> {
    if !home.has_identity() {
        bail!(
            "no identity in {}; run `threnody init` first",
            home.dir().display()
        );
    }
    if !home.identity_is_sealed()? {
        return home.load_identity(None).context("loading identity");
    }
    let pw = read_passphrase("Passphrase: ")?;
    match home.load_identity(Some(pw.as_bytes())) {
        Err(threnody_core::Error::Decrypt) => bail!("wrong passphrase"),
        r => r.context("loading identity"),
    }
}

/// Reads a passphrase from `$THRENODY_PASSPHRASE` or the terminal.
fn read_passphrase(prompt: &str) -> Result<zeroize_string::Secret> {
    if let Ok(p) = std::env::var("THRENODY_PASSPHRASE") {
        return Ok(zeroize_string::Secret(p));
    }
    Ok(zeroize_string::Secret(rpassword::prompt_password(prompt)?))
}

/// Asks for a new passphrase twice.
fn new_passphrase() -> Result<zeroize_string::Secret> {
    let a = read_passphrase("New passphrase: ")?;
    if std::env::var_os("THRENODY_PASSPHRASE").is_none() {
        let b = read_passphrase("Repeat passphrase: ")?;
        if a.as_bytes() != b.as_bytes() {
            bail!("passphrases do not match");
        }
    }
    if a.as_bytes().is_empty() {
        bail!("empty passphrase; use --remove to store the key unprotected");
    }
    Ok(a)
}

mod zeroize_string {
    /// A passphrase string that is wiped from memory on drop.
    pub struct Secret(pub String);

    impl Secret {
        pub fn as_bytes(&self) -> &[u8] {
            self.0.as_bytes()
        }
    }

    impl Drop for Secret {
        fn drop(&mut self) {
            zeroize::Zeroize::zeroize(&mut self.0);
        }
    }
}

/// Resolves a contact by name or fingerprint prefix.
pub fn find_contact<'a>(
    contacts: &'a threnody_core::store::Contacts,
    q: &str,
) -> Result<&'a Contact> {
    match contacts.find(q) {
        Lookup::Found(c) => Ok(c),
        Lookup::None => bail!("no contact matches {q:?}"),
        Lookup::Ambiguous(v) => {
            let names: Vec<_> = v.iter().map(|c| c.label()).collect();
            bail!("{q:?} is ambiguous: {}", names.join(", "))
        }
    }
}

pub fn describe_contact(c: &Contact) -> String {
    let approval = match (c.local_approved, c.remote_approved) {
        (true, true) => "mutually approved",
        (true, false) => "approved by you",
        (false, true) => "approved by them",
        (false, false) => "not approved",
    };
    let verified = if c.verified { "verified" } else { "unverified" };
    let addr = c.last_addr.as_deref().unwrap_or("-");
    format!("{:<40} {approval}, {verified}, last addr {addr}", c.label())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = Home::new(cli.home.unwrap_or_else(default_home));
    match cli.cmd {
        Cmd::Init { force, passphrase } => {
            if home.has_identity() && !force {
                bail!(
                    "identity already exists in {} (use --force to replace)",
                    home.dir().display()
                );
            }
            let pw = passphrase.then(new_passphrase).transpose()?;
            let id = home.create_identity(pw.as_ref().map(|p| p.as_bytes()))?;
            println!("Created identity in {}", home.dir().display());
            println!("Fingerprint: {}", id.public().fingerprint());
        }
        Cmd::Passphrase { remove } => {
            let current = if home.identity_is_sealed()? {
                Some(read_passphrase("Current passphrase: ")?)
            } else {
                None
            };
            let new = if remove {
                None
            } else {
                Some(new_passphrase()?)
            };
            match home.change_passphrase(
                current.as_ref().map(|p| p.as_bytes()),
                new.as_ref().map(|p| p.as_bytes()),
            ) {
                Err(threnody_core::Error::Decrypt) => bail!("wrong passphrase"),
                r => r?,
            }
            println!(
                "{}",
                if remove {
                    "Passphrase removed."
                } else {
                    "Passphrase set."
                }
            );
        }
        Cmd::Tunnel => {
            let id = load_identity(&home)?;
            let keys = threnody_core::tunnel::WgKeys::derive(&id);
            println!("WireGuard public key  {}", keys.public_base64());
            println!(
                "Overlay address       {}",
                threnody_core::tunnel::overlay_addr(&id.public())
            );
            let p = threnody_core::tunnel::overlay_prefix();
            println!(
                "Overlay network       {:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}::/48",
                p[0], p[1], p[2], p[3], p[4], p[5]
            );
        }
        Cmd::Id => {
            let id = load_identity(&home)?;
            println!("{}", id.public().fingerprint());
            if let Some(b) = home.load_state(&id, "account")?
                && let Ok(chain) = threnody_core::account::AccountChain::decode(&b)
            {
                println!(
                    "account {} ({} device(s))",
                    chain.id().fingerprint(),
                    chain.state().devices.len()
                );
            }
        }
        Cmd::Link { code } => {
            let code: threnody_core::account::LinkCode =
                code.parse().context("parsing link code")?;
            let identity = load_identity(&home)?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let (node, mut events) = threnody_net::Node::new(threnody_net::NodeConfig {
                    home,
                    identity,
                    policy: threnody_net::AcceptPolicy::Anyone,
                    constant_rate: None,
                    tunnel_port: None,
                })?;
                println!("Linking with {} at {}…", code.device, code.addr);
                let account = node.link_with(&code).await?;
                println!("Joined account {}", account.fingerprint());
                // Give the existing device a moment to send our contacts.
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while let Some(e) = events.recv().await {
                        if matches!(e, threnody_net::Event::ContactsSynced { .. }) {
                            println!("Contacts synced.");
                            break;
                        }
                    }
                })
                .await;
                node.shutdown();
                anyhow::Ok(())
            })?;
        }
        Cmd::Invite { addr, no_qr } => {
            let id = load_identity(&home)?;
            let link = target::invite_link(&id.public().fingerprint(), &addr);
            println!("{link}");
            if !no_qr {
                let code = qrcode::QrCode::new(link.as_bytes())?;
                let art = code
                    .render::<qrcode::render::unicode::Dense1x2>()
                    .dark_color(qrcode::render::unicode::Dense1x2::Light)
                    .light_color(qrcode::render::unicode::Dense1x2::Dark)
                    .build();
                println!("{art}");
            }
        }
        Cmd::Contacts => {
            let contacts = home.load_contacts()?;
            let mut any = false;
            for c in contacts.iter() {
                any = true;
                println!("{}", describe_contact(c));
            }
            if !any {
                println!("No contacts yet.");
            }
        }
        Cmd::Name { peer, name } => edit_contact(&home, &peer, |c| c.petname = Some(name))?,
        Cmd::Approve { peer } => {
            edit_contact(&home, &peer, |c| c.local_approved = true)?;
            println!("Approved. The peer learns this at your next session.");
        }
        Cmd::Revoke { peer } => {
            edit_contact(&home, &peer, |c| c.local_approved = false)?;
            println!("Revoked. The peer learns this at your next session.");
        }
        Cmd::Verify { peer } => {
            let id = load_identity(&home)?;
            let contacts = home.load_contacts()?;
            let c = find_contact(&contacts, &peer)?;
            println!(
                "Safety number with {}:\n\n  {}\n",
                c.label(),
                safety_number(&id.public(), &c.key)
            );
            print!("Does it match what your contact sees? [y/N] ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let ok = line.trim().eq_ignore_ascii_case("y");
            edit_contact(&home, &peer, |c| c.verified = ok)?;
            println!(
                "{}",
                if ok {
                    "Marked verified."
                } else {
                    "Not verified."
                }
            );
        }
        Cmd::Forget { peer } => {
            let mut contacts = home.load_contacts()?;
            let key = find_contact(&contacts, &peer)?.key;
            contacts.remove(&key);
            home.save_contacts(&contacts)?;
            println!("Forgot {}", key.fingerprint());
        }
        Cmd::Run {
            listen,
            no_listen,
            connect,
            policy,
            constant_rate_ms,
            tunnel,
            wg_iface,
            wg_apply,
            no_discover,
            discover_port,
        } => {
            let identity = load_identity(&home)?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(chat::run(chat::Options {
                home,
                identity,
                listen: (!no_listen).then_some(listen),
                connect,
                policy: policy.into(),
                constant_rate: constant_rate_ms.map(std::time::Duration::from_millis),
                discover: (!no_discover).then_some(discover_port),
                tunnel: tunnel.map(|port| chat::TunnelOptions {
                    port,
                    iface: wg_iface,
                    apply: wg_apply,
                }),
            }))?;
        }
    }
    Ok(())
}

fn edit_contact(home: &Home, peer: &str, f: impl FnOnce(&mut Contact)) -> Result<()> {
    let mut contacts = home.load_contacts()?;
    let key = find_contact(&contacts, peer)?.key;
    if let Some(c) = contacts.get_mut(&key) {
        f(c);
    }
    home.save_contacts(&contacts)?;
    Ok(())
}
