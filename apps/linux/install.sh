#!/usr/bin/env bash
# Builds Threnody for the Linux desktop and installs it for this user:
# the binary in ~/.local/bin, a launcher in the app menu, its icons, and
# the handler for threnody:// invite links.
#
# Closing Threnody's window keeps it running, so an old copy may still be
# up: it is asked to quit before the new binary goes in, and started again
# afterwards.
#
#   apps/linux/install.sh              # build (release) and install
#   apps/linux/install.sh --uninstall  # remove what it installed
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
id=org.threnody.Threnody
exe="$bin/threnody-desktop"

# Copies running from the installed binary (also once it was replaced: the
# kernel then reports the path with " (deleted)"). Copies run from a source
# tree, or under another app id, are left alone.
running() {
    local p path
    for p in $(pgrep -x threnody-deskto || true); do
        path=$(readlink "/proc/$p/exe" 2>/dev/null || true)
        [[ "${path% (deleted)}" == "$exe" ]] && echo "$p"
    done
    return 0
}

# Asks the running copy to quit (its own `quit` action, so the node shuts
# down cleanly), then signals it if it hasn't after 5 seconds.
# Returns 0 if one was running.
stop_running() {
    [[ -z "$(running)" ]] && return 1
    echo "Stopping the running Threnody…"
    gdbus call --session --dest "$id" --object-path "/${id//./\/}" \
        --method org.gtk.Actions.Activate quit '[]' '{}' >/dev/null 2>&1 || true
    for _ in $(seq 50); do
        [[ -z "$(running)" ]] && return 0
        sleep 0.1
    done
    # shellcheck disable=SC2046
    kill $(running) 2>/dev/null || true
    for _ in $(seq 30); do
        [[ -z "$(running)" ]] && return 0
        sleep 0.1
    done
    echo "Threnody didn't stop; quit it (Ctrl+Q) and run this again." >&2
    exit 1
}

if [[ "${1:-}" == "--uninstall" ]]; then
    stop_running || true
    rm -f "$exe" \
        "$data/applications/$id.desktop" \
        "$data/icons/hicolor/scalable/apps/$id.svg" \
        "$data/icons/hicolor/symbolic/apps/threnody-qr-symbolic.svg"
    update-desktop-database -q "$data/applications" 2>/dev/null || true
    echo "Removed. Your identity and messages are untouched."
    exit 0
fi

# Build first: the old copy keeps running meanwhile.
cargo build --release --locked --manifest-path "$root/Cargo.toml" -p threnody-desktop

was_running=false
if stop_running; then
    was_running=true
fi
install -Dm755 "$root/target/release/threnody-desktop" "$exe"
install -Dm644 "$here/data/$id.desktop" "$data/applications/$id.desktop"
install -Dm644 "$here/data/icons/hicolor/scalable/apps/$id.svg" "$data/icons/hicolor/scalable/apps/$id.svg"
install -Dm644 "$here/data/icons/hicolor/symbolic/apps/threnody-qr-symbolic.svg" \
    "$data/icons/hicolor/symbolic/apps/threnody-qr-symbolic.svg"
update-desktop-database -q "$data/applications" 2>/dev/null || true
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
xdg-mime default "$id.desktop" x-scheme-handler/threnody x-scheme-handler/threnody-link 2>/dev/null || true

if $was_running; then
    setsid -f "$exe" >/dev/null 2>&1 </dev/null
    echo "Installed and restarted $exe."
else
    echo "Installed $exe. Start Threnody from your app menu."
fi
