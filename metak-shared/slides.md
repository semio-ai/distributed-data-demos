# Distributed Data Replication — Team Brief

> Async-readable. ~5 minutes end-to-end. Each slide stands alone;
> horizontal rules separate them. Detail in
> [`presentation-brief.md`](presentation-brief.md). The rendered deck is
> [`presentation.html`](presentation.html).
>
> **Status:** study design + results from the May 2026 full-matrix runs —
> same-machine (loopback, 2026-05-21) and two-machine (WiFi 2.4 GHz,
> 2026-05-23), all six variants, every dimension.

---

## Slide 1 — The question + abstract

> **How does Zenoh perform — and where are its limits?**

**Abstract.** We have settled on **Eclipse Zenoh** as the transport for
keeping multiple LAN peers in sync under large, high-frequency change
diffs. This study characterizes **how it performs and where its limits
are** — not whether to adopt it. We built **six interchangeable
implementations** of the same single-writer key-value replicator — Zenoh
plus five baselines (Custom UDP, Hybrid UDP/TCP, QUIC, WebSocket,
WebRTC) — behind one common interface, and run them all through an
**identical benchmark matrix** sweeping rate, workload shape, QoS level,
and threading mode. The baselines aren't candidates; they **bracket
what's achievable** so we can read Zenoh's numbers in context: how close
it runs to the performance floor (Custom UDP), and how much headroom we
trade for its simplicity at our **~100 K updates/sec, sub-10 ms p99**
targets.

The framing of the lineup: Zenoh is chosen because it works for us —
discovery, QoS, and recovery out of the box, no custom transport to
maintain. Custom UDP is the performance floor; Hybrid UDP/TCP, QUIC,
WebSocket, and WebRTC bracket what heavier framings cost. They exist to
contextualize Zenoh's numbers, not as alternatives we'd switch to.

---

## Slide 2 — The setup

- **Two runners** (alice, bob), leaderless coordination, same machine or
  LAN.
- **Six variants** of the replicator — Zenoh + 5 baselines (next slide).
- A **five-dimension matrix**, expanding to **704 spawns** in the full
  config:
  - **Variant** × **rate** (`vpt × tick_rate_hz`) × **workload shape**
    × **QoS** × **threading mode**.
- **Per-spawn lifecycle**: `connect` → `stabilize 3 s` → `operate 30 s`
  → `silent 3 s`. Metrics computed over `operate`; the operate window is
  bounded by the writer's `eot_sent` marker.
- **What's a "write"?** One update to a single path in the replicated
  tree — publishing one **`arora_types::Value`** (our universal data
  type: a 35-variant enum of scalars, typed arrays, and nested key-value
  trees). **`vpt` = how many such values per tick.**
- **Rate glossary**: `vpt × tick_rate_hz` = `arora_types::Value`
  updates/sec. In **scalar-flood** (the headline profile) each write is
  one scalar Value, so `100x100hz` = 100 values/tick × 100 = 10 k/s;
  `1000x100hz` = 100 k/s; `max` ≈ 1 M/s (saturation probe).
- **Workload shapes**: scalar-flood (one scalar Value per write),
  block-flood (each write a fixed block of values), mixed-types (nested
  scalar+array+struct Values). Same leaf count, different packing.
- **Threading modes**: single (sync, WASM-compatible) / multi (per-peer
  reader threads). Output: integrity + performance reports + diagrams.

---

## Slide 3 — The transport and the bracketing baselines

**The transport we're characterizing (already chosen)**
- **Zenoh** — Rust pub/sub framework, zero-conf discovery, Zenoh-native
  QoS. *We're mapping its envelope and limits, not deciding whether to
  use it.*

**Performance-floor reference**
- **Custom UDP** — raw `UdpSocket` + hand-written NACK protocol. The
  *floor* — how far Zenoh sits from minimum-overhead is the framework
  tax we accept.

