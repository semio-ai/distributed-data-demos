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

Repeated analysis runs should be fast. The tool uses a pickle cache to avoid
re-parsing unchanged log files.

```
┌──────────────┐    ┌──────────────┐    ┌──────────────┐    ┌──────────────┐
│ Check for    │──►│ Detect new / │──►│ Parse &      │──►│ Pickle the   │
│ pickle cache │    │ changed files │    │ merge into   │    │ updated      │
│              │    │              │    │ cache        │    │ cache        │
└──────────────┘    └──────────────┘    └──────────────┘    └──────────────┘
         │                                                         │
         │  (cache exists, no changes)                             ▼
         └───────────────────────────────────────────────► Run analysis
```

### 3.1 Steps

1. **Check for pickle cache** — look for `<logs-dir>/.analysis_cache.pkl`.
   If absent (or `--clear` was passed), start with an empty cache.
2. **Detect new or changed files** — the cache stores the path and
   `mtime` of every JSONL file it has ingested. Scan `<logs-dir>/*.jsonl`
   and compare. Files with a newer `mtime` or not in the cache are
   marked for (re)loading.
3. **Parse and merge** — for each changed or new file, parse all JSONL
   lines and insert/replace them in the cache keyed by
   `(variant, runner, run)`. A changed file fully replaces its previous
   entries.
4. **Pickle the cache** — write the updated cache back to
   `<logs-dir>/.analysis_cache.pkl`.
5. **Run analysis** — integrity verification and performance analysis
   operate on the cached data.

### 3.2 The `--clear` Flag

Deletes `.analysis_cache.pkl` before step 1, forcing a full rebuild from
every JSONL file. Use this after manually editing or deleting log files.

## 4. Data Model

After ingestion, the cache holds a flat table of events. Each event retains
all fields from its JSONL line. The composite key
`(variant, run, writer, seq, path)` uniquely identifies a logical write and
is used to correlate writes with their corresponding receives across runners.

### 4.1 Correlation

For every `write` event, the analysis tool finds the matching `receive`
events on other runners by joining on `(variant, run, seq, path)` where the
`receive` event's `writer` field matches the `write` event's `runner` field.

This produces a **delivery record** per (write, receiver) pair:

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
| `latency` | `receive_ts − write_ts` |

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

The analysis tool will be built incrementally:

### Phase 1 — Foundation

- Caching pipeline (pickle load/save, change detection, `--clear`).
- JSONL parsing and data model.
- Write-receive correlation.
- CLI summary tables (integrity + performance).

### Phase 2 — Diagrams

- Latency histogram, CDF, box plot.
- Throughput bar chart.
- Connection time bar chart.

### Phase 3 — Time-Series and Advanced

- Latency time-series.
- Throughput time-series.
- Resource usage time-series and bar charts.
- Jitter time-series.
- Radar chart for cross-variant comparison.
