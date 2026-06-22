# Distributed Data Replication — Presentation Brief

A reference for building slides on the variants we benchmark, the QoS
levels each supports, the dimensions we sweep, and what we measure.

> **Status of the numbers (read first).** The figures in §5 are from
> two **full-matrix runs in May 2026**, both under the current benchmark
> design (no-skip QoS, threading modes, all workload shapes):
>
> - **Same-machine** (loopback), `same-machine-all-variants-01-20260521_210958`.
> - **Two-machine** over **WiFi 2.4 GHz**,
>   `two-machines-wifi24g-all-variants-01-20260523_083845` — both peers'
>   logs present, so cross-machine delivery is true pairwise.
>
> Both live under `C:\repo\shared\ddd\` with full analysis output
> (`analysis/summary_*.md` + PNG charts). Latencies are reported as
> **mean ± std** (the pivot tables' unit); per-percentile breakdowns are
> in each run's `summary_performance.md`. A wired-GbE run and the
> receive-throughput-headline pivot are still ahead (see §6).

## 1. The benchmark in one paragraph

**Zenoh is our chosen transport — the goal is to characterize how it
performs and where its limits are, not to decide whether to adopt it.**
We replicate a single-writer, key-value tree across nodes on a LAN at
~100 K updates/sec aggregate with sub-10 ms latency, and we want a
defensible picture of Zenoh's envelope: where it meets our targets,
where it doesn't, and what it costs us. Five other variants are
*reference baselines*, not candidates: they bracket what's achievable on
this hardware — from the performance floor (Custom UDP — minimum
framework overhead) up through alternative framings (Hybrid UDP/TCP,
QUIC, WebSocket, WebRTC). They all solve the **same problem** and run
through the **same workload**, so Zenoh's numbers can be read in context
— how close it runs to the floor, and how much headroom we trade for its
simplicity.

## 1a. The benchmark matrix

A single run sweeps the cross-product of five dimensions. Each
combination is one **spawn** with its own stabilize/operate/silent
cycle, and its own log files. The current
`configs/two-runner-all-variants.toml` expands to **704 spawns**.

| Dimension | Values |
|---|---|
| **Variant** | zenoh, custom-udp, quic, hybrid, websocket, webrtc |
| **Rate** `vpt × tick_rate_hz` | `10×100`, `10×1000`, `100×10`, `100×100`, `100×1000`, `1000×10`, `1000×100`, plus a `max` saturation probe |
| **Workload shape** | scalar-flood, block-flood, mixed-types |
| **QoS** | 1–4 (WebSocket is 3–4 only, by design) |
| **Threading mode** | single (sync, no tokio) / multi (per-peer reader threads) |

Not every combination exists for every variant:

- **Async-only variants (quic, zenoh, webrtc)** declare
  `supported_modes = ["multi"]` — their single-threaded expansions are
  skipped, because they fundamentally rely on an async runtime.
  Claiming a "single-threaded QUIC/Zenoh/WebRTC" number would be
  misleading.
- **TCP-family variants (custom-udp, hybrid, websocket)** run in both
  threading modes — this is the WASM-relevant comparison (see §1c).
- **WebSocket** runs QoS 3–4 only (it is TCP-based; there is no
  unreliable mode).

Spawn names encode the dimensions, e.g.
`zenoh-1000x100hz-scalar-qos1-multi`.

## 1b. Workload glossary

- **What is a "write"?** A *write* (a `WriteOp`) is one update to a
  single path in the replicated key-value tree: a `Variant::try_publish`
  of one **`arora_types::Value`** — the project's universal data type, a
  35-variant enum spanning scalars, typed arrays, and nested key-value
  structures (source: `semio-ai/arora-types`). The transport ships it as
  one logical message.
- **`tick_rate_hz`** — how many times per second the writer wakes up to
  emit a batch. 100 hz = 10 ms between ticks, 1000 hz = 1 ms.
- **`values_per_tick` (`vpt`)** — total **leaf scalar values** emitted
  per tick. This is the comparable denominator across all workload
  shapes; the shapes differ only in *how* those leaves are packed into
  writes. In **scalar-flood** (the profile behind the headline rates)
  each leaf is its own write — one scalar `arora_types::Value` — so
  `vpt` equals the number of `arora_types::Value` updates per tick
  (e.g. `100x100hz` = 100 values/tick × 100 ticks/s = **10 k value
  updates/sec**). In block-flood / mixed-types the same leaf count is
  packed into fewer, composite `arora_types::Value` writes.
- **Aggregate write rate** = `vpt × tick_rate_hz`. So `100x100hz` is
  10 k writes/sec; `1000x100hz` is 100 k writes/sec; `max` targets
  ~1 M writes/sec and is intentionally above what loopback can sustain
  — a saturation probe, not a "normal" measurement.
- **Workload shapes** (E19):
  - **scalar-flood** — `vpt` distinct single-scalar write ops per tick.
    The per-message-overhead extreme.
  - **block-flood** — `vpt / blob_size` write ops per tick, each a
    fixed-size block of scalars. Stresses serialization and
    large-message handling.
  - **mixed-types** — a heterogeneous tree per tick (scalars + arrays +
    nested dicts) totalling `vpt` leaves. Stresses the full nested
    serialization path. Deterministic via a fixed `workload_seed`.
  - Realistic robotics/sensor workloads sit in the middle (a handful of
    structured blocks per tick), which is why block/mixed exist
    alongside scalar-flood's extreme.
- **Per-spawn lifecycle**: `connect` → `stabilize 3 s` → `operate 30 s`
  → `silent 3 s`. Interesting metrics are computed over the `operate`
  window; the operate window is bounded by the writer's `eot_sent`
  marker (see §1d).

## 1c. Threading-mode dimension (E14) and the WASM motivation

Some variant crates are intended to compile to **WASM** for the team's
production scenarios. Browser-WASM does not support multi-threaded
async runtimes, so **single-threaded synchronous operation is a
first-class requirement**, not a fallback. The benchmark therefore
measures each capable variant under both regimes:

- **single** — sync, no tokio; the WASM-compatible path.
- **multi** — per-peer reader threads draining into bounded channels;
  the conventional high-throughput path.

This matters because the receive side, not the writer, is the bottleneck
(see §4). Single-threaded variants must still drain incoming traffic on
the same thread that does everything else.

## 1d. Termination and the operate window (E15 — replaces the old EOT phase)

> Earlier revisions of this brief described an **on-wire end-of-test
> (EOT) handshake** where each variant broadcast an EOT marker over its
> transport and waited up to 30 s for peers. **That mechanism was
> removed (E15).** Any slide or note referencing "EOT timeout because
> the marker queued behind the data" describes the old architecture.

Today, termination is **runner-coordinated and activity-based**:

- Each variant emits a one-line-per-second **progress event to stdout**
  (`sent`, `received`, `phase`). The runner reads it.
- The runners exchange per-spawn progress over their coordination
  channel. When every runner reports its variant has been idle (no new
  sends and no new receives) for a few seconds during `operate`, they
  agree the operate phase is naturally done and the variant advances to
  `silent`.
- A per-spawn `max_spawn_secs` wall-clock budget remains only as a
  safety net.
- The variant still writes a single **`eot_sent` marker to its log**
  when its writer finishes operate. The analysis tool uses that marker
  to bound the operate window. There is no longer any on-wire EOT
  exchange to time out.

## 2. The variants

Six implementations, **all implemented and exercised**. Zenoh is the
chosen transport under study; the other five are reference baselines,
not alternatives we'd switch to. (Aeron was evaluated in E0 but
permanently excluded — Windows C-FFI toolchain blocker — and is not in
the lineup.)

### The transport under study

- **Zenoh** — a Rust-native pub/sub framework with built-in zero-conf
  discovery and configurable QoS. *This is the transport we've chosen;
  the study maps its performance envelope and limits.* We want to know:
  what delivery rate, latency, and tail behaviour does Zenoh actually
  give us against our sub-10 ms p99 / ~100 K writes/sec targets, at what
  resource cost, and where does it break down. As of T9.5d the variant
  uses **Zenoh-native QoS only** (the prior application-level
  credit/ack/dedup wrapper was removed), so its reliable-QoS behaviour
  under sustained load is being re-characterised.

### Performance-floor baseline (hand-rolled, minimum overhead)

- **Custom UDP** — raw `UdpSocket` plus a hand-written protocol. UDP
  multicast for fan-out, unicast NACKs for recovery, mDNS for discovery.
  Implements all four QoS levels at the application layer. *Establishes
  the latency / throughput floor.*

### Alternative-framework baselines (comparable abstractions)

- **Hybrid UDP/TCP** — UDP multicast for unreliable QoS (1–2), one TCP
  connection per peer pair for reliable QoS (3–4). Kernel handles
  retransmission/ordering on the reliable side. *Sanity check on "would
  a simple hand-rolled hybrid have been good enough?"*

- **QUIC (quinn)** — UDP-based, multiplexed reliable streams plus
  unreliable datagrams, mandatory TLS 1.3. Streams map cleanly to QoS
  levels. *Compares Zenoh against a modern, low-overhead, encrypted
  transport.*

- **WebSocket** — TCP with WebSocket framing on top. Reliable QoS only
  (3–4); refuses unreliable QoS by design. *Isolates the cost of
  WebSocket framing on top of raw TCP (compare to Hybrid's TCP at QoS 4).*

- **WebRTC DataChannels** — SCTP-over-DTLS-over-UDP. Each DataChannel is
  configurable for ordered/unordered + reliable/`maxRetransmits=0`,
  giving native support for all four QoS levels. *Compares Zenoh against
  a heavier off-the-shelf reliable+unreliable mux.* (Known limit: the
  implemented variant supports exactly one peer per spawn — fits the
  two-runner case.)

### How the baselines bracket Zenoh

The baselines are reference points, not options on the table. Each
comparison quantifies *where Zenoh sits*, not whether to replace it.

| Direction | Variant | What the comparison quantifies |
|---|---|---|
| ↓ floor | Custom UDP | How far Zenoh runs from minimum-overhead — the framework tax we accept |
| ≈ peer | Hybrid, QUIC | Whether a mature framework's latency is in the same class as Zenoh's, or a tier away |
| ↑ ceiling | WebSocket, WebRTC | How much headroom Zenoh has over the heaviest off-the-shelf stacks |

## 3. The four QoS levels

QoS is configured **per subtree branch** by the writer that owns it. A
single tree can carry all four levels simultaneously.

| Level | Name | Transport intent | Ordering | Loss behaviour |
|---|---|---|---|---|
| 1 | Best-Effort | UDP, fire-and-forget | None | Tolerated, ignored |
| 2 | Latest-Value | UDP, seq-tagged | Latest-wins (drop stale) | Tolerated, skipped |
| 3 | Reliable-UDP | UDP + NACK | Strict | Recovered (lags) |
| 4 | Reliable-TCP | TCP (or equivalent) | Strict | Recovered (kernel) |

### The Strict No-Skip Contract for QoS 3/4 (E17 — important)

As of 2026-05-18, QoS 3 and QoS 4 **prioritise delivery over
throughput**:

- A variant **MUST deliver 100 % of accepted writes** at QoS 3/4 and
  **MUST NOT** silently drop at the publish path.
- If the send path is full (kernel buffer, app queue, congestion
  window, peer credit), the variant **blocks the writer** until the
  message is accepted or the spawn terminates.
- The acceptable failure mode under sustained overload at QoS 3/4 is
  **throughput collapse, not delivery shortfall**.
- QoS 1/2 keep the opposite priority: throughput/latency over delivery.
  Skipping is the contractual back-pressure mechanism there (the driver
  records `backpressure_skipped` and moves on).

**How this changes the reading of results:** at QoS 3/4, a saturated
variant now shows up as *low throughput* (and possibly a
self-terminated spawn), **not** as low delivery. Older numbers showing
"~26–36 % delivery at saturation" for reliable QoS reflect the
pre-contract behaviour and are no longer how the system behaves.

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

### Receive throughput is the headline metric (E14 / T11.5)

The project's stated goal is "keep multiple peers in sync under huge
change diffs with lowest latency possible." The metric that decides
whether peers are *in sync* is **receive throughput**, not write
throughput: writers ship at the requested rate almost always; receivers
face buffer pressure, parse cost, and application work, and are the
actual bottleneck. Summary tables now lead with receive throughput per
`(writer, receiver, variant, qos, threading_mode)`; write throughput is
the "requested rate" context column.

### Integrity (correctness)

- **Delivery rate** — `receives / writes` per (writer → receiver) pair.
  At L3/L4 we expect 100 % (per the no-skip contract); at L1/L2 we
  report whatever it is.
- **Ordering violations** — out-of-order receives on ordered QoS levels
  (2/3/4).
- **Duplicates** — same logical update received twice.
- **Unresolved gaps** — for L3, every detected gap must eventually be
  filled before the run ends.
- **`backpressure_skipped` at QoS 3/4** is now an integrity *failure*
  (it violates the no-skip contract).

### Performance

- **Replication latency** — per published write op, wall-clock from the
  writer's emit to the matching receive on every other node. Reported
  as **p50 / p95 / p99 / max**, with per-path and per-receiver
  breakdowns.
- **Throughput** — sustained receives/sec (headline) and writes/sec
  (context) during operate.
- **Jitter** — rolling standard deviation of latency.
- **Packet loss rate** — for QoS levels with sequence tracking (2/3/4);
  for L3 this is *transient* loss before recovery.
- **Connection time** — process start to "ready to publish". Mostly
  interesting for QUIC and WebRTC, where handshakes dominate cold start.
- **Resource usage** — CPU % and memory MB sampled during the run.

### Logging and how latency is computed

- Per-message observations are written in a **compact columnar format
  (Apache Parquet)**, not per-event JSONL (E18). Legacy per-event JSONL
  was removed (E19); JSONL is now **lifecycle-only** (`phase`,
  `connected`, `eot_sent`, `resource`, `clock_sync`).
- Latency is computed by correlating each write with its matching
  receive at every other node. The compact format carries no `seq`;
  correlation is **ordering-based** (the Nth write on a `(writer, path)`
  matches the Nth receive at a receiver), which is exact at QoS 3/4
  (strict order, no drops).
- Cross-machine clocks are reconciled via an **application-level,
  NTP-style 4-timestamp offset exchange** between runner pairs (E8) —
  **not** hardware PTP. Single-host runs share one wall-clock, so no
  correction is needed there.

## 5. Results

All figures are **scalar-flood at the realistic rate (10 k writes/s =
100 vpt × 100 hz), multi-threaded**. Each cell is **delivery % · mean
latency (ms)** across all four QoS levels (1 = best-effort, 2 =
latest-value, 3 = reliable-UDP, 4 = reliable-TCP). Per-percentile and
std breakdowns are in each run's `summary_performance.md`. WebSocket is
QoS 3/4 only. Hybrid/Custom-UDP multicast double-count on loopback; the
delivery figures are completeness (100 %). The rendered deck colours
these cells green→red (RdYlGn, matching the drop-rate heatmaps).

### 5.1 Same-machine (loopback, 2026-05-21)

| Variant | Q1 dlv | Q1 ms | Q2 dlv | Q2 ms | Q3 dlv | Q3 ms | Q4 dlv | Q4 ms |
|---|---|---|---|---|---|---|---|---|
| Custom UDP | 100 % | 5.0 | 100 % | 5.2 | 100 % | 5.7 | 100 % | 5.5 |
| Hybrid | 100 % | 5.9 | 100 % | 6.3 | 100 % | 1.3 | 100 % | 1.3 |
| QUIC | 100 % | 16.1 | 100 % | 13.8 | 100 % | 9.8 | 100 % | 5.2 |
| WebSocket | — | — | — | — | 100 % | 0.1 | 100 % | 0.1 |
| WebRTC | 94.8 % | 10.0 | 95.1 % | 10.1 | 100 % | 9.0 | 100 % | 6.8 |
| **Zenoh** | 100 % | 10.2 | 100 % | 10.3 | 100 % | 11.1 | 100 % | 11.5 |

At **100 k writes/s** (1000 vpt × 100 hz) same-machine: Zenoh and QUIC
still hold **100 %** delivery (Zenoh ~11.8 ms, QUIC ~3.6 ms); Custom-UDP
multi 100 % but ~45.7 ms; WebRTC collapses to ~47.5 %; single-threaded
Hybrid collapses to 78.7 % with multi-second latency. **No QoS 3/4
`backpressure_skipped` violations anywhere** — the no-skip contract
held.

### 5.2 Two machines, WiFi 2.4 GHz (2026-05-23)

Both peers' logs present → true pairwise delivery. The link is
deliberately constrained, so absolute latency reflects the network.
(QUIC and Hybrid carry heavy tails the mean understates — see
`summary_performance.md`.)

| Variant | Q1 dlv | Q1 ms | Q2 dlv | Q2 ms | Q3 dlv | Q3 ms | Q4 dlv | Q4 ms |
|---|---|---|---|---|---|---|---|---|
| Custom UDP | 100 % | 9.7 | 100 % | 10.0 | 100 % | 11.3 | 100 % | 8.2 |
| Hybrid | 100 % | 7.5 | 100 % | 7.2 | 100 % | 10.0 | 100 % | 10.0 |
| QUIC | 100 % | 11.3 | 100 % | 8.9 | 100 % | 13.9 | 100 % | 12.6 |
| WebSocket | — | — | — | — | 100 % | 3.3 | 100 % | 4.0 |
| WebRTC | 95.3 % | 6.8 | 95.6 % | 6.1 | 100 % | 7.9 | 100 % | 5.6 |
| **Zenoh** | 100 % | 8.0 | 100 % | 14.6 | 100 % | 7.7 | 100 % | 8.8 |

At **100 k writes/s** over WiFi: Zenoh holds **100 %** (~15.4 ms); QUIC
99.5 % (~11.2 ms). Custom-UDP and Hybrid keep delivery but latency
balloons to **seconds** (link buffering); WebRTC ~48 %; single-threaded
Custom-UDP QoS 4 → 0 %.

### 5.3 Zenoh operating envelope (two machines, WiFi 2.4 GHz, mean latency ms)

Zenoh-only, multi-threaded, across **workload shape × QoS × rate**, from
a **single consistent run** (`zenoh-all-20260619_132224`, two machines
WiFi 2.4 GHz, with the fixed receive-timestamping). All cells 100 %
delivery except `✗` (93–98 %). The deck renders this as a flame heatmap.

| Rate (vpt×hz) | Sc Q1 | Q2 | Q3 | Q4 | Bl Q1 | Q2 | Q3 | Q4 | Mx Q1 | Q2 | Q3 | Q4 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10×100hz | 1.9 | 7.0 | 1.8 | 1.7 | 2.2 | 2.1 | 1.9 | 2.0 | 5.1 | 2.2 | 1.7 | 1.9 |
| 10×1000hz | 5.0 | 5.0 | 3.9 | 4.5 | 4.2 | 4.3 | 4.2 | 4.5 | 4.0 | 4.0 | 3.9 | 5.9 |
| 100×10hz | 3.5 | 3.6 | 3.8 | 3.7 | 2.0 | 2.5 | 1.7 | 5.4 | 5.3 | 4.4 | 3.6 | 3.4 |
| 100×100hz | 4.3 | 4.4 | 4.3 | 4.1 | 4.5 | 2.2 | 1.9 | 2.0 | 6.5 | 4.9 | 4.4 | 4.3 |
| 100×1000hz | 11.1 | 11.1 | 14.1 | 10.8 | 5.3 | 5.4 | 5.4 | 4.6 | 573✗ | 599✗ | 193 | 211 |
| 1000×10hz | 12.7 | 11.1 | 11.7 | 12.4 | 2.7 | 2.6 | 2.4 | 2.2 | 993 | 998 | 1236 | 1213 |
| 1000×100hz | 11.9 | 12.7 | 12.6 | 15.3 | 2.8 | 2.9 | 2.4 | 2.6 | 961 | 1202✗ | 1652 | 1415 |

(Sc = scalar-flood, Bl = block-flood, Mx = mixed-types.) Decision
guidance:

- **Ideal** — scalar or block at **any rate**: ~2–15 ms, 100 % delivery,
  any QoS. Zenoh has **no low-rate or reliable-QoS latency penalty**.
- **Avoid** — mixed-types at high volume (≥ 1000 vpt, or ~100 k
  leaves/s): 0.2–1.6 s latency. Delivery mostly holds (only a couple of
  best-effort cells dip to ~93 %), so the mixed problem is **latency,
  not loss**. The fix is **block-flood packing**, which stays
  single-digit ms.

**Note — the earlier ×10hz "latency" was a benchmark harness bug, now
fixed.** Zenoh's multi-mode reader path pushed decoded updates onto an
mpsc and let the driver stamp `receive_ts` at its **per-tick drain**
(`variant-base` `record_receive` → `Utc::now()`), adding ~half a tick
(~50 ms @ 10 Hz). An express A/B (`zenoh-all-20260616_143733`) ruled out
publisher batching; the fix (commit `5401c93`) makes the Zenoh reader
thread stamp `receive_ts` **on arrival** (mirroring websocket). Validated
loopback p50 50.07 ms → 0.400 ms, then this full envelope re-run. Note:
the cross-variant tables (§5.1, §5.2) still show Zenoh's pre-fix cells,
so Zenoh's true latency there is ~5 ms lower than printed; a full
all-variants re-run would refresh those.

### 5.4 What the results say about Zenoh

- **Operationally simple** — discovery, QoS, recovery all out of the box.
- **Delivers 100 % across QoS 1–4** at the realistic rate, on loopback
  *and* real WiFi.
- **Holds up under stress** — 100 % delivery at 100 k writes/s, where
  WebRTC and single-threaded Hybrid collapse.
- **Most consistent latency in the field** — ~8–11 ms mean with a tight
  spread; steadier than QUIC, which is 100 % but jittery (±50–84 ms).
- **Limit — heterogeneous payloads at high rate.** The mixed-types
  workload at 100 k drives Zenoh latency into the **seconds**
  (same-machine `1000x100hz-mixed-qos1` ≈ 3208 ± 1208 ms; WiFi mixed
  QoS 3/4 delivery ~68–70 %). This is Zenoh's clearest weak spot.
- **Limit — multi-threaded only.** No native single-threaded / WASM path
  yet (router-RPC sidecar is the planned route).

Against the target: at the realistic rate Zenoh sits ~10 ms mean —
around the 10 ms goal (a strict sub-10 ms *p99* isn't guaranteed), with
full delivery.

> **Provenance caveat.** These two matrix runs (2026-05-21 / 05-23)
> predate the T9.5d removal of Zenoh's application-level QoS wrapper
> (2026-05-25). The reliable-QoS (3/4) figures above were therefore
> achieved with that wrapper in place. At the realistic 10 k rate this
> is not expected to matter, but Zenoh-native-only reliable behaviour at
> high reliable load (≳50 k msg/s) still needs confirming on a fresh run
> — it is the one place the current build could differ from these
> numbers.

## 6. How to replicate this benchmark

Anyone with two Windows machines on the same LAN can reproduce these
results.

**Prerequisites:** the Rust toolchain (`rustup`) and Python 3.12 with
the analysis deps (`polars`, `matplotlib`).

**1. Both machines — clone & build:**

```powershell
git clone https://github.com/semio-ai/distributed-data-demos.git
cd distributed-data-demos
cargo build --release
```

**2. Network.** Put both machines on the same subnet. For a WiFi test,
leave *only* the WiFi adapter up — a second active NIC (e.g. Ethernet on
the same subnet) makes the discovery multicast bind to the wrong
interface and the peers won't find each other:

```powershell
Disable-NetAdapter -Name "Ethernet" -Confirm:$false   # WiFi tests only; Enable-NetAdapter after
```

**3. Run** — one machine each, logs pointed at a folder both can write.
The runners agree on the same `<run>-<timestamp>` subfolder name, so a
shared `--log-dir` auto-collects both peers into one run:

```powershell
# machine A
target\release\runner.exe --name alice --config configs\two-runner-all-variants.toml --log-dir z:\shared\ddd
# machine B
target\release\runner.exe --name bob   --config configs\two-runner-all-variants.toml --log-dir z:\shared\ddd
```

`two-runner-all-variants.toml` runs all six variants;
`two-runner-zenoh-all.toml` is the Zenoh-only subset. With no shared
drive, use a local `--log-dir` on each machine and copy bob's files into
alice's run folder before analyzing.

**4. Analyze** (either machine, once the run finishes):

```powershell
python analysis\analyze.py z:\shared\ddd\<run-folder> --summary --dump --diagrams --output z:\shared\ddd\<run-folder>\analysis
```

**Worth comparing across links.** Run the identical matrix over **WiFi
2.4 GHz**, **WiFi 5 GHz**, and **wired gigabit**, changing only the link
between runs. Comparing the three separates the transport's own
behaviour from what the network imposes — and is the natural way to see
how much headroom a better link buys.

## 7. Suggested slide flow

See `metak-shared/slides.md` and the rendered `metak-shared/presentation.html`.

1. **Title + abstract** — "How does Zenoh perform, and where are its
   limits?" (Zenoh is chosen; baselines bracket what's achievable.)
2. **Setup** — two runners, six variants, the five-dimension matrix.
3. **Variants** — one line each, grouped as in §2.
4. **QoS matrix + the no-skip contract** — §3.
5. **What we measure** — receive throughput as headline; integrity;
   performance; compact logs + NTP clock sync.
6. **Termination model** — runner-coordinated, activity-based (§1d).
7. **Results — same-machine** — §5.1.
8. **Zenoh operating envelope** — shape × QoS × rate heatmap; ideal /
   watch / avoid (§5.3).
9. **Results — two machines (WiFi)** — §5.2.
10. **What we can say about Zenoh** — strengths + limits (§5.4).
11. **Replicate it yourself** — two-machine steps + commands; compare
    2.4 GHz / 5 GHz / wired gigabit (§6).
12-15. **Appendix** — all variants × all four QoS, one flame table per
    QoS (§8).

## 8. Appendix — all variants across all four QoS

Mean latency (ms), scalar-flood, multi-threaded, two machines WiFi
2.4 GHz (`two-machines-wifi24g-all-variants-01-20260523_083845`).
`✗` = delivery < 100 %. `†` hybrid/custom-udp multicast double-counts
(delivery = completeness). `‡` Zenoh cells predate the receive-timestamp
fix (§5.3 has corrected Zenoh).

> **`⚠` the ×10hz columns carry a ~50 ms receive-timestamp artifact for
> every variant *except* WebSocket.** The benchmark stamps `receive_ts`
> at the driver's per-tick drain for all variants except WebSocket
> (which stamps on arrival in its reader thread) — so at 10 Hz the mean
> is inflated by ~half a tick. The same bug was fixed for Zenoh
> (commit `5401c93`, §5.3); the other variants still carry it at low
> rate. Columns ≥ 100 Hz are unaffected, and WebSocket's row shows the
> true single-digit-ms low-rate latency the others' ×10hz numbers mask.

### QoS 1 · Best-Effort

| Variant | 10×100 | 10×1000 | 100×10 ⚠ | 100×100 | 100×1000 | 1000×10 ⚠ | 1000×100 |
|---|---|---|---|---|---|---|---|
| Custom UDP | 6.6 | 5.1 | 52.5 | 9.7 | 1488 | 56.3 | 2613 |
| Hybrid † | 3.1 | 3.7 | 29.1 | 7.5 | 13.6 | 44.6 | 3990✗ |
| QUIC | 8.8 | 6.5 | 51.6 | 11.3 | 6.8 | 76.5 | 11.2 |
| WebSocket | — | — | — | — | — | — | — |
| WebRTC | 8.4 | 3.4 | 50✗ | 6.8✗ | 76✗ | 52✗ | 78✗ |
| Zenoh ‡ | 10.3 | 6.1 | 50.1 | 8.0 | 14.3 | 51.2 | 15.4 |

### QoS 2 · Latest-Value

| Variant | 10×100 | 10×1000 | 100×10 ⚠ | 100×100 | 100×1000 | 1000×10 ⚠ | 1000×100 |
|---|---|---|---|---|---|---|---|
| Custom UDP | 6.9 | 6.7 | 54.3 | 10.0 | 2277 | 73.9 | 3375 |
| Hybrid † | 3.1 | 3.6 | 29.1 | 7.2 | 11.9 | 44.0 | 6752✗ |
| QUIC | 12.8 | 6.8 | 58.8 | 8.9 | 7.0 | 54.7 | 10.5 |
| WebSocket | — | — | — | — | — | — | — |
| WebRTC | 8.2 | 2.8 | 50✗ | 6.1✗ | 77✗ | 51✗ | 79✗ |
| Zenoh ‡ | 6.3 | 6.1 | 51.1 | 14.6 | 12.3 | 76.8 | 15.5 |

### QoS 3 · Reliable-UDP

| Variant | 10×100 | 10×1000 | 100×10 ⚠ | 100×100 | 100×1000 | 1000×10 ⚠ | 1000×100 |
|---|---|---|---|---|---|---|---|
| Custom UDP | 7.0 | 7.3 | 54.0 | 11.3 | 21.2 | 62.3 | 77.2 |
| Hybrid | 5.4 | 4.8 | 55.2 | 10.0 | 304 | 69.2 | 252 |
| QUIC | 16.5 | 6.9 | 56.1 | 13.9 | 5.7 | 75.6 | 10.3 |
| WebSocket | 1.9 | 3.8 | 6.3 | 3.3 | 71.9 | 18.6 | 128 |
| WebRTC | 5.0 | 2.9 | 50.1 | 7.9 | 73.4 | 53.7 | 86.4 |
| Zenoh ‡ | 5.8 | 4.8 | 50.1 | 7.7 | 12.5 | 90.9 | 20.3 |

(Reliable QoS — 100 % delivery across the board, no skips.)

### QoS 4 · Reliable-TCP

| Variant | 10×100 | 10×1000 | 100×10 ⚠ | 100×100 | 100×1000 | 1000×10 ⚠ | 1000×100 |
|---|---|---|---|---|---|---|---|
| Custom UDP | 5.1 | 4.1 | 51.4 | 8.2 | 109✗ | 82.9 | 86✗ |
| Hybrid | 8.7 | 5.1 | 61.3 | 10.0 | 220 | 84.0 | 95.1 |
| QUIC | 7.5 | 10.6 | 56.4 | 12.6 | 7.6 | 54.5 | 10.4 |
| WebSocket | 1.8 | 8.9 | 4.5 | 4.0 | 87.0 | 18.3 | 132 |
| WebRTC | 8.0 | 3.2 | 50.3 | 5.6 | 79.1 | 51.6 | 83.7 |
| Zenoh ‡ | 9.4 | 5.4 | 54.4 | 8.8 | 12.5 | 50.7 | 15.8 |

Full delivery %s, percentiles, single-threaded mode, and the block-flood
/ mixed-types shapes are in each run's `analysis/summary_*.md`. A
notable read: **Custom-UDP and Hybrid buffer rather than drop** at
QoS 1/2 saturation (seconds of latency, ~100 % delivery), while
**WebRTC drops** (best-effort, ~25–55 %); at QoS 4 **Custom-UDP's TCP
path drops to ~50 %** at 100 k writes/s. QUIC and WebSocket hold low
latency + full delivery widest across the grid.
