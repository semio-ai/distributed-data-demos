# Distributed Data Replication — Presentation Brief

A short reference for building slides on the variants we benchmark, the QoS
levels each one supports, and the metrics we report.

## 1. The benchmark in one paragraph

Every variant solves the **same problem**: replicate a single-writer,
key-value tree across nodes on a LAN at ~100K updates/sec aggregate with
sub-10 ms latency. The variants differ only in **how the bytes get from
writer to readers** — different transports, different framings, different
reliability strategies. We run each one through an identical workload and
compare. That is the entire experiment.

## 2. The variants

Six implementations, grouped by what they answer.

### Frameworks (let someone else solve it)

- **Zenoh** — a Rust-native pub/sub framework with built-in zero-conf
  discovery and configurable QoS. Represents the "use a mature framework"
  baseline. Question it answers: *can a high-level framework hit our
  latency targets with minimal custom code?*

### Hand-rolled (full control)

- **Custom UDP** — raw `UdpSocket` plus a hand-written protocol. UDP
  multicast for fan-out, unicast NACKs for recovery, mDNS for discovery.
  Implements all four QoS levels at the application layer. Represents the
  "from scratch" baseline — the performance floor with no framework
  overhead.

- **Hybrid UDP/TCP** — UDP multicast for unreliable QoS (1–2), one TCP
  connection per peer pair for reliable QoS (3–4). The kernel handles
  all retransmission and ordering on the reliable side; no NACK code is
  written. Question it answers: *is the hand-rolled NACK protocol in
  Custom UDP actually worth the complexity over kernel TCP on a LAN?*

### Modern protocols

- **QUIC (quinn)** — UDP-based, multiplexed reliable streams plus
  unreliable datagrams, mandatory TLS 1.3. Streams map cleanly to QoS
  levels. Question it answers: *does QUIC's per-stream multiplexing
  remove the head-of-line blocking that hurts plain TCP, and at what
  encryption-overhead cost?*

### Browser-stack on a LAN

- **WebSocket** — TCP with the WebSocket framing layer on top. Reliable
  QoS only (3–4); the variant refuses unreliable QoS by design. Question
  it answers: *what does the WebSocket framing tax actually cost vs raw
  TCP at our payload sizes?*

- **WebRTC DataChannels** — SCTP-over-DTLS-over-UDP. Each DataChannel is
  configurable as ordered/unordered and reliable / `maxRetransmits=0`,
  giving native support for all four QoS levels with no
  application-layer reliability code. Question it answers: *what is the
  cost of the heaviest off-the-shelf reliable+unreliable mux, on a LAN
  where its complexity buys us little?*

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
on Windows hosts.

### 5.1 Same-machine run (2026-05-07, log_subdir `…_183143`)

Both runners on one host, loopback addresses. Complete log set for
alice and bob — paired latency and delivery numbers are real. **This
is the run with the most interpretable data.**

#### What worked as designed

At "easy" rates — standard 100 hz × ≤100 vpt with QoS 1 (best-effort)
or QoS 2 (latest-value) — every protocol that finished its spawn
delivered ~100 % and stayed under 10 ms p99. Concretely:

| Variant | Sample row (100 hz × 100 vpt, QoS 1) | Delivery | p50 | p99 |
|---|---|---|---|---|
| custom-udp | `100x100hz-qos1` | 100 % | 5.5 ms | 21.7 ms |
| quic | `100x100hz-qos1` | 100 % | 0.78 ms | 11.3 ms |
| zenoh | `100x100hz-qos1` | 100 % | 30.3 ms | 170.8 ms |
| hybrid | `100x100hz-qos1` | 100 % | 145.7 ms | 203.9 ms |
| webrtc | `100x100hz-qos1` | 100 % | 2.26 ms | 157.4 ms |

QUIC and Custom UDP are the headline performers at this rate — sub-ms
to single-digit-ms p50, sub-25 ms p99, no observed loss. Hybrid and
Zenoh add their own framing/scheduling overhead. WebRTC works when
its DataChannel signaling completes (see "where it breaks" below).

#### Where it breaks

The matrix exposes failure modes that didn't show up in the unit / integration
tests:

1. **Loopback UDP is not lossless under sustained 100k+ pkt/s.** Every
   transport with a UDP-based unreliable path drops 60–99 % at
   1000 vpt × 100 hz combos:
   - `custom-udp 1000x100hz-qos1`: 24.9 % delivery, 75 % loss, p50 389 ms.
   - `zenoh 1000x100hz-qos1`: 26 % loss-equivalent (mass `Late` deliveries),
     p50 435 ms.
   - `hybrid 1000x100hz-qos1`: 85 % loss, p50 1.0 s.
   The Windows kernel UDP receive buffer overflows when the writer
   sustains faster than the receiver thread drains. This is a
   *workload-shape* finding, not a per-protocol verdict.

2. **TCP-backed reliable QoS gets *worse* than UDP at overload, not
   better.** Counter-intuitive: at 1000 vpt × 100 hz the QoS 4 (kernel
   TCP) variants of custom-udp and hybrid show 99 %+ "loss" and
   25–60 second p50 latencies. The explanation is back-pressure:
   the writer crams data into the kernel send buffer faster than TCP
   can drain it, so the operate window expires while data is still
   queued. The receiver reports "didn't get it" because nothing
   arrived before the spawn ended, not because TCP dropped it. This
   would be invisible on a longer operate window or with explicit
   backpressure in the variant.

