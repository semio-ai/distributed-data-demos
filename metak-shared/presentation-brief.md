# Distributed Data Replication — Presentation Brief

A short reference for building slides on the variants we benchmark, the QoS
levels each one supports, and the metrics we report.

## 1. The benchmark in one paragraph

**The goal is to validate whether Zenoh is a good fit for our distributed
data replication needs.** We replicate a single-writer, key-value tree
across nodes on a LAN at ~100 K updates/sec aggregate with sub-10 ms
latency, and we want a defensible answer on whether Zenoh meets that
envelope before we commit to it. The five other variants are *baselines*
that bracket Zenoh from below (Custom UDP — the performance floor with no
framework overhead) and around it (Hybrid UDP/TCP, QUIC, WebSocket,
WebRTC). They all solve the **same problem** and run through the **same
workload**, so any difference in performance is attributable to the
transport / framework choice.

## 1a. Workload glossary

Each spawn is configured as `<vpt>x<tick_rate_hz>` with a QoS suffix:

- **`tick_rate_hz`** — how many times per second the writer wakes up to
  emit a batch. 100 hz = 10 ms between ticks, 1000 hz = 1 ms.
- **`values_per_tick` (`vpt`)** — how many distinct key/value updates are
  emitted on each tick (each one is a separate `write` event with its own
  path / seq).
- **Aggregate write rate** = `vpt × tick_rate_hz`. So `100x100hz` is
  10 k writes/sec; `1000x100hz` is 100 k writes/sec; `100x1000hz` is also
  100 k writes/sec but in many small ticks; `1000x1000hz` (the `max`
  profile) targets 1 M writes/sec and is intentionally above what loopback
  can sustain — a saturation probe, not a "normal" measurement.
- **QoS (1–4)** — see §3 below. Same value emitted on different QoS levels
  is a different spawn.
- **Per-spawn lifecycle**: `connect` → `stabilize 3 s` → `operate 30 s` →
  `eot ≤30 s` → `silent 3 s`. The interesting metrics are computed over
  the `operate` window; `eot` is a designed drain hook for late deliveries
  (see §1b below).

The all-variants matrix sweeps `vpt ∈ {10, 100, 1000, max}` ×
`tick_rate_hz ∈ {10, 100, 1000}` × `qos ∈ {1, 2, 3, 4}` per variant.

## 1b. End-of-Test (EOT) phase

After `operate` ends, each variant enters an **EOT phase** designed to
let receivers drain late deliveries before the spawn shuts down. The
writer emits a single EOT marker over the transport (`eot_sent` event,
default impl returns id 0; per-variant overrides emit a real
unique id). Each peer waits up to `--eot-timeout-secs`
(default `max(operate_secs, 5)` = **30 s** at our current `operate_secs = 30`)
for every other peer's EOT marker, logging `eot_received` as each lands.
During the wait, `poll_receive` keeps draining so deliveries that arrived
inside the kernel buffer mid-`operate` still get counted (and recorded as
`late_receives` for the analysis).

If a peer's EOT never arrives, `eot_timeout` is logged with the missing
peer set and the spawn proceeds to `silent`.

**Variant implementation status (2026-05-10):**
- ✅ Custom UDP — own EOT marker, sent over multicast for QoS 1–2, over
  TCP for QoS 3–4.
- ✅ Hybrid UDP/TCP — UDP multicast for QoS 1–2, TCP broadcast for QoS 3–4.
- ✅ QUIC — separate datagram / control stream depending on QoS path.
- ✅ Zenoh — published as a distinguished EOT key, subscribed via the
  same session as data.
- ❌ WebSocket, WebRTC — no override yet. Falls through to the no-op
  default, which always emits `eot_timeout`. Filed: E12 sprint (T12.4,
  T12.5).

**This affects how to read the results.** Where you see `late_receives`
in the per-row output, the EOT drain captured deliveries that arrived
after the writer's `eot_sent`. Where you see `eot_timeout` (alice
missing bob or vice versa) in the JSONL log, the drain stopped at the
30 s budget without catching everything still in flight.

## 2. The variants

Six implementations: **Zenoh is the candidate we're evaluating**; the
other five are baselines that establish what's possible (faster) and
what's plausible (alternative frameworks).

### The candidate

