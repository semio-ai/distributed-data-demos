# API Contract: TOML Config Schema

Defines the benchmark configuration file format. A single TOML file represents
a complete benchmark run and is the only file that needs to be copied to each
machine.

Source: BENCHMARK.md S4.

## Schema

```toml
# Unique identifier for this benchmark run.
# Included in every log line so repeated runs are distinguishable.
run = "<string>"                        # e.g. "run01"

# Runner names expected in this benchmark.
# Discovery completes when all names have been seen.
runners = ["<string>", ...]             # e.g. ["a", "b", "c"]

# Default timeout for variant processes (seconds).
# Can be overridden per variant.
default_timeout_secs = <integer>        # e.g. 120

# Variant definitions — executed in order.
[[variant]]
name = "<string>"                       # unique name, e.g. "zenoh-replication"
binary = "<path>"                       # path to the variant executable
timeout_secs = <integer>                # optional, overrides default_timeout_secs

  # Common section — passed to ALL variant instances as CLI args.
  [variant.common]
  tick_rate_hz = <integer>              # target tick rate in Hz
  stabilize_secs = <integer>            # stabilize phase duration
  operate_secs = <integer>              # operate phase duration
  silent_secs = <integer>               # silent/drain phase duration
  workload = "<string>"                 # workload profile name
  values_per_tick = <integer>           # writes per tick
  qos = <integer | array | omitted>     # QoS level(s) — see "QoS Expansion"
  log_dir = "<path>"                    # directory for JSONL output

  # Variant-specific options — only this implementation uses these.
  # All keys are passed as --kebab-case CLI args.
  [variant.specific]
  # (implementation-defined key-value pairs)
```

## Field Details

### Top-Level

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `run` | yes | string | Unique run identifier. Appears in every log line. |
| `runners` | yes | array of strings | Names of all expected runners. Discovery waits for all. |
| `default_timeout_secs` | yes | integer | Default child process timeout in seconds. |

### `[[variant]]`

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `name` | yes | string | Unique variant name. Used in log files and reports. |
| `binary` | yes | string | Path to the variant executable (relative to runner CWD). |
| `timeout_secs` | no | integer | Per-variant timeout override. Falls back to `default_timeout_secs`. |

### `[variant.common]`

All fields below are passed to the variant as CLI arguments. The runner
converts `snake_case` keys to `--kebab-case` args.

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `tick_rate_hz` | yes | integer | Target tick rate |
| `stabilize_secs` | yes | integer | Stabilize phase duration |
| `operate_secs` | yes | integer | Operate phase duration |
| `silent_secs` | yes | integer | Silent phase duration |
| `workload` | yes | string | Workload profile name |
| `values_per_tick` | yes | integer | Values written per tick |
| `qos` | no | integer OR array of integers (1-4) | If omitted, the runner expands the entry into 4 spawn invocations (qos 1, 2, 3, 4). If an array (e.g. `qos = [1, 3]`), the runner expands into one spawn per listed level. If an integer, behaves as before — one spawn at that level. See "QoS Expansion" below. |
| `log_dir` | yes | string | JSONL output directory |

### `[variant.specific]`

Implementation-defined. All key-value pairs are passed as `--kebab-case` CLI
args. Examples:

- Zenoh: `zenoh_mode`, `zenoh_listen`
- Custom UDP: `buffer_size`, `multicast_group`
- QUIC: `base_port` (single integer; bind/connect ports are derived by the
  variant from `--peers` + `--runner` + `--qos` per the port-stride rules
  below)

#### Port stride and QoS expansion

When a variant entry has multiple QoS levels (omitted or array form) AND a
variant binds ports that must not collide across consecutive QoS runs, the
runner sequentially executes one full stabilize/operate/silent cycle per QoS
level. Port reuse across cycles is generally safe (the prior child has
exited), but variants that hold TCP listeners with TIME_WAIT-prone ports
may collide.

Convention for variants that need QoS-disjoint ports:
- The variant's `[variant.specific]` section provides a single `base_port`
  integer.
- The variant computes `effective_port = base_port + (runner_index * runner_stride) + ((qos - 1) * qos_stride)`
  where `runner_stride` defaults to 1 and `qos_stride` defaults to 10
  (chosen to keep dimensions disjoint with up to 10 runners).
- The variant determines `runner_index` by looking up `--runner` in `--peers`.
- The variant determines `qos` from `--qos`.

This convention is variant-implementation-defined — the runner does not
manipulate ports inside `[variant.specific]`. It only injects `--peers` and
`--qos` and lets each variant compute what it needs.

## Validation Rules

1. `run` must be non-empty.
2. `runners` must contain at least one name.
3. Each `[[variant]]` must have a unique `name`.
4. `binary` paths should be validated at launch time (runner checks existence
   before discovery).
5. If `qos` is an integer, it must be in range 1-4. If an array, every
   element must be in range 1-4 and the array must be non-empty. If
   omitted, the runner treats it as `[1, 2, 3, 4]`.
6. `timeout_secs` (or `default_timeout_secs`) must be positive.

## QoS Expansion

When a `[[variant]]` entry resolves to more than one QoS level, the runner
executes one full lifecycle per level — back-to-back, in ascending QoS
order — under a synthesized name:

- Effective spawn name: `<variant.name>-qos<N>` (e.g. `custom-udp-1000x100hz-qos2`).
  This is what the runner passes as `--variant` to the spawn AND uses for
  ready/done barrier identifiers.
- Each spawn runs the full `stabilize_secs / operate_secs / silent_secs`
  cycle, gets its own `--launch-ts`, and produces its own JSONL log file.
- Ready/done barriers happen per spawn, so all runners stay in lockstep
  per QoS level.
- The base name (`<variant.name>` without the `-qosN` suffix) and the QoS
  level are recoverable from log records via `(variant, qos)`. The
  analysis tool groups by `(variant_base, qos)` for per-variant per-QoS
  statistics.

Single-QoS entries (integer form) skip the expansion and use the original
`<variant.name>` as before — backward compatible.

## Known Deviations

_None yet._
