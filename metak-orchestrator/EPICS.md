# Epics

## E0: Variant Exploration

**Repo**: none (documentation only)
**Goal**: Research existing libraries, frameworks, and protocols that could
serve as the transport layer for a replication variant. Produce a shortlist
of candidates worth benchmarking, with enough technical detail to inform
implementation.

Scope:
- Survey the landscape: pub/sub frameworks, distributed KV stores, messaging
  libraries, raw protocol approaches (UDP multicast, QUIC, etc.) that operate
  on a local network with low-latency goals.
- For each candidate, document:
  - Name and project URL.
  - Transport model (how data moves between nodes).
  - Discovery mechanism (zero-conf, explicit peers, broker, etc.).
  - QoS capabilities (reliability, ordering guarantees).
  - Rust support (native crate, bindings, or would need FFI).
  - Fit with our design: single-writer model, `arora_types::Value`
    serialization, per-subtree QoS, leaderless topology.
  - Known limitations or concerns.
  - Relevant documentation and examples.
- Produce a recommendation: which candidates to include in the benchmark
  and why. Flag any that would require design compromises.
- Output: a research document in `metak-shared/` (e.g. `variant-candidates.md`)
  plus updates to the epics below reflecting the chosen variants.

Deliverables:
- `metak-shared/variant-candidates.md` — full research with per-candidate
  assessment.
- Updated variant epic list (E3+) reflecting the chosen candidates.

Dependencies: DESIGN.md (the criteria come from the replication design).

---

## E1: Variant Base Crate

**Repo**: `variant-base/` (Rust library crate)
**Goal**: Provide a shared foundation that all variant implementations build
on. Defines the common trait, handles everything that is not
transport-specific, and includes a dummy variant for testing.

Built **before the runner** so the trait, protocol driver, and logging can be
validated in isolation. Findings from this work may surface changes needed
in the runner design or API contracts.

Scope:
- **`Variant` trait** — the interface each implementation must fulfill:
  - `connect(&self, peers: ...) -> Result<()>` — establish transport channels.
  - `publish(&self, path, value, qos, seq) -> Result<()>` — send an update.
  - `poll_receive(&self) -> Result<Option<ReceivedUpdate>>` — receive an
    update from a peer (non-blocking or async).
  - `disconnect(&self) -> Result<()>` — clean shutdown.
  - (Exact signatures TBD during implementation; these illustrate intent.)
- **CLI parsing** for common arguments (tick rate, phase durations, workload,
  QoS, log dir, launch-ts, variant/runner/run identity). Variant-specific
  args are passed through as a raw map or via a generic mechanism.
- **Test protocol driver** — orchestrates the four phases:
  1. Connect — calls `Variant::connect`, logs `connected` event.
  2. Stabilize — waits for `stabilize_secs`, logs `phase` event.
  3. Operate — runs the workload loop at the configured tick rate, calling
     `Variant::publish` and `Variant::poll_receive`, logging `write` and
     `receive` events.
  4. Silent — waits for `silent_secs` (draining receives), flushes logs.
- **JSONL logger** — structured log writer that enforces the schema from
  `api-contracts/jsonl-log-schema.md`. Every line automatically includes
  `ts`, `variant`, `runner`, `run`.
- **Resource monitor** — periodic CPU/memory sampling, emitting `resource`
  events.
- **Workload profiles** — pluggable workload definitions (start with
  `scalar-flood`). The operate phase calls the active profile to decide
  what/how much to write each tick.
- **Sequence number generator** — per-writer monotonic counter.
- **`VariantDummy`** — a concrete `Variant` implementation with no
  networking. Publishes write to an in-process data board and immediately
  makes them available via `poll_receive` on the same node. Purposes:
  - Unit and integration testing of the base crate itself (protocol driver,
    logger, workload profiles, CLI parsing) without any network dependencies.
  - Harness testing for the runner (E2) — the runner can spawn the dummy
    binary to verify spawning, CLI arg passing, timeout handling, barrier
    sync, and log collection without needing multiple machines or real
    network traffic.
  - Zero-network performance baseline — measures the overhead of everything
    except the transport layer (serialization, logging, tick loop, resource
    monitoring).
  - Ships as a binary alongside the library: `variant-dummy`.

What this crate does NOT do:
- Any real networking or transport logic — that is the variant's job.
- Peer discovery — each real variant handles this its own way.
  (`VariantDummy` skips discovery entirely.)

Dependencies: API contracts (variant-cli, jsonl-log-schema). Informed by
E0 output (knowing what the variants need helps shape the trait).

---

## E2: Benchmark Runner

**Repo**: `runner/`
**Goal**: Implement the leaderless runner binary that coordinates benchmark
execution across machines.

Can be tested locally using `VariantDummy` from E1 — spawn the dummy binary
to verify the full runner lifecycle (config parsing, discovery, barrier sync,
child spawning, CLI arg construction, timeout, exit code collection) on a
single machine before any real variants exist.

Scope:
- CLI: `runner --name <name> --config <path.toml>`
- TOML config parsing (runners list, default timeout, variant definitions
  with common/specific sections).
- UDP broadcast discovery with config-hash verification.
- Barrier sync protocol (ready / done per variant).
- Child process spawning with CLI args derived from config (common + specific
  + `--launch-ts`, `--variant`, `--runner`, `--run`).
- Monitor child for exit/timeout. Record exit status.
- Proceed through variants in config order.

Dependencies: E1 (variant-base + VariantDummy for testing). API contracts
for runner CLI, TOML config schema, runner coordination protocol, and
variant CLI contract must be finalized first.

---

## E3a: Zenoh Variant

**Repo**: `variants/zenoh/`
**Goal**: Implement a replication variant using Eclipse Zenoh as the transport.
Represents the "high-level framework" approach.

Scope:
- Implements `Variant` trait from `variant-base`.
- Peer discovery via Zenoh scouting (zero-conf multicast).
- Maps key paths to Zenoh key expressions for pub/sub.
- Zenoh peer-to-peer mode (no router).
- Zenoh-specific CLI args: `zenoh_mode`, `zenoh_listen`.
- Dependencies: `zenoh` crate. Use blocking API wrappers (sync trait).
- Start with `scalar-flood` workload profile.

Dependencies: E1 (base crate), E2 (runner to spawn it).

---

## E3b: Custom UDP Variant

**Repo**: `variants/custom-udp/`
**Goal**: Implement a replication variant using raw UDP sockets with manual
protocol logic. Represents the "from scratch" approach.

Scope:
- Implements `Variant` trait from `variant-base`.
- Peer discovery via mDNS (`mdns-sd` crate).
- UDP multicast for data fan-out.
- Implements all four QoS levels manually:
  - L1: fire-and-forget multicast
  - L2: sequence tracking, receiver discards stale
  - L3: sequence gaps + NACK-based retransmit
  - L4: TCP connection per peer pair
- Custom CLI args: `buffer_size`, `multicast_group`.
- Dependencies: `std::net::UdpSocket`, `socket2`, `mdns-sd`.
- Application-layer fragmentation for payloads > 1472 bytes.

Dependencies: E1 (base crate), E2 (runner to spawn it).

---

## E3d: QUIC Variant

**Repo**: `variants/quic/`
**Goal**: Implement a replication variant using QUIC via the quinn crate.
Represents the "modern protocol" approach.

Scope:
- Implements `Variant` trait from `variant-base`.
- Peer discovery via mDNS (`mdns-sd` crate).
- One QUIC connection per peer. Multiplexed streams for data.
- Maps QoS levels to QUIC features:
  - L1/L2: unreliable datagrams
  - L3/L4: reliable streams
- Internal tokio runtime, bridged to sync trait via `block_on`.
- QUIC-specific CLI args: `cert_path`, `bind_addr`.
- Self-signed certificates for LAN benchmarking.

Dependencies: E1 (base crate), E2 (runner to spawn it).

---

## E3e: Hybrid UDP/TCP Variant

**Repo**: `variants/hybrid/`
**Goal**: Implement a replication variant that uses UDP for best-effort
traffic and TCP for reliable traffic. Represents the "simplest correct"
approach — avoids all application-layer reliability logic by delegating
to the kernel's TCP stack for QoS levels that require ordering and
completeness.

Scope:
- Implements `Variant` trait from `variant-base`.
- Peer discovery via mDNS (`mdns-sd` crate).
- Transport split by QoS level:
  - L1 (best-effort): UDP multicast, fire-and-forget
  - L2 (latest-value): UDP multicast, receiver-side sequence filtering
  - L3 (reliable-ordered): TCP connection per peer pair
  - L4 (reliable-TCP): TCP connection per peer pair (same as L3)
- No NACK protocol, no gap detection, no retransmit buffers.
  Reliable delivery is handled entirely by the kernel TCP stack.