- **Zenoh** — a Rust-native pub/sub framework with built-in zero-conf
  discovery and configurable QoS. *This is the variant we're evaluating
  for production use.* Specifically we want to know: does Zenoh's
  out-of-the-box delivery rate, latency, and tail behaviour meet our
  sub-10 ms p99 / ~100 K writes/sec target, with acceptable resource
  overhead, and without us writing a custom transport.

### Performance-floor baseline (hand-rolled, minimum overhead)

- **Custom UDP** — raw `UdpSocket` plus a hand-written protocol. UDP
  multicast for fan-out, unicast NACKs for recovery, mDNS for discovery.
  Implements all four QoS levels at the application layer. *Establishes
  the latency / throughput floor: if Zenoh is N× slower than Custom UDP,
  this is the cost we're paying for the framework.*

### Alternative-framework baselines (comparable abstractions)

- **Hybrid UDP/TCP** — UDP multicast for unreliable QoS (1–2), one TCP
  connection per peer pair for reliable QoS (3–4). Kernel handles
  retransmission/ordering on the reliable side. *Sanity check on "would
  a simple hand-rolled hybrid have been good enough?"*

- **QUIC (quinn)** — UDP-based, multiplexed reliable streams plus
  unreliable datagrams, mandatory TLS 1.3. Streams map cleanly to QoS
  levels. *Compares Zenoh against a modern, low-overhead, encrypted
  transport that solves similar problems with different abstractions.*

- **WebSocket** — TCP with WebSocket framing on top. Reliable QoS only
  (3–4); refuses unreliable QoS by design. *Compares Zenoh against a
  ubiquitous, browser-compatible reliable transport when only QoS 3–4
  is needed.*

- **WebRTC DataChannels** — SCTP-over-DTLS-over-UDP. Each DataChannel
  is configurable for ordered/unordered + reliable/`maxRetransmits=0`,
  giving native support for all four QoS levels. *Compares Zenoh
  against a heavier off-the-shelf reliable+unreliable mux.*

### How the baselines bracket Zenoh

| Direction | Variant | What it tells us about Zenoh |
|---|---|---|
| ↓ floor | Custom UDP | If Zenoh is much slower → framework overhead is the cost |
| ≈ peer | Hybrid, QUIC | If Zenoh wins → mature framework adds real value; if loses → think twice |
| ↑ ceiling | WebSocket, WebRTC | If Zenoh is faster than these heavy stacks, the choice is obvious |

## 3. The four QoS levels

QoS is configured **per subtree branch** by the writer that owns it. A
single tree can carry all four levels simultaneously.

| Level | Name | Transport intent | Ordering | Loss behaviour |
|---|---|---|---|---|
| 1 | Best-Effort | UDP, fire-and-forget | None | Tolerated, ignored |
| 2 | Latest-Value | UDP, seq-tagged | Latest-wins (drop stale) | Tolerated, skipped |
| 3 | Reliable-UDP | UDP + NACK | Strict | Recovered (lags) |
| 4 | Reliable-TCP | TCP (or equivalent) | Strict | Recovered (kernel) |

### How each variant maps the four levels

| Variant | L1 Best-Effort | L2 Latest-Value | L3 Reliable-UDP | L4 Reliable-TCP |
|---|---|---|---|---|
| **Zenoh** | Best-effort pub | Best-effort + receiver seq filter | Reliable pub | Reliable pub |
| **Custom UDP** | Multicast send, no tracking | Multicast + per-writer seq, drop stale | Multicast + NACK retransmit | TCP per peer |
| **Hybrid UDP/TCP** | UDP multicast, no tracking | UDP multicast + seq filter | TCP (kernel reliability) | TCP (kernel reliability) |
| **QUIC** | Unreliable datagram | Unreliable datagram + seq filter | Reliable stream | Reliable stream |
| **WebSocket** | *not supported* | *not supported* | TCP+WS frame, ordered | TCP+WS frame, ordered |
| **WebRTC** | DataChannel, unordered, `maxRetransmits=0` | DataChannel, unordered, `maxRetransmits=0` + seq filter | DataChannel, ordered, reliable | DataChannel, ordered, reliable |

Notes for the slide:

