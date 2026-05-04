# Distributed Data Replication System — Analysis

Tool and methodology for analysing benchmark results produced by the runner
system described in [BENCHMARK.md](BENCHMARK.md).

## 1. Overview

The analysis tool is a **Python script** (`analyze.py`) that:

1. Ingests JSONL log files from all runners, variants, and runs.
2. Caches the parsed data in a pickle file so that repeated analysis runs
   (e.g. while iterating on presentation) skip the parsing step entirely.
3. Runs **integrity verification** — did the data arrive completely, in order,
   and without corruption?
4. Runs **performance analysis** — latency, throughput, jitter, resource
   usage, connection time.
5. Presents results as **CLI summary tables** and **comprehensive diagrams**.

## 2. CLI Interface

```
python analyze.py <logs-dir> [options]
```

| Argument / Flag | Description |
|---|---|
| `<logs-dir>` | Directory containing `.jsonl` log files (and the pickle cache). |
| `--clear` | Delete the pickle cache and rebuild from all JSONL files. |
| `--summary` | Print CLI summary tables only (no diagrams). |
| `--diagrams` | Generate diagrams only (no CLI output). |
| `--output <dir>` | Directory for generated diagrams and reports. Defaults to `<logs-dir>/analysis/`. |

When neither `--summary` nor `--diagrams` is given, both are produced.

## 3. Caching Pipeline

Repeated analysis runs should be fast. The tool maintains a per-file
columnar cache so that large datasets (tens of GB, 100M+ events) can be
analysed with bounded memory and incremental updates.

> **Implementation note.** The original Phase 1 design (E4) used a single
> monolithic pickle file holding every parsed event in RAM. That approach
> works for small datasets but does not scale: it is O(total dataset size)
> in both load time and peak memory, with no way to process subsets.
> Phase 1.5 (E11) replaces it with the design below. See § 8.

### 3.1 Storage layout

```
<logs-dir>/
  <name>-<runner>-<run>.jsonl              # source logs (untouched)
  ...
  .cache/
    <name>-<runner>-<run>.parquet          # columnar shard, one per JSONL
    <name>-<runner>-<run>.meta.json        # sidecar: mtime + schema version + row count
    ...
    _cache_schema_version.json             # global schema sentinel
```

Each source JSONL file gets exactly one Parquet shard plus one tiny meta
sidecar. There is **no global index file**. The set of shards present on
disk *is* the cache.

### 3.2 Why Parquet (not pickle)

| Property | Pickle (Phase 1) | Parquet (Phase 1.5) |
|---|---|---|
| Random access | No — must load whole file | Yes — read columns, row-groups, predicate-pushdown |
| Compression | None | ~5-10x (snappy/zstd) on event data |
| Memory footprint when read | ~3x dataset size in Python objects | Native Arrow buffers, ~10-30x denser than Python objects |
| Streaming reads | No | Yes (mmap or row-group iterator) |
| Schema evolution safety | Brittle (class identity tied to Python source) | Schema metadata in file; easy to detect mismatch |
| Crash safety during build | Whole-file rewrite each update | Per-shard write; partial updates are recoverable |

### 3.3 Steps

```
┌────────────────┐  ┌─────────────────┐  ┌──────────────┐  ┌────────────────┐
│ Discover JSONL │─►│ Diff against    │─►│ Stream-parse │─►│ Run analysis   │
│ + shards       │  │ shard meta      │  │ stale files  │  │ via lazy reads │
│                │  │ (mtime, schema) │  │ → parquet    │  │ over shards    │
└────────────────┘  └─────────────────┘  └──────────────┘  └────────────────┘
```

1. **Discover** — list `<logs-dir>/*.jsonl` and `<logs-dir>/.cache/*.parquet`.
2. **Diff** — for each JSONL, look up its sidecar. A shard is **stale**
   when: the sidecar is missing, sidecar `mtime` is older than the JSONL
   `mtime`, or the global schema version has changed. Stale shards are
   marked for rebuild. Orphan shards (no matching JSONL) are removed.
