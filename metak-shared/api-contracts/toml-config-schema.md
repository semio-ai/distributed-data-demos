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
  qos = <integer>                       # QoS level (1-4)
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
| `qos` | yes | integer | QoS level (1-4) |
| `log_dir` | yes | string | JSONL output directory |

### `[variant.specific]`

Implementation-defined. All key-value pairs are passed as `--kebab-case` CLI
args. Examples:

- Zenoh: `zenoh_mode`, `zenoh_listen`
- Custom UDP: `buffer_size`, `multicast_group`

## Validation Rules

1. `run` must be non-empty.
2. `runners` must contain at least one name.
3. Each `[[variant]]` must have a unique `name`.
4. `binary` paths should be validated at launch time (runner checks existence
   before discovery).
5. `qos` must be in range 1-4.
6. `timeout_secs` (or `default_timeout_secs`) must be positive.

## Known Deviations

_None yet._
