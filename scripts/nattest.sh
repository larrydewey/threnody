#!/usr/bin/env bash
# Two-phone NAT test topology (Appendix N, "Two phones" item).
#
# A and B each in their own netns, behind the host's NAT: A's traffic is
# SNAT'd to 198.51.100.2, B's to 203.0.113.2. A third "internet" netns
# holds the private DHT and the reflector, so all traffic crosses the NAT.
#
# `plain` models a cone NAT: the source port is preserved and inbound
# traffic for the mapped port is delivered, so hole punching works. This
# is the same inbound path a router port mapping (UPnP IGD / PCP / NAT-PMP)
# grants. `random` models carrier-grade NAT: a fresh external port per flow
# and no inbound path at all, so punching cannot succeed.
#
# With no arguments, every combination runs and each gets a pass/fail
# line. Exits non-zero if any combination fails.
#
#   sudo ./scripts/nattest.sh            # the whole matrix
#   sudo ./scripts/nattest.sh plain      # one side cone, one symmetric
#   sudo ./scripts/nattest.sh plain plain
set -euo pipefail

BIN="$(dirname "$0")/../target/debug/examples/probe"
cargo build -p threnody-net --example probe >/dev/null 2>&1

# No arguments: run the whole matrix. Otherwise run the given pair.
if [ $# -eq 0 ]; then
    set --
    for combo in "plain plain" "plain random" "random random"; do
        # shellcheck disable=SC2086
        "$0" $combo || true
    done
    exit 0
fi
A_MODE="$1"
B_MODE="$2"

# Identify this run, so pasted output can never be confused with an older
# run's: which script, which commit, and a fresh identity pair each time.
echo "nattest: $A_MODE/$B_MODE  script=$0  sha=$(sha256sum "$0" | cut -c1-12)  HEAD=$(git -C "$(dirname "$0")/.." rev-parse --short HEAD 2>/dev/null || echo unknown)"
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
    iptables -t nat -D POSTROUTING -s 10.0.1.0/24 -o vethB -j SNAT --to-source 198.51.100.2 $([ "$A_MODE" = random ] && echo --random) 2>/dev/null
    iptables -t nat -D POSTROUTING -s 10.0.2.0/24 -o vethA -j SNAT --to-source 203.0.113.2 $([ "$B_MODE" = random ] && echo --random) 2>/dev/null
    # Port mappings: the cone NAT's inbound path. Without these nothing owns
    # the NAT aliases, so packets addressed to them loop host->INT->host.
    for p in udp tcp; do
        iptables -t nat -D PREROUTING -p "$p" -d 198.51.100.2 --dport 7450 -j DNAT --to-destination 10.0.1.2:7450 2>/dev/null
        iptables -t nat -D PREROUTING -p "$p" -d 203.0.113.2 --dport 7450 -j DNAT --to-destination 10.0.2.2:7450 2>/dev/null
    done
    iptables -D FORWARD -s 10.0.0.0/16 -d 10.0.0.0/16 -p tcp --dport 7451 -j ACCEPT 2>/dev/null
    iptables -D FORWARD -m mark --mark 0x1 -j DROP 2>/dev/null
    iptables -D FORWARD -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null
    iptables -t mangle -D PREROUTING -s 10.0.0.0/16 -d 10.0.0.0/16 -m conntrack --ctstate NEW -j MARK --set-mark 0x1 2>/dev/null
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
# Block the LAN shortcut: A reaching B's private address directly.
#
# Matching on the *current* destination is wrong here. A cone NAT's DNAT
# rewrites 203.0.113.2:7450 to 10.0.2.2:7450, so by FORWARD time the
# legitimate path looks exactly like the shortcut and gets dropped too --
# which is exactly how the first plain/plain run failed with no packet ever
# reaching the peer's alias.
#
# So mark it in mangle PREROUTING, which runs *before* nat PREROUTING and
# therefore still sees the pre-DNAT destination: the shortcut was addressed
# to a lab subnet, the NAT path was addressed to an alias. Replies are
# un-SNAT'd in nat PREROUTING (after mangle), so they are never marked and
# are matched earlier by ESTABLISHED anyway.
SHORTCUT=0x1
iptables -t mangle -C PREROUTING -s 10.0.0.0/16 -d 10.0.0.0/16 -m conntrack --ctstate NEW -j MARK --set-mark "$SHORTCUT" 2>/dev/null || \
    iptables -t mangle -I PREROUTING 1 -s 10.0.0.0/16 -d 10.0.0.0/16 -m conntrack --ctstate NEW -j MARK --set-mark "$SHORTCUT"
# Inserted at the front in reverse: ESTABLISHED, bootstrap, marked-drop.
iptables -C FORWARD -m mark --mark "$SHORTCUT" -j DROP 2>/dev/null || \
    iptables -I FORWARD 1 -m mark --mark "$SHORTCUT" -j DROP
iptables -C FORWARD -s 10.0.0.0/16 -d 10.0.0.0/16 -p tcp --dport 7451 -j ACCEPT 2>/dev/null || \
    iptables -I FORWARD 1 -s 10.0.0.0/16 -d 10.0.0.0/16 -p tcp --dport 7451 -j ACCEPT
iptables -C FORWARD -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || \
    iptables -I FORWARD 1 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
# Route the NAT aliases out to the internet ns. Inbound packets the NAT
# rewrites (a cone NAT's DNAT, or a conntrack reply) reach the LAN behind
# PREROUTING instead, so these routes only carry outward-bound traffic.
ip route add 198.51.100.0/24 via 172.16.0.2 2>/dev/null || true
ip route add 203.0.113.0/24 via 172.16.0.2 2>/dev/null || true
echo "FORWARD policy/rules: $(iptables -L FORWARD -n | head -1)"

iptables -t nat -A POSTROUTING -s 10.0.1.0/24 -o vethC -j SNAT --to-source 198.51.100.2 $(mkmode "$A_MODE")
iptables -t nat -A POSTROUTING -s 10.0.2.0/24 -o vethC -j SNAT --to-source 203.0.113.2 $(mkmode "$B_MODE")

# A cone NAT's inbound DNAT redirects to the peer's *private* address, so
# the packet then leaves via that LAN's veth, not vethC. Without these the
# `-o vethC` rules above never fire for it and the flow completes
# LAN-to-LAN with no translation at all: the alias resolves, and then the
# two nodes are talking directly behind the NAT's back. Translate the
# reply direction too, so a punched session really does cross the NAT.
iptables -t nat -A POSTROUTING -s 10.0.1.0/24 -o vethB -j SNAT --to-source 198.51.100.2 $(mkmode "$A_MODE")
iptables -t nat -A POSTROUTING -s 10.0.2.0/24 -o vethA -j SNAT --to-source 203.0.113.2 $(mkmode "$B_MODE")

# A cone NAT keeps one mapping per internal port, so anything arriving for
# that port is delivered. That is the inbound path hole punching relies on,
# and the same one a router port mapping (UPnP IGD / PCP / NAT-PMP) grants.
# Only the plain side gets it: a symmetric NAT like `random` must not.
nat_dnat() {
    iptables -t nat -A PREROUTING -p udp -d 198.51.100.2 --dport 7450 -j DNAT --to-destination 10.0.1.2:7450
    iptables -t nat -A PREROUTING -p tcp -d 198.51.100.2 --dport 7450 -j DNAT --to-destination 10.0.1.2:7450
    iptables -t nat -A PREROUTING -p udp -d 203.0.113.2 --dport 7450 -j DNAT --to-destination 10.0.2.2:7450
    iptables -t nat -A PREROUTING -p tcp -d 203.0.113.2 --dport 7450 -j DNAT --to-destination 10.0.2.2:7450
}
if [ "$A_MODE" = plain ] || [ "$B_MODE" = plain ]; then
    nat_dnat
fi

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

ip netns exec A timeout 120 "$BIN" --home "$TMP/a" --listen-port 7450 --no-local --clear-addrs --bootstrap "$BOOT" --reflect 198.51.100.1:7462 --seek "$FPB" --one > "$TMP/a-run.log" 2>&1 &
PA=$!
ip netns exec B timeout 120 "$BIN" --home "$TMP/b" --listen-port 7450 --no-local --clear-addrs --bootstrap "$BOOT" --reflect 198.51.100.1:7462 --seek "$FPA" --one > "$TMP/b-run.log" 2>&1 &
PB=$!
wait $PA $PB || true

# What the NAT actually did: which flows existed, and whether either
# direction of A<->B traffic was seen at all.
dump_nat() {
    echo "--- NAT rules in effect:"
    iptables -t nat -S | grep -E '198\.51\.100\.2|203\.0\.113\.2'
    echo "--- conntrack flows touching the NAT subnets:"
    conntrack -L -p udp 2>/dev/null | grep -E '10\.0\.1\.2|10\.0\.2\.2' | tail -20
    echo "--- which side dialed:"
    grep -ho 'dialed over quic at [^ ]*\|dialing .* failed' "$TMP/a-run.log" "$TMP/b-run.log" 2>/dev/null | tail -5
    echo "--- ICMP rejected in INT (packets nothing owned):"
    ip netns exec I nft list chain inet filter input 2>/dev/null | tail -5 || echo "(nft unavailable)"
}

echo "--- A:"; grep -h '^connected to\|^addresses' "$TMP/a-run.log" || tail -3 "$TMP/a-run.log"
echo "--- B:"; grep -h '^connected to\|^addresses' "$TMP/b-run.log" || tail -3 "$TMP/b-run.log"

a_dir=$(grep -c '^connected to .* via direct' "$TMP/a-run.log" || true)
b_dir=$(grep -c '^connected to .* via direct' "$TMP/b-run.log" || true)
a_relay=$(grep -c '^connected to .* via relay' "$TMP/a-run.log" || true)
b_relay=$(grep -c '^connected to .* via relay' "$TMP/b-run.log" || true)

# The app logs the address it actually dialled ("session with ... dialed
# over quic at <addr>"), from its own knowledge of the candidate list. That
# is the only signal worth grading: an alias means the punch crossed the
# NAT, a 10.0.x.x LAN address means the two nodes dialled each other
# directly, which is the back door the isolation rules exist to close.
#
# Deliberately not judging from the "connected ... via direct" line: for an
# accepted inbound session the peer is reported as it arrived at the socket,
# i.e. post-DNAT, so a LAN address there is legitimate. Nor from conntrack:
# it records the expected reply tuple even when no reply ever arrives.
nat_dials=$(grep -ho 'dialed over quic at [^ ]*' "$TMP/a-run.log" "$TMP/b-run.log" 2>/dev/null | sort -u || true)
dial_alias=$(echo "$nat_dials" | grep -c '198\.51\.100\.2\|203\.0\.113\.2' || true)
dial_lan=$(echo "$nat_dials" | grep -c ' 10\.0\.' || true)

echo "note: dialled aliases=$dial_alias lan=$dial_lan"
if [ "$dial_lan" -gt 0 ]; then
    echo "note: a dial targeted a lab LAN address; the shortcut was used"
fi

if [ "$dial_alias" -gt 0 ]; then
    a_dir=1; b_dir=1
else
    a_dir=0; b_dir=0
fi

# A punch only works when both NATs are cone: a symmetric side has no
# inbound path, so neither can reach the other directly.
if [ "$A_MODE" = plain ] && [ "$B_MODE" = plain ]; then
    want="direct"
elif [ "$a_relay" -gt 0 ] || [ "$b_relay" -gt 0 ]; then
    want="relay"
else
    want="none"
fi

if [ "$want" = direct ] && [ "$a_dir" -gt 0 ] && [ "$b_dir" -gt 0 ]; then
    echo "RESULT: PASS ($A_MODE/$B_MODE) direct from both sides"
    exit 0
elif [ "$want" = relay ] && { [ "$a_relay" -gt 0 ] || [ "$b_relay" -gt 0 ]; }; then
    echo "RESULT: PASS ($A_MODE/$B_MODE) fell back to a relay circuit"
    exit 0
elif [ "$want" = none ] && [ "$a_dir" -eq 0 ] && [ "$b_dir" -eq 0 ]; then
    echo "RESULT: PASS ($A_MODE/$B_MODE) no direct path, as expected (no relay in this topology)"
    exit 0
fi

echo "RESULT: FAIL ($A_MODE/$B_MODE) wanted $want; got direct=$a_dir/$b_dir relay=$a_relay/$b_relay${dial_lan:+ lan=$dial_lan}"
echo "--- refl.log (NATed sources seen by the reflector):"; tail -4 "$TMP/refl.log" 2>/dev/null || true
dump_nat
echo "--- last dial attempts:"; grep -h 'dialing\|dialed over' "$TMP/a-run.log" "$TMP/b-run.log" 2>/dev/null | tail -6 || true
rm -rf /tmp/nattest-last; cp -r "$TMP" /tmp/nattest-last
exit 1