- CLI args: `multicast_group`, `tcp_base_port`.
- Dependencies: `std::net::{UdpSocket, TcpStream, TcpListener}`, `socket2`,
  `mdns-sd`.

The key benchmark question this variant answers: **is NACK-based
reliable-UDP (QoS 3 in E3b) worth the implementation complexity, or does
TCP's kernel-managed reliability perform equally well on a LAN where
packet loss is rare?** Comparing E3b vs E3e at QoS 3 directly tests
whether head-of-line blocking matters at our throughput targets.

Dependencies: E1 (base crate), E2 (runner to spawn it).

---

## E4: Analysis Tool — Phase 1 (Foundation)

**Repo**: `analysis/`
**Goal**: Implement the core analysis pipeline: parsing, caching, integrity
verification, and CLI summary tables.

Scope:
- CLI: `python analyze.py <logs-dir> [--clear] [--summary] [--diagrams] [--output <dir>]`
- JSONL parsing and data model.
- Pickle caching pipeline (load, detect changes by mtime, parse/merge, save).
- `--clear` flag to force full rebuild.
- Write-receive correlation by `(variant, run, seq, path)`.
- Integrity verification: delivery completeness, ordering, duplicates,
  gap/recovery checks (per QoS level).
- Performance analysis: connection time, latency percentiles (p50/p95/p99),
  throughput, jitter, packet loss, resource usage.
- CLI summary tables (integrity report + performance report).

Can be tested early using JSONL logs produced by `VariantDummy` runs.

Dependencies: JSONL log schema contract (must match what variants produce).

---

## E5: Analysis Tool — Phase 2 (Diagrams)

**Repo**: `analysis/`
**Goal**: Add diagram generation to the analysis tool.

Scope:
- Latency: histogram, CDF, box plot.
- Throughput: bar chart.
- Connection time: bar chart.
- Output as PNG to `<logs-dir>/analysis/`.

Dependencies: E4 (foundation must be working first).

---

## E6: Analysis Tool — Phase 3 (Time-Series and Advanced)

**Repo**: `analysis/`
**Goal**: Add time-series charts and the cross-variant radar chart.

Scope:
- Latency time-series.
- Throughput time-series.
- Resource usage time-series and bar charts.
- Jitter time-series.
- Radar/spider chart for cross-variant comparison.

Dependencies: E5.

---

## E8: Application-Level Clock Synchronization

**Repos**: `runner/`, `analysis/`, plus contract updates in `metak-shared/`
**Goal**: Measure pairwise clock offsets between runner machines so that
cross-machine `receive_ts − write_ts` values logged by variants can be
corrected to true network latency. Without this, two-machine runs cannot
report meaningful replication latency: Windows w32time can drift by hundreds
of ms, dwarfing the 10 ms latency target.

Approach: NTP-style 4-timestamp exchange between every pair of runners,
N=32 samples, best-sample-by-min-RTT. Run once after discovery and once
before each variant launch (catches drift). Runner writes a sibling
`<runner>-clock-sync-<run>.jsonl`. Variant code is unchanged. Analysis
joins by `(run, runner_pair)` and applies the offset when computing
cross-machine latency.

Contract: `metak-shared/api-contracts/clock-sync.md`. Cross-references in
`runner-coordination.md` (Phase 1.5 + per-variant resync) and
`jsonl-log-schema.md` (new `clock_sync` event type).

What this epic does NOT address:
- Hardware PTP / IEEE 1588 (out of scope — needs OS + NIC support).
- Asymmetric-path correction (NTP estimator assumes symmetric delay; on a
  quiet LAN this is acceptable).
- Adversarial scenarios where the OS clock jumps mid-run.

Dependencies: E2 (runner exists), E4 (analysis exists).

---

## E9: Peer Discovery Injection + QoS Expansion

**Repos**: `runner/`, `variants/quic/`, plus contract updates in `metak-shared/`
**Goal**: Two coupled improvements that share the runner's port-derivation
logic and so are best implemented together:

1. **Peer discovery injection.** Today the QUIC variant is the only one that
   needs explicit peer IPs, and they are hard-coded as `127.0.0.1:...` in
   the TOML config. This breaks any inter-machine run. The runner already
   sees every peer's source IP during Phase 1 discovery — it just doesn't
   capture or forward them. Capture them, do same-host detection, and pass
   them to spawned variants as a new injected CLI arg `--peers`.

2. **QoS expansion.** Today every variant entry in a config must hard-code
   `qos = N`, which forces 4× duplication if you want to compare a variant
   across all QoS levels. Make `qos` optional or list-typed in the TOML
   schema, and have the runner expand a single entry into N synthesized
   per-QoS spawns (named `<variant>-qosN`) that go through full
   stabilize/operate/silent cycles. Per-spawn barriers keep runners in
   lockstep per QoS level. Logs naturally separate by spawn name.

The two pieces couple at the QUIC variant: with `--peers` injected and QoS
varying per spawn, QUIC can derive both bind and connect ports from a
single `base_port` plus its known runner index and current QoS level.

Out of scope for this epic:
- Migrating the Hybrid variant off its `peers` config field. Hybrid currently
  works (it does its own setup using TCP per peer pair), and switching it to
  consume `--peers` is a follow-up if/when its same-machine `peers` field
  becomes a problem on a real two-machine run.
- Changing what Zenoh, custom-udp, or hybrid do with `--peers` — they may
  ignore it.

Contracts touched:
- `metak-shared/api-contracts/runner-coordination.md` (Phase 1 capture +
  same-host detection)
- `metak-shared/api-contracts/variant-cli.md` (`--peers` injected arg,
  `--qos` semantics under expansion)
- `metak-shared/api-contracts/toml-config-schema.md` (optional/list `qos`,
  port-stride convention, expanded spawn naming)

Dependencies: E2 (runner exists), E3d (QUIC variant exists).

---

## E12: End-of-Test Handshake

**Repos**: `variant-base/`, `variants/custom-udp/`, `variants/hybrid/`,
`variants/quic/`, `variants/zenoh/`, `analysis/`, plus contract additions
in `metak-shared/`.
**Goal**: Replace the wall-clock `silent_secs` drain with an explicit
end-of-test (EOT) handshake. After the operate phase ends, each variant
broadcasts an EOT marker to all peers and waits (bounded) until every
peer has been observed before moving into a small `silent_secs` grace
window. The operate window becomes self-terminating, and delivery
percentages can be scoped to writes whose ts is in
`[operate_start, eot_sent.ts]` rather than the silent-deadline cutoff.

Driving design: `metak-shared/api-contracts/eot-protocol.md` (new
contract — review before workers spawn).

Why now: T10.6b validation showed `silent_secs = 1` is materially too
short for TCP back-pressure to drain on localhost at 100K msg/s. With
silent_secs alone, the regression test thresholds had to be relaxed
to 20% TCP — which barely tells us anything beyond "did the spawn
exit." EOT lets us re-tighten to >=99% TCP without the fixture timing
becoming part of the contract.

Scope:
- New contract `metak-shared/api-contracts/eot-protocol.md`.
- Updated event types in `metak-shared/api-contracts/jsonl-log-schema.md`
  (`eot_sent`, `eot_received`, `eot_timeout`, plus `phase=eot`).
- `variant-base`: add `signal_end_of_test`/`wait_for_peer_eots` to the
  Variant trait with no-op defaults; insert the EOT phase between
  operate and silent in the protocol driver; emit the new JSONL events
  and the `phase=eot` event; new CLI flag `--eot-timeout-secs`.
- All four working variants implement EOT per the contract:
  Hybrid (TCP frame + UDP multicast), Custom UDP (TCP frame + UDP
  multicast), QUIC (stream-end + datagram), Zenoh
  (`bench/__eot__/<writer>` key).
- `analysis/`: scope operate window to `eot_sent.ts` when present, fall
  back to `phase=silent.ts` for legacy logs. Add a `late_receives`
  metric for receives between EOT and silent. Update integrity /
  performance to use the new window definition.
- Re-tighten the T10.6 regression test thresholds to:
  - Hybrid TCP qos 3-4: `>=99%`
  - Hybrid UDP qos 1-2: `>=99%` (correctness) / `>=95%` (high-rate)
  - Custom UDP TCP qos 4: `>=99%`
  - Custom UDP UDP qos 1-3: per-fixture spec, retighten where
    appropriate
  - QUIC qos 1-4: `>=99%` (with possible relaxation on datagram qos
    1-2 if measured loss exceeds 1%)
  - Zenoh `1000paths`: `==100%` (already locked in; should still hold)
  - Zenoh `max-throughput`: `>=80%` (documented mpsc-receive drop)

Out of scope:
- Aeron. The user has elected to exclude Aeron permanently (E3c stays
  blocked, code removal scheduled in E13). Aeron is intentionally not
  given an EOT implementation.
