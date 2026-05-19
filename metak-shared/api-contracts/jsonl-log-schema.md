# API Contract: JSONL Log Schema

Defines the structured log format produced by variant processes and consumed
by the analysis tool.

Source: BENCHMARK.md S8, ANALYSIS.md S4-S6.

> **Scope (post-E19 cleanup, 2026-05-19)**: this JSONL stream carries
> **lifecycle events only** — `phase`, `connected`, `eot_*`, `resource`,
> `clock_sync`. Per-event observations (`write`, `receive`,
> `backpressure_skipped`, `gap_detected`, `gap_filled`) are written to a
> compact Parquet digest at the end of each spawn — see
> [`compact-log-schema.md`](compact-log-schema.md). The E18 dual-emission
> opt-in (`--legacy-jsonl-events`) was removed in the E19 follow-up
> cleanup; there is no path that emits per-event data to JSONL any more.

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
| `threading_mode` | string (optional during E14 rollout, required once T14.8 lands) | The threading execution model the variant ran under for this spawn: `"single"` or `"multi"`. Matches the `--threading-mode` injected arg. See `variant-cli.md` "E14 additions". Variant-base logs this field unconditionally once T14.1 lands; logs produced by binaries that pre-date T14.1 will omit it. |
| `recv_buffer_kb` | integer (optional during E14 rollout, required once T14.8 lands) | The OS-level recv buffer size in kibibytes the variant was asked to apply (range `64..=65536`, default `4096`). Variants apply `setsockopt(SO_RCVBUF, recv_buffer_kb * 1024)` on every recv-side socket they own; async-only variants whose transport library hides the socket treat the arg as advisory but still record it for offline reproducibility. |

### `phase`

Logged at the start of each test protocol phase.

| Field | Type | Description |
|-------|------|-------------|
| `phase` | string | One of: `connect`, `stabilize`, `operate`, `eot`, `silent` |
| `profile` | string (optional) | Workload profile name (only for `operate`) |

The `eot` phase is the bounded end-of-test handshake between `operate`
and `silent`. See `eot-protocol.md` for the full contract.

### `eot_sent`

Logged once by the writer immediately after the variant's
`signal_end_of_test` returns, at the start of the EOT phase. See
`eot-protocol.md` for the full contract.

| Field | Type | Description |
|-------|------|-------------|
| `eot_id` | integer | The 64-bit id used for this writer's EOT. Lets a receiver's `eot_received.eot_id` join with the writer's `eot_sent.eot_id`. |

### `eot_received`

Logged once per (writer, eot_id) by the receiver, after dedup.

| Field | Type | Description |
|-------|------|-------------|
| `writer` | string | Runner name of the writer whose EOT was just observed |
| `eot_id` | integer | The id from the writer's `eot_sent` |

### `eot_timeout`

Logged once at the end of the EOT phase IF the variant's
`wait_for_peer_eots` returned `EotOutcome::TimedOut`. Diagnostic only —
presence does NOT abort the spawn.

| Field | Type | Description |
|-------|------|-------------|
| `missing` | array of strings | Peer runner names that never signalled EOT |
| `wait_ms` | integer | Wall-clock duration of the wait |

### `resource`

Logged periodically (e.g. every 100 ms) during operation phases.

| Field | Type | Description |
|-------|------|-------------|
| `cpu_percent` | float | CPU usage percentage |
| `memory_mb` | float | Memory usage in megabytes |

### `clock_sync`

Logged by the **runner** (not variants) into a sibling log file
`<runner>-clock-sync-<run>.jsonl`, one entry per peer per measurement.
Used by analysis to correct cross-machine `receive_ts − write_ts` for
inter-machine clock skew. See `clock-sync.md` for the measurement protocol.

Required (columnar) fields:

| Field | Type | Description |
|-------|------|-------------|
| `peer` | string | Peer runner name (the other side of the pair) |
| `offset_ms` | float | `peer.clock − self.clock` in milliseconds (best sample) |
| `rtt_ms` | float | RTT of the selected best sample, in milliseconds |

Optional diagnostic fields (kept in JSONL only, not in `SHARD_SCHEMA`):

| Field | Type | Description |
|-------|------|-------------|
| `samples` | integer | Number of samples taken |
| `min_rtt_ms` | float | Minimum RTT across all samples |
| `max_rtt_ms` | float | Maximum RTT across all samples |
| `outlier_rejected` | bool | `true` if the min-RTT sample was rejected and the median-of-three-lowest-RTT fallback fired (T8.4) |

A sibling `<runner>-clock-sync-debug-<run>.jsonl` file is also written
with one line per raw sample (per-sample t1/t2/t3/t4, derived rtt/offset,
and a `chosen` flag). Used for offline diagnosis only; analysis ignores
this file entirely.

Note: in clock-sync events, the `variant` common field carries the variant
about to start (or `""` for the initial sync that runs before any variant).

## Known Deviations

_None yet._
