# Running an Always-On Relay Node

Two phones on mobile data, or behind home routers, often can't open a direct path to each other. When one of them is in the background, it may also not notice for many minutes that the other is looking for it (Appendix N). A node you run on a server with a public address fixes both: each phone keeps a session with it, and when no direct path opens, the phones reach each other through it as a relay circuit (Appendix G).

The relay is an ordinary Threnody identity that is a mutually approved contact of each phone. It is not linked to anyone's account, so it never gets your contacts or message history.

## What it can and can't see

- **It can't read anything.** Sessions through it are end-to-end encrypted between the phones, with their own handshake and ratchet. The relay forwards ciphertext.
- **It does see metadata:** which of its contacts talks to which, when, and how much. That is why you run it yourself.
- **Its address is in your invite to it.** Anyone with that invite can try to connect. Only contacts you approve can use it as a relay, and with `--policy approved` nobody else gets a session at all.

## What you need

- A small Linux server with a public IPv4 address (IPv6 too, if possible): 1 vCPU and 512 MB of memory are plenty. Pick a region near the people using it, since every relayed packet goes through it.
- TCP and UDP port 7450 open to the internet, in the provider's firewall as well as the server's.
- systemd 250 or newer, for encrypted credentials (any current distribution).

Traffic: each connected phone sends a padded frame every 10 seconds in each direction (cover traffic), roughly 46 MB a day per phone. A relayed voice call adds about 25 MB an hour, and video a few hundred MB an hour.

## 1. Install the binary

Download `threnody-<version>-x86_64-unknown-linux-gnu.tar.gz` (or `aarch64-…` for ARM servers) from the GitHub release, then:

```sh
tar xzf threnody-*-linux-gnu.tar.gz
sudo install -m 755 threnody-*/threnody /usr/local/bin/threnody
```

## 2. Create the service user and identity

```sh
sudo useradd --system --home-dir /var/lib/threnody --shell /usr/sbin/nologin threnody
sudo install -d -o threnody -g threnody -m 700 /var/lib/threnody
sudo -u threnody env THRENODY_HOME=/var/lib/threnody threnody init --passphrase
```

Servers have no desktop keyring, so the identity key is sealed with a passphrase instead. Store the same passphrase as an encrypted systemd credential. Only this machine's systemd can decrypt it, and the service gets it at start without it ever sitting in a plain file:

```sh
sudo mkdir -p /etc/credstore.encrypted
sudo systemd-creds encrypt --name=threnody-passphrase - /etc/credstore.encrypted/threnody-passphrase
# type the passphrase, Enter, then Ctrl-D (a trailing newline is ignored)
```

The node also reads a passphrase from `$THRENODY_PASSPHRASE_FILE`, if you'd rather manage the file yourself.

## 3. The service

`/etc/systemd/system/threnody.service`:

```ini
[Unit]
Description=Threnody always-on relay
After=network-online.target
Wants=network-online.target

[Service]
User=threnody
Group=threnody
Environment=THRENODY_HOME=/var/lib/threnody
LoadCredentialEncrypted=threnody-passphrase:/etc/credstore.encrypted/threnody-passphrase
ExecStart=/usr/local/bin/threnody run --daemon --listen [::]:7450 --policy approved \
    --constant-rate-ms 10000 --no-discover
Restart=on-failure
RestartSec=5

StateDirectory=threnody
StateDirectoryMode=0700
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
CapabilityBoundingSet=
UMask=0077

[Install]
WantedBy=multi-user.target
```

- `--daemon` runs without a terminal and takes commands over a control socket, `/var/lib/threnody/control.sock` (owner-only), through `threnody ctl`.
- `--listen [::]:7450` takes IPv6 and IPv4 on TCP and QUIC (UDP).
- `--policy approved` gives sessions only to contacts you approved (see step 4 for adding the first ones).
- `--constant-rate-ms 10000` keeps cover traffic on, but at the rate phones use on mobile data, so the relay doesn't cost them more than any other contact.
- `--no-discover`: there is no LAN to announce on.

The log (`journalctl -u threnody`) shows sessions and events, but never message text or the answers to `threnody ctl`.

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now threnody
sudo ufw allow 7450/tcp && sudo ufw allow 7450/udp   # or your firewall's equivalent
```

A shorthand for the commands below:

```sh
alias tctl='sudo -u threnody env THRENODY_HOME=/var/lib/threnody threnody ctl'
```

## 4. Add each phone

For each phone that should use the relay (yours, your partner's, any other device):

1. Let new devices in while you add them: `tctl /policy anyone`.
2. Print the relay's invite, with the server's public addresses (comma-separated, an IPv6 address in brackets and first):
   `sudo -u threnody env THRENODY_HOME=/var/lib/threnody threnody invite '[<public-ipv6>]:7450,<public-ip>:7450'`
   The link also carries a key (`?r=…`) naming a meeting place in the DHT, where the running node keeps its current addresses (Appendix S, *Invite links*), so the invite still works if the server's address changes. The running node reads a new key only when it starts, so after the first `threnody invite`, run `sudo systemctl restart threnody` once.
3. On the phone, scan the QR code (or open the `threnody://…` link). The relay shows up as a new contact.
4. On the server, find the phone and approve it:
   `tctl /contacts`, then `tctl /approve <fingerprint>`.
5. On the phone, open the relay's chat and tap **Approve**. Approval has to be mutual for relaying.
6. Compare safety numbers (`tctl /safety <fingerprint>` against the phone's) and mark it verified on both sides (`tctl /verify <fingerprint>`).
7. When every phone is in: `tctl /policy approved`.

`tctl /peers` lists live sessions; each phone should be there. `tctl /help` lists every command.

## 5. Use it on the phones

On each phone, open Settings › Network & Connectivity › **Always-on relay** and choose the relay contact. On the Linux app it's Settings › **Always-on relay**. The device then keeps a session with the relay at all times, in the background too, and redials it within seconds when the network changes. Calls and messages go through it whenever no direct path opens, and a call that starts through it moves to a direct path if the two devices can open one.

The setting's title says whether the relay is connected right now. On the server, `tctl /peers` lists every device that is.

## Updating

Replace `/usr/local/bin/threnody` with the new release's binary and run `sudo systemctl restart threnody`. Phones reconnect within seconds.