3. **Stream-parse** — for each stale or missing shard, read the JSONL line
   by line, project into the columnar schema (§ 4.1), and write a Parquet
   shard incrementally via row-group batches (e.g. 100k rows per batch).
   Memory is bounded by the batch size, not the file size.
   Write the sidecar after the shard is fully flushed.
4. **Run analysis** — integrity and performance both operate on
   `pl.scan_parquet(<logs-dir>/.cache/*.parquet)`, a lazy frame that
   pushes filters and projections down into the Parquet readers. No
   step in the analysis pipeline materializes the full dataset in memory.

### 3.4 The `--clear` flag

Deletes the entire `.cache/` directory before step 1, forcing a full
rebuild from every JSONL file. Use this after manually editing or deleting
log files, or after a schema change forces it.

### 3.5 Migrating from the Phase 1 pickle cache

On startup, if a legacy `<logs-dir>/.analysis_cache.pkl` exists, the tool
deletes it (after a one-line stderr notice). Do **not** attempt to
convert — re-parsing from the source JSONL is the source of truth.

## 4. Data Model

### 4.1 Columnar event schema

After ingestion, events live in a single flat columnar schema, one row per
JSONL line. Event-specific fields share the same row; columns that don't
apply to a given event type are null. This is the schema written to every
Parquet shard and read by every analysis step.

| Column | Polars dtype | Source |
|---|---|---|
| `ts` | `Datetime("ns", "UTC")` | every event |
| `variant` | `Categorical` | every event |
| `runner` | `Categorical` | every event |
| `run` | `Categorical` | every event |
| `event` | `Categorical` | every event (enum: `connected`, `phase`, `write`, `receive`, `gap_detected`, `gap_filled`, `resource`, `clock_sync`) |
| `seq` | `Int64` (nullable) | `write`, `receive` |
| `path` | `Utf8` (nullable) | `write`, `receive` |
| `writer` | `Utf8` (nullable) | `receive`, `gap_detected`, `gap_filled` |
| `qos` | `Int8` (nullable) | `write`, `receive`, `gap_*` |
| `elapsed_ms` | `Float64` (nullable) | `connected` |
| `phase` | `Utf8` (nullable) | `phase` |
| `missing_seq` | `Int64` (nullable) | `gap_detected` |
| `recovered_seq` | `Int64` (nullable) | `gap_filled` |
| `cpu_percent` | `Float32` (nullable) | `resource` |
| `memory_mb` | `Float32` (nullable) | `resource` |
| `peer` | `Utf8` (nullable) | `clock_sync` (E8) |
| `offset_ms` | `Float64` (nullable) | `clock_sync` |
| `rtt_ms` | `Float64` (nullable) | `clock_sync` |

Categorical encoding is essential: `variant`, `runner`, `run`, `event` are
low-cardinality (under ~50 distinct values across an entire dataset).
Dictionary encoding shrinks them from ~30 bytes/row to ~1 byte/row.

Schema is centrally defined in code; bumping the version sentinel forces
all shards to rebuild. New event types add nullable columns — they do not
require a schema bump for older logs.

### 4.2 Correlation

For every `write` event, the analysis tool finds the matching `receive`
events on other runners by joining on `(variant, run, seq, path)` where the
`receive` event's `writer` field matches the `write` event's `runner` field.

This is expressed as a polars join:

```python
writes = lazy.filter(pl.col("event") == "write").select(
    "variant", "run", pl.col("runner").alias("writer"), "seq", "path",
    pl.col("ts").alias("write_ts"), "qos",
)
receives = lazy.filter(pl.col("event") == "receive").select(
    "variant", "run", pl.col("runner").alias("receiver"),
    "writer", "seq", "path", pl.col("ts").alias("receive_ts"),
)
deliveries = receives.join(writes, on=["variant", "run", "writer", "seq", "path"], how="inner") \
                     .with_columns(
                         ((pl.col("receive_ts") - pl.col("write_ts")).dt.total_microseconds() / 1000.0)
                         .alias("latency_ms")
                     )
```

