# API Contract: JSONL Log Schema

Defines the structured log format produced by variant processes and consumed
by the analysis tool.

Source: BENCHMARK.md S8, ANALYSIS.md S4-S6.

## General Rules

- One JSON object per line (JSON Lines format).
- Every line MUST include: `ts`, `variant`, `runner`, `run`, `event`.
- Files are named `<variant>-<runner>-<run>.jsonl` but the file name is NOT
  authoritative — the fields inside each line are.
- Timestamps (`ts`) use RFC 3339 with nanosecond precision
  (e.g. `2026-04-12T14:00:01.123456789Z`).
- If all log files from all nodes, variants, and runs are concatenated into a
  single file, the full dataset must be recoverable by grouping on any
  combination of `(variant, runner, run)`.

## Common Fields (all events)

| Field | Type | Description |
|-------|------|-------------|
| `ts` | string (RFC 3339, nanosecond) | Wall-clock timestamp of the event |
| `variant` | string | Variant name (e.g. `zenoh-replication`) |
| `runner` | string | Runner name (e.g. `a`) |
| `run` | string | Run identifier (e.g. `run01`) |
| `event` | string | Event type (see below) |

## Event Types

### `connected`

Logged once when the variant has established connections to all peers.

| Field | Type | Description |
|-------|------|-------------|
| `launch_ts` | string (RFC 3339) | The `--launch-ts` value from the runner |
| `elapsed_ms` | float | `connected_ts - launch_ts` in milliseconds |

### `phase`

Logged at the start of each test protocol phase.

| Field | Type | Description |
|-------|------|-------------|
| `phase` | string | One of: `connect`, `stabilize`, `operate`, `silent` |
| `profile` | string (optional) | Workload profile name (only for `operate`) |

### `write`

Logged by the writer each time a value is written during the operate phase.

| Field | Type | Description |
|-------|------|-------------|
| `seq` | integer | Monotonic sequence number for this writer |
| `path` | string | Key path (e.g. `/sensors/lidar`) |
| `qos` | integer | QoS level (1-4) |
| `bytes` | integer | Serialized size of the value in bytes |

### `receive`

Logged by a reader each time a replicated value is received.

| Field | Type | Description |
|-------|------|-------------|
| `writer` | string | Runner name of the node that wrote the value |
| `seq` | integer | The writer's sequence number for this update |
| `path` | string | Key path |
| `qos` | integer | QoS level |
| `bytes` | integer | Serialized size |

### `gap_detected`

Logged by a reader when a sequence gap is detected (QoS 3).

| Field | Type | Description |
|-------|------|-------------|
| `writer` | string | Runner name of the writer |
| `missing_seq` | integer | The missing sequence number |

### `gap_filled`

Logged by a reader when a previously detected gap is recovered (QoS 3).

| Field | Type | Description |
|-------|------|-------------|
| `writer` | string | Runner name of the writer |
| `recovered_seq` | integer | The recovered sequence number |

### `resource`

Logged periodically (e.g. every 100 ms) during operation phases.

| Field | Type | Description |
|-------|------|-------------|
| `cpu_percent` | float | CPU usage percentage |
| `memory_mb` | float | Memory usage in megabytes |

## Correlation Key

The analysis tool correlates writes and receives using:

```
(variant, run, writer, seq, path)
```

where `writer` on the receive side comes from the `receive` event's `writer`
field, and on the write side from the `write` event's `runner` field.

## Known Deviations

_None yet._
