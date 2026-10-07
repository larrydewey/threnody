#!/usr/bin/env bash
# Two-phone NAT test topology (Appendix N, "Two phones" item).
#
# A and B each in their own netns, behind the host's NAT:
#   A's traffic SNAT'd to 198.51.100.2, B's to 203.0.113.2
# (plain = port-preserving, random = per-flow port like CGNAT).
# A third "internet" netns holds the private DHT + reflector.
#
#   sudo ./scripts/nattest.sh plain plain   # expect direct QUIC
#   sudo ./scripts/nattest.sh random random # expect no direct path
set -euo pipefail

BIN="$(dirname "$0")/../target/debug/examples/probe"
cargo build -p threnody-net --example probe >/dev/null 2>&1

A_MODE="${1:-plain}"
B_MODE="${2:-plain}"
TMP="$(mktemp -d)"

cleanup() {
    set +e
    kill $(jobs -p) 2>/dev/null
    ip netns del A
    ip netns del B
    ip netns del I
    ip addr del 10.0.1.1/24 dev vethA 2>/dev/null
    ip addr del 10.0.2.1/24 dev vethB 2>/dev/null
    ip addr del 172.16.0.1/24 dev vethC 2>/dev/null
    ip link del vethA 2>/dev/null
    ip link del vethB 2>/dev/null
    ip link del vethC 2>/dev/null
    ip route del 198.51.100.0/24 2>/dev/null
    ip route del 203.0.113.0/24 2>/dev/null
    iptables -t nat -D POSTROUTING -s 10.0.1.0/24 -o vethC -j SNAT --to-source 198.51.100.2 $([ "$A_MODE" = random ] && echo --random) 2>/dev/null
    iptables -t nat -D POSTROUTING -s 10.0.2.0/24 -o vethC -j SNAT --to-source 203.0.113.2 $([ "$B_MODE" = random ] && echo --random) 2>/dev/null
    iptables -D FORWARD -s 10.0.0.0/16 -j ACCEPT 2>/dev/null
    iptables -D FORWARD -s 10.0.0.0/16 -d 10.0.0.0/16 -j ACCEPT 2>/dev/null
    iptables -D FORWARD -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

mkmode() { [ "$1" = random ] && echo "--random" || echo ""; }

mknet() {
    local ns="$1" n="$2"
    ip netns add "$ns"
    ip link add "veth$ns" type veth peer name "vethz$ns"
    ip link set "vethz$ns" netns "$ns"
    ip addr add "10.0.$n.1/24" dev "veth$ns"
    ip link set "veth$ns" up
    ip netns exec "$ns" ip addr add "10.0.$n.2/24" dev "vethz$ns"
    ip netns exec "$ns" ip link set "vethz$ns" up
    ip netns exec "$ns" ip link set lo up
    ip netns exec "$ns" ip route add default via "10.0.$n.1"
}

mknet A 1
mknet B 2

# "Internet" namespace.
ip netns add I
ip link add vethC type veth peer name vethzC
ip link set vethzC netns I
ip addr add 172.16.0.1/24 dev vethC
ip link set vethC up
ip netns exec I ip addr add 172.16.0.2/24 dev vethzC
ip netns exec I ip link set vethzC up
ip netns exec I ip link set lo up
ip netns exec I ip route add default via 172.16.0.1
# Alias IPs belonging to the private internet; /32 so unmatched reply-src
# connections do not pull ARP onto this interface.
ip netns exec I ip addr add 198.51.100.1/32 dev vethzC
ip netns exec I ip addr add 203.0.113.1/32 dev vethzC

echo 1 > /proc/sys/net/ipv4/ip_forward
iptables -C FORWARD -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || iptables -A FORWARD -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
iptables -C FORWARD -s 10.0.0.0/16 -d 10.0.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -s 10.0.0.0/16 -d 10.0.0.0/16 -j ACCEPT
iptables -C FORWARD -s 10.0.0.0/16 -j ACCEPT 2>/dev/null || iptables -A FORWARD -s 10.0.0.0/16 -j ACCEPT
# Anything to the NAT aliases that conntrack does not rewrite is not for us.
ip route add 198.51.100.0/24 via 172.16.0.2 2>/dev/null || true
ip route add 203.0.113.0/24 via 172.16.0.2 2>/dev/null || true
echo "FORWARD policy/rules: $(iptables -L FORWARD -n | head -1)"

iptables -t nat -A POSTROUTING -s 10.0.1.0/24 -o vethC -j SNAT --to-source 198.51.100.2 $(mkmode "$A_MODE")
iptables -t nat -A POSTROUTING -s 10.0.2.0/24 -o vethC -j SNAT --to-source 203.0.113.2 $(mkmode "$B_MODE")

# Private DHT: a canned mainline testnet inside the internet netns.
ip netns exec I timeout 300 "$BIN" --serve-testnet 8 > "$TMP/dht.log" 2>&1 &
sleep 3
BOOT=$(grep '^TESTNET ' "$TMP/dht.log" | cut -d' ' -f2 | paste -sd,)
echo "bootstrap: $BOOT"
ip netns exec I timeout 300 "$BIN" --serve-reflector 198.51.100.1:7462 > "$TMP/refl.log" 2>&1 &
sleep 1

# Bootstrap an approved contact over the direct host link.
ip netns exec A "$BIN" --home "$TMP/a" --accept 7451 > "$TMP/a-accept.log" 2>&1 &
sleep 1
ip netns exec B "$BIN" --home "$TMP/b" --dial 10.0.1.2:7451 > "$TMP/dial.log" 2>&1 &
sleep 3
FPA=$(grep '^fingerprint ' "$TMP/a-accept.log" | cut -d' ' -f2)
FPB=$(grep '^fingerprint ' "$TMP/dial.log" | cut -d' ' -f2)
echo "A=$FPA B=$FPB"

ip netns exec A timeout 120 "$BIN" --home "$TMP/a" --listen-port 7450 --no-local --bootstrap "$BOOT" --reflect 198.51.100.1:7462 --seek "$FPB" > "$TMP/a-run.log" 2>&1 &
PA=$!
ip netns exec B timeout 120 "$BIN" --home "$TMP/b" --listen-port 7450 --no-local --bootstrap "$BOOT" --reflect 198.51.100.1:7462 --seek "$FPA" > "$TMP/b-run.log" 2>&1 &
PB=$!
wait $PA $PB || true

# What the NAT actually did: which flows existed, and whether either
# direction of A<->B traffic was seen at all.
dump_nat() {
    echo "--- conntrack (udp, both NAT subnets):"
    conntrack -L -p udp 2>/dev/null | grep -E '10\.0\.1\.2|10\.0\.2\.2' | tail -20 || echo "(conntrack unavailable)"
    echo "--- vethC counters:"
    ip -s link show vethC 2>/dev/null | tail -4
    echo "--- A/B socket state:"
    for ns in A B; do
        echo -n "  $ns: "
        ip netns exec "$ns" ss -un 2>/dev/null | grep -c ':7450' || echo 0
    done
}

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
    echo "--- refl.log:"; cat "$TMP/refl.log" 2>/dev/null || true
    echo "--- dht.log:"; head -3 "$TMP/dht0.log" 2>/dev/null || true
    echo "--- dht.log:"; head -3 "$TMP/dht1.log" 2>/dev/null || true
    echo "--- a-run.log tail:"; tail -3 "$TMP/a-run.log" 2>/dev/null || true
    echo "--- b-run.log tail:"; tail -3 "$TMP/b-run.log" 2>/dev/null || true
    dump_nat
    rm -rf /tmp/nattest-last; cp -r "$TMP" /tmp/nattest-last
    exit 1
fi
