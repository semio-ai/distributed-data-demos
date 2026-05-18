# API Contract: Compact Log Format

Defines the post-run digest format that supersedes per-message JSONL
for benchmark spawn output. Filed under epic E18 (2026-05-18); status
**DRAFT** until T18.2 (variant-base writer) lands and the format
round-trips through a real spawn + analysis run.

## Motivation

Per-message JSONL produces thousands of GB on a full-matrix two-machine
run (100 K msg/s × 30 s × ~200 spawns × ~100 bytes/line ≈ 60 GB). The
compact format batches per-spawn events into a single binary file
written after the operate + silent phases complete. Target reduction:
30-50× smaller files via columnar layout, dict-encoding, and
path-interning.

Legacy per-message JSONL remains supported (read-side by the analyzer,
write-side by the variant under an opt-in `--legacy-jsonl-events` flag)
for live debugging and for replay of older datasets.

## File format

**Apache Parquet** (binary columnar). Chosen over JSON / pickle /
MessagePack because:
- The analysis cache pipeline already uses Parquet — the variant writes
  the final storage shape directly, eliminating the JSONL → Parquet
  cache rebuild step.
- Built-in dict-encoding compresses repeated values (paths, writer
  names, event types) at the column level.
- Built-in snappy / zstd compression.
- Lazy / streaming read via `pl.scan_parquet` keeps the analyzer's
  bounded-memory pipeline intact.

## File naming

One file per spawn:

```
<variant>-<runner>-<run>.compact.parquet
```

Same `<variant>-<runner>-<run>` triple as the legacy JSONL filename.
The `.compact.parquet` extension distinguishes from `.jsonl`; the
analyzer's format detector reads whichever is present.

## Tables

The Parquet file contains multiple **row groups** (or separate
Parquet "tables" — implementation choice; the writer picks whichever
the chosen Rust crate supports cleanly). Each row group has a
`kind` discriminator in its key-value metadata:

| `kind` | Contents |
|---|---|
| `metainfo` | One-row table: spawn identity + workload params + phase timestamps + peer list. See § Metainfo below. |
| `writes` | Columnar `(ts: i64 ns, path_idx: u32)`. One row per `write` event. |
| `receives` | Columnar `(ts: i64 ns, path_idx: u32, writer_idx: u8)`. One row per `receive` event. |
| `paths` | Path-intern table: `(path_idx: u32, path: utf8)`. |
| `peers` | Peer-intern table: `(writer_idx: u8, runner_name: utf8)`. |
| `aux_events` | Low-cardinality events: `(ts: i64 ns, event: utf8, qos: i8, seq: i64, writer: utf8, missing_seq: i64, recovered_seq: i64, eot_id: u64, ...)`. Covers `gap_detected`, `gap_filled`, `backpressure_skipped`, `eot_sent`, `eot_received`, `eot_timeout`, `clock_sync`. Most columns null for any given row. |
| `resource` | Columnar `(ts: i64 ns, cpu_percent: f32, memory_mb: f32)`. |
| `connected` | One-row-per-peer table: `(ts: i64 ns, peer: utf8, elapsed_ms: f64, threading_mode: utf8, recv_buffer_kb: u32)`. |
| `phase` | Columnar `(ts: i64 ns, phase: utf8)`. One row per `phase=<state>` transition. |

The writer SHOULD use Parquet's `snappy` compression by default. The
writer MAY pick `zstd` if its benchmark on a `1000x100hz × 30 s` spawn
shows a meaningful size win at acceptable CPU cost; the choice is
documented in `variant-base/CUSTOM.md`.

## Metainfo

The `metainfo` row group has one row with these columns (or
equivalent representation in Parquet key-value file metadata; writer
picks the cleaner shape, both are spec-conformant):

| Column | Type | Description |
|---|---|---|
| `variant` | utf8 | Spawn variant identity (e.g. `custom-udp-1000x100hz-qos4-multi`). |
| `runner` | utf8 | This runner's name (e.g. `alice`). |
| `run` | utf8 | Run identifier (e.g. `all-variants-01`). |
| `workload` | utf8 | Workload profile (e.g. `scalar-flood`). |
| `values_per_tick` | u32 | Workload parameter. |
| `tick_rate_hz` | u32 | Workload parameter. |
| `qos` | i8 | QoS level (1-4). |
| `threading_mode` | utf8 | `single` or `multi`. |
| `recv_buffer_kb` | u32 | Runner-injected recv-buffer floor. |
| `operate_start_ts` | i64 ns | When the operate phase began (writer clock). |
| `eot_sent_ts` | i64 ns nullable | When `eot_sent` fired; null on legacy / aborted spawns. |
| `silent_start_ts` | i64 ns nullable | When the silent phase began. |
| `digest_start_ts` | i64 ns | When the digest phase began (= when this file was opened for writing). |
| `digest_end_ts` | i64 ns | When the digest phase ended (= when this file was closed). |
| `schema_version` | u32 | Bumped by future schema-breaking changes. Initial value: `1`. |
| `path_count` | u32 | Number of rows in the `paths` table (for quick validation). |
| `peer_count` | u8 | Number of rows in the `peers` table. |
| `events_total` | u64 | Sum of rows across writes + receives + aux_events (sanity check). |