- The **interesting comparison** at L3 is Custom UDP (NACKs, per-path
  independence) vs Hybrid (TCP, head-of-line blocking) — directly
  measures whether per-path independence pays off on a LAN.
- L1 vs L2 is "do we even tag sequences?" — L1 has no receiver-side
  state, L2 just discards anything older than the highest seen.
- L3 vs L4 is "what happens to other paths when one packet is lost?"
  L3 keeps unrelated paths flowing; L4 stalls everything on the same
  connection until the gap is recovered.

## 4. What we measure

The analysis tool ingests JSONL logs from every node and produces both
**integrity** (did it work correctly?) and **performance** (how fast?)
reports.

### Integrity (correctness)

- **Delivery rate** — `receives / writes` per (writer → receiver) pair.
  At L3/L4 we expect 100%; at L1/L2 we report whatever it is.
- **Ordering violations** — out-of-order receives on ordered QoS
  levels (2/3/4).
- **Duplicates** — same `(writer, seq, path)` received twice.
- **Unresolved gaps** — for L3, every detected gap must eventually
  be filled before the run ends.

### Performance (the headline numbers)

- **Replication latency** — wall-clock time from the writer's `write`
  event to the matching `receive` event on every other node. Reported as
  **p50 / p95 / p99 / max**, with breakdowns per path and per receiver
  to spot hot paths and slow nodes. The single most important metric.
- **Throughput** — sustained `writes/sec` per writer and `receives/sec`
  per receiver during the operate phase. Aggregate across all writers
  is the headline rate.
- **Jitter** — rolling standard deviation of latency. Tells us whether
  latency is *consistent* or just has a good average.
- **Packet loss rate** — for QoS levels with sequence tracking
  (2/3/4), missing-seq ratio. For L3, this is *transient* loss before
  recovery.
- **Connection time** — how long from process start to "ready to publish"
  (`connected` event). Mostly interesting for QUIC and WebRTC, where
  handshakes dominate cold start.
- **Resource usage** — CPU% and memory MB sampled during the run.

### How latency is computed (one-line version)

For every `write` event on the writer's log, we find the matching
`receive` event on every other runner's log by joining on
`(variant, run, writer, seq, path)`. The delta of their timestamps is
the latency. Clocks across machines are reconciled separately via PTP
sync. We then aggregate those per-delivery latencies into the
percentiles above.

## 5. Results from the all-variants matrix

Two runs of `configs/two-runner-all-variants.toml` (48 [[variant]]
entries × QoS expansion = 176 spawns) form the basis below. Both ran
on Windows hosts. **Both runs were taken AFTER the EOT phase landed
in the driver** (commit `5faf7a8`, 2026-05-04). So the EOT drain hook
was active for every spawn in both runs — but as shown below, EOT
doesn't fully rescue the high-rate TCP cases, for reasons unpacked
below.

### 5.1 Same-machine run (2026-05-07, log_subdir `…_183143`)

Both runners on one host, loopback addresses. Complete log set for
alice and bob — paired latency and delivery numbers are real. **This
is the run with the most interpretable data.**

#### Zenoh — headline result for the candidate

| Spawn | Aggregate rate | Delivery | p50 | p95 | p99 | Loss% | Notes |
|---|---|---|---|---|---|---|---|
| `zenoh-10x100hz-qos1` | 1 k writes/s | 100 % | 10.7 ms | 29.8 ms | 30.1 ms | 0 % | clean |
| `zenoh-100x100hz-qos1` | 10 k writes/s | 100 % | 30.3 ms | 159.6 ms | 170.8 ms | 0 % | clean |
| `zenoh-100x100hz-qos3` | 10 k writes/s | 100 % | 40.1 ms | 311 ms | 340 ms | 0 % | clean (reliable) |
| `zenoh-1000x100hz-qos1` | 100 k writes/s | ~26 % | 435 ms | 507 ms | 612 ms | 64 % | saturated |
| `zenoh-1000x100hz-qos3` | 100 k writes/s | ~36 % | 423 ms | 524 ms | 535 ms | 64 % | saturated |
| `zenoh-max-qos1` | 1 M target | low | 203 ms | 227 ms | 309 ms | 63 % | saturation probe |

**Read of Zenoh against our target (sub-10 ms p99, ~100 K writes/sec
aggregate):**