- Cross-machine validation. Stays user-owned (T10.5 / a future re-run
  task).
- Changes to the runner-runner coordination protocol. EOT is
  variant-to-variant within a spawn; the runner's barriers are
  unchanged.

Dependencies:
- E1 (Variant trait + protocol driver): touched.
- E3a/b/d/e (the four active variants): touched.
- E4 / E11 (analysis tool): touched in T12.6.
- T10.6: thresholds retightened in T12.7 once T12.2-5 + T12.6 land.

Acceptance:
- All four active variants log `eot_sent` once and the expected
  `eot_received{writer=peer}` lines on every localhost two-runner run
  of every existing reproducer fixture.
- T10.6 regression suite passes deterministically across 3 runs at
  the retightened thresholds above.
- `metak-shared/ANALYSIS.md` updated to describe the operate-window
  definition.
- All existing tests still pass (E11 analysis tests, per-variant unit
  tests, T10.6a/b/c).

---

## E11: Analysis Tool — Large-Dataset Cache Rework (Phase 1.5)

**Repo**: `analysis/`
**Goal**: Re-architect the analysis tool's caching and execution pipeline
so it scales to multi-tens-of-GB datasets with bounded memory. The current
Phase 1 (E4) implementation cannot complete on the user's
`inter-machine-all-variants-01-20260501_150858` dataset (40 GB across 128
JSONL files, ~148 M events): it builds a 14.5 GB pickle and then attempts
to flatten every event into one Python list and globally sort it, which
on the user's machine has been thrashing on swap for hours without
producing any output.

The output behaviour (integrity/performance tables, eventually diagrams)
must be preserved; what changes is how data is stored and processed.

Driving design: see `metak-shared/ANALYSIS.md` §§ 3-4 (revised) and § 8
Phase 1.5.

Scope:
- Replace the single `<logs-dir>/.analysis_cache.pkl` with a per-source-
  file Parquet shard cache under `<logs-dir>/.cache/`, plus per-shard
  meta sidecars and a global schema-version sentinel.
- Adopt **polars** as the analytics engine (justified addition to the
  Python stack — see CUSTOM.md). Use `pl.scan_parquet` lazy frames
  throughout; never materialize the full dataset.
- Replace the in-memory `Event(... data: dict)` dataclass with the
  flat columnar schema documented in ANALYSIS.md § 4.1. JSONL parsing
  becomes a streaming projection, not a full Python-object materialization.
- Rework `correlate.py`, `integrity.py`, `performance.py` to do their
  joins / groupbys via polars expressions, executed per `(variant, run)`
  group so peak memory is bounded by the largest single group, not by
  the whole dataset.
- `tables.py` / `plots.py` consumers stay output-compatible: each receives
  a list of result dataclasses or a polars DataFrame with the same fields
  as today, just sourced through the new pipeline.
- Migrate cleanly from any pre-existing `.analysis_cache.pkl` (delete on
  first run with a stderr notice; do not attempt to convert).
- Cover clock-sync events (E8) in the schema as nullable columns so E8
  can land without further rework.

What this epic does NOT address:
- Adding new metrics or output. The set of tables and metrics stays the
  same. (New output is E5 / E6 / E8 work.)
- Multi-process or GPU acceleration. Polars's threaded engine is more
  than sufficient.
- Replacing matplotlib for plots. Plotting is downstream of materialized
  per-group frames and unaffected.

Acceptance gate:
- The 40 GB user dataset (`logs/inter-machine-all-variants-01-20260501_150858/`)
  analyses end-to-end with `--summary` in under 10 minutes wall-clock on
  first run and under 30 seconds on a re-run with no JSONL changes; peak
  RSS under 4 GB throughout.
- Existing analysis output on the small `logs/same-machine-20260430_140856/`
  dataset matches the Phase 1 output byte-for-byte (where deterministic)
  or value-for-value (where ordering of equal-key rows is implementation-
  defined).

Dependencies: E4 (Phase 1 — shipped). Independent of E5/E6/E8 — they all
benefit from this rework but none block it.

---

## E10: Variant Robustness Under Load

**Repos**: `variants/custom-udp/`, `variants/hybrid/`, `variants/zenoh/`
**Goal**: Fix variant-implementation weaknesses uncovered by the first
full-matrix two-machine run of `configs/two-runner-all-variants.toml`.
These are not E9 contract issues — the runner contract is sound and 128/128
spawns invoke correctly on both machines. They're pre-existing bugs in
the variants that QoS-expansion now exercises (and that cross-machine
network behaviour exposes more aggressively than loopback).

Three independent threads:

1. **Custom UDP TCP framing panic** (T10.4) — `src/udp.rs:233` slices a
   length-prefix into a `Vec` sized by an untrusted length read off the
   wire, with no `>= 4` check. A torn TCP read at peer-shutdown returns
   a zero/garbage length and the variant panics. Cross-machine only.
2. **Hybrid high-throughput failures** (T10.1) — both UDP send (returns
   `WSAEWOULDBLOCK` on Windows under load) and TCP send (same, plus
   `CONNABORTED`/`CONNRESET` cascade once one side bails) fail at high
   rate. The TCP read loop also bails on transient connection errors.
   The original `variants/hybrid/CUSTOM.md` already specified blocking
   writes for TCP — implementation diverged.
3. **Zenoh path-count scaling** (T10.2) — workloads with 1000 distinct
   keys per tick time out independent of total throughput. 100-key
   workloads at 100K msg/s succeed; 1000-key workloads at 10K msg/s
   time out. The `max-throughput` workload also times out (different
   code path, separate failure mode worth investigating).

T10.3 (cross-machine smoke) was completed by the user as part of T9.4c
acceptance and is closed.

Out of scope for this epic:
- Performance tuning beyond the minimum needed to make spawns finish.
- Protocol redesign. Each variant should keep its existing semantics;
  fixes are at the I/O / framing layer.
- Aeron (E3c is still blocked on the Windows toolchain).

Dependencies: E9 (closed). E10 fixes can run in parallel — T10.1 and
T10.4 are small, T10.2 is investigation-heavy.

---

## E7: End-to-End Validation

**Goal**: Run the full benchmark pipeline across two machines and validate
results.

Scope:
- Deploy runner + all chosen variants on two LAN machines.
- Run a benchmark with the `scalar-flood` profile.
- Collect logs, run analysis, verify integrity passes and performance
  numbers are in the expected range per DESIGN.md targets.
- Document results and any issues discovered.

Dependencies: E2, at least one E3 variant, E4.

---

## E3f: WebSocket Variant

**Repo**: `variants/websocket/` (Rust binary)
**Goal**: Implement a replication variant using WebSocket as the reliable
transport. Represents the "browser-compatible reliable transport"
comparison and isolates the cost of WebSocket framing on top of TCP.

The interesting question this variant answers: **what does the WebSocket
framing layer (handshake, masking, length-prefixed frames) cost on top of
raw TCP under our workload?** Comparing E3f to E3e (Hybrid) at QoS 4
isolates that cost directly — both run TCP underneath, both use the same
runner-injected peer setup, differing only in what sits between the
application and the kernel socket.

Scope:
- Implements `Variant` trait from `variant-base`.
- **Reliable QoS only (3-4).** WebSocket is TCP-based; there is no
  unreliable mode. For QoS 1-2 the variant's `publish` returns a clear
  error and the spawn exits non-zero with a recognisable message — the
  benchmark configs simply do not spawn it at unreliable QoS levels.
  This is intentional: this variant exists to characterise reliable
  framing overhead, not to duplicate Hybrid's UDP path.
- Symmetric peer pairing: lower-sorted-index runner connects (WS client),
  higher-sorted-index runner accepts (WS server). One WS connection per
  peer pair, full-duplex.
- Peer hosts come from the runner-injected `--peers` (E9). Port derivation
  follows the same `runner_stride = 1 / qos_stride = 10` convention used
  by Hybrid TCP and QUIC. Variant-specific `--ws-base-port`.
- Same compact binary header on top of the WebSocket binary frame as the
  other variants use; WebSocket adds its own framing on top.
- Sync API: use the synchronous `tungstenite` crate over
  `std::net::TcpStream`. No tokio. Same blocking-write + short
  `SO_RCVTIMEO` polling trick as Hybrid, so kernel back-pressure remains
  the measured signal (matching Hybrid's design rationale, see its
  CUSTOM.md).
- EOT (E12): implement the TCP-frame variant of the protocol per
  `eot-protocol.md` — broadcast `eot_sent` frame to every peer over the
  same WS connection at end-of-operate, collect peers' `eot_sent` via
  the same channel.

Out of scope:
- TLS / `wss://`. Self-signed certificate juggling adds noise without
  measuring anything new (QUIC already pays for TLS in its numbers).
