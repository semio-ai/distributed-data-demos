# Epics

## E1: Benchmark Runner

**Repo**: `runner/`
**Goal**: Implement the leaderless runner binary that coordinates benchmark
execution across machines.

Scope:
- CLI: `runner --name <name> --config <path.toml>`
- TOML config parsing (runners list, default timeout, variant definitions
  with common/specific sections).
- UDP broadcast discovery with config-hash verification.
- Barrier sync protocol (ready / done per variant).
- Child process spawning with CLI args derived from config (common + specific
  + `--launch-ts`).
- Monitor child for exit/timeout. Record exit status.
- Proceed through variants in config order.

Dependencies: API contracts for runner CLI, TOML config schema, runner
coordination protocol, and variant CLI contract must be finalized first.

---

## E2: First Variant — Zenoh

**Repo**: `variants/zenoh/`
**Goal**: Implement a replication variant using Zenoh as the transport layer.

Scope:
- CLI arg parsing (common + Zenoh-specific options).
- Peer discovery and connection via Zenoh.
- Test protocol phases: connect, stabilize, operate, silent.
- Workload execution (start with `scalar-flood` profile).
- Single-writer subtree ownership and value publishing.
- JSONL logging per the log format contract.
- Exit 0 on success.

Dependencies: E1 (runner must be able to spawn it), variant CLI contract,
JSONL log schema contract.

---

## E3: Analysis Tool — Phase 1 (Foundation)

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

Dependencies: JSONL log schema contract (must match what variants produce).

---

## E4: Second Variant — Custom UDP

**Repo**: `variants/custom-udp/`
**Goal**: Implement a replication variant using raw UDP sockets with custom
protocol logic.

Scope:
- Same contract as E2 but with custom networking: multicast/unicast UDP,
  manual serialization, sequence numbers, optional NACK recovery.
- Supports all four QoS levels natively.
- CLI arg parsing (common + custom-specific options like `buffer_size`,
  `multicast_group`).

Dependencies: E1, variant CLI contract, JSONL log schema, learnings from E2.

---

## E5: Analysis Tool — Phase 2 (Diagrams)

**Repo**: `analysis/`
**Goal**: Add diagram generation to the analysis tool.

Scope:
- Latency: histogram, CDF, box plot.
- Throughput: bar chart.
- Connection time: bar chart.
- Output as PNG to `<logs-dir>/analysis/`.

Dependencies: E3 (foundation must be working first).

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

## E7: End-to-End Validation

**Goal**: Run the full benchmark pipeline across two machines and validate
results.

Scope:
- Deploy runner + Zenoh variant + custom-UDP variant on two LAN machines.
- Run a benchmark with the `scalar-flood` profile.
- Collect logs, run analysis, verify integrity passes and performance
  numbers are in the expected range per DESIGN.md targets.
- Document results and any issues discovered.

Dependencies: E1, E2, E4, E3 (at minimum).