## Correlation

**No `seq` field on writes/receives.** This is intentional, prepares
for the N-peer case (where each peer registers sequences from multiple
peers and a single global seq doesn't make sense). Correlation is
**ordering-based**:

- For each `(writer, path)` tuple, the Nth `write` event in the
  writer's `writes` table corresponds to the Nth `receive` event with
  matching `(writer, path)` in any receiver's `receives` table.
- At QoS 3 / QoS 4 (strict-order, no-drop per DESIGN.md § 6.5)
  correlation is exact: latency for delivery `i` is
  `receive[i].ts - write[i].ts`.
- At QoS 1 / QoS 2 (drops + reorders allowed), per-message correlation
  is best-effort. Aggregate metrics — delivery %, throughput, latency
  distribution — remain well-defined. The analyzer falls back to:
  - Delivery % = receives_count / writes_count per `(writer, path)`.
  - Latency distribution = collected from successfully-paired writes
    in arrival order; unpaired tail receives discarded, unpaired tail
    writes count as drops.

For a single-spawn file the writer's perspective is implicit (writes
in this file are by `metainfo.runner`); receives in this file are by
the same runner but include a `writer_idx` per row identifying the
source peer.

## Cross-file correlation

The analyzer joins across spawn files by:
- Within a `(variant, run)` group: all spawn files share the variant
  identity and workload params (metainfo equality).
- Per-peer: writer's `writes` file × receiver's `receives` file are
  matched by metainfo's `runner` field and the receive row's
  `writer_idx` (resolved through `peers`).

## Cross-clock note

`ts` in any one file is in that file's `runner` clock. Cross-machine
latency (writer's `ts` vs receiver's `ts`) is meaningful only after
clock-sync correction (E8). Until E8 lands, the analyzer treats
cross-machine latency as uncorrected and surfaces it accordingly.

## Memory budget

Variants buffer writes/receives in memory during operate + silent and
flush in `digest`. With `(ts: i64, path_idx: u32)` rows = 12 bytes
uncompressed, a `1000x100hz × 30 s` spawn (3 M writes + 3 M receives)
needs ~72 MB before serialization.

**Configurable thresholds** (variant-base):
- `--digest-mem-soft-mb` (default `1024`): variant logs a warning when
  buffer occupancy crosses this.
- `--digest-mem-hard-mb` (default `2048`): variant exits non-zero with
  a clear error if occupancy exceeds this.

Defaults sized for 100 K msg/s × 30 s × 1000-path workloads with
generous headroom. Long-running soak tests may need to bump.

## Coexistence with legacy JSONL

- The analyzer's format detector picks compact-parquet if present,
  otherwise legacy JSONL.
- The variant defaults to writing compact-parquet from T18.2 onward.
- A new variant CLI flag `--legacy-jsonl-events` (default OFF) makes
  the variant ALSO stream legacy per-message JSONL alongside the
  compact file — useful for live debugging where you want to `tail -f`
  events as they happen. When the flag is set, the same data is
  written to both formats; analysis prefers compact.

## Out of scope (for E18)

- Streaming digest (writing the file incrementally during operate).
  Buffer-then-flush is simpler. May revisit if memory ceiling proves
  too tight.
- Zstd compression as the default (snappy is default; zstd is an
  opt-in writer choice).
- Multi-peer fan-in into a single compact file. One file per spawn
  remains the unit of analysis.
- Clock-sync (E8) — separate epic. Compact format reserves
  `offset_ms` / `peer` / `rtt_ms` columns implicitly via the
  `aux_events` `clock_sync` rows (same shape as the legacy
  jsonl-log-schema definition).
- Distributed log collection. `--log-dir` on the runner (T18.5)
  points at a shared network folder; that solves the collection
  problem.

## Schema-version bumping

Bump `metainfo.schema_version` on any breaking change to:
- Column dtypes (e.g. `path_idx: u32 → u64`).
- Required columns added / removed.
- Per-row semantics changed.

Additive changes (new optional columns, new `aux_events` event types)
do NOT require a bump if older readers can simply ignore them.

The analyzer's per-shard parquet cache (`analysis/cache.py`) treats
this `metainfo.schema_version` as the rebuild trigger, parallel to
its own `SCHEMA_VERSION` constant for the internal cache shards.