**Bracketing baselines (context, not options we'd switch to)**
- **Hybrid UDP/TCP** — UDP multicast for QoS 1–2, kernel TCP for QoS 3–4.
- **QUIC (quinn)** — UDP multiplexed streams + datagrams + TLS 1.3.
- **WebSocket** — TCP + WebSocket framing, reliable QoS (3–4) only.
- **WebRTC DataChannels** — SCTP-over-DTLS-over-UDP, all four QoS levels.

All six are implemented and exercised. (Aeron was evaluated but
permanently excluded — Windows C-FFI blocker.) Async-only variants
(QUIC, Zenoh, WebRTC) run multi-threaded only; TCP-family variants run
both threading modes.

---

## Slide 4 — QoS matrix + the no-skip contract

| Lvl | Name | Transport | Ordering | Loss |
|---|---|---|---|---|
| 1 | Best-Effort | UDP, fire-and-forget | None | Tolerated |
| 2 | Latest-Value | UDP + seq filter | Latest-wins | Skipped |
| 3 | Reliable-UDP | UDP + NACK | Strict | Recovered |
| 4 | Reliable-TCP | TCP (or eq.) | Strict | Kernel-recovered |

Each variant runs all four levels (except **WebSocket**, L3/L4 only).

**Strict No-Skip Contract (QoS 3/4):** variants **must deliver 100 % of
accepted writes** and **block the writer** rather than drop. The
acceptable failure mode under overload is **throughput collapse, not
delivery shortfall**. QoS 1/2 keep the opposite priority (skip + log to
relieve back-pressure). *How to read it: a saturated reliable variant
shows up as low throughput, not low delivery.*

Full table in [§3 of the brief](presentation-brief.md).

---

## Slide 5 — What we measure

**Receive throughput is the headline metric** — receivers, not writers,
are the sync bottleneck.

**Integrity**
- Delivery rate per `(writer → receiver)` pair.
- Out-of-order receives (where ordering is required); duplicates; gaps.
- `backpressure_skipped` at QoS 3/4 = integrity failure (no-skip).

**Performance**
- Receive throughput (headline); write throughput (context).
- Latency p50 / p95 / p99 / max, per published write op.
- Jitter, loss %, connect time, CPU %, MemMB.

**Logging & latency:** per-message data is compact Parquet (JSONL is
lifecycle-only). Latency = matching each write to its receive by
**ordering** (no `seq`), exact at QoS 3/4. Cross-machine clocks
reconciled via an **app-level NTP-style offset exchange** (not PTP);
single-host runs share one clock.

---

## Slide 6 — How a spawn knows it's done

Termination is **runner-coordinated and activity-based**:

1. Each variant emits a 1 Hz **progress event to stdout** (`sent`,
   `received`, `phase`). The runner reads it.
2. Runners exchange per-spawn progress over their coordination channel.
3. When every runner reports its variant has been **idle** (no new sends
   or receives) for a few seconds during `operate`, they agree it's done
   and the variant moves to `silent`.
4. A per-spawn `max_spawn_secs` wall-clock budget is the safety net.

The variant writes a single **`eot_sent` marker to its log** to bound
the operate window, so the analysis tool scopes every metric to the same
well-defined window across variants.

---

## Slide 7 — Results: same-machine (loopback)

**Realistic rate — 10 k writes/s** (100×100hz scalar), multi-threaded.
Each cell: delivery % · mean latency (ms). In the rendered deck these
cells are flame-coloured (green = good → red = bad — the RdYlGn scheme
from the drop-rate heatmaps).

| Variant | Q1 dlv | Q1 ms | Q2 dlv | Q2 ms | Q3 dlv | Q3 ms | Q4 dlv | Q4 ms |
|---|---|---|---|---|---|---|---|---|
| Custom UDP | 100 % | 5.0 | 100 % | 5.2 | 100 % | 5.7 | 100 % | 5.5 |
| Hybrid † | 100 % | 5.9 | 100 % | 6.3 | 100 % | 1.3 | 100 % | 1.3 |
| QUIC | 100 % | 16.1 | 100 % | 13.8 | 100 % | 9.8 | 100 % | 5.2 |
| WebSocket | — | — | — | — | 100 % | 0.1 | 100 % | 0.1 |
| WebRTC | 94.8 % | 10.0 | 95.1 % | 10.1 | 100 % | 9.0 | 100 % | 6.8 |
| **Zenoh** | 100 % | 10.2 | 100 % | 10.3 | 100 % | 11.1 | 100 % | 11.5 |

QoS 1 = best-effort, 2 = latest-value, 3 = reliable-UDP, 4 =
reliable-TCP. † Hybrid/Custom-UDP multicast double-counts on loopback;
completeness is 100 %.

**At saturation (100 k writes/s):** Zenoh and QUIC still hold **100 %**
delivery (~12 ms / ~4 ms); Custom-UDP 100 % but ~46 ms; WebRTC drops to
~48 %; single-threaded Hybrid collapses (79 %, multi-second). QoS 3/4
honoured the no-skip contract everywhere — **zero dropped writes**.

_Same-machine loopback, 2026-05-21 · latency = mean over the operate
window._

---

## Slide 8 — Zenoh's operating envelope (two machines, WiFi 2.4 GHz)

Mean latency (ms), Zenoh multi-threaded, across **workload shape × QoS ×
rate** — flame-coloured in the rendered deck (green = fast → red = slow).
All cells 100 % delivery except `✗` (68–97 %). ⚠ = under review (see
note).

| Rate (vpt×hz) | Sc Q1 | Q2 | Q3 | Q4 | Bl Q1 | Q2 | Q3 | Q4 | Mx Q1 | Q2 | Q3 | Q4 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10×100hz | 10.3 | 6.3 | 5.8 | 9.4 | 9.0 | 10.5 | 10.2 | 9.8 | 6.2 | 6.0 | 5.5 | 5.6 |
| 10×1000hz | 6.1 | 6.1 | 4.8 | 5.4 | 6.2 | 6.2 | 5.3 | 5.4 | 5.5 | 5.9 | 4.8 | 4.9 |
| 100×10hz ⚠ | 50.1 | 51.1 | 50.1 | 54.4 | 50.0 | 49.9 | 50.1 | 50.0 | 50.2 | 99.1 | 50.2 | 63.3 |
| 100×100hz | 8.0 | 14.6 | 7.7 | 8.8 | 5.9 | 7.2 | 9.8 | 6.5 | 11.1 | 10.6 | 7.3 | 7.0 |
| 100×1000hz | 14.3 | 12.3 | 12.5 | 12.5 | 6.1 | 5.9 | 5.1 | 5.3 | 601✗ | 587✗ | 415 | 468 |
| 1000×10hz ⚠ | 51.2 | 76.8 | 90.9 | 50.7 | 49.8 | 53.6 | 49.9 | 75.1 | 812 | 825 | 1010 | 1303 |
| 1000×100hz | 15.4 | 15.5 | 20.3 | 15.8 | 10.1 | 6.8 | 5.9 | 8.6 | 1053✗ | 1146✗ | 3145✗ | 2942✗ |

- 🟢 **Ideal** — scalar or block, tick ≥ 100 Hz → ≤ ~15 ms, 100 %
  delivery, any QoS.
- 🔴 **Avoid** — mixed-types at ≥ 1000 vpt: 0.4–3.1 s latency, and
  delivery drops to 68–97 % — even reliable QoS 3/4 can't hold it on a
  real link.
- ⚠ **Under review** — the 10 Hz (×10hz) rows: low-rate Zenoh shows a
  clean ~one-tick median latency that Custom-UDP doesn't. Likely Zenoh
  publisher batching / flush timing; being confirmed by a targeted
  re-run before we quote it.

Shape and rate dominate. The real danger is mixed-types at high vpt —
and on a real link reliable QoS doesn't rescue it. The fix is
**block-flood packing**, which stays fast and complete everywhere. Two
machines, WiFi 2.4 GHz, 2026-05-23.

---

## Slide 9 — Results: two machines (WiFi 2.4 GHz)

**Realistic rate — 10 k writes/s**, real LAN, both peers logged (true
pairwise). Each cell: delivery % · mean latency (ms); flame-coloured in
the rendered deck.

| Variant | Q1 dlv | Q1 ms | Q2 dlv | Q2 ms | Q3 dlv | Q3 ms | Q4 dlv | Q4 ms |
|---|---|---|---|---|---|---|---|---|
| Custom UDP | 100 % | 9.7 | 100 % | 10.0 | 100 % | 11.3 | 100 % | 8.2 |
| Hybrid † | 100 % | 7.5 | 100 % | 7.2 | 100 % | 10.0 | 100 % | 10.0 |
| QUIC | 100 % | 11.3 | 100 % | 8.9 | 100 % | 13.9 | 100 % | 12.6 |
| WebSocket | — | — | — | — | 100 % | 3.3 | 100 % | 4.0 |
| WebRTC | 95.3 % | 6.8 | 95.6 % | 6.1 | 100 % | 7.9 | 100 % | 5.6 |
| **Zenoh** | 100 % | 8.0 | 100 % | 14.6 | 100 % | 7.7 | 100 % | 8.8 |

† Multicast double-counts on loopback paths; completeness is 100 %.
Mean latency shown — QUIC and Hybrid carry heavy tails (high variance)
the mean doesn't capture.

**At saturation (100 k writes/s) over WiFi:** Zenoh holds **100 %**
delivery (~15 ms); QUIC 99.5 % (~11 ms). Custom-UDP / Hybrid keep
delivery but latency balloons to **seconds** (buffering on the
constrained link); WebRTC ~48 %; single-threaded Custom-UDP QoS 4 → 0 %.

_Two machines over WiFi 2.4 GHz, 2026-05-23 — a constrained link, so
absolute latency reflects the network. Zenoh's tail is the tightest in
the field here._

---

## Slide 10 — What we can say about Zenoh

- ✓ **Operationally simple** — discovery, QoS, recovery all out of the
  box, no transport code.
- ✓ **Delivers 100 % across QoS 1–4** at the realistic 10 k rate — on
  loopback *and* on real WiFi.
- ✓ **Holds up under stress** — 100 % delivery at 100 k writes/s, where
  WebRTC and single-threaded Hybrid collapse.
- ✓ **Most consistent latency in the field** — ~8–11 ms mean with a tight
  spread; steadier than QUIC's jitter (±50–84 ms).
- ✗ **Limit — heterogeneous payloads at high rate**: the mixed-types
  workload at 100 k drives Zenoh latency into the **seconds** (and
  ~68–70 % delivery on WiFi). Its clearest weak spot.
- ✗ **Limit — multi-threaded only**: no native single-threaded / WASM
  path yet (the router-RPC sidecar is the planned route).

Against our targets: at the realistic rate Zenoh sits ~10 ms mean —
right around the 10 ms goal (a strict sub-10 ms *p99* isn't guaranteed),
with full delivery.

---

## Slide 11 — What's next

1. **Close the mixed-types latency gap.** Zenoh's clearest limit — pin
   down whether it's batching, congestion control, or serialization, and
   fix it.
2. **Run on a wired link (GbE).** WiFi 2.4 GHz floors latency; a wired
   run isolates Zenoh's true tail from the network and confirms the
   sub-10 ms picture.
3. **Add a single-threaded / WASM path** for the deployments that need
   it (Zenoh router-RPC sidecar).
4. **Write deployment guidance.** Where sub-10 ms matters, which QoS /
   threading / topology to use — the baselines quantify the trade.

**Bottom line:** Zenoh meets our needs at the realistic rate — 100 %
delivery, ~10 ms latency on loopback and real WiFi — and degrades
gracefully under stress where others collapse. Its main limit is
high-rate heterogeneous payloads. The open work is tuning and deployment
guidance, not re-selection.
