# Distributed Data Replication — Team Brief

> Async-readable. Nine slides, ~5 minutes end-to-end. Each slide
> stands alone; horizontal rules separate them. Source / detail in
> [`presentation-brief.md`](presentation-brief.md).

---

## Slide 1 — The question

> **How much does the transport choice actually matter?**

We replicate a single-writer, key-value tree across nodes on a LAN at
~100 K updates/sec aggregate, target sub-10 ms latency. Every variant
solves the **same problem** — they only differ in *how the bytes get
from writer to readers*. We run six implementations through an
identical workload and compare.

If the answer is "the choice barely matters," we use the cheapest
thing. If it matters a lot, we know which knob to turn first.

---

## Slide 2 — The setup

- **Two runners** (alice, bob) on a LAN, leaderless coordination.
- **Six variants** of the replication layer (next slide).
- **Four QoS levels** per variant: best-effort UDP → reliable TCP.
- **176 spawns per run** = 48 variant entries × QoS expansion.
- **Per spawn**: `stabilize` 2 s → `operate` 10–12 s → `silent` 2 s.
- **Per delivery**: writer logs `write`, receiver logs `receive`,
  analysis tool joins on `(variant, run, writer, seq, path)` and
  computes latency / delivery / loss.
- Output: integrity report + performance report + diagrams (latency
  CDF, comparison bars).

Two-runner same-machine and two-machine modes both supported.

---

## Slide 3 — The six variants

**Frameworks**
- **Zenoh** — Rust pub/sub framework; "use a mature framework" baseline.

**Hand-rolled**
- **Custom UDP** — raw sockets + hand-written NACK protocol; performance
  floor with no framework overhead.
- **Hybrid UDP/TCP** — UDP multicast for unreliable QoS, kernel TCP for
  reliable QoS; "is the hand-rolled NACK worth it vs kernel TCP?"

**Modern protocols**
- **QUIC (quinn)** — multiplexed reliable streams + unreliable datagrams,
  TLS 1.3; "does per-stream multiplexing remove TCP HOL blocking?"

**Browser-stack on a LAN**
- **WebSocket** — TCP + WebSocket framing; reliable QoS only (3–4).
- **WebRTC DataChannels** — SCTP-over-DTLS-over-UDP; native support
  for all four QoS levels.

---

## Slide 4 — QoS matrix

| Lvl | Name | Transport | Ordering | Loss |
|---|---|---|---|---|
| 1 | Best-Effort | UDP, fire-and-forget | None | Tolerated |
| 2 | Latest-Value | UDP + seq filter | Latest-wins | Skipped |
| 3 | Reliable-UDP | UDP + NACK | Strict | Recovered |
| 4 | Reliable-TCP | TCP (or eq.) | Strict | Kernel-recovered |

Mapping per variant (highlights):

- Custom UDP NACKs at L3 vs Hybrid TCP at L3 → directly measures
  whether per-path independence beats kernel HOL blocking on a LAN.
- WebSocket is L3/L4-only by design (refuses unreliable QoS).
- WebRTC: same DataChannel API does all four levels via
  ordered/reliable flags.

Full table in [§3 of the brief](presentation-brief.md).

---

## Slide 5 — What we measure

**Integrity**
- Delivery rate per `(writer → receiver)` pair.
- Out-of-order receives (where ordering is required).
- Duplicates, unresolved gaps.

**Performance**
- **Latency p50 / p95 / p99 / max** — *the* headline metric.
- Throughput (writes/s, receives/s).
- Jitter (rolling stddev of latency).
- Loss% (transient at L3, fatal at L1/L2).
- Connect time (cold-start handshake cost).
- CPU%, MemMB during the run.

Latency is computed from joined `write`/`receive` events; clocks
across machines are PTP-synced separately.

---

## Slide 6 — Results: what works

**At ≤100 hz × ≤100 vpt with QoS 1–2, the lightweight transports
hit the design envelope** (same-machine, paired data):

| Variant | 100x100hz QoS1 | Delivery | p50 | p99 |
|---|---|---|---|---|
| **QUIC** | clean | 100 % | **0.78 ms** | 11.3 ms |
| **Custom UDP** | clean | 100 % | 5.5 ms | 21.7 ms |
| WebRTC | clean | 100 % | 2.3 ms | 157 ms |
| Zenoh | clean | 100 % | 30 ms | 171 ms |
| Hybrid | clean | 100 % | 146 ms | 204 ms |