The resulting **delivery record** carries the same fields as before:

| Field | Source |
|---|---|
| `variant` | shared |
| `run` | shared |
| `path` | shared |
| `seq` | shared |
| `qos` | shared |
| `writer` | write event's `runner` |
| `receiver` | receive event's `runner` |
| `write_ts` | write event's `ts` |
| `receive_ts` | receive event's `ts` |
| `latency_ms` | `(receive_ts − write_ts).total_milliseconds()` |

### 4.3 Per-group execution

The analysis is naturally partitioned by `(variant, run)`. The driver
iterates groups and applies all downstream steps (correlation, integrity,
performance) in a per-group lazy pipeline. At any moment only one group
is materialized, bounding peak memory regardless of total dataset size.

```
groups = lazy.select(["variant", "run"]).unique().collect()
for variant, run in groups.iter_rows():
    g = lazy.filter((pl.col("variant") == variant) & (pl.col("run") == run))
    deliveries = correlate_group(g)
    integrity_results.extend(integrity_group(g, deliveries))
    performance_results.append(performance_group(g, deliveries))
```

The only cross-group step is the final summary table, which operates on
already-aggregated metrics — never raw events.

## 5. Integrity Verification

Integrity checks verify that the replication system delivered data completely,
in order, and without corruption. Results are reported per
`(variant, run, writer → receiver)` pair.

### 5.1 Delivery Completeness

For **fault-intolerant QoS levels (3 and 4)**: every `write` event from a
writer must have a corresponding `receive` event on every other runner.
Missing receives are flagged.

For **fault-tolerant QoS levels (1 and 2)**: missing receives are expected.
The tool reports the **delivery rate** (receives / writes) but does not flag
missing ones as errors.

### 5.2 Ordering

For **QoS levels 2, 3, and 4** (all ordered modes): receives on a given
runner from a given writer must arrive with non-decreasing sequence numbers
(strictly increasing for levels 3 and 4). Out-of-order receives are flagged.

For **QoS level 1** (unordered): no ordering check.

### 5.3 Duplicates

For **fault-intolerant QoS levels (3 and 4)**: a `receive` with the same
`(writer, seq, path)` appearing more than once on the same runner is flagged
as a duplicate.

For **fault-tolerant levels (1 and 2)**: duplicates are noted in the report
but not flagged as errors.

### 5.4 Sequence Gaps and Recovery

For **QoS level 3** (reliable-UDP): the tool checks that every `gap_detected`
event has a corresponding `gap_filled` event. Unresolved gaps at the end of
the run are flagged. Recovery time (time between detection and fill) is
reported.

### 5.5 Integrity Summary Table

The CLI prints a per-variant, per-run summary:

```
Integrity Report
─────────────────────────────────────────────────────────────────────
Variant                  Run     QoS  Delivery%  Out-of-order  Dupes  Unresolved gaps
zenoh-replication        run01   2    99.87%     0             0      -
zenoh-replication        run02   2    99.91%     0             0      -
custom-udp-replication   run01   3    100.00%    0             0      0
custom-udp-replication   run01   4    100.00%    0             0      -
```

## 6. Performance Analysis

### 6.1 Connection Time

Derived from `connected` events. Reported per variant, per runner, per run.

| Statistic | Description |
|---|---|
| Per-node value | `elapsed_ms` from the `connected` event |
| Per-variant mean | Average across all runners and runs |
| Per-variant max | Slowest runner across all runs (bottleneck) |

### 6.2 Replication Latency

Derived from delivery records (§4.1).

| Statistic | Description |
|---|---|
| p50, p95, p99, max | Percentiles over all delivery records for a given `(variant, run)` |
| Per-path breakdown | Same percentiles grouped by `path`, to detect hot-path outliers |
| Per-receiver breakdown | Same percentiles grouped by `receiver`, to detect slow nodes |

### 6.3 Throughput

Derived from `write` and `receive` event counts within the operate phase.

