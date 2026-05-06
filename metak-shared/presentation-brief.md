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

## 5. Suggested slide flow

1. **Title + the question** — "How much does the transport choice matter?"
2. **The setup** — one figure: two runners, identical workload, six
   variants take turns over the same data.
3. **The variants** — one bullet each, grouped as in §2.
4. **The QoS matrix** — the table from §3 (variants × QoS levels).
5. **What we measure** — latency p50/p95/p99, throughput, loss, jitter,
   connection time. One line each.
6. **Then the actual results** — the latency CDF, throughput bars, and
   the radar chart from `analyze.py --diagrams`.
