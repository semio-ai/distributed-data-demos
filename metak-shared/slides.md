# Distributed Data Replication — Team Brief

> Async-readable. Ten slides, ~5 minutes end-to-end. Each slide stands
> alone; horizontal rules separate them. Detail in
> [`presentation-brief.md`](presentation-brief.md).

---

## Slide 1 — The question

> **Is Zenoh a good fit for our distributed data replication needs?**

We need to replicate a single-writer, key-value tree across nodes on
a LAN at ~100 K updates/sec aggregate with sub-10 ms p99 latency. We
want to use Zenoh if it works — it gives us discovery, QoS, and
recovery out of the box, no custom transport to maintain.

To validate it, we built five other variants of the same replicator
and ran them all through an identical workload. Custom UDP is the
performance floor (hand-rolled, minimum overhead). Hybrid UDP/TCP,
QUIC, WebSocket, and WebRTC are alternatives that bracket Zenoh
around and above. If Zenoh meets the envelope and the alternatives
don't dramatically beat it, we use it.

---

## Slide 2 — The setup

- **Two runners** (alice, bob), leaderless coordination, same machine
  or LAN.
- **Six variants** of the replicator — Zenoh + 5 baselines (next slide).
- **Four QoS levels** per variant: best-effort UDP → reliable TCP.
- **176 spawns per run** = 48 variant entries × QoS expansion.
- **Per spawn lifecycle**: `connect` → `stabilize 3 s` → `operate 30 s`
  → `eot ≤30 s` → `silent 3 s`. Metrics computed over `operate`; `eot`
  drains late deliveries (slide 6).
- **Workload shape glossary** (the matrix sweeps these):
  - `vpt` = **values per tick** — distinct key/value updates per tick.
  - `tick_rate_hz` = ticks per second.
  - `vpt × tick_rate_hz` = aggregate writes/sec. So `100x100hz` is
    10 k writes/s; `1000x100hz` is 100 k writes/s; `max` targets ~1 M
    writes/s as a saturation probe.
- Output: integrity report + performance report + diagrams (latency
  CDF, comparison bars), via `analyze.py`.

---

## Slide 3 — The candidate and the baselines