| Statistic | Description |
|---|---|
| Write rate | `writes / operate_duration` per writer |
| Receive rate | `receives / operate_duration` per receiver |
| Aggregate | Sum of all writers' write rates |

### 6.4 Jitter

Standard deviation of replication latency within a sliding window (e.g.
1 second). Reported as a time-series and as an aggregate statistic.

### 6.5 Packet Loss Rate

For QoS levels with sequence tracking (2, 3, 4): ratio of missing sequence
numbers to total expected. For level 3, this is the *transient* loss (before
recovery).

### 6.6 Resource Usage

Derived from `resource` events. Reported as time-series and summary
statistics (mean, peak) per `(variant, runner, run)`.

### 6.7 Performance Summary Table

```
Performance Report
────────────────────────────────────────────────────────────────────────────────────
Variant                  Run     Connect(ms)  Latency p50  p95     p99     Writes/s   Loss%
zenoh-replication        run01   487          1.2ms        3.4ms   7.1ms   98,750     0.13%
zenoh-replication        run02   512          1.3ms        3.5ms   6.8ms   99,100     0.09%
custom-udp-replication   run01   102          0.8ms        2.1ms   4.3ms   99,900     0.00%
```

## 7. Diagrams

Comprehensive visual output for deeper comparison. All diagrams are saved to
the output directory as PNG files.

### 7.1 Latency

- **Histogram**: latency distribution per variant, overlaid for comparison.
- **CDF (cumulative distribution)**: same data as CDF — easier to compare
  tail behavior across variants.
- **Time-series**: latency over the duration of the operate phase. One line
  per variant. Reveals whether latency degrades over time.
- **Box plot**: per-variant latency box plots for quick visual comparison
  across all variants and runs.

### 7.2 Throughput

- **Bar chart**: write rate and receive rate per variant, grouped by run.
- **Time-series**: instantaneous throughput (e.g. per-second buckets) over
  the operate phase.

### 7.3 Connection Time

- **Bar chart**: per-variant connection time, one bar per runner, grouped
  by variant.

### 7.4 Resource Usage

- **Time-series**: CPU and memory over the operate phase, one line per
  variant-runner pair.
- **Bar chart**: peak CPU and peak memory per variant.

### 7.5 Jitter

- **Time-series**: jitter (rolling std-dev of latency) over the operate
  phase, one line per variant.

### 7.6 Cross-Variant Comparison (combined)

- **Radar / spider chart**: one axis per metric (latency p50, p99,
  throughput, loss%, connection time, peak CPU). One polygon per variant.
  Gives a single-glance overview of trade-offs.

## 8. Phased Delivery

The analysis tool is built incrementally:

### Phase 1 — Foundation (E4, done)

- Caching pipeline (pickle load/save, change detection, `--clear`).
- JSONL parsing and data model (in-memory dataclasses).
- Write-receive correlation.
- CLI summary tables (integrity + performance).

> Phase 1 is superseded by Phase 1.5 below. The analysis-output behaviour
> (CLI tables, integrity/performance semantics) is preserved; the
> storage and execution model is replaced.

### Phase 1.5 — Large-Dataset Cache and Pipeline Rework (E11)

Replace the monolithic-pickle cache with the per-shard Parquet cache and
the lazy / per-group execution model described in §§ 3-4. Required for
any dataset larger than a few hundred MB. Acceptance: a 40 GB dataset
analyses to completion with bounded memory (target <4 GB peak) within
minutes, and re-runs against an unchanged dataset complete in seconds.

### Phase 2 — Diagrams

- Latency histogram, CDF, box plot.
- Throughput bar chart.
- Connection time bar chart.

Built on top of the Phase 1.5 lazy pipeline — plot generators consume
materialized per-group polars frames, not raw event lists.

### Phase 3 — Time-Series and Advanced

- Latency time-series.
- Throughput time-series.
- Resource usage time-series and bar charts.
- Jitter time-series.
- Radar chart for cross-variant comparison.