- Subprotocols, extensions (compression, etc.).
- HTTP/2 WebSockets (RFC 8441). Plain HTTP/1.1 upgrade only.
- QoS 1-2 over UDP. Use Hybrid for that comparison.

Dependencies: E1 (base crate), E2 (runner), E9 (`--peers` injection),
E12 (EOT trait + driver).

---

## E3g: WebRTC DataChannel Variant

**Repo**: `variants/webrtc/` (Rust binary)
**Goal**: Implement a replication variant using WebRTC DataChannels as the
transport. Represents the "browser stack on a LAN" comparison — the
heaviest stack in the lineup (DTLS + SCTP over UDP), but the only one
that natively offers both reliable and unreliable modes from a single
session.

The interesting question this variant answers: **what does the WebRTC
stack cost on a LAN compared to raw QUIC, raw UDP, and raw TCP?** It is
the only candidate that natively maps to all four QoS levels through
DataChannel options (ordered/unordered × reliable/maxRetransmits=0)
without any application-layer reliability code on our side.

Scope:
- Implements `Variant` trait from `variant-base`.
- All four QoS levels mapped to DataChannel configurations:
  - L1 (best-effort): unordered, `maxRetransmits=0`
  - L2 (latest-value): same as L1; receiver does seq filtering
  - L3 (reliable-ordered): ordered, default reliable
  - L4 (reliable): ordered, default reliable (same channel config as L3)
- Peer hosts come from the runner-injected `--peers`. ICE uses **host
  candidates only** — no STUN, no TURN, no mDNS candidates. Hard-coded
  host candidates derived from `--peers` and the variant-specific
  `--media-base-port` are sufficient on a LAN.
- **Signaling**: a small TCP signaling channel between every peer pair on
  a derived port (`--signaling-base-port`). The lower-sorted runner
  initiates the TCP connection and sends an SDP offer; the higher
  responds with an SDP answer; ICE candidates are exchanged over the
  same socket; the socket closes once the DataChannel reports `open`.
  Pairing and port derivation use the same `runner_stride = 1 /
  qos_stride = 10` convention as Hybrid/QUIC. The runner does NOT
  participate — signaling is entirely variant-to-variant.
- Sync-to-async bridge: the `webrtc` crate is async (tokio). Same pattern
  as the QUIC variant — internal tokio runtime, mpsc channels between
  the sync trait surface and async tasks. See `variants/quic/CUSTOM.md`
  for the established pattern; the worker should mirror it.
- EOT (E12): implement the DataChannel variant of the protocol per
  `eot-protocol.md` — send `eot_sent` over the L3/L4 (reliable) channel
  to every peer, parallel to QUIC's stream-end approach.
- Same compact binary header as other variants on top of the
  DataChannel message body.

Crate choice: `webrtc` (webrtc-rs). Pulls in many transitive deps but is
the most complete and best-documented Rust WebRTC implementation. Not
`str0m` for this benchmark — a sans-IO library would force the worker
to write the same kind of glue we already have in custom-udp; that
defeats the purpose of "what does the off-the-shelf WebRTC stack
cost?" If the build proves problematic on Windows the worker should
flag and we will reconsider.

Out of scope:
- STUN/TURN. LAN-only.
- mDNS ICE candidates.
- Browser interop (no SDP-munging or compatibility shims).
- DTLS certificate pinning beyond the bare minimum.
- Multiple DataChannels per peer pair beyond the four logical QoS
  channels.

Dependencies: E1 (base crate), E2 (runner), E9 (`--peers` injection),
E12 (EOT trait + driver). E3d (QUIC) provides the async-bridge pattern
to copy.

**Known limitation (T3g.2 outcome, 2026-05-06)**: the implemented
variant supports exactly **one peer per spawn**. webrtc-rs ties one
`RTCPeerConnection` to one UDP socket via `SettingEngine`, and our
`EphemeralUDP::new(p, p)` pin to a single derived `--media-base-port`
makes that socket unique per spawn. The two-runner case fits exactly;
N>2 runners would need a per-peer media-port stride or a Muxed UDP
setup. Variant errors clearly when violated. A future N-peer
extension is a separate epic if/when N-peer benchmarks become a
priority — not on the current backlog.

---

## E14: Threading-Mode Dimension and Receive-Centric Analysis

**Repos**: `variant-base/`, `variants/websocket/`, `variants/custom-udp/`,
`variants/hybrid/`, `variants/quic/`, `variants/webrtc/`, `variants/zenoh/`,
`runner/`, `analysis/`, plus contract updates in `metak-shared/`.

**Goal**: Add a `threading_mode` dimension to the benchmark matrix so each
variant can be measured under both single-threaded (sync, no tokio) and
multi-threaded (per-peer reader thread) execution models. Lift the
receive side from "a column in the metrics table" to "the headline metric
the benchmark optimises for", on the grounds that writers ship at requested
rate almost always but receivers are the actual sync bottleneck.

This epic exists because:

1. **WASM compilation target.** The team plans to compile some variant
   crates to WASM (browser and/or WASI). Browser-WASM does not support
   multi-threaded tokio runtimes; WASI is restricted. The team has
   real production scenarios that must adhere to strictly single-threaded
   operation, alongside other scenarios where multi-threading is allowed.
   The benchmark must characterise both.
2. **T-impl.10 acceptance partial.** The 2026-05-11 diagnostic on
   `configs/two-runner-websocket-qos4.toml` showed that a single-threaded
   WebSocket variant cannot drain at 100 K msg/s symmetric on this
   hardware: the per-message WS frame-parse cost caps receive
   throughput regardless of driver-side drain budget. The fix is to
   move the parse off the driver thread — and we should do this in a
   way that is generalisable across variants and observable to operators
   who want to compare the two regimes.
3. **Project framing.** Per `overview.md`: the goal is "keep multiple
   peers in sync under huge change diffs with lowest latency possible."
   The metric that decides whether peers are in sync is **receive
   throughput**, not write throughput. The analysis tool's headline
   number should reflect that.

### Scope

#### Variant-side (T14.1-T14.7)

- **T14.1** — `variant-base` infrastructure for threading-mode dimension.
  New `ThreadingMode { Single, Multi }` type, new `--threading-mode`
  injected CLI arg, new `Variant::supported_threading_modes()` trait
  method (default `&[Single]`), no-op `start_reader_threads` /
  `stop_reader_threads` hooks. New `--recv-buffer-kb` injected CLI arg
  (default 4096, range 64-65536). Driver passes the chosen mode to
  the variant via `Variant::connect`. No transport changes here.
- **T14.2** — `variants/websocket` implements `Multi`: per-peer reader
  thread per WS connection, decoded frames pushed into a bounded
  `mpsc::Sender<ReceivedUpdate>`. `poll_receive` becomes a fast
  channel `try_recv`. `SO_RCVBUF` set from `--recv-buffer-kb`.
  Capability declared `[Single, Multi]`. Closes the immediate T-impl.10
  follow-up.
- **T14.3** — `variants/custom-udp` implements `Multi`: per-socket recv
  thread for both UDP and TCP paths. `SO_RCVBUF` configurable.
  Capability `[Single, Multi]`.
- **T14.4** — `variants/hybrid` implements `Multi`: per-peer TCP reader
  thread, single recv thread for the UDP multicast socket.
  Capability `[Single, Multi]`. May discover Hybrid is already
  partly multi-threaded -- audit and align before extending.
- **T14.5** — `variants/quic` declares capability `[Multi]` only. No
  code change beyond the declaration and a `CUSTOM.md` note explaining
  why (quinn is fundamentally async; "single-threaded QUIC" would be
  misleading to claim).
- **T14.6** — `variants/webrtc` declares capability `[Multi]` only.
  Same shape as T14.5.
- **T14.7** — `variants/zenoh` declares capability `[Multi]` only.
  Same shape as T14.5.

#### Runner + config (T14.8)

- **T14.8** — runner + TOML schema add `threading_modes` expansion.
  Schema change: `[variant.common] threading_modes = ["single", "multi"]`
  (default `["single"]` for backwards compatibility -- existing configs
  continue to spawn single-threaded). Runner expands the cross-product
  with `qos`: a variant entry with `qos = [3, 4]` and
  `threading_modes = ["single", "multi"]` becomes four spawns
  (`<name>-qos3-single`, `<name>-qos3-multi`, `<name>-qos4-single`,
  `<name>-qos4-multi`). The runner consults each variant's declared
  `supported_threading_modes()` (via a sidecar `--probe` invocation or
  static declaration in TOML -- decide in the task) and silently skips
  unsupported modes with a stderr notice. Spawn naming and per-spawn
  log filenames preserve the new suffix.

#### Analysis (T11.5 -- filed under E11, can start in parallel)