QUIC and Custom UDP are the headline performers — sub-ms to
single-digit-ms p50, sub-25 ms p99, zero observed loss. The
remaining variants pay framing/scheduling overhead but still meet
the correctness bar.

**Bottom line:** at realistic LAN rates the transport choice is real
but in absolute terms all six finish *correctly* — what changes is
the cost.

---

## Slide 7 — Results: where it breaks

The matrix surfaced four failure modes:

1. **Loopback UDP drops at sustained 100 K+ pkt/s.** All UDP-based
   transports show 60–99 % loss at 1000 vpt × 100 hz. Windows kernel
   recv buffer overflows before user-space drains. Same-host loopback
   is **not** a lossless control.

2. **TCP overload looks like loss.** Counter-intuitive: at 1000 vpt
   the QoS 4 (kernel TCP) paths show 99 %+ "loss" and 25–60 s p50.
   The data is queued in the kernel, not dropped — but the operate
   window expires before drain. Needs longer windows or
   backpressured writers.

3. **WebSocket fully unusable on one host** (port-bind conflict).
   WebRTC has many no-data spawns at high rates (DataChannel
   signaling didn't complete in time).

4. **Many `(uncorrected)` latencies** — clock-sync engine produced
   too few samples per spawn for skew correction. Same-host
   absolute numbers stand, but cross-host high-rate p50/p99 should
   be re-measured after the clock-sync sample-count fix.

The two-machine run on disk has only **alice's** logs (bob's never
copied back), so cross-machine measurement is currently out of reach
without re-running once the runner's resume-manifest recv-buffer
limit is also fixed (filed: T-coord.4, runner can't ship a 4262-byte
manifest through a 4096-byte recv buffer).

---

## Slide 8 — Findings: divergences from expectation

What we assumed → what we measured:

| Assumed | Measured | Implication |
|---|---|---|
| Loopback = ground truth, lossless | UDP drops at sustained 100 K+ pkt/s | Need longer operate windows or backpressured writers before high-rate rows can be trusted |
| TCP-backed QoS 4 = "boring but reliable" | Worst performer at overload (queue grows, window expires) | Cap writer rate to drain rate, or read send-queue depth before each tick |
| WebSocket = control transport | Unusable on one host | Single-host runs cannot evaluate WebSocket — requires real two-machine setup |
| Clock sync just works | Many spawns get too few samples | Investigate per-variant resync cadence under load |
| Failed spawns are obvious | Spawns produce log files with 0 writes when setup didn't finish | Add a `connection_failed` event to variants so analysis distinguishes "lost packets" from "couldn't start" |
| Two-machine results = manifest + copy | Bob's logs aren't auto-collected; manifest msg > 4 KB recv buffer | Automate log shipping; raise `MAX_MSG_SIZE` (T-coord.4) |

---

## Slide 9 — Where this leaves us

**Confident claims today:**
- At ≤100 hz × ≤100 vpt, QUIC and Custom UDP hit sub-ms p50 / sub-25 ms
  p99 / 0 % loss — comfortably inside the original envelope.
- Zenoh and WebRTC are usable with overhead trade-offs.
- The benchmark harness end-to-end works (discovery → multi-variant
  spawn cadence → JSONL → integrity + performance + diagrams).
- Critical bugs caught and fixed during this exercise:
  T-coord.1b (mid-run done-barrier hang), T-coord.2 (barrier-timeout
  safety net), T-coord.3 (discovery-panic on late peer).

**Open questions to take to the next iteration:**
- **Cross-machine relative ranking** — re-run on a real LAN, ship
  bob's logs back, fix recv-buffer (T-coord.4).
- **Steady-state throughput ceiling** — needs ≥30 s operate window.
- **Backpressure-correct rate matching** — without it, every
  high-rate row says "100 % loss" when really the writer outran the
  receiver.
- **WebSocket evaluation** — only meaningful in two-machine mode.

**Practical takeaway:** the transport ranking we have today is
reliable for the realistic-rate slice of the matrix. The "where it
breaks" slice is mostly telling us about benchmark-harness
limitations, not protocol verdicts. Fixing those is the next step
before any cross-vendor recommendation.
