# Project Custom Instructions

## Project Context

This project benchmarks distributed data replication strategies on a local
network. The design documents in `metak-shared/` (DESIGN.md, BENCHMARK.md,
ANALYSIS.md) are the source of truth for requirements.

## Tech Stack

- **Runner and variants**: Rust. Use `arora_types::Value` from
  `semio-ai/arora-types` as the universal data type.
- **Analysis tool**: Python 3.10+. Matplotlib for diagrams.
- **Config format**: TOML (single file per benchmark run).
- **Log format**: JSONL with self-describing fields per
  `metak-shared/api-contracts/jsonl-log-schema.md`.

## Key Constraints

- Runner and variant processes communicate only via CLI arguments at spawn
  time. No IPC, no shared memory, no pipes.
- All cross-node latency measurement depends on clock synchronization (PTP
  preferred, NTP acceptable). The analysis tool must report which method
  was used.
- QoS is per-subtree-branch, not global. Variants must support mixed QoS
  within a single run.
- Performance targets: 100 Hz tick rate, <10 ms replication latency on LAN,
  ~100k value updates/sec aggregate.

## Repo Layout (planned)

```
runner/              -- benchmark runner (Rust)
variant-base/        -- shared Variant trait + test protocol driver (Rust lib)
variants/zenoh/      -- Zenoh-based replication variant (Rust bin, placeholder)
variants/custom-udp/ -- custom UDP replication variant (Rust bin, placeholder)
variants/...         -- additional variants chosen during E0 exploration
analysis/            -- analysis tool (Python)
```

Concrete variant repos are placeholders until E0 (variant exploration)
determines the final candidate list. Sub-repos will be created as epics
begin. Use `metak add` to scaffold them.