- At **10 k writes/sec aggregate** (100 vpt × 100 hz), Zenoh delivers
  100 % of messages at p99 ≈ 170 ms — *misses our latency target*. Same
  pattern across QoS levels.
- At **100 k writes/sec aggregate** (1000 vpt × 100 hz), Zenoh saturates
  to ~26–36 % delivery with p99 in the 500–700 ms range — *misses both
  delivery and latency targets at our headline rate*.
- Zenoh's behaviour is consistent across QoS 1–4 (no obvious cliff at the
  reliable-vs-unreliable boundary), and resource usage stays modest
  (sub-100 % CPU mean, low MB memory).
- A few zenoh rows show sub-ms p50 (`zenoh-100x1000hz-qos1`: 0.54 ms
  p50, 110 ms p95). Those rates run mostly through Zenoh's
  same-process subscriber-cache path; they aren't measuring the wire
  fairly and shouldn't be presented as a cross-protocol comparison.

#### Baselines — context for the Zenoh numbers

At the 10 k-writes/sec rate (the standard `100x100hz-qos1`):

| Variant | Delivery | p50 | p99 |
|---|---|---|---|
| QUIC | 100 % | **0.78 ms** | 11.3 ms |
| Custom UDP | 100 % | 5.5 ms | 21.7 ms |
| WebRTC | 100 % | 2.3 ms | 157 ms |
| Zenoh | 100 % | 30 ms | 171 ms |
| Hybrid | 100 % | 146 ms | 204 ms |

At this rate every variant that completed its spawn delivered 100 %, so
the comparison is purely on latency: **Zenoh is ~40× slower at p50
than QUIC, and ~6× slower than Custom UDP**, but it's a finished, working
result. It misses the sub-10 ms p99 target while QUIC clears it.

At 100 k writes/sec (`1000x100hz-qos1`):

| Variant | Delivery | p50 | Notes |
|---|---|---|---|
| Custom UDP | 25 % | 389 ms | UDP buffer overflow |
| Zenoh | 26 % | 435 ms | similar saturation |
| Hybrid | 15 % | 1.0 s | TCP buffer queueing |
| QUIC | 17 % | 21 s | catastrophic, see below |

**No transport hits sub-10 ms p99 at 100 K writes/sec on this hardware.**
This is a workload-shape finding, not a Zenoh verdict.

#### What the data says about EOT specifically

EOT *fires* for the variants that implement it (custom-udp, hybrid,
quic, zenoh) — the JSONL logs contain `eot_sent` events and either
`eot_received` (success) or `eot_timeout` (drain budget exhausted).
But it doesn't fully rescue the worst rows:

- **Hybrid TCP (`hybrid-1000x100hz-qos3`)**: alice emits `eot_sent` at
  operate+30 s, then waits 30 s for bob's EOT marker, then logs
  `eot_timeout` with bob still missing. Bob does the same in reverse.
  Reason: hybrid sends the EOT marker **over the same TCP stream as the
  data**, so the EOT marker queues behind the data deluge that already
  overflowed the kernel send/recv buffer. By the time the queue would
  drain, the 30 s EOT budget has elapsed. EOT works fine for hybrid at
  rates where the data queue *can* drain; at 1000 vpt × 100 hz it can't.
- **Zenoh (`zenoh-1000x100hz-qos3`)**: alice emits `eot_sent`, but the
  matching bob's log file is **truncated mid-write** at seq ~1432 of
  ~30 k. Bob's variant child crashed (or was killed) during the
  operate phase — not the EOT phase. So Zenoh's high-rate row isn't a
  "TCP queue overflow" story like hybrid's; it's a variant-stability
  story at sustained 100 K writes/s. Worth investigating: did Zenoh
  OOM, panic, or get killed by the OS scheduler?
- **WebSocket / WebRTC**: no EOT override yet, so the default no-op
  fires `eot_timeout` unconditionally. Not load-related — just missing
  trait impl. Filed in E12 (T12.4, T12.5).

#### Other failure modes

1. **`workload = max-throughput` is a saturation probe**, not a normal
   profile. Across all transports the `*-max-*` rows show 75–99 %
   loss and tens-of-seconds p50. Useful as a saturation ceiling.

