# T16.20 — Zenoh cross-machine smoke procedure

This procedure walks two operators (or one operator with two terminals
into two machines) through a four-test bisection of the cross-WiFi data-
plane loss the bench is hitting on
`configs/two-runner-zenoh-all.toml`.

The smoke binary is `examples/cross_machine_smoke.rs`. It is a 200-line
standalone Zenoh pub/sub program — no `variant-base`, no JSONL, no
bench plumbing. It DOES use the same Zenoh dep, the same transport
queue/buffer config, the same `CongestionControl` mapping, the same
`scouting/multicast/interface` pin, and the same `<key>/**` wildcard
subscriber pattern that the bench uses. Anything it does differently is
called out at the top of `cross_machine_smoke.rs`.

## Build (both machines)

```powershell
cargo build --release -p variant-zenoh --example cross_machine_smoke
```

Binary lands at `target\release\examples\cross_machine_smoke.exe`.

## Naming convention used in this doc

- **alice**: machine 1, WiFi IPv4 e.g. `192.168.1.80`.
- **bob**: machine 2, WiFi IPv4 e.g. `192.168.1.81`.

Substitute your actual IPs throughout. Run all commands from the repo
root in PowerShell.

## CLI cheat-sheet

```
--mode {pub|sub|both}                  required
--peer-mode {peer|client}              default: peer
--multicast-interface <ipv4>           default: auto
--qos {1|2|3|4}                        default: 1
   1 (BestEffort)  -> CongestionControl::Drop
   2 (LatestValue) -> CongestionControl::Drop
   3 (ReliableUdp) -> CongestionControl::Block
   4 (ReliableTcp) -> CongestionControl::Block
--key <prefix>                         default: smoke/test
--rate-hz <u32>                        default: 100  (0 = unthrottled)
--values-per-tick <u32>                default: 1000
--duration-secs <u32>                  default: 30
--payload-size-bytes <usize>           default: 16
--connect <endpoint>                   repeatable, bypass multicast
--listen <endpoint>                    repeatable
--wait-peers <usize>                   default: 0 (no wait)
--wait-peers-timeout-secs <u32>        default: 10
```

The smoke prints a startup banner with the Zenoh git_version, the chosen
QoS mapping, and the session zid, then emits one `pub:` or `sub:` tally
line every 5 seconds. On exit it prints `[smoke] FINAL pub total = ...`
(and `sub total = ...` in `both` mode).

The set of connected peers is polled once per second and printed
whenever it changes (`[smoke] peers: <zid>, ...` or `[smoke] peers:
<none>`).

## What each test bisects

```
TEST 1: low-rate bare multicast at QoS 1 (BestEffort/Drop)
   -> if PASS: multicast discovery and the basic CC=Drop data path work
   -> if FAIL: multicast HELLOs or the data port are blocked by AP/firewall

TEST 2: low-rate bare multicast at QoS 4 (ReliableTcp/Block)
   -> if PASS but TEST 1 failed: weird, would be very useful evidence
   -> if FAIL but TEST 1 passed: T17.8 strict-window watchdog is implicated
                                 even at minimal load

TEST 3: matrix-rate (100 hz x 1000 vpt) at QoS 1 — the failing bench shape
   -> if FAIL with receives=0: we've reproduced the bench bug in 200 lines

TEST 4: explicit --connect tcp/<peer>:7447 at QoS 1, low rate (no multicast)
   -> if PASS while TEST 1 failed: multicast announcement is being suppressed
                                   or peers can't reach each other's
                                   multicast-discovered TCP endpoint
   -> if FAIL: the TCP data path between the two WiFi clients is broken
               (AP client isolation, Windows firewall, ...)
```

## Test 1 — low-rate bare multicast at QoS 1

Goal: confirm the simplest case works at all on this network.

**On bob (subscriber)**:

```powershell
target\release\examples\cross_machine_smoke.exe `
  --mode sub --qos 1 `
  --key smoke/t1 `
  --rate-hz 10 --values-per-tick 10 --duration-secs 30 `
  --multicast-interface <bob-wifi-ipv4>
```

**On alice (publisher)**, starting ~5 seconds AFTER bob:

```powershell
target\release\examples\cross_machine_smoke.exe `
  --mode pub --qos 1 `
  --key smoke/t1 `
  --rate-hz 10 --values-per-tick 10 --duration-secs 15 `
  --multicast-interface <alice-wifi-ipv4> `
  --wait-peers 1 --wait-peers-timeout-secs 15
