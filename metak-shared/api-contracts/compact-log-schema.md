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

Per-event JSONL emission was removed in the E19 follow-up cleanup
(2026-05-19). The `--legacy-jsonl-events` opt-in is gone; per-event
observations are compact-Parquet only. Lifecycle events (`phase`,
`connected`, `eot_*`, `resource`, `clock_sync`) continue to be written
as JSONL — see [`jsonl-log-schema.md`](jsonl-log-schema.md).

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

The Parquet file contains **one columnar tagged-union event table**
plus **Parquet key-value file metadata** carrying the spawn identity
and intern dictionaries. The single-table design (chosen 2026-05-18
after T18.2 implementation review) is simpler than the earlier
multi-row-group design — one `pl.scan_parquet` + filter selects all
event types at once, and the analyzer's downstream polars pipeline
already discriminates by `event` column.

### Event table (`compact_events`)

Columnar schema, one row per logical event:

| Column | Type | Description |
|---|---|---|
| `ts` | i64 ns | Wall-clock timestamp in the **writer's clock** for `write` / phase / lifecycle, **receiver's clock** for `receive`. Cross-clock semantics handled by E8. |
| `kind` | i32 (enum) | Event discriminator. See § Event kinds below. |
| `seq` | i64 nullable | Per-writer monotonic sequence (when applicable; null for lifecycle events). Retained for forward compatibility with seq-based analysis; the **default correlation rule** is ordering-based (see § Correlation) so `seq` is not load-bearing for the post-T17.10 pipeline. |
| `path_idx` | i32 nullable | Path-intern index. Null for lifecycle events that have no path. |
| `peer_idx` | i32 nullable | Peer-intern index — writer for receive events, peer for connected/eot events, null for self-only events. |
| `qos` | i8 nullable | QoS level (1-4) for write/receive/skip/gap events; null for lifecycle. |
| `bytes` | i32 nullable | Serialized payload size for write/receive; null otherwise. |
| `extra_f32` | f32 nullable | Polymorphic numeric slot: `cpu_percent` on `resource` events; `elapsed_ms` on `connected`; null otherwise. |
| `extra_f32_b` | f32 nullable | Polymorphic numeric slot: `memory_mb` on `resource`; null otherwise. |
| `extra_i64` | i64 nullable | Polymorphic int slot: `missing_seq` on `gap_detected`, `recovered_seq` on `gap_filled`, `eot_id` on `eot_*`, `wait_ms` on `eot_timeout`; null otherwise. |
| `extra_utf8` | utf8 nullable | Polymorphic string slot: `phase` name on `phase` events; `threading_mode` on `connected`; `eot_missing_json` on `eot_timeout`; null otherwise. |

### Event kinds

| `kind` (int) | Symbolic | Required columns | Notes |
|---|---|---|---|
| 0 | `write` | `ts`, `seq`, `path_idx`, `qos`, `bytes` | One row per published value. |
| 1 | `receive` | `ts`, `seq`, `path_idx`, `peer_idx`, `qos`, `bytes` | `peer_idx` = writer. |
| 2 | `backpressure_skipped` | `ts`, `path_idx`, `qos` | Only valid at qos 1/2 (DESIGN.md § 6.5). |
| 3 | `gap_detected` | `ts`, `path_idx`, `peer_idx`, `extra_i64=missing_seq` | QoS 3 only. |
| 4 | `gap_filled` | `ts`, `path_idx`, `peer_idx`, `extra_i64=recovered_seq` | QoS 3 only. |
| 5 | `phase` | `ts`, `extra_utf8=phase_name` | One row per phase transition (`connect`, `stabilize`, `operate`, `eot`, `silent`, `digest`, `done`). |
| 6 | `connected` | `ts`, `peer_idx`, `extra_f32=elapsed_ms`, `extra_utf8=threading_mode` | One row per peer connection establishment. |
| 7 | `eot_sent` | `ts`, `extra_i64=eot_id` | One row per spawn. |
| 8 | `eot_received` | `ts`, `peer_idx`, `extra_i64=eot_id` | One per (writer, eot_id) observed by receiver. |
| 9 | `eot_timeout` | `ts`, `extra_i64=wait_ms`, `extra_utf8=eot_missing_json` | Diagnostic only. |
| 10 | `resource` | `ts`, `extra_f32=cpu_percent`, `extra_f32_b=memory_mb` | Periodic sampling. |
| 11 | `clock_sync` | `ts`, `peer_idx`, `extra_i64` (offset_ns), `extra_f32` (rtt_ms) | Reserved for E8; field mapping confirmed when E8 lands. |

