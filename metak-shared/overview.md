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

3. **Variant base crate** (Rust library) — shared foundation providing the
   `Variant` trait, common CLI parsing, test protocol driver (connect,
   stabilize, operate, silent), JSONL logger, resource monitor, and workload
   profiles. Each concrete variant only implements transport-specific logic.
   Includes `VariantDummy` — a no-network implementation using an in-process
   data board, used for base crate testing, runner harness testing, and as a
   zero-network performance baseline.

4. **Variant implementations** (Rust binaries) — thin executables that
   implement the `Variant` trait using a specific transport stack. Candidates
   are selected through a research/exploration phase (E0) before any code is
   written. See `metak-shared/BENCHMARK.md` S5.

5. **Analysis tool** (Python script) — ingests JSONL logs from all nodes,
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

1. Research and select variant candidates (E0) — survey transport libraries
   and protocols, produce `variant-candidates.md`.
2. Finalize architecture and API contracts (runner-variant CLI contract,
   JSONL log schema, runner coordination protocol, TOML config schema).
3. Implement the variant base crate with `Variant` trait + `VariantDummy` (E1).
4. Implement the benchmark runner, tested against `VariantDummy` (E2).
5. Implement the first concrete variant (E3, chosen from E0 results).
6. Implement the analysis tool — Phase 1 foundation (E4).
7. Run the first end-to-end benchmark.