```

Expected:
- alice prints `[smoke] peers: <bob-zid>` within ~5 s and proceeds.
- alice exits with `pub total = 150`.
- bob exits with `sub total >= ~140 unique_keys = 10` (allow a few
  drops on CC=Drop). If `sub total = 0`, this test FAILS — see the
  bisection map above.

Repeat in the opposite direction: alice = sub, bob = pub. Both
directions should pass.

## Test 2 — low-rate bare multicast at QoS 4

Same commands as Test 1, but with `--qos 4` on both sides. CC=Block
makes the publisher park on its own queue if the peer never opens a
receive channel, so a clean test 2 needs `--wait-peers 1` on the pub
side AND the alice deadline to be slightly more generous than test 1.
With both pinned, expected `sub total = 150` exactly (no drops on the
Block path).

If Test 1 PASSES and Test 2 FAILS: T17.8 strict-window watchdog is
implicated even at 100 msg/s (only 1.5K samples total over the test).

## Test 3 — matrix-rate (100 hz x 1000 vpt) at QoS 1

This reproduces the bench's failing workload at the lowest plausible
diagnostic abstraction.

**On bob (subscriber)**:

```powershell
target\release\examples\cross_machine_smoke.exe `
  --mode sub --qos 1 `
  --key smoke/t3 `
  --rate-hz 100 --values-per-tick 1000 --duration-secs 60 `
  --multicast-interface <bob-wifi-ipv4>
```

**On alice (publisher)** (start after bob):

```powershell
target\release\examples\cross_machine_smoke.exe `
  --mode pub --qos 1 `
  --key smoke/t3 `
  --rate-hz 100 --values-per-tick 1000 --duration-secs 30 `
  --multicast-interface <alice-wifi-ipv4> `
  --wait-peers 1 --wait-peers-timeout-secs 30
```

Expected:
- alice publishes 100 hz x 1000 vpt x 30 s = 3,000,000 samples.
- bob receives some fraction.
- Record `pub total`, `sub total`, and the per-5s `sub: last5s=...`
  lines. The shape of the receive curve (steady vs spike-and-collapse)
  is itself a useful diagnostic.

If `sub total = 0` here while Test 1 passed: we've reproduced the
bench's failure in the standalone binary, and Zenoh's data plane on
this WiFi cannot sustain the rate even without any T17.8 layering.

## Test 4 — explicit `--connect` (multicast bypass) at QoS 1, low rate

Goal: take multicast scouting out of the equation entirely.

Pick a listen port that's free on both sides; we use 7447 below (the
Zenoh default). The publisher dials the subscriber by IP.

**On bob (subscriber + listener)**:

```powershell
target\release\examples\cross_machine_smoke.exe `
  --mode sub --qos 1 `
  --key smoke/t4 `
  --rate-hz 10 --values-per-tick 10 --duration-secs 30 `
  --listen tcp/0.0.0.0:7447
```

**On alice (publisher, connect)**:

```powershell
target\release\examples\cross_machine_smoke.exe `
  --mode pub --qos 1 `
  --key smoke/t4 `
  --rate-hz 10 --values-per-tick 10 --duration-secs 15 `
  --connect tcp/<bob-wifi-ipv4>:7447 `
  --wait-peers 1 --wait-peers-timeout-secs 15
```

Expected (if WiFi TCP between the two hosts works at all):
- alice's `[smoke] peers: <bob-zid>` line appears within 1-2 seconds.
- `sub total ~ 150 unique_keys = 10`.

If Test 4 PASSES while Test 1 FAILED: the multicast HELLO is being
suppressed by the AP, OR the multicast-discovered TCP endpoint isn't
reachable from the peer (Windows Defender Firewall blocking inbound
TCP/7447 on the receiving side is a common cause).

If Test 4 FAILS: the TCP data path itself between the two WiFi clients
is broken. Likely culprits in order: Windows Defender Firewall inbound
rule, AP client-isolation (a "guest-mode" feature some consumer APs
enable for security), or NAT/port-forward weirdness if the two hosts
aren't actually on the same subnet.

## Recording results

For each test, record in a notes file:

```
TEST <n> direction <pub>-><sub>:
  pub total:       <number>
  sub total:       <number>
  receive ratio:   <sub/pub %>
  peers visible:   <yes/no, time-to-peer>
  notable lines:   <any FINAL or peer or error lines>
```

Then report the four ratios back to the orchestrator; the next task
will use them to pick whether to file a firewall/AP fix, a Zenoh-side
network fix, or a bench-side bug.

## Repeat on wired

Run the same four tests on the LS105G wired switch setup (same hosts,
ethernet only). Cross-WiFi vs cross-wired is the most informative
single comparison the smoke can give us.