Adding a new event kind = adding a new `kind` value and (optionally)
documenting which `extra_*` slot carries new payload. Schema-version
bump is NOT required for additive event kinds.

### Intern dictionaries

Stored in Parquet **key-value file metadata** (not as separate row
groups). Keys / values:

- `path_intern` — JSON-encoded `Vec<String>`; index = `path_idx`.
- `peer_intern` — JSON-encoded `Vec<String>`; index = `peer_idx`.

JSON encoding chosen over a separate row group for simplicity; the
intern tables are small (≤ a few hundred entries on realistic
workloads) and the size win from columnar storage is negligible at
that cardinality.

The writer SHOULD use Parquet's `snappy` compression by default. The
writer MAY pick `zstd` if its benchmark on a `1000x100hz × 30 s` spawn
shows a meaningful size win at acceptable CPU cost; the choice is
documented in `variant-base/CUSTOM.md`.

## Metainfo (Parquet KV file metadata)

Stored in Parquet **key-value file metadata** (not a row group). Keys
mirror the table below; values are utf8 (JSON-encoded for non-scalar
values):

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

## Per-spawn file pair

Each spawn produces TWO files in `<log_dir>/<run>-<launch_ts>/`:

- `<variant>-<runner>-<run>.jsonl` — lifecycle events only (phase,
  connected, eot_*, resource, clock_sync). See
  [`jsonl-log-schema.md`](jsonl-log-schema.md).
- `<variant>-<runner>-<run>.compact.parquet` — per-event observations
  (this schema).

The analyzer reads both and joins them by `(variant, runner, run)`
provenance.

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

---

## E19 additions: `leaf_count` and `shape` on write rows

Approved 2026-05-19. Mirrors the `jsonl-log-schema.md` E19 additions
for the columnar event table. Backward-compatible: legacy compact
files default to `leaf_count = 1` and `shape = "scalar"`.

### Event table additions

Two new columns on `compact_events`:

| Column | Type | Description |
|---|---|---|
| `leaf_count` | u32 nullable | Number of scalar leaves carried by the WriteOp. Null on non-write rows. Defaults to `1` for legacy files (pre-E19). |
| `shape_idx` | u8 nullable | Index into the `shape_intern` dictionary. Null on non-write rows. |

A new intern dictionary `shape_intern` in the KV file metadata maps
`shape_idx` → one of `"scalar"`, `"array"`, `"struct"`. Defaults to
`["scalar"]` so legacy files (without the column) read back as
`shape = "scalar"` consistently.

Receive rows (`kind = 1`) leave both columns null. The analyzer
correlates receives with their matching write rows by
`(writer, seq, path_idx)` and inherits `leaf_count` / `shape` from the
write side. This keeps the wire opaque and the receive row's storage
minimal.

### Aggregate throughput numbers

The analysis tool reports three distinct throughput metrics per spawn,
all derived from the compact-Parquet write rows:

- `ops_per_sec` = `count(write rows) / operate_secs` — number of
  publish calls per second.
- `leaves_per_sec` = `sum(leaf_count) / operate_secs` — the canonical
  cross-workload comparable metric.
- `bytes_per_sec` = `sum(bytes) / operate_secs`.

For `scalar-flood` runs, `leaves_per_sec == ops_per_sec` because every
WriteOp carries one leaf.

### Latency unit

Replication latency is reported **per WriteOp** across all workload
profiles. Block-flood and mixed-types produce one latency sample per
published block, which is what the transport actually measures
end-to-end. Scalar-flood happens to coincide (1 leaf = 1 op) so
existing scalar-flood latency results are unchanged.

### Schema-version

This is an **additive** change — no `metainfo.schema_version` bump
required per the existing rule (additive new columns can be ignored by
older readers). New compact-writer code emits the column; older files
read back as default `1` / `"scalar"`.
