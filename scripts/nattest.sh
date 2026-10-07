#!/usr/bin/env bash
# Two-phone NAT test topology (Appendix N, "Two phones" item).
#
# Two probe nodes, each in its own netns, behind the host's NAT:
# A-subnet traffic is SNAT'd to 198.51.100.2, B-subnet to 203.0.113.2.
#   plain   = port-preserving masquerade (home-router-like)
#   random  = random per-flow external port (carrier-NAT-like)
# A private DHT testnet runs on 198.51.100.1:7460/7461 in the host ns.
#
#   sudo ./scripts/nattest.sh plain plain   # expect direct QUIC
#   sudo ./scripts/nattest.sh random random # expect symmetric NAT, no direct
set -euo pipefail

BIN="$(dirname "$0")/../target/debug/examples/probe"
[ -x "$BIN" ] || cargo build -p threnody-net --example probe

A_MODE="${1:-plain}"
B_MODE="${2:-plain}"
TMP="$(mktemp -d)"

cleanup() {
    set +e
    kill $(jobs -p) 2>/dev/null
    ip netns del A; ip netns del B
    ip addr del 198.51.100.1/24 dev lo
    ip addr del 203.0.113.1/24 dev lo
    iptables -t nat -D POSTROUTING -s 10.0.1.0/24 -o lo -j SNAT --to-source 198.51.100.2 $([ "$A_MODE" = random ] && echo --random)
    iptables -t nat -D POSTROUTING -s 10.0.2.0/24 -o lo -j SNAT --to-source 203.0.113.2 $([ "$B_MODE" = random ] && echo --random)
    rm -rf "$TMP"
}
trap cleanup EXIT

mkmode() { [ "$1" = random ] && echo "--random" || echo ""; }

ip netns add A
ip netns add B
ip link add vethA type veth peer name vethzA
ip link add vethB type veth peer name vethzB
ip link set vethzA netns A
ip link set vethzB netns B
# A side
ip addr add 10.0.1.1/24 dev vethA
ip link set vethA up
ip netns exec A ip addr add 10.0.1.2/24 dev vethzA
ip netns exec A ip link set vethzA up
ip netns exec A ip link set lo up
ip netns exec A ip route add default via 10.0.1.1
# B side
ip addr add 10.0.2.1/24 dev vethB
ip link set vethB up
ip netns exec B ip addr add 10.0.2.2/24 dev vethzB
ip netns exec B ip link set vethzB up
ip netns exec B ip link set lo up
ip netns exec B ip route add default via 10.0.2.1
echo 1 > /proc/sys/net/ipv4/ip_forward
# "Internet": loopback aliases; DHT testnet inside host netns.
ip addr add 198.51.100.1/24 dev lo
ip addr add 203.0.113.1/24 dev lo

iptables -t nat -A POSTROUTING -s 10.0.1.0/24 -o lo -j SNAT --to-source 198.51.100.2 $(mkmode "$A_MODE")
iptables -t nat -A POSTROUTING -s 10.0.2.0/24 -o lo -j SNAT --to-source 203.0.113.2 $(mkmode "$B_MODE")

# Private DHT testnet (two nodes, second seeded from the first).
"$BIN" --serve-dht 7460 > "$TMP/dht0.log" 2>&1 &
sleep 1
"$BIN" --serve-dht 7461 --extra 127.0.0.1:7460 > "$TMP/dht1.log" 2>&1 &
sleep 1
BOOT="198.51.100.1:7460,198.51.100.1:7461"

# Bootstrap an approved contact over the direct veth link.
ip netns exec A "$BIN" --home "$TMP/a" --accept 7451 > "$TMP/a-accept.log" 2>&1 &
sleep 1
ip netns exec B "$BIN" --home "$TMP/b" --dial 10.0.1.2:7451 > "$TMP/dial.log" 2>&1 &
sleep 3
FPA=$(grep '^fingerprint ' "$TMP/a-accept.log" | cut -d' ' -f2)
FPB=$(grep '^fingerprint ' "$TMP/dial.log" | cut -d' ' -f2)
echo "A=$FPA B=$FPB"

ip netns exec A timeout 120 "$BIN" --home "$TMP/a" --listen-port 7450 --no-local --bootstrap "$BOOT" --seek "$FPB" > "$TMP/a-run.log" 2>&1 &
ip netns exec B timeout 120 "$BIN" --home "$TMP/b" --listen-port 7450 --no-local --bootstrap "$BOOT" --seek "$FPA" > "$TMP/b-run.log" 2>&1 &
wait

echo "--- A:"; grep -h '^connected\|^addresses\|^note:' "$TMP/a-run.log" || tail -5 "$TMP/a-run.log"
echo "--- B:"; grep -h '^connected\|^addresses\|^note:' "$TMP/b-run.log" || tail -5 "$TMP/b-run.log"
if grep -q '^connected to .* via direct' "$TMP/a-run.log" && grep -q '^connected to .* via direct' "$TMP/b-run.log"; then
    echo "RESULT: DIRECT (both sides)"
    exit 0
elif grep -q '^connected to .* via relay' "$TMP/a-run.log"; then
    echo "RESULT: VIA RELAY"
    exit 0
else
    echo "RESULT: NO DIRECT PATH"
    exit 1
fi