- **T11.5** — Promote receive throughput to the headline metric.
  Summary tables lead with receive throughput per
  `(writer, receiver, variant, qos, threading_mode)`. Write throughput
  becomes the "requested rate" context column. Add a late-receive tail
  metric (receives whose `receive_ts - write_ts` exceeds 10x the 99th
  percentile of that group). All metric definitions stay
  backwards-compatible -- only the ordering and emphasis change.
  Threading-mode column appears in tables once T14.8 logs include it;
  before that the column is constant. Listed under E11 so it can start
  before T14.x lands.

### Out of scope

- N>2-peer benchmarks (orthogonal). WebRTC's known 1-peer limit from
  T3g.2 still stands.
- Replacing tokio with another async runtime for the async-only
  variants.
- WASM compilation itself. The team owns that separately. This epic
  only ensures the variants that need to compile to WASM have a
  single-threaded mode that is sync, not async-single-threaded.
- New transport variants. Existing five plus dummy are enough to
  characterise the threading-mode dimension.
- Changes to EOT (E12), clock-sync (E8), or runner-runner coordination
  (E2 + E9).

### Dependencies

- E1 (Variant trait + protocol driver): touched.
- E2 (runner): touched at T14.8.
- E3a/b/d/e/f/g (the six active variants): each touched at exactly
  one task.
- E9 (`--peers` injection): touched lightly to add `--threading-mode`
  and `--recv-buffer-kb` to the injected-arg list.
- E11 (analysis): T11.5 lives here.

### Acceptance

- A two-runner config that lists `threading_modes = ["single", "multi"]`
  for the websocket variant runs both modes back-to-back, produces
  per-mode JSONL logs, and the analysis tool reports receive throughput
  for each mode separately.
- WebSocket multi-threaded mode sustains the previously-failing
  `websocket-1000x100hz-qos4` symmetric flood with delivery >= 99%
  on the same machine as the 2026-05-11 incident.
- Single-threaded WebSocket on the same workload completes the spawn
  without `WSAECONNRESET` (it may show <100% delivery -- that is a
  legitimate measured result, not a failure).
- QUIC, WebRTC, Zenoh continue to function exactly as before; their
  capability declaration is the only change.
- Analysis summary tables lead with receive throughput; existing
  metrics still computed and visible.

### Future work (deferred -- NOT part of E14)

#### Zenoh single-threaded client via router-RPC

Although the Zenoh crate is internally multi-threaded, Zenoh's
architecture naturally supports an out-of-process router. A future
variant configuration could launch a separate `zenohd` process as a
sidecar and have the variant client (a thin RPC client over Zenoh's
admin/control surface or a custom RPC channel) operate strictly
single-threaded, with the router process absorbing all concurrency.

This is the WASM-friendly path for Zenoh: the WASM binary contains
only the single-threaded RPC client; the multi-threaded router runs
natively beside it. Real production scenarios in the team's target
deployments would use exactly this topology.

Filed as **T14.9** in `TASKS.md` (skeleton; not scheduled). When
implemented, Zenoh's capability declaration becomes `[Single, Multi]`
and the existing Multi-only declaration from T14.7 is updated. T14.9
also requires a small runner change to optionally spawn a sidecar
process per variant (the router) and tear it down after the spawn --
that mechanism would be reusable for any future variant that benefits
from a sidecar.

Out of scope for E14: the router-spawning mechanism, the RPC protocol
between client and router, and any analysis-side changes to report
router resource usage separately from variant resource usage.

---

## E15: Stdout Progress + Runner-Coordinated Termination

**Repos**: `variant-base/`, `runner/`, every concrete variant
(`variants/*/`), `analysis/`. Plus contract changes to
`metak-shared/api-contracts/variant-cli.md`,
`metak-shared/api-contracts/jsonl-log-schema.md`,
`metak-shared/api-contracts/runner-coordination.md`,
`metak-shared/architecture.md`.

**Status**: filed 2026-05-12 by orchestrator after the user's
architectural feedback on the recurring resume / EOT / asymmetric-
timeout failure modes that E14 closed reactively. E15 is the
proactive simplification.

### Motivation

E14 fixed many specific failures (T14.13 QUIC ordering; T14.16
Data/Lifecycle channel split; T14.17 timeout classifier; T14.18 EOT
TCP control channel for UDP-family; T14.19 SO_SNDTIMEO for TCP
single-mode; T14.22 startup retry; T14.23 resume manifest classifier;
T14.24 resume_manifest TCP barrier). Most of these existed to work
around a single core architectural choice: **EOT is signaled by the
variant over the data transport (or a side-channel parallel to it),
and the runner times out the spawn on a wall-clock budget**.

User observation (2026-05-12): this architecture has accumulated more
complexity than the underlying problem warrants. There is a simpler
model:

1. The **variant** emits a one-line-per-second JSON progress event to
   stdout: `{"event":"progress","ts":...,"phase":"operate","sent":N,
   "received":M,"eot_sent":bool,"eot_received":bool}`. It still writes
   its full JSONL event log to disk (unchanged). It still writes
   `eot_sent` to the JSONL when applicable (analysis needs the marker
   to bound the operate window).
2. The **runner** reads the child's stdout line-by-line, parses the
   progress events, tracks per-spawn state.
3. The **two (or N) runners** coordinate over their existing
   runner-runner channel to exchange per-runner aggregate progress
   every ~1 second. Each runner knows the OTHER runner's variant's
   progress.
4. Termination decision is **activity-based + phase-aware**:
   - Stabilize phase: silence is expected; only the phase clock
     advances spawn state.
   - Operate phase: when EVERY runner reports its variant has been
     idle (no new sends AND no new receives) for >= 5 seconds, all
     runners agree the operate phase is naturally done.
   - Silent phase: short drain window before disconnect, same as
     today.
5. Per-spawn wall-clock timeout disappears as the primary control
   signal. It remains as a fallback safety net (e.g. an absolute
   max of `max_spawn_secs = 5 minutes` per spawn) only.

### What this unifies and removes

After E15 lands, the following becomes redundant and can be removed
or simplified:

- **T14.18** (custom-udp + hybrid TCP control side-channel for EOT):
  the data transport no longer carries EOT; the runner-coord channel
  does. Variants no longer need the dedicated TCP control connection.
- **T14.20** (websocket TCP control side-channel for EOT, in flight
  but cancelled in favour of E15): the same logic. Never lands.
- **The per-variant EOT trait surface** (`signal_end_of_test`,
  `wait_for_peer_eots`, `eot_timeout_internal` classification path in
  T14.17): unwound because the variant doesn't run an EOT phase any
  more. The variant's JSONL still emits an `eot_sent` event when the
  WRITER finishes its operate window (i.e. when it observes its own
  idle condition), so analysis (T11.5) keeps the marker. But the
  on-wire EOT exchange is gone.
- Most of the **runner's wall-clock-timeout machinery** for the
  per-spawn case: the runner still kills a spawn that exceeds the
  hard safety budget, but the common case is activity-driven
  termination.
- Various per-variant CUSTOM.md sections documenting EOT routing
  (T14.18, T14.20 historical notes).

### Scaling notes

N>2 runners scale naturally: each runner publishes its aggregate
progress; each runner consumes every peer runner's progress. The
"all idle for 5s" predicate generalises trivially. Wait times can
scale with N (e.g. `idle_threshold_secs = 5 + N * 0.5`) if
benchmark variance grows with peer count -- experiment when needed.
No per-peer breakdown needed at the variant level; per-peer analysis
is downstream (T11.5 already does it from JSONL `writer` fields).

### Scope (T15.x sub-tasks)

- **T15.1** -- `variant-base`: progress emission to stdout. New CLI
  arg `--progress-stdout-interval-ms` (default 1000; 0 disables for
  back-compat). Stable JSON schema. Atomic line writes (one
  `println!` per event). Phase-aware: `phase` field reflects current
  protocol-driver phase (`connect | stabilize | operate | eot |
  silent | done`). Counters (`sent`, `received`) are monotonic
  per-spawn aggregates across all peers; per-peer breakdown stays
  in JSONL only.
- **T15.2** -- `runner`: read each child's stdout line-by-line.
  Parse progress events. Maintain per-spawn `LocalProgressTracker`
  with `last_sent_change_ts`, `last_received_change_ts`, `phase`,
  raw counter snapshots. Existing T-impl.1 stderr capture path
  stays; stdout becomes a parallel stream.
- **T15.3** -- `runner-coord`: extend the runner-runner channel to
  exchange `RemoteProgressSnapshot` every ~1 second per active
  spawn. Use the same TCP-per-peer transport that T14.24 introduced
  for resume_manifest (reliable, large-payload friendly). New
  protocol message `ProgressUpdate { runner, spawn, ts, phase,
  sent, received, eot_sent, eot_received }`. Reuse port from T14.24
  if convenient; otherwise its own offset.