2. **WebSocket fully broken on one host** — every `websocket-*` spawn
   shows 0 writes/s and 100 % loss. Two same-machine processes can't
   both bind the WebSocket server port. WebSocket can only be
   evaluated in a real two-machine setup.

3. **WebRTC has signaling fragility** — many high-rate WebRTC spawns
   produce no data at all (`0 ms / 0 writes`) because the DataChannel
   handshake didn't complete before the operate window opened. Spawns
   that *do* connect look reasonable (1–5 ms p50).

4. **Latency labelled "(uncorrected)"** on most same-machine rows —
   the clock-sync engine produced too few samples per variant for the
   timestamp pipeline to apply skew correction. On a single host the
   wall-clock should be identical anyway, so the numbers are still
   directionally correct; the absolute high-end numbers should be
   re-measured once the resync sample-count gap is fixed.

### 5.2 Two-machine run (2026-05-07, log_subdir `two-machines-…_093412`)

Alice's logs only — bob's logs from the second host were never copied
back to this machine. The analysis tool can therefore only compute
`alice → alice` self-paths, which makes the cross-machine comparison
**incomplete**. Reporting what's there for completeness, with that
caveat:

- Variants with broadcast / multicast architectures (zenoh, hybrid,
  custom-udp's multicast send path) produce some self-delivery rows
  with realistic numbers — `zenoh 100x100hz-qos2` reports 0.42 ms
  p50, 0.75 ms p95, 0 % loss on alice → alice. Suggests the in-process
  Zenoh path is fast even when crossing the publish/subscribe cycle.
- Pure point-to-point variants (quic, websocket, the unicast paths of
  custom-udp / hybrid) report 0 ms / 100 % "loss" because alice's
  log has no matching receiver — bob is simply absent from the
  dataset.
- Connect times are universally ~500 ms higher than same-machine,
  matching the cross-machine TCP/QUIC handshake cost.

**Action items before any Zenoh recommendation can be defended:**
1. Copy bob's logs from the second machine into this folder and re-run
   `analyze.py`. Without them the two-machine run cannot make a real
   cross-machine claim.
2. Re-run end-to-end now that T-coord.3 + T-coord.1b are landed and
   T-coord.4 (4262-byte resume-manifest > 4096-byte recv buffer) is
   filed. A clean run should complete without the 9-spawn hang we hit
   on 2026-05-07.
3. Investigate the Zenoh-bob mid-operate truncation at 1000 vpt to
   distinguish "Zenoh saturates" from "Zenoh crashes."

## 6. Findings — divergences from expectation

What we assumed when we built the benchmark vs what the matrix
actually showed us:

1. **Assumed**: same-machine = ground truth, no noise. **Measured**:
   loopback drops UDP at sustained 100 K+ pkt/s; TCP overload looks
   like loss because the data is queued in the kernel and the EOT
   drain budget (= operate_secs = 30 s) is too short to clear it.

2. **Assumed**: the EOT phase would close the "data still in kernel
   queue at operate-end" gap, so reliable QoS would report ~100 %
   delivery up to the saturation point. **Measured**: EOT *runs* for
   every variant that implements it, but at 100 K writes/sec on
   hybrid-TCP it `eot_timeout`s because the EOT marker is queued
   behind 100 K msg/s × 30 s = ~3 M unacknowledged messages in TCP.
   Lengthening `--eot-timeout-secs` or sending EOT over an
   out-of-band control channel would help; the current "send EOT
   over the data path" doesn't.

3. **Assumed**: WebSocket would be a useful control — well-understood
   reliable transport. **Measured**: unusable on a single host
   because each peer wants a server port and they collide. Real-world
   deployment isn't single-host, so this is a benchmark-harness
   limitation, not a protocol verdict.

4. **Assumed**: Clock sync would just work and we'd get skew-corrected
   latencies for free. **Measured**: many spawns produce too few
   samples for the corrector to apply, so we're reading "(uncorrected)"
   timestamps on critical rows. Need to investigate the per-variant
   resync sample count under load.

5. **Assumed**: A failed spawn would be obvious. **Measured**: many
   spawns ran their full window and produced log files but with
   `0 writes / 0 ms` because the variant's own setup phase didn't
   finish in time (webrtc signaling, websocket port bind). Worse, the
   Zenoh 1000 vpt rows show **bob's log truncated mid-write** — the
   variant child crashed (or was killed) under load, mid-`operate`.
   Need a `connection_failed` / `child_died` event so analysis can
   distinguish "ran and lost packets" from "couldn't even start" from
   "crashed under load."

6. **Assumed**: Two-machine results would just take a manifest
   exchange and a log copy. **Measured**: (a) bob's logs need to
   physically move back to alice's machine; we don't yet have an
   automated step for that. (b) The resume-manifest message size for
   176 completed jobs is 4262 bytes against a 4096-byte recv buffer
   — the runner's manifest-exchange path needs the buffer raised
   before any cross-machine resume actually round-trips. (Filed:
   T-coord.4 in `metak-orchestrator/TASKS.md`.)

## 7. Where this leaves us — Zenoh validation

**The candidate verdict, in one paragraph:**

At the realistic LAN rate (10 k writes/sec aggregate) Zenoh delivers
100 % of messages but at **p99 ≈ 170 ms**, which is **~17× above our
sub-10 ms target**. At our headline target rate (100 k writes/sec
aggregate) Zenoh saturates to ~26 % delivery, the same saturation
profile every other transport hits on this hardware. Zenoh is
*correct* and *operationally simple* — discovery, QoS, recovery all
work out of the box — but it is meaningfully slower than QUIC (~40×
on p50 at the realistic rate) and Custom UDP (~6× on p50). The
saturation point is no worse than the alternatives.

**Pre-conditions before this verdict can be defended publicly:**

1. **Re-measure with two-machine logs collected from both sides.** The
   same-machine numbers above carry the "(uncorrected)" caveat and
   the loopback-loss caveat; only a LAN run with both peers' logs
   establishes the real-world Zenoh ranking.
2. **Investigate the Zenoh-bob mid-operate truncation at 1000 vpt.**
   If Zenoh's child crashed/OOM'd under sustained load, that's a
   reliability concern for the candidate that we need to characterise
   before recommending. If it was simply the runner timeout firing,
   the row is recoverable with a longer per-variant timeout.
3. **Decide whether to lengthen `--eot-timeout-secs` or move EOT
   off-band.** Until then, the high-rate reliable rows reflect EOT
   timing, not transport performance, and shouldn't be presented as
   either a Zenoh strength or weakness.

**What we can confidently say *today*:**

- Zenoh delivers correctly (100 % completeness at 10 k writes/sec,
  including reliable QoS) — *the candidate passes the correctness bar*.
- Zenoh saturates at roughly the same point every alternative does on
  this hardware — *the candidate has no obvious throughput floor*.
- Zenoh's latency is ~30 ms p50 at 10 k writes/sec, ~430 ms p50 at
  saturation — *the candidate misses our sub-10 ms p99 target* and
  would force a re-think of either the target or the candidate.

**What the alternatives tell us about the cost of choosing Zenoh:**

- QUIC sits ~40× below Zenoh on p50 at the realistic rate with the same
  correctness and substantially more complexity. If sub-10 ms p99 is a
  hard requirement, QUIC is in the running.
- Custom UDP sits ~6× below Zenoh and is the absolute floor; it is also
  the most code to maintain and the least feature-complete (no
  discovery, no recovery, no built-in QoS abstraction beyond what we
  wrote).
- Hybrid, WebSocket, WebRTC are not credible alternatives at this
  point given the benchmark-harness limitations and the framing-tax
  costs observed.

## 8. Suggested slide flow

Five-minute read; nine slides. See `metak-shared/slides.md`.

1. **Title + the question** — "How much does the transport choice matter?"
2. **Setup** — two runners, six variants, four QoS, 176 spawns.
3. **Variants** — one line each, grouped as in §2.
4. **QoS matrix** — the table from §3 (variants × QoS levels).
5. **What we measure** — latency p50/p95/p99, throughput, loss,
   jitter, connect time, resources.
6. **Results — what works** — sub-ms / sub-25 ms p99 at standard
   rates for custom-udp + quic.
7. **Results — where it breaks** — UDP loopback drop, TCP-overload
   tail, websocket / webrtc setup fragility on one host.
8. **Findings / divergences from expectation** — the six bullets in §6.
9. **Where this leaves us** — confident claims + open questions
   from §7.