**Candidate (the one we're evaluating)**
- **Zenoh** — Rust pub/sub framework, zero-conf discovery, configurable
  QoS. *Will it meet our envelope without us writing transport code?*

**Performance-floor baseline**
- **Custom UDP** — raw `UdpSocket` + hand-written NACK protocol. The
  *floor* — if Zenoh is N× slower, that's the framework tax.

**Alternative-framework baselines (bracket Zenoh around and above)**
- **Hybrid UDP/TCP** — UDP multicast for QoS 1–2, kernel TCP for QoS 3–4.
- **QUIC (quinn)** — UDP-based multiplexed streams + datagrams + TLS 1.3.
- **WebSocket** — TCP + WebSocket framing, reliable QoS only.
- **WebRTC DataChannels** — SCTP-over-DTLS-over-UDP, all four QoS levels.

Direction-of-bracket: **Custom UDP / QUIC** establish the latency
floor; **Hybrid / WebSocket / WebRTC** establish whether heavier
framings cost more than Zenoh does.

---

## Slide 4 — QoS matrix

| Lvl | Name | Transport | Ordering | Loss |
|---|---|---|---|---|
| 1 | Best-Effort | UDP, fire-and-forget | None | Tolerated |
| 2 | Latest-Value | UDP + seq filter | Latest-wins | Skipped |
| 3 | Reliable-UDP | UDP + NACK | Strict | Recovered |
| 4 | Reliable-TCP | TCP (or eq.) | Strict | Kernel-recovered |

Mapping per variant: each runs all four levels (except **WebSocket**,
which only supports L3/L4 by design; refuses unreliable QoS).
Comparison gold-rows:

- L3 across variants — different reliability strategies, same goal.
- Reliable-vs-unreliable cliff within a variant — exposes how much
  reliability machinery costs.

Full table in [§3 of the brief](presentation-brief.md).

---

## Slide 5 — What we measure

**Integrity**
- Delivery rate per `(writer → receiver)` pair.
- Out-of-order receives (where ordering is required).
- Duplicates, unresolved gaps.
- **EOT drain** — late deliveries that arrived after the writer's
  `eot_sent` are counted as `late_receives`, not as loss.

**Performance**
- **Latency p50 / p95 / p99 / max** — *the* headline.
- Throughput (writes/s, receives/s).
- Jitter (rolling stddev of latency).
- Loss% (transient at L3, fatal at L1/L2).
- Connect time (cold-start handshake cost).
- CPU%, MemMB.

Latency = `receive_ts − write_ts`, joined on `(variant, run, writer,
seq, path)`. Clocks PTP-synced across machines (single-host runs use
the same wall-clock).

---

## Slide 6 — How the EOT phase reads the numbers

After `operate` ends, every variant enters an **EOT phase** designed
to drain late deliveries:

1. Writer emits a single EOT marker (`eot_sent`).
2. Each peer waits up to **30 s** for every other peer's EOT.
3. While waiting, `poll_receive` keeps draining — late deliveries are
   tagged and counted.
4. If a peer's EOT never arrives, `eot_timeout` is logged with the
   missing peer set; the spawn proceeds to `silent`.

**Variant support (today):**
- ✅ Custom UDP, Hybrid, QUIC, **Zenoh** — all implement the override.
- ❌ WebSocket, WebRTC — no override yet. Default no-op fires
  `eot_timeout` unconditionally on these variants. Filed in E12.

**So how do you read the results?**

- A row that shows `late_receives` > 0: EOT did its job — those messages
  were in flight at operate-end and the drain caught them.
- A row that shows `eot_timeout` in the JSONL: the 30 s drain budget
  expired and not everything was caught. Usually means the queue
  exceeded what could drain in 30 s.

For the same-machine `_183143` run, **EOT was active on every Zenoh
spawn** — its numbers are not "operate-window-truncated"; they
reflect what Zenoh actually delivered within 60 s of starting.

---

## Slide 7 — Zenoh headline numbers

Same-machine run, paired alice+bob logs, EOT active.

| Spawn | Aggregate rate | Delivery | p50 | p99 | Verdict |
|---|---|---|---|---|---|
| `zenoh-10x100hz-qos1` | 1 k writes/s | 100 % | 11 ms | 30 ms | ✓ correctness; misses p99 |
| `zenoh-100x100hz-qos1` | 10 k writes/s | 100 % | **30 ms** | **171 ms** | ✓ correctness; misses p99 by 17× |
| `zenoh-100x100hz-qos3` | 10 k writes/s | 100 % | 40 ms | 340 ms | ✓ reliable QoS holds; misses p99 |
| `zenoh-1000x100hz-qos1` | 100 k writes/s | ~26 % | 435 ms | 612 ms | ✗ saturated at headline rate |
| `zenoh-1000x100hz-qos3` | 100 k writes/s | ~36 % | 423 ms | 535 ms | ✗ saturated at headline rate |
| `zenoh-max-qos1` | ~1 M target | low | 203 ms | 309 ms | saturation probe, expected |

**Read on Zenoh against our target (sub-10 ms p99 @ ~100 K writes/s):**

- At **10 k writes/sec aggregate** (`100x100hz`): Zenoh is **correct**
  (100 % delivery, including reliable QoS) but **~17× over the latency
  target** at p99.
- At **100 k writes/sec aggregate** (`1000x100hz`, our headline rate):
  Zenoh **saturates** to ~26–36 % delivery — but so does every other
  variant on this hardware. Saturation is workload-shape, not
  Zenoh-specific.

---

## Slide 8 — Zenoh vs the baselines

**At 10 k writes/sec aggregate (the standard `100x100hz-qos1`):**

| Variant | Delivery | p50 | p99 | vs Zenoh p50 |
|---|---|---|---|---|
| **QUIC** | 100 % | **0.78 ms** | **11.3 ms** | **~40× faster** |
| Custom UDP | 100 % | 5.5 ms | 21.7 ms | ~6× faster |
| WebRTC | 100 % | 2.3 ms | 157 ms | ~13× faster |
| **Zenoh** | 100 % | 30 ms | 171 ms | — |
| Hybrid | 100 % | 146 ms | 204 ms | ~5× slower |

Every variant that completed delivers 100 % at this rate, so the
comparison is purely on latency. **Zenoh is operationally simple but
significantly slower than QUIC** at our realistic rate.

**At 100 k writes/sec (`1000x100hz-qos1`):**

| Variant | Delivery | p50 | Notes |
|---|---|---|---|
| Custom UDP | 25 % | 389 ms | UDP buffer overflow |
| **Zenoh** | **26 %** | 435 ms | similar saturation |
| Hybrid | 15 % | 1.0 s | TCP queueing, EOT timed out |
| QUIC | 17 % | 21 s | catastrophic |

**At saturation Zenoh is no worse than the alternatives.** The
loss/latency at this rate is largely a benchmark-hardware story.

---

## Slide 9 — What didn't go as expected

Findings that should shape any future iteration:

1. **Loopback isn't lossless.** UDP loopback drops 60–99 % at sustained
   100 K+ pkt/s on Windows. So the high-rate same-machine rows are a
   benchmark-hardware ceiling, not a protocol verdict.
2. **EOT works but is bounded by 30 s.** For hybrid-TCP at 100 K writes/s
   the EOT marker queues *behind* the data in TCP, so it can't drain
   in 30 s. Lengthening `--eot-timeout-secs` or moving EOT off-band is
   the obvious fix.
3. **Zenoh's 1000-vpt rows show bob's log truncated mid-write** — bob's
   variant child died/was-killed during `operate` at sustained 100 K
   writes/s. Could be OOM, could be a runner timeout firing too early.
   **This is the single most important Zenoh question to resolve
   before we recommend it.**
4. **WebSocket fully unusable on one host** (port-bind conflict) and
   **WebRTC has signaling fragility** at high rates. Neither has an
   EOT override yet either. Both need a real two-machine setup to
   evaluate fairly.
5. **The two-machine run is missing bob's logs entirely** — they were
   never copied back. Cross-machine ranking blocked on that + on
   T-coord.4 (runner can't ship a 4262-byte resume manifest through
   a 4096-byte recv buffer).
6. **Most same-machine latencies are tagged "(uncorrected)"** — clock
   sync got too few samples per spawn. Absolute numbers stand
   directionally on one host; cross-machine numbers need a re-sync.

---

## Slide 10 — Where this leaves us

**Confident claims today:**
- Zenoh is **correct** — 100 % delivery at 10 k writes/sec on this
  hardware, reliable QoS holds.
- Zenoh is **operationally simple** — discovery, QoS, recovery all
  worked out of the box without protocol code.
- Zenoh **saturates at the same rate as every other variant** on this
  hardware — no obvious throughput penalty.

**Concerns about Zenoh:**
- **Misses our latency target at the realistic rate** (~30 ms p50,
  ~170 ms p99 vs sub-10 ms p99 goal at 10 k writes/sec).
- **Unexplained mid-operate child termination at 1000-vpt** — needs
  investigation before any production recommendation.

**Pre-conditions before recommending one way or the other:**
1. Investigate the Zenoh 1000-vpt child-death (OOM? runner timeout? panic?).
2. Two-machine re-run with both peers' logs collected and the EOT phase
   given a longer budget (or an off-band channel).
3. Decide whether sub-10 ms p99 is a hard requirement. If it is, QUIC
   is in the running and Zenoh likely isn't. If it isn't, Zenoh is
   the simplest viable choice on the table.

**Bottom line:** Zenoh works correctly and is the simplest option,
but at our realistic rate it's ~17× over our p99 target. The right
next move is to decide whether the target is firm before we invest
more in either Zenoh or a faster alternative.