- **T15.4** -- `runner`: phase-aware termination state machine.
  - During `stabilize`: spawn termination is driven by
    `stabilize_secs` elapsing on the variant's side (it transitions
    to operate naturally via its existing phase logic and the
    runner just observes via progress events).
  - During `operate`: when local AND every remote runner reports
    its variant's `(sent, received)` counters have not advanced for
    >= `operate_idle_secs` (default 5), the runner notes "operate
    done locally". When all runners have noted "operate done", they
    agree (via the coord channel) and the next progress tick will
    show the variant having advanced its own phase to `silent` via
    its own phase logic. The runner does NOT push state TO the
    variant -- the variant's protocol driver advances its own
    phase via the same mechanism it uses today (just earlier, when
    its own idle-detection fires; see T15.5).
  - During `silent`: `silent_secs` elapses, variant exits, runner
    collects exit code as today.
  - Safety net: per-spawn `max_spawn_secs` (default 300) -- if the
    activity-based path doesn't fire after this absolute deadline,
    runner kills the child as today. Should rarely fire.
- **T15.5** -- `variant-base`: variant-side idle detection. Same
  threshold logic as the runner uses, but observed locally inside
  the variant's protocol driver: when both local `sent` and
  `received` counters haven't moved for `operate_idle_secs`,
  variant emits `eot_sent` to its JSONL (the marker analysis needs)
  and transitions internally to `silent` phase. No on-wire EOT
  exchange. Progress events emitted from `silent` then `done` so
  the runner sees the transitions.
- **T15.6** -- `analysis`: integrate the new state. The T11.5
  receive-headline pivot already uses `eot_sent.ts` to bound the
  operate window. That continues to work because variants keep
  emitting `eot_sent` to JSONL. T14.17 classifications adapt:
  `eot_timeout_internal` and `eot_lost` become much rarer (only the
  `max_spawn_secs` safety-net case). New classification:
  `runner_idle_terminated` (clean exit by activity detection).
- **T15.7** -- contract updates: `variant-cli.md` documents
  `--progress-stdout-interval-ms` and the stdout JSON schema.
  `runner-coordination.md` documents the new `ProgressUpdate` message
  and the cross-runner idle-agreement protocol. `architecture.md`
  retracts the "No IPC between runner and variant" sentence; the
  rationale is updated: one-way stdout from variant to runner is
  observational, not directive, so the original "runner must not
  interfere with measurements" principle is preserved.
- **T15.8** -- cleanup (DEFERRED until T15.1-7 are stable):
  - Remove `signal_end_of_test` / `wait_for_peer_eots` from the
    `Variant` trait. Each variant's implementation removed.
  - Remove the per-variant control TCP connections (T14.18 in
    custom-udp + hybrid; T14.20 was cancelled before landing).
  - Remove `--eot-timeout-secs` arg (no on-wire EOT phase to time
    out anymore).
  - Update each variant's `CUSTOM.md` to retract the EOT-routing
    sections.
  - Remove `eot_timeout_internal` classification path in T14.17 if
    it has no real triggers left.
- **T15.9** -- test adaptation: existing variant integration tests,
  runner integration tests, and the T11.5 / T14.17 analysis tests
  must be updated to the new architecture. **Unit-test coverage of
  the new state machine** (T15.4 + T15.5) is mandatory; each new
  T15.x task ships its own unit tests.

### Out of scope

- Runtime tuning of `operate_idle_secs`, `max_spawn_secs` beyond
  reasonable defaults. Tune later if real workloads demand.
- Changing what the variant emits to JSONL (other than removing the
  on-wire EOT events that no longer exist). Analysis stays compatible.
- Killing the existing E14 follow-ups that are deferred separately
  (T14.9 Zenoh router-RPC stays deferred; the new architecture
  doesn't require it but doesn't preclude it either).
- WebRTC / Zenoh internal threading. E15 doesn't ask the variants to
  change their threading mode; it just changes what the runner
  observes and how it decides termination.

### Acceptance gates

- Existing stress smoke `configs/two-runner-stress-e14.toml` runs
  end-to-end without per-spawn wall-clock timeouts firing. Every
  spawn either reaches activity-based termination cleanly or hits
  the safety-net `max_spawn_secs` (which should be rare).
- Zenoh asymmetric timeouts no longer manifest as `eot_timeout_internal`
  or `eot_lost` -- they manifest only as `runner_idle_terminated` or
  the safety-net kill, both of which are clean classifications.
- WebSocket Single mode at high rate classifies `runner_idle_terminated`
  instead of `eot_timeout_internal` (closes the T14.20 motivation
  permanently).
- Analysis re-runs on existing datasets produce identical numerical
  output (modulo the new classification labels).
- Each new T15.x task ships unit tests; the runner gains state-machine
  tests for the phase-aware idle detector.

### Dependencies

