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

## E3c: Aeron Variant

**Repo**: `variants/aeron/`
**Goal**: Implement a replication variant using Adaptive Aeron. Represents
the "finance-grade" performance ceiling.

Scope:
- Implements `Variant` trait from `variant-base`.
- Uses `rusteron-client` crate (C bindings to Aeron).
- Aeron media driver handles peer coordination.
- UDP multicast for data distribution.
- Aeron-specific CLI args: `aeron_dir`, `channel`, `stream_id`.
- Buffer callbacks into internal queue, drain via `poll_receive`.

Dependencies: E1 (base crate), E2 (runner to spawn it). Aeron media
driver must be running on each machine.

Note: C bindings introduce unsafe Rust. Extra review needed.

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