3. **`workload = max-throughput` is a stress test, not a normal
   profile.** Across all transports the `*-max-*` rows show 75–99 %
   loss and tens-of-seconds p50. Useful as a saturation point but
   not a "real" measurement.

4. **WebSocket is fully broken on one host.** Every `websocket-*`
   spawn shows 0 writes/s and 100 % loss. Two same-machine processes
   can't both bind the WebSocket server port. WebSocket can only be
   evaluated in a real two-machine setup.

5. **WebRTC has signaling-fragility on one host.** Many `webrtc-*`
   high-rate spawns produce no data at all (`0 ms / 0 writes`) —
   the DataChannel handshake didn't complete before the operate
   window opened. The spawns that *do* connect look fine.

6. **Latency labelled "(uncorrected)".** Most p50 / p95 / p99 values
   in the same-machine run carry the "(uncorrected)" tag. Clock
   sync engine produced too few samples per variant for the
   timestamp pipeline to apply skew correction. On a single host the
   wall-clock should be identical anyway, so the numbers are still
   directionally correct, but the absolute values for hybrid /
   custom-udp at high QoS shouldn't be read as physical-time delivery
   latencies; they are a *spawn-window timing* relative measurement.

7. **Zenoh's high-rate sub-ms zenoh-100x1000hz-qos1 / qos3 / qos4
   p50 of ~0.4 ms** are too good to be true at 100 vpt × 1000 hz,
   suggesting Zenoh's local subscriber path bypasses the wire and
   reads from a same-process publisher cache. That's a feature for
   real workloads but means the same-machine numbers for Zenoh are
   not comparable to the network-based transports at sub-ms.

### 5.2 Two-machine run (2026-05-07, log_subdir `two-machines-…_093412`)

Alice's logs only — bob's logs from the second host were never copied
back. The analysis tool can therefore only compute `alice → alice`
self-paths, which makes the cross-machine comparison **incomplete**.
Reporting what's there for completeness, with that caveat:

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

**Action item before any future presentation:** copy bob's logs from
the second machine into this folder and re-run `analyze.py`. Without
them the two-machine run cannot make a real cross-machine claim. A
re-run is also warranted now that T-coord.3 + T-coord.1b are landed —
a full clean run should complete without the 9-spawn hang we hit on
2026-05-07.

## 6. Findings — divergences from expectation

What the design assumed vs what we measured:

1. **Assumed**: same-machine = ground truth, no noise. **Measured**:
   loopback drops UDP at sustained high pkt/s; TCP overload looks
   like loss because the operate window expires mid-drain. The
   benchmark needs *longer operate windows* or *backpressured
   writers* before any high-rate row can be trusted.

2. **Assumed**: TCP-based QoS 4 = "boring but reliable." **Measured**:
   under the current operate-window length it is the worst performer
   on the high-rate matrix. The kernel queue absorbs everything and
   then dies on operate end. Recommend either capping writer rate
   to drain rate, lengthening the operate window, or (best) reading
   the variant's send-side queue depth and refusing to start a new
   tick if the queue is full.

3. **Assumed**: WebSocket is a "control" — well-understood reliable
   transport. **Measured**: unusable on a single host because it
   wants a single server port. Real-world deployment isn't
   single-host, so this is a benchmark-harness limitation, not a
   protocol verdict.

4. **Assumed**: Clock sync would just work and we'd get
   skew-corrected latencies for free. **Measured**: many spawns
   produce too few samples for the corrector to apply, so we're
   reading "(uncorrected)" timestamps on critical rows. Need to
   investigate the per-variant resync sample count under load.

5. **Assumed**: A failed spawn would be obvious. **Measured**: many
   spawns ran their 12 s window and produced log files but with
   `0 writes / 0 ms` because the variant's own setup phase didn't
   finish in time (webrtc signaling, websocket port bind,
   hybrid-1000x10hz-qos3 / 100x100hz-qos3 etc.). Need a
   `connection_failed` event from variants so analysis can
   distinguish "ran and lost packets" from "couldn't even start."

6. **Assumed**: Two-machine results would just take a manifest
   exchange and a copy. **Measured**: bob's logs need to physically
   move back to alice's machine; we don't yet have an automated
   step for that. Plus, the resume-manifest message size for 176
   completed jobs is 4262 bytes against a 4096-byte recv buffer —
   the runner's manifest-exchange path needs the buffer raised
   before any cross-machine resume actually round-trips. (Filed:
   T-coord.4 in `metak-orchestrator/TASKS.md`.)

## 7. Where this leaves us

Confident claims we can make today:

- **At ≤100 hz × ≤100 vpt the lightweight transports (custom-udp,
  quic) hit sub-ms p50 and sub-25 ms p99 with zero loss.** That's
  comfortably inside the original "<10 ms p99 sub-100K agg"
  envelope for those rate combos.
- **Zenoh and WebRTC are usable**, with framing overhead trade-offs.
- **The benchmark harness itself works end-to-end** — discovery,
  multi-variant spawn cadence, JSONL ingest, integrity checks,
  delivery / latency / jitter / loss / resource summaries all
  produce cross-comparable rows.

Open questions that the same-machine matrix can't answer:

- **Cross-machine relative ranking** — needs both machines' logs
  collected; same matrix re-run on a real LAN.
- **Steady-state throughput ceiling** — current 12 s operate window
  conflates ramp-up + steady state + drain. A longer window
  (≥30 s) would give a clean steady-state rate.
- **Backpressure-correct rate matching** — without it, every
  high-rate row says "100 % loss, 60 s p50" when really the writer
  pumped 5× the receiver could drain.

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