- E1 (Variant trait + protocol driver): touched.
- E2 (runner): touched.
- E3a/b/d/e/f/g (all six active variants): touched lightly (the
  variant trait gains a per-variant idle-detection hook, but each
  variant's transport code is unchanged).
- E11 (analysis): touched lightly at T11.5 / T14.17.
- T14.18, T14.20: invalidated. T14.20 cancelled in favour of E15;
  T14.18 stays landed but its code is targeted for removal in T15.8.

---

## E16: Diagnostic Cleanup from 2026-05-14 Full-Matrix Analysis

**Repos**: `analysis/`, `variants/hybrid/`, `variants/websocket/`,
`variants/custom-udp/`, `variants/zenoh/`, plus doc updates in
`metak-shared/`.

**Status**: filed 2026-05-14 by the orchestrator after a `--dump`
summary run of the 112 GB
`logs/same-machine-all-variants-01-20260514_084636/` dataset surfaced
267 incomplete-sample warnings, four classes of variant-side
regression, and one analysis-tool noise issue. See
`logs/same-machine-all-variants-01-20260514_084636/analysis/analyze_report.md`
for the source observations.

### Motivation

The first end-to-end matrix run that exercises the threading-mode
dimension (E14) plus the runner-idle termination (E15) plus all six
active variants on every workload × QoS combination produced a clear
ranking of pre-existing weaknesses:

1. **Measurement integrity bug** in `websocket-multi`: negative p50
   latencies on QoS 3/4 (e.g. `-0.025 ms` on `100x100hz qos3`). Writes
   are appearing in the analysis pipeline with `write_ts > receive_ts`.
   This blocks T11.5's receive-throughput headline from being trusted
   as the project's headline metric.
2. **Single-threaded hybrid TCP is non-functional under load** at QoS
   3/4. 0.1–24.7 % delivery at rates as low as 1 000 msg/s. Single-mode
   is the WASM-compatible path; this defeats the WASM goal for hybrid.
3. **Zenoh at 1 000-path workloads collapses asymmetrically.** One
   peer keeps writing, the other gives up (~500 000 backpressure-skip
   events) and one direction of delivery goes to 0 %. Nine spawns
   terminate via `variant_self_killed_idle`.
4. **Custom-UDP multi regresses below single** at high path counts.
5. **Analysis tool emits warnings on values that round to 100 %.**
   ~30 false-positive lines clutter the warning list.

### Scope (T16.x sub-tasks)

- **T16.1** — analysis: tolerance fix in `incomplete_warnings.py` so
  rows whose delivery rounds to `100.00 %` (i.e. raw >= 99.995 %) do
  not emit a "<100.0%" warning. Keep the `[FAIL: completeness]`
  annotation on the integrity row (that's correct), only the
  user-visible warning line is suppressed when the rounded display
  equals 100. Unit test in `analysis/tests/`.
- **T16.2** — `variants/websocket/`: diagnose and fix the negative
  latency observed in `multi` mode. Inspect when `write_ts` is
  captured relative to the publish call and the multi-thread reader's
  `receive_ts` capture site. Hypothesis: the multi reader records
  `receive_ts` on the reader thread *before* the writer thread takes
  its `write_ts` after returning from the non-blocking send. Fix the
  ts-capture ordering so `write_ts` is taken *before* the send completes
  (or use a single time source consistently). Validate with the
  existing `tests/fixtures/two-runner-websocket-qos4.toml` reproducer.
- **T16.3** — `variants/hybrid/`: restore TCP back-pressure handling
  in single-threaded mode (audit whether T14.19's SO_SNDTIMEO is
  actually in the single-mode path, or only the multi-mode path).
  Acceptance: hybrid-single QoS 3/4 reaches >=99 % delivery on
  `10x100hz` and >=80 % on `100x100hz`. The `1000x100hz` cell may
  stay below threshold for hardware reasons; document the achievable
  ceiling in `variants/hybrid/CUSTOM.md`.
- **T16.4** — `variants/custom-udp/`: investigate why multi mode is
  worse than single mode at `1000x10hz qos3` (16.1 % vs 64.0 %) and
  `1000x100hz qos3` (10.6 % vs 55.8 %). Suspect reader-thread
  contention on the NACK feedback path; instrument and confirm
  before changing code. Reproducer config likely small.
- **T16.5** — `variants/zenoh/`: investigate the asymmetric
  1000-path collapse. Symptom: one peer writes ~3 M messages, the
  other writes ~2 000 and accumulates ~500 000 `backpressure_skipped`
  events; one delivery direction reads 0 %. Look at Zenoh declaration
  propagation timing for 1000 paths. The variant's
  `bench/__eot__/<writer>` key path and declaration order may matter.
  Acceptance: symmetric writer counts (delta < 10 %) on a small
  1000-path reproducer; one-direction delivery > 50 % at QoS 1/2,
  > 90 % at QoS 3/4.
- **T16.6** — `metak-shared/ANALYSIS.md`: docs-only. Add a paragraph
  to the pivot-table section explaining why `zenoh-multi` shows
  ~400 % multicast loopback ratio (Zenoh's subscription topology
  reflects each message back from both the local data board AND the
  Zenoh fabric subscription, doubling the 200 % convention seen in
  the other multicast variants).
- **T16.7** — analysis: in `incomplete_warnings.py`, when emitting a
  `delivery shortfall` line, also emit the Ratio% from the pivot
  parser (writer-side shortfall) so operators see *both* numbers.
  Example: `delivery 100.0% (ratio 9.3% — writer-side shortfall)`.
  Cosmetic improvement; no behaviour change.

### Out of scope

- The known WebRTC 1-peer limit (T3g.2). 67–82 % delivery on
  `1000x100hz qos3/4` is the design ceiling, not a regression.
- The `1000x100hz` writer-side bottleneck across all variants. This
  is a hardware/workload characterisation, not a bug. T16.7 surfaces
  it; it does not get fixed.
- A full-matrix 112 GB re-run between fixes. Each task uses a
  targeted reproducer config so the fix loop stays under 10 minutes.
- Cross-machine validation. Stays user-owned.

### Acceptance gates

- T16.1: re-running analyze on the same dataset shows ~30 fewer
  warning lines and zero `100.0% (<100.0%)` lines.
- T16.2: websocket-multi p50 on any QoS 3/4 cell is positive.
  Latency monotonically non-decreasing across p50 → p95 → p99 → max.
- T16.3: hybrid-single QoS 3/4 reaches the threshold listed above
  on the reproducer fixture.
- T16.4: custom-udp multi mode is at least as good as single mode
  on `1000x10hz qos3` reproducer.
- T16.5: zenoh-multi `1000x10hz qos3` reproducer shows symmetric
  writer counts and >=90 % delivery in both directions.
- T16.6: a code reviewer reading ANALYSIS.md understands the 400 %
  ratio without seeing the source code.
- T16.7: the warnings file emits the Ratio% column for every
  delivery-shortfall line.

### Dependencies

- E1 (Variant trait + protocol driver): T16.2, T16.3 may touch.
- E3a/b/e/f: T16.2, T16.3, T16.4, T16.5 each touch one variant.
- E4/E11 (analysis): T16.1, T16.6, T16.7 each touch one analysis
  module.
- E14 / E15: this epic is downstream of both. The dataset under
  analysis is the first one to exercise both together.

---

## E17: Strict No-Skip Contract for QoS 3 / QoS 4

**Repos**: `metak-shared/` (contracts + DESIGN.md, T17.1), `variant-base/`,
every concrete variant (`variants/*/`), `analysis/`, `runner/`.

**Status**: filed 2026-05-18. Wave 1 (T17.2 foundation) + Wave 2
(T17.3-T17.8 per-variant fixes) **complete**. Wave 3 (T17.9 analysis +
T17.10 user-owned matrix re-run) pending.

### Motivation

The post-T16.16 QoS-4 heatmap revealed that several variants drop
30-95% of writes at saturation rates (`1000x100hz qos4`):
- websocket-multi 100.0% ✓ (reference impl), custom-udp-multi qos3 99.8% ✓
- custom-udp qos4 (TCP) 31.8%/44.9%, hybrid qos3/4 10-86%, websocket-single
  qos3/4 2.4%, quic-multi 86-89%, webrtc-multi ~55%, zenoh-multi ~44-57%

User decision 2026-05-18: at QoS 3/4 prefer delivery over throughput,
even Zenoh must coordinate peers. At QoS 1/2 keep throughput priority.

### Contract changes (T17.1 — done)

- `metak-shared/DESIGN.md` § 6.5 new "Strict No-Skip Contract": variant
  MUST block at QoS 3/4 publish, MUST NOT return `Ok(false)`,
  application-level back-pressure required if transport doesn't natively
  provide it.
- `metak-shared/DESIGN.md` § 6.6 QoS Summary table updated with `Skips
  allowed` + `Priority` columns.
- `metak-shared/api-contracts/jsonl-log-schema.md` — `backpressure_skipped`
  restricted to QoS 1/2.
- `metak-shared/api-contracts/variant-cli.md` — `--qos` row notes the
  publish-blocking semantics.

### Sub-tasks (status)

- T17.1 Contract updates — **DONE** (orchestrator)
- T17.2 variant-base driver loop on Ok(false) at qos3/4 — **DONE**
  (commits `842fb5e`, `adf39f7`)
- T17.3 custom-udp TCP qos4 blocking — **DONE**
  (commits `62a0e0c`, `744443a`, `4946310`)
- T17.4 hybrid TCP qos3/4 blocking (subsumes T16.3) — **DONE**
  (commits `9d2f15d`, `1b94bfc`, `fc67f64`, `aa0775b`)
- T17.5 websocket single-mode drain-and-retry — **DONE**
  (commit `c8e6629`)
- T17.6 quic bounded-mpsc + tight stream windows — **DONE**
  (commits `e8ff2bb`, `b07c5b3`)
- T17.7 webrtc bounded-mpsc + drain-on-disconnect — **DONE**
  (commits `ea45545`, `50707e8`, `b56fa7f`, `9a675b6`)
- T17.8 zenoh credit/window over Zenoh side-channel (reopens T16.12) —
  **DONE** (commits `4195a2d`, `24dd4ad`)
- T17.9 analysis: flag `backpressure_skipped` at qos3/4 — **PENDING**
- T17.10 user-owned full-matrix re-run + acceptance heatmap — **PENDING**

### Acceptance gates

- Post-T17.10 heatmap shows 100.0% delivery on every QoS 3/4 cell across
  every variant on `configs/two-runner-all-variants.toml`.
- Throughput ratio cells may drop arbitrarily — that's the trade.
- T17.9 integrity check catches any `backpressure_skipped` at QoS 3/4.

### Side-effects observed during Wave 2 (worth flagging for T17.10)

- webrtc reproducer needed `silent_secs = 30` for SCTP outbound drain;
  matrix default is 2. Consider per-variant override or global bump.
- hybrid reproducer needed `silent_secs = 10` for in-flight TCP drain.
- websocket-multi saturation test surfaced ~55 duplicate
  `(writer, seq, path)` triples (0.012%) — uniq delivery is 100% but
  integrity flags `[FAIL: duplicates]`. Filed for follow-up (T17.11
  TBD if user wants to chase the tungstenite internal cause).
- Three workers reported "working tree appears to have been reverted by
  some external action" mid-task during the parallel-commit wave —
  cost of 6 concurrent commits to `main`. None lost data but consider
  worktree isolation for future waves.

---

## E18: Compact Log Format + Runner Log-Path + Auto-Analyze

**Repos**: `variant-base/`, `variants/*/`, `runner/`, `analysis/`,
`metak-shared/`.

**Status**: filed 2026-05-18 at user request. **Waits on E17 completion**
(T17.10 acceptance) before any T18.x worker spawns, so the matrix
re-run produces meaningful baselines on both formats.

### Motivation

Full-matrix two-machine runs produce thousands of GB of per-message
JSONL. User wants:
1. **Compact post-run digest format** — variant accumulates writes/
   receives in memory during operate/silent, then in a new `digest`
   phase serializes a single compact file per spawn (target: 30-50×
   smaller than current JSONL).
2. **Configurable log location** — runner accepts `--log-dir <path>`
   for shared network folder output.
3. **Auto-analyze flag** — runner accepts `--analyze-full`; after the
   matrix completes the lower-sorted-index runner shells out to
   `python analysis/analyze.py <log-dir> --summary --dump --diagrams
   --output <log-dir>/analysis`.

### Format decisions (user-approved 2026-05-18)

- **Columnar arrays**: `(ts: i64 ns, path_idx: u32)` for writes;
  `(ts: i64 ns, path_idx: u32, writer_idx: u8)` for receives. **No
  `seq`** — prepares for N>2 peers; correlation is ordering-based
  (Nth write on `(writer, path)` ↔ Nth receive at receiver). Per-message
  latency exact at QoS 3/4 (strict order, no drops); QoS 1/2 keeps
  aggregate metrics only.
- **Binary format: Apache Parquet** (not JSON, not pickle). Reuses the
  analysis cache pipeline so the variant writes the final storage shape
  directly, eliminating the JSONL→Parquet cache rebuild step. Dict-
  encoding + snappy/zstd compression. Expected ~30-50× size reduction.
- **Path-intern table** in the metainfo header; per-event arrays carry
  the path_idx (u32). Workloads with repeated path strings (the
  dominant case) get an additional 10-100× win on path storage alone.
- **Legacy JSONL** stays as opt-in debug mode under
  `--legacy-jsonl-events` (default OFF).

### Sub-tasks (planned, none started)

- T18.1 Contract: `metak-shared/api-contracts/compact-log-schema.md`
  (orchestrator-only).
- T18.2 variant-base in-memory buffers + digest phase + Parquet writer.
- T18.3 variant audit for any that bypass the variant-base logger.
- T18.4 analysis: load both compact and legacy formats.
- T18.5 runner: `--log-dir <path>` arg + TOML key.
- T18.6 runner: `--analyze-full` arg, lower-sorted-index runner invokes
  Python analyzer post-matrix.
- T18.7 user-owned: re-run + size/correctness validation.

### Dependencies

E17 completion (T17.10 acceptance) is required before T18.7 can produce
a meaningful baseline. T18.1-T18.6 implementation can start once
T17.9-T17.10 land.

---

## E19: Workload-Shape Dimension

**Repos**: `variant-base/`, `runner/`, `analysis/`, `metak-shared/`.

**Status**: filed 2026-05-19 at user request. Implementation starts
immediately — additive on top of E17 / E18 work, no blocking
dependency.

### Motivation

The existing benchmark exclusively exercises the **per-message-overhead
extreme**: `scalar-flood` with `vpt = 1000` produces 1000 distinct
WriteOps per tick, each carrying a single 8-byte f64. This is one end
of the spectrum.

Realistic robotics / sensor-control workloads sit in the middle: a
handful of WriteOps per tick (5-50), each carrying a structured block
of values (a joint-state array, a trajectory waypoint set, a camera
buffer). Total scalar count per tick is still ~1000, but the
wire/serialization shape is radically different from `scalar-flood`.
Today nothing in the benchmark surfaces this.

E19 adds two workload profiles to cover this gap:

- **`block-flood`** — fixed-size blocks. `vpt / blob_size` WriteOps per
  tick, each carrying a `blob_size`-element block of scalars. Stresses
  serialization cost and large-message transport handling.
- **`mixed-types`** — heterogeneous tree per tick composed of scalars
  + arrays + nested dicts, with exactly `vpt` total leaves distributed
  across all three shapes. Stresses the full serialization path
  including nested `KeyValue` structures.

### Locked spec (user-approved 2026-05-19)

- **`vpt` invariant**: across all workload profiles, `values_per_tick`
  = total scalar (leaf) values per tick. Profiles differ in *how*
  those leaves are packed into WriteOps; the leaf count remains the
  analysis tool's comparable headline denominator.
- **`block-flood` params**: `blob_size: u32` (default 100). Derives
  `writes_per_tick = vpt / blob_size`. Validation: `vpt % blob_size ==
  0`.
- **`mixed-types` params**:
  - `mixed_scalars_min`, `mixed_scalars_max` — count of standalone
    scalar WriteOps per tick. Drawn as `nS = rand(min, max)`.
  - `mixed_arrays_min`, `mixed_arrays_max` — total leaves to allocate
    to arrays. Drawn as `nA = rand(min, min(max, (vpt - nS) / 2))`,
    distributed across `rand(1, mixed_arrays_max)` array WriteOps.
  - `mixed_dict_split_max` — max branching factor at each level of
    the nested-dict allocation (min implicitly 1). Dicts absorb the
    remainder `vpt - nS - nA`, splitting recursively using
    `rand(1, mixed_dict_split_max)` at each level until each leaf
    bucket holds one scalar.
  - **Allocation order**: scalars → arrays → dicts. Biases the
    generated tree toward nested structure.
- **Latency canonical unit**: per-WriteOp everywhere. Scalar-flood
  per-leaf latency is preserved as a coincidence (1 leaf = 1 op).
  Block-flood / mixed-types report one latency sample per published
  block — honest across all profiles. Cross-profile latency charts
  need no footnote.
- **Wire encoding**: opaque blob per WriteOp. The variant trait
  signature `publish(path, &[u8], qos, seq)` is unchanged. Variants
  treat the blob as one logical unit; transport-level
  batching/coalescing remains variant-implementation-defined.
  Receiver does NOT introspect payload bytes.
- **Receive-side `leaf_count`**: not on the wire, not on the `receive`
  event. The analysis tool correlates receives with their matching
  write by `(writer, seq, path)` and inherits `leaf_count` / `shape`
  from the write side.

### Sub-tasks

- **T19.1** Contract updates (jsonl-log-schema, compact-log-schema,
  toml-config-schema, variant-cli, glossary) — **orchestrator-self**.
- **T19.2** variant-base: workload structs (`BlockFlood`,
  `MixedTypes`), `WriteOp` extension (`leaf_count`, `shape`), logger
  emission of new fields.
- **T19.3** variant-base: CLI plumbing for new workload params +
  validation. Depends on T19.2.
- **T19.4** runner: TOML schema for new `[variant.common]` keys, CLI
  forwarding (verbatim — runner does not interpret). Depends on T19.3.
- **T19.5** analysis: parse new fields, correlate receives to inherit
  `leaf_count` / `shape`, report leaves/sec, ops/sec, bytes/sec
  separately. Backfill defaults for legacy logs.
- **T19.6** analysis: plots — restructure comparison-qos chart
  (vertical stack + per-variant grouping with workload fill patterns
  + threading distinction), add per-variant throughput-vs-shape chart.
- **T19.7** docs: glossary terms, `BENCHMARK.md` § 6 updated with
  locked spec.
- **T19.8** E2E validation — two-runner config with scalar-flood +
  block-flood + mixed-types back-to-back on at least one variant.

### Post-validation follow-ups (filed 2026-05-19 after T19.8 report)

- **T19.9** analysis: post-validation UX fixes (pivot regex for
  unsuffixed variant names; throughput chart x-axis = workload name
  not shape; canonical sort order for shape and workload axes).
- **T19.10** legacy JSONL cleanup: remove `--legacy-jsonl-events`
  opt-in and the per-event JSONL emission/parse paths. JSONL becomes
  lifecycle-only (`phase`, `connected`, `eot_*`, `resource`,
  `clock_sync`). Per-event observations are compact-Parquet only.
  User-directed at T19.8 acceptance: "we don't have or want to ever
  keep any legacy behaviour, clear it out please." Split into
  T19.10a (variant-base), T19.10b (runner), T19.10c (analysis).
- **T19.11** variant-base: remove vestigial `attach_compact_sink`
  bool param shim that T19.10a deliberately left because its only
  consumer (`variants/websocket` in-tree test) was outside scope.
- **T19.12** analysis: fix `Shape` column for `mixed-types` rows —
  derive from delivery-level shape distribution rather than the
  single-dominant `PerformanceResult.shape`. Pre-existing concern
  flagged by T19.8 (issue #6) and explicitly excluded from T19.9.

### Sequencing

```
T19.1 (orchestrator) --> T19.2 --> T19.3 --> T19.4 --|
                         T19.5 --> T19.6 -----------|--> T19.8
                         T19.7 ----------------------|
```

Wave 1 (after T19.1): T19.2, T19.5, T19.7 in parallel — only need the
contracts. Wave 2: T19.3, T19.6. Wave 3: T19.4. Wave 4: T19.8.

### Backward compatibility

Schema changes are additive. Legacy logs (pre-E19) backfill to
`leaf_count = 1`, `shape = "scalar"`. Existing scalar-flood results
stay valid and remain on the same axis as new block-flood /
mixed-types results. No re-run needed.

### Existing variant binaries

No code changes expected in any of the concrete variants (zenoh,
custom-udp, quic, hybrid, websocket, webrtc, aeron). They accept
opaque `&[u8]` payloads and ship them; payload size grows but the
trait interface is unchanged. T19.3 includes a smoke test against
variant-dummy + custom-udp at `block-flood blob_size = 100`. Variants
with fixed buffer-size hints (e.g. custom-udp's `buffer_size = 65536`)
get a startup validation warning if `blob_size * 8 > buffer_size`.

