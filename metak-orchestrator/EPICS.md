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
