# Two-Phone Field Test Matrix

Context: every automated reachability test so far runs on one machine.
Hardware NATs differ in filtering and mapping behaviour; behaviours that
matter:

- **Mapping:** endpoint-independent (fixed external port per internal
  socket, "cone") vs endpoint-dependent (per-destination port,
  "symmetric"/CGNAT).
- **Filtering:** any source on a mapped port (full cone) vs only sources
  our socket talked to (restricted/port-restricted).
- **Hairpin, port preservation, lease time, IPv6** all interact.

## Lab equivalent (no phones, one Linux box)

```sh
sudo ./scripts/nattest.sh plain  plain   # both home-router-like: expect direct QUIC
sudo ./scripts/nattest.sh plain  random  # one CGNAT-like side: no predictable punch
sudo ./scripts/nattest.sh random random  # both CGNAT-like: expect symmetric fallback
```

The script puts both probes in netns, SNATs A's traffic to 198.51.100.2 and
B's to 203.0.113.2 (`--random` = random external port per flow, like CGNAT),
runs a private DHT (mainline testnet), and expects both sides to detect the
session route: `via direct` / `via relay` / none.

## On-device matrix (two phones)

Each combination gets: fresh mutual approval, both apps closed and
reopened (cold rendezvous), network-change event (Wi-Fi off / LTE on /
Airplane), and reachability within the listed target.

|          | Home router (port-preserving cone) | CGNAT/LTE (random, symmetric-ish) |
|----------|------------------------------------|-----------------------------------|
| **Home router** | direct QUIC, < 60 s cold | direct QUIC only if LTE side's mappings preserved; else relay |
| **CGNAT/LTE**   | same as above                    | symmetric both sides: no direct, must fall back to relay/mailbox; if relay used, session up < 30 s |

Pass criteria, one row per combination:

- both sides print `connected to <fp> via <route>` where route is one of
  `direct` / `relay` / `mailbox`, matching the expectation above;
- recovery after a reported network change: under ~10 s (recovery slot),
  under ~2 s while the anchor session lives (Appendix N anchor feature);
- no crash, no busy-loop candidates; Event log shows `symmetric` only when
  CGNAT was in play.

Fail-log columns for the report: `{"combo", "route", "cold_start_s",
"network_change_s", "candidates", "symmetric", "notes"}`.

## Notes from the 2026-10-05 field test

- Pixel 8a on AT&T CGNAT and a laptop behind two home NATs did connect
  directly over QUIC — carrier filtering still let their mutual probes
  open, and AT&T kept idle UDP mappings 60–120 s.
- Router port mapping would have needed cooperation of *both* home NATs.
- Sessions recovered in ~1 m 45 s before fast recovery; the standby hole
  and recovery slot brought that to ~8–9 s.
