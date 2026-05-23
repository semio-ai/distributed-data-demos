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
| `threading_mode` | `Utf8` (nullable) | `connected` (E14 / T11.5; null on pre-T14.8 logs -- analysis defaults the grouping value to `"single"`) |
| `recv_buffer_kb` | `UInt32` (nullable) | `connected` (E14 / T11.5; captured for offline reproducibility) |

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

For **QoS levels 3 and 4** (reliable-ordered, reliable-tcp): receives on
a given runner from a given writer must arrive with strictly increasing
sequence numbers. Out-of-order receives flag ``[FAIL: ordering]``.

For **QoS levels 1 and 2** (best-effort, latest-value): no ordering
guarantee by design. qos1 is best-effort datagram (no order, no retry);
qos2 is latest-value datagram (newest write wins, no order). The
WebRTC qos1/qos2 implementations carry data over an unreliable /
unordered SCTP channel, so out-of-order receives are a normal feature
of the protocol, not a failure. The analyzer still counts and reports
``out_of_order`` so operators can see the absolute number, but the
``[FAIL: ordering]`` annotation does NOT fire for qos1-2 rows.

The post-2026-05-14 QoS-aware ordering rule was filed as a T14.17
follow-up after WebRTC qos2 rows on the ``stress-e14`` dataset showed
``[FAIL: ordering]`` despite the variant operating exactly as designed.

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
Variant                  Run     QoS  Delivery%  Out-of-order  Dupes  Unresolved gaps  Timeout
zenoh-replication        run01   2    99.87%     0             0      -                completed
zenoh-replication        run02   2    99.91%     0             0      -                completed
custom-udp-replication   run01   3    100.00%    0             0      0                completed
custom-udp-replication   run01   4    100.00%    0             0      -                completed
```

### 5.6 Timeout classification (T14.17, extended in T15.6, T15.11, and a 2026-05-14 follow-up)

The `Timeout` column carries a per-spawn classification of the writer
side's exit cause. Spawns that share a `(variant, run)` group but
differ by runner (writer side) get one classification each; the
classification is replicated onto every `(writer -> receiver)` row
that shares the writer.

The classifier inspects the per-spawn JSONL events plus the runner's
stderr capture (`<log_subdir>/<variant>-<runner>-stderr.txt`, per
`runner/src/spawn.rs::stderr_capture_path`) and emits one of nine
enum values, in the precedence below. The first matching rule wins.

| Value | When | Operator reading |
|---|---|---|
| `eot_timeout_internal` | Writer logged both `eot_sent` and `eot_timeout` | The variant itself gave up waiting for peer EOTs per the E12 protocol -- this is a clean exit, not a kill. Look at the `missing` field on `eot_timeout` to see which peers never confirmed. **Post-E15 this fires only for legacy code paths that still run the on-wire EOT phase** (e.g. websocket Single before T15.8 cleanup); the new architecture short-circuits the EOT-wait via variant-side idle detection (T15.5). |
| `completed` | Writer logged `eot_sent`, reached `phase=silent`, and at least one peer logged `eot_received{writer=<this>}` | Healthy spawn with peer-confirmed E12 handshake. No action required. |
| `runner_idle_terminated` (T15.6) | Writer logged `eot_sent`, reached `phase=silent`, did NOT log `eot_timeout`, and NO peer logged `eot_received{writer=<this>}` | Healthy spawn that exited cleanly via the E15 variant-side idle-detection path (T15.5). No on-wire EOT handshake happens in E15, so the absence of a peer `eot_received` is expected, not a failure. No action required. |
| `eot_lost` | Writer logged `eot_sent` but never reached `phase=silent` | Legacy E12/E14 failure shape: writer published EOT but something on the other side prevented the spawn from completing cleanly. Suspect peer-side saturation if delivery throughput was close to the variant's headroom. |
| `variant_rejected` | Writer never reached `phase=operate` and stderr capture is non-empty | The variant exited cleanly before operate, typically because the configured QoS / threading mode / port is unsupported. Substring matches against `does not support single-threaded mode`, `does not support QoS`, `port collision`, `unsupported` confirm known rejection paths. |
| `variant_self_killed_idle` (T15.11) | Writer reached `phase=operate`, did NOT emit `eot_sent`, did NOT reach `phase=silent`, and stderr capture contains `watchdog: no progress` | The variant's in-process watchdog thread (`variant-base/src/watchdog.rs`) detected no progress on either counter for `--watchdog-secs` consecutive seconds during operate, flushed the JSONL, and called `std::process::exit(2)`. Distinct from `deadlock`: the watchdog flushes before exit, so the JSONL ends cleanly; the stderr line is the load-bearing signal. Distinct from `eot_lost`: the watchdog case has no `eot_sent`. The typical trigger is a wedged transport library (e.g. Zenoh qos3/qos4 multi under symmetric flood). |
| `variant_crashed` (2026-05-14 follow-up) | Writer reached `phase=operate`, did NOT emit `eot_sent`, did NOT reach `phase=silent`, stderr capture LACKS the watchdog signature, AND the JSONL ends mid-record | The variant entered operate then crashed abnormally (typically a panic inside a transport library) too fast for T15.11's watchdog to fire. The Zenoh qos3 alice case under stress is the motivating example: variant exits within ~2s of operate, faster than the default 30s watchdog threshold, leaving a truncated JSONL with no stderr signature. Distinct from `variant_self_killed_idle` (which requires the watchdog signature) and from `deadlock` (which is reserved for pre-operate truncations or callers that omit `logs_dir`). |
| `deadlock` | No `eot_sent`, no `phase=silent`, JSONL ends mid-record, AND either no `phase=operate` was reached OR `logs_dir` was unavailable for stderr reads | Killed mid-operate with neither the watchdog signature nor a `phase=operate` event in the JSONL. With T15.11 plus the 2026-05-14 `variant_crashed` rule, the post-operate crash cases are now siphoned off into `variant_self_killed_idle` / `variant_crashed`; `deadlock` remains the label for the legacy pre-operate truncation edge cases and the regression-test path where the caller omits `logs_dir`. |
| `unknown` | No rule matched | Operator must inspect manually. Should be rare; if it fires repeatedly the classifier needs another rule. |

Both `completed` and `runner_idle_terminated` are clean-exit
classifications. The T14.21 incomplete-samples warning treats both
identically and emits no `not completed` warning for either.

**Precedence note (T15.6).** `completed` sits above
`runner_idle_terminated` so that peer-confirmed handshakes (still
observable on variants that retain on-wire EOT, e.g. websocket Multi)
keep the more specific label. `runner_idle_terminated` and `eot_lost`
do not overlap: the new rule requires `phase=silent`, the legacy
`eot_lost` rule requires its absence. `eot_timeout_internal` keeps
its top-of-list precedence so any spawn that explicitly logged a
self-abort is never silently relabelled as a clean idle exit.

**Precedence note (T15.11).** `variant_self_killed_idle` sits
between `variant_rejected` and `deadlock` so the watchdog's explicit
diagnostic substring wins over the generic truncation heuristic in
the (currently empty) overlap case where both could match. It does
NOT overlap with `eot_lost` (which requires `eot_sent`), nor with
`runner_idle_terminated` (which requires both `eot_sent` and
`phase=silent`), nor with `variant_rejected` (which requires the
absence of `phase=operate`). The stderr signature is the load-
bearing signal; the classifier does not key on the exit code today
(though `2` is the documented value -- see
`variant-base/CUSTOM.md` "Internal-stall watchdog (T15.11)").

**Precedence note (2026-05-14 follow-up).** `variant_crashed` sits
between `variant_self_killed_idle` and `deadlock`. The three rules
share the same JSONL-truncation precondition and run under the same
`has_phase_operate` + `not has_eot_sent` + `not has_phase_silent`
gate. The discriminator is the stderr signature:
  - Signature present -> `variant_self_killed_idle` (slow stall).
  - Signature absent -> `variant_crashed` (fast panic).
  - `has_phase_operate` false OR `logs_dir` absent -> `deadlock`
    (legacy fallthrough).

Spawn-status caveat: ideally `variant_crashed` would also gate on
the runner's `ChildOutcome::Failed(_)` (non-zero, non-timeout exit
code), but the analysis pipeline has no access to that signal today
-- the runner does not write a spawn-status sidecar. With T15.11
active in practice the only path into this branch is a true variant
crash; the runner safety-net normally triggers the watchdog first.
If a future change writes a status sidecar, the rule can tighten by
additionally requiring `status == failed`.

Sub-tags are appended to the same row when applicable. Currently only
one is defined:

- **`eot_lost_likely_saturation`** -- attached to an `eot_lost` row
  when the asymmetric (apparently-successful) peer's stderr capture
  contains the substring `reader channel full`. Strong hint that the
  peer's reader-side mpsc channel saturated and the EOT marker was
  dropped along with the data tail.

Stderr capture reads are lazy: the classifier only opens a stderr
file when an `eot_lost` candidate needs the saturation sub-tag check,
a `variant_rejected` candidate needs the non-empty check, or a
`variant_self_killed_idle` / `variant_crashed` candidate (post-T15.11
and the 2026-05-14 follow-up) needs the watchdog signature check
(present -> self-killed-idle, absent -> variant_crashed). Spawns
that classify as `completed` / `runner_idle_terminated` /
`eot_timeout_internal` / `deadlock` / `unknown` without first being
a watchdog candidate never touch stderr.

The rules treat events as monotonically additive: presence of an
event is meaningful, absence is not (a still-running spawn would
look identical to a killed-before-EOT one if the file weren't also
truncated). The deadlock rule's truncation check on the JSONL tail
is therefore load-bearing -- it distinguishes "killed mid-operate"
from "operate finished cleanly but no EOT plumbing".

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
| `late_receives_tail_count` | Count of deliveries whose latency exceeds `10 * p99` (T11.5) -- extreme outliers within the distribution |
| `late_receives_tail_pct` | `100 * late_receives_tail_count / total_receives` -- the percentage of deliveries that landed in the late tail |

The late-receive-tail metric (T11.5) is distinct from the
`late_receives` field (which counts post-EOT pre-silent receives per
E12). The tail metric operates on the delivery latency distribution
itself: any receive whose corrected latency exceeds ten times the
group's 99th-percentile latency is flagged as a tail outlier. The
integrity report annotates rows from groups with a non-zero tail
percentage as `[late_tail_present]` so the operator sees the
outlier signal alongside delivery integrity data.

### 6.3 Throughput

Derived from `write` and `receive` event counts within the operate phase.

| Statistic | Description |
|---|---|
| Receive rate | `receives / operate_duration` per receiver -- **headline metric** (T11.5) |
| Write rate | `writes / operate_duration` per writer -- "requested rate" context |
| Delivery percentage | `100 * receive_rate / write_rate` -- ratio derived from the two throughputs |
| Aggregate | Sum of all writers' write rates |

The receive rate is the headline because receiver-side handling
(buffer pressure, parse cost, application work) is the actual sync
bottleneck in the project's "keep peers in sync under huge change
diffs" use case. Writers almost always ship at requested rate; the
write rate is shown next so the gap between request and delivery is
visible at a glance.

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

Column ordering (T11.5) puts the receive throughput first as the
headline metric. The write rate is labelled as the "requested" rate
and the derived delivery percentage follows. Latency percentiles,
jitter, loss, the existing `Late` (post-EOT) column, and the new
`LateTail%` outlier column come after. A `Thread` column reports the
group's `threading_mode` (defaulting to `"single"` for pre-T14.8
logs).

Grouping dimension: `(writer, receiver, variant, qos, threading_mode)`.
The per-(variant, run) summary inherits the threading_mode value from
the connected events in the group; the integrity report continues to
break out the (writer, receiver, qos) sub-grouping.

```
Performance Report
─────────────────────────────────────────────────────────────────────────────────────────────────────
Variant                  Run     Thread  Receives/s  Writes/s(req)  Delivery%  Connect(ms)  Lat p50  ... LateTail%
zenoh-replication        run01   single  98,500      98,750         99.74%     487          1.2ms    ... 0
custom-udp-replication   run01   single  99,900      99,900         100.00%    102          0.8ms    ... 0
```

No metric is removed relative to pre-T11.5 output: the column ORDER
and EMPHASIS shifts but every previously-reported value is preserved.

### 6.8 Pivot tables (variant × workload)

A pivot-tables section is appended to `--summary` after the three
existing reports (Integrity, Performance, Resource). It re-pivots the
PerformanceResult set onto a 2-D grid that lets the operator scan a
single QoS level at a glance:

- **Rows**: `(variant family, threading mode)` pairs in a canonical
  order — `custom-udp-single`, `custom-udp-multi`, `hybrid-single`,
  `hybrid-multi`, `websocket-single`, `websocket-multi`, `quic-multi`,
  `webrtc-multi`, `zenoh-multi`. Families that lack a Single binary
  (quic, webrtc, zenoh in the canonical config) appear only as
  `*-multi`. If the dataset includes a row outside the canonical
  ordering it is appended after the canonical rows so nothing is
  hidden.
- **Columns**: workload profile (`<vpt>x<hz>hz`) from the canonical
  config: `1000x100hz`, `1000x10hz`, `100x1000hz`, `100x100hz`,
  `100x10hz`, `10x100hz`, `10x1000hz`, plus the unbounded `max`
  workload. Columns outside the canonical set are appended after.
- **One table per QoS level** (1..4 in the canonical config; the table
  set only emits a level if at least one spawn populated it).

Every cell renders three sub-cell lines:

1. **Delivery%** — `100 × receives_per_sec / writes_per_sec`. Same
   formula as the flat performance table.
2. **Ratio%** — `100 × receives_per_sec / expected_writes_per_sec`
   where `expected_writes_per_sec = tick_rate_hz × values_per_tick`
   parsed from the spawn name. This is the "expected 10k/s but got
   5k/s = 50%" metric.
   - For the **max-throughput** workload no nominal rate exists, so
     this sub-cell is rendered as `n/a` (the other two sub-cells are
     still shown).
   - For **multicast** variants where the receiver also gets its own
     loopback writes (e.g. custom-udp single-mode subscribes to its
     own multicast group), the ratio can exceed 100%. This is
     expected behaviour and not a bug — the ratio measures
     receives-against-one-writer's-nominal-rate, and a multicast
     loopback adds the local writer's traffic on top.
   - For **Zenoh** the historical pre-2026-05-21 baseline showed
     ratios up to ~400% at low path-count workloads (e.g.
     `100x100hz qos1 multi`) because Zenoh's wildcard subscriber
     matched the variant's own publishes and the variant did not
     filter self-echoes at the receive boundary. The 2026-05-21
     self-writer filter (`variants/zenoh/src/{zenoh,rest_client}.rs`)
     drops self-echoes before they reach `inc_received`, per
     `compact-log-schema.md` event kind 1 (`receive`). The Zenoh
     ratio now matches the rest of the family at ~100% (one peer
     writing, one peer receiving) or ~200% (two-runner symmetric
     traffic, both peers receiving from each other). Any future
     ratios above the 200% multi-peer baseline indicate a real
     regression.
3. **mean ± std (ms)** — sample mean and (ddof=1) sample standard
   deviation of the per-message latency vector already stored on
   `PerformanceResult.latency_samples_ms`. Renders as `-` when the
   sample vector is empty (no deliveries, e.g. variant_self_killed_idle
   cases with no completed writes).

Empty cells (no spawn for the (family, mode, workload, qos) combination
or capability-gated combinations like quic-single) render as a triple
of dashes; the renderer does not crash on edge cases.

### 6.9 CSV export

`--csv-out <path>` writes a long-form CSV with one row per
`(variant, run)` and columns covering both the new pivot-table fields
and every existing PerformanceResult column. Operators can pivot this
in Excel/Sheets if the built-in pivot-table layout doesn't match the
desired slice. The CSV is well-formed even when the input is empty
(header row always emitted).

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
