#!/usr/bin/env bash
# Builds Threnody for the Linux desktop and installs it for this user:
# the binary in ~/.local/bin, a launcher in the app menu, its icons, and
# the handler for threnody:// invite links.
#
#   apps/linux/install.sh              # build (release) and install
#   apps/linux/install.sh --uninstall  # remove what it installed
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
id=org.threnody.Threnody

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f "$bin/threnody-desktop" \
        "$data/applications/$id.desktop" \
        "$data/icons/hicolor/scalable/apps/$id.svg" \
        "$data/icons/hicolor/symbolic/apps/threnody-qr-symbolic.svg"
    update-desktop-database -q "$data/applications" 2>/dev/null || true
    echo "Removed. Your identity and messages are untouched."
    exit 0
fi

cargo build --release --locked --manifest-path "$root/Cargo.toml" -p threnody-desktop
install -Dm755 "$root/target/release/threnody-desktop" "$bin/threnody-desktop"
install -Dm644 "$here/data/$id.desktop" "$data/applications/$id.desktop"
install -Dm644 "$here/data/icons/hicolor/scalable/apps/$id.svg" "$data/icons/hicolor/scalable/apps/$id.svg"
install -Dm644 "$here/data/icons/hicolor/symbolic/apps/threnody-qr-symbolic.svg" \
    "$data/icons/hicolor/symbolic/apps/threnody-qr-symbolic.svg"
update-desktop-database -q "$data/applications" 2>/dev/null || true
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
xdg-mime default "$id.desktop" x-scheme-handler/threnody x-scheme-handler/threnody-link 2>/dev/null || true

echo "Installed $bin/threnody-desktop. Start Threnody from your app menu."
