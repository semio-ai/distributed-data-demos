# Project Overview

## Goal

Build and empirically compare multiple implementations of a low-latency,
high-throughput distributed data replication system for local networks. The
project produces a benchmark harness (runner + analysis tool) and at least two
replication variants so their performance characteristics can be measured
side-by-side under identical conditions.

## What we're building

1. **Replication system design** (documented) — a leaderless, single-writer
   key-value tree replicated across nodes on a LAN, with four QoS tiers
   ranging from best-effort UDP to reliable TCP. Built on `arora_types::Value`.
   See `metak-shared/DESIGN.md`.

2. **Benchmark runner** (Rust binary) — a leaderless coordinator that runs on
   each machine, discovers peers, barrier-syncs, and spawns variant processes
   in lockstep. Runners have no IPC with variants; they only spawn, monitor,
   and collect exit codes. See `metak-shared/BENCHMARK.md`.

3. **Variant implementations** (Rust binaries) — standalone executables that
   each implement the replication design using a different stack (e.g. Zenoh,
   custom UDP). Variants receive configuration via CLI args, discover peers
   autonomously, execute the test protocol (connect, stabilize, operate,
   silent), log events to JSONL, and exit. See `metak-shared/BENCHMARK.md` S5.

4. **Analysis tool** (Python script) — ingests JSONL logs from all nodes,
   variants, and runs; verifies data integrity; computes performance metrics
   (latency percentiles, throughput, jitter, loss, resource usage); and
   produces CLI summary tables and PNG diagrams for comparison.
   See `metak-shared/ANALYSIS.md`.

## Current state

- Design documents are complete: DESIGN.md, BENCHMARK.md, ANALYSIS.md.
- The metak orchestration scaffold is in place.
- No application code has been written yet. No sub-repos for the runner,
  variants, or analysis tool exist.

## What's next

1. Finalize architecture and API contracts (runner-variant CLI contract,
   JSONL log schema, runner coordination protocol, TOML config schema).
2. Create sub-repos: `runner/`, `variants/zenoh/`, `variants/custom-udp/`,
   `analysis/`.
3. Implement the runner (Epic 1).
4. Implement the first variant — Zenoh (Epic 2).
5. Implement the analysis tool — Phase 1 foundation (Epic 3).
6. Run the first end-to-end benchmark.
