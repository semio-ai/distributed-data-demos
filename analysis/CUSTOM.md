# Analysis Tool — Custom Instructions

## Overview

Python script that ingests JSONL log files from benchmark runs, verifies
data integrity, computes performance metrics, and produces CLI summary
tables and diagrams. Phase 1 (E4) is shipped. **Phase 1.5 (E11) reworks
the storage and execution model to scale to multi-tens-of-GB datasets;
Phase 2/3 (E5/E6) add diagrams on top of the Phase 1.5 lazy pipeline.**

The full spec is in `metak-shared/ANALYSIS.md`. Sections 3-4 describe
the post-rework caching and data model. Section 8 lists the phases.

## Tech Stack

- **Language**: Python 3.10+
- **Type hints**: required throughout
- **Formatting**: `ruff format`
- **Linting**: `ruff check`
- **Testing**: `pytest`
- **Dependencies** (Phase 1.5):
  - `polars >= 0.20` — analytics engine (lazy frames, per-group execution,
    Parquet I/O via the bundled Arrow). **Justified addition** to the
    Python stack (per `coding-standards.md`'s "no pandas unless
    justified" rule): the 40 GB dataset cannot be analysed via Python
    dataclasses + standard library; polars's columnar Arrow buffers,
    lazy evaluation, and Parquet support are load-bearing for the
    target performance budget. See `metak-shared/ANALYSIS.md` § 3.2.
  - `pyarrow` — pulled in transitively by polars; do not add directly.
  - `matplotlib` — required for Phase 2 diagrams (already used by E4
    placeholder).
  - Standard library: `dataclasses`, `json`, `pathlib`, `statistics`,
    `argparse`.
  - **Removed in Phase 1.5**: `pickle` (replaced by per-shard Parquet).
- Follow `metak-shared/coding-standards.md` (Python section).

## Build and Test

```
cd analysis
python -m pytest tests/ -v
ruff format --check .
ruff check .
```

No build step — it's a Python script.

## Integration Contracts

Consumes JSONL log files per `metak-shared/api-contracts/jsonl-log-schema.md`.

Key fields on every line: `ts`, `variant`, `runner`, `run`, `event`.
Event types: `connected`, `phase`, `write`, `receive`, `gap_detected`,
`gap_filled`, `resource`.

## Test Data

Real JSONL logs available under `../logs/`. Recommended use:

| Dataset | Size | Use |
|---|---|---|
| `same-machine-20260430_140856/` | 3.6 GB | Phase 1.5 integration tests + reference output capture |
| `same-machine-all-variants-01-20260430_191914/` | 7.1 GB | Mid-size sanity check |
| `inter-machine-all-variants-01-20260501_150858/` | 40 GB | **Acceptance gate**: must complete <10 min cold, <30 s warm, <4 GB RSS |

Smaller synthetic JSONL fixtures live under `tests/fixtures/` for unit
tests. Create more as needed; do NOT commit anything from `../logs/`.

The 40 GB dataset already contains a 14.5 GB `.analysis_cache.pkl` from
Phase 1 attempts — see the pre-work step in T11.1 about handling this
file.

## Architecture

Phase 1.5 architecture (after E11 rework). Modules with the same name as
Phase 1 are reworked, not preserved by line.

```
analysis/
  analyze.py          -- CLI entry point: drives cache update, then per-(variant,run) lazy analysis
  cache.py            -- per-shard Parquet cache: discover JSONL, diff vs sidecars, stream-build stale shards
  schema.py           -- SHARD_SCHEMA + SCHEMA_VERSION (single source of truth for the columnar layout)
  parse.py            -- streaming JSONL line -> columnar row projection (no in-memory Event materialization)
  correlate.py        -- polars filter+join producing a delivery-records DataFrame per group
  integrity.py        -- polars groupbys for completeness, ordering, duplicates, gap recovery
  performance.py      -- polars groupbys / dynamic windows for latency, throughput, jitter, loss, resources
  tables.py           -- CLI summary table formatting (consumes IntegrityResult / PerformanceResult dataclasses)
  plots.py            -- matplotlib comparison chart (consumes PerformanceResult dataclasses)
  tests/
    test_schema.py        -- SHARD_SCHEMA round-trip, SCHEMA_VERSION sentinel
    test_cache.py         -- shard build + stale detection (mtime, schema, orphan)
    test_parse.py         -- line-to-row projection per event type
    test_correlate.py     -- polars-vs-Phase1 parity on a synthetic fixture
    test_integrity.py     -- per-QoS integrity rules
    test_performance.py   -- latency percentiles, throughput, jitter, loss
    test_integration.py   -- end-to-end against logs/same-machine-20260430_140856/
    fixtures/
      phase1_reference_summary.txt  -- captured Phase 1 stdout, regression target
      ...synthetic JSONL files...
```

Cache layout under any `<logs-dir>/`:

```
<logs-dir>/
  <name>-<runner>-<run>.jsonl          -- source logs (untouched)
  ...
  .cache/
    <name>-<runner>-<run>.parquet      -- columnar shard, one per source JSONL
    <name>-<runner>-<run>.meta.json    -- {mtime, row_count, schema_version}
    ...
    _cache_schema_version.json         -- global sentinel; bump to force a global rebuild
```

The Phase 1 monolithic `<logs-dir>/.analysis_cache.pkl` is removed on
first Phase 1.5 run with a stderr notice.

## Design Guidance

### Data Model (Phase 1.5)

Events are stored as a flat columnar schema. The full column list and
dtypes are in `metak-shared/ANALYSIS.md` § 4.1; mirror them in
`schema.py::SHARD_SCHEMA` and reference that constant from both the
ingester and the analysis readers. Categorical encoding is essential
for `variant` / `runner` / `run` / `event` (low cardinality, big size
win).

The Phase 1 `Event(ts, variant, runner, run, event, data: dict)`
dataclass is **removed**: no in-memory event objects exist anywhere in
the new pipeline. `DeliveryRecord`, `IntegrityResult`, `PerformanceResult`
dataclasses are **kept** because they are the API surface consumed by
`tables.py` / `plots.py`. They are populated from polars query results
at the boundary between analysis and presentation.

### Caching Pipeline (Phase 1.5)

1. Discover `<logs-dir>/*.jsonl` and `<logs-dir>/.cache/*.parquet`.
2. For each JSONL, look up its `.cache/<stem>.meta.json`. Stale when:
   sidecar missing/malformed, sidecar `schema_version` differs from
   `SCHEMA_VERSION`, sidecar `mtime` < JSONL `mtime`, or shard file
   missing.
3. For each stale shard: stream-parse the JSONL line by line, project
   into the `SHARD_SCHEMA` columns, write a Parquet shard via row-group
   batching (e.g. 100k rows per batch) so peak memory is bounded by the
   batch buffer, not the file size. Flush the sidecar last, after the
   shard write succeeds.
4. Remove orphan shards (no matching JSONL).
5. `--clear` deletes the entire `.cache/` directory before step 1.

A legacy `<logs-dir>/.analysis_cache.pkl` from Phase 1 is removed on
first run with a single-line stderr notice. No conversion attempted.

### Correlation

Join `write` events with `receive` events on `(variant, run, seq, path)`
where `receive.writer == write.runner`. Produces one `DeliveryRecord` per
(write, receiver) pair.

For VariantDummy (single-runner loopback), the writer and receiver are the
same runner — this is expected and should still produce valid delivery
records with near-zero latency.

### Integrity Verification

Per (variant, run, writer -> receiver) pair:
- **Completeness**: every write has a receive (QoS 3-4 only; 1-2 are loss-tolerant)
- **Ordering**: receives have non-decreasing seq (QoS 2-4)
- **Duplicates**: same (writer, seq, path) received twice (flag for QoS 3-4)
- **Gap recovery**: every gap_detected has gap_filled (QoS 3 only)

### Performance Analysis

All derived from delivery records and event timestamps using polars
groupbys per `(variant, run)`:
- **Connection time**: from `connected` events (`elapsed_ms`)
- **Latency**: p50, p95, p99, max from `latency_ms` on delivery records
  (use `pl.quantile`)
- **Throughput**: writes/sec and receives/sec from event counts and
  operate duration (operate-phase boundaries from `phase` events)
- **Jitter**: std-dev of latency within sliding 1-second windows
  (use `groupby_dynamic` over `receive_ts`)
- **Packet loss**: missing receives / total writes (QoS 2-4)
- **Resource usage**: mean and peak CPU/memory from `resource` events

### CLI Output

Two tables printed to stdout:

```
Integrity Report
---
Variant              Run          QoS  Delivery%  Out-of-order  Dupes  Gaps
custom-udp           local-test   1    99.8%      0             0      -
dummy                local-test   1    100.0%     0             0      -

Performance Report
---
Variant              Run          Connect(ms)  Latency p50  p95     p99     Writes/s  Loss%
custom-udp           local-test   12.3         0.5ms        1.2ms   2.3ms   50        0.2%
dummy                local-test   0.1          0.01ms       0.02ms  0.03ms  50        0.0%
```
