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

# OPTIONAL: reusable variant defaults referenced by `[[variant]]` entries.
# Templates do NOT spawn — they only provide defaults. See "Variant Templates".
[[variant_template]]
name = "<string>"                       # template identifier, unique
binary = "<path>"                       # default binary
timeout_secs = <integer>                # optional default timeout
  [variant_template.common]
  # any subset of the common keys below
  [variant_template.specific]
  # any variant-specific defaults

# Variant definitions — executed in order.
[[variant]]
name = "<string>"                       # unique name, e.g. "zenoh-replication"
template = "<string>"                   # OPTIONAL — name of a [[variant_template]] to inherit from
binary = "<path>"                       # path to the variant executable (may come from template)
timeout_secs = <integer>                # optional, overrides default_timeout_secs

  # Common section — passed to ALL variant instances as CLI args.
  # When `template` is set, missing keys are taken from the template's common.
  [variant.common]
  tick_rate_hz = <integer | array>      # target tick rate in Hz — array form expands; see "Array Expansion"
  stabilize_secs = <integer>            # stabilize phase duration
  operate_secs = <integer>              # operate phase duration
  silent_secs = <integer>               # silent/drain phase duration
  workload = "<string>"                 # workload profile name
  values_per_tick = <integer | array>   # writes per tick — array form expands; see "Array Expansion"
  qos = <integer | array | omitted>     # QoS level(s) — see "QoS Expansion"
  log_dir = "<path>"                    # directory for JSONL output

  # Variant-specific options — only this implementation uses these.
  # When `template` is set, missing keys are taken from the template's specific.
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
| `name` | yes | string | Unique variant name. Used in log files and reports. Acts as the base name for spawn auto-naming when array expansion produces multiple spawns from this entry. |
| `template` | no | string | Name of a `[[variant_template]]` to inherit from. See "Variant Templates". |
| `binary` | conditional | string | Path to the variant executable (relative to runner CWD). Required either here or in the referenced template. |
| `timeout_secs` | no | integer | Per-variant timeout override. Falls back to template, then `default_timeout_secs`. |

### `[variant.common]`

All fields below are passed to the variant as CLI arguments. The runner
converts `snake_case` keys to `--kebab-case` args.

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `tick_rate_hz` | yes | integer OR array of integers | Target tick rate. Array form expands the entry into one spawn per listed Hz value (deduplicated, sorted ascending). See "Array Expansion" below. |
| `stabilize_secs` | yes | integer | Stabilize phase duration |
| `operate_secs` | yes | integer | Operate phase duration |
| `silent_secs` | yes | integer | Silent phase duration |
| `workload` | yes | string | Workload profile name |
| `values_per_tick` | yes | integer OR array of integers | Values written per tick. Array form expands the entry into one spawn per listed value (deduplicated, sorted ascending). See "Array Expansion" below. |
| `qos` | no | integer OR array of integers (1-4) | If omitted, the runner expands the entry into 4 spawn invocations (qos 1, 2, 3, 4). If an array (e.g. `qos = [1, 3]`), the runner expands into one spawn per listed level. If an integer, behaves as before — one spawn at that level. See "QoS Expansion" below. |
| `log_dir` | yes | string | JSONL output directory. **MUST be `"./logs"`** for every config and every test/validation fixture. Per-run isolation is provided by the auto-created session subfolder `<log_dir>/<run-name>-<launch-ts>/`; tests and ad-hoc validations MUST NOT introduce sibling `logs-<tag>/` roots at the repo level. Anything a task wants to "break out" goes inside the session subfolder, not next to `logs/`. |

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
7. If `tick_rate_hz` or `values_per_tick` is an array, every element must
   be a positive integer and the array must be non-empty.
8. Each `[[variant_template]]` must have a unique `name`. A `template = "..."`
   reference in a `[[variant]]` must resolve to a defined template.
9. After template resolution, every `[[variant]]` must have a non-empty
   `binary` and the validation rules above apply to the merged values.

## Variant Templates

Many configs need to define several `[[variant]]` entries that share the same
binary, common settings, and specific settings, varying only in workload,
tick rate, or values-per-tick. To avoid bulky duplication a `[[variant]]`
entry can reference a `[[variant_template]]` and inherit its fields.

### Resolution semantics

When a `[[variant]]` has `template = "<name>"`:

1. The named template must exist (validation error otherwise).
2. **Top-level scalars** (`binary`, `timeout_secs`): the variant entry's value
   wins if present; otherwise the template's value is used. After resolution
   `binary` must be non-empty.
3. **`[variant.common]` and `[variant.specific]`** are deep-merged on a
   per-key basis: every key from the template's matching section is taken
   unless the variant entry specifies the same key, in which case the variant
   entry wins. There is no array-merging or deep-table-merging — only
   key-level union with the variant entry overriding.
4. The merged result is then validated and expanded (templates do not skip
   any validation; they only provide defaults).

Templates are NOT spawned themselves. They exist solely to provide defaults
to one or more `[[variant]]` entries. A template not referenced by any
variant is allowed (warning, not error).

### Example

```toml
[[variant_template]]
name = "custom-udp-base"
binary = "target/release/variant-custom-udp.exe"
  [variant_template.common]
  stabilize_secs = 3
  operate_secs = 30
  silent_secs = 3
  workload = "scalar-flood"
  log_dir = "./logs"
  [variant_template.specific]
  multicast_group = "239.0.0.1:19501"
  buffer_size = 65536
  tcp_base_port = 19800

[[variant]]
template = "custom-udp-base"
name = "custom-udp"
  [variant.common]
  tick_rate_hz = [10, 100, 1000]
  values_per_tick = [10, 100, 1000]
  # qos omitted -> all four levels

[[variant]]
template = "custom-udp-base"
name = "custom-udp-max"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1000
  workload = "max-throughput"   # overrides the template's "scalar-flood"
```

## Array Expansion

`tick_rate_hz`, `values_per_tick`, and `qos` accept either a scalar or a
non-empty integer array. When more than one of them is an array, the
runner expands the entry into the **Cartesian product** of all listed
values, producing one spawn per combination. Each spawn runs the full
`stabilize_secs / operate_secs / silent_secs` cycle, gets its own
`--launch-ts`, and produces its own JSONL log file. Ready/done barriers
happen per spawn, so runners stay in lockstep per spawn.

### Spawn auto-naming

Effective spawn name is built from the post-template `[[variant]]` `name`
plus suffixes for the dimensions that actually expanded:

```
<name>[-<vpt>x<hz>hz][-qos<N>]
```

Suffix rules:

- `-<vpt>x<hz>hz` is appended whenever `tick_rate_hz` OR `values_per_tick`
  was given as an array (i.e. the entry produces more than one (vpt, hz)
  combination). Both numbers always appear (even the dimension that was
  scalar) so the suffix uniquely identifies the spawn within its parent
  entry. Example: `custom-udp-1000x100hz`.
- `-qos<N>` is appended whenever `qos` resolves to more than one level
  (array form, or omitted). Example: `custom-udp-1000x100hz-qos2`.
- A single-element array (e.g. `qos = [3]`, `tick_rate_hz = [100]`) counts
  as scalar — no suffix from that dimension, matching existing QoS
  behavior.

The base name and the per-dimension values are recoverable from log
records via `(variant, qos)` plus the in-log fields the variant emits
(tick rate, values per tick are already available in the `start` event).
The analysis tool can group by `(variant_base, qos, hz, vpt)` as needed.

### Sequential execution + grace period

Spawns derived from one source entry run **sequentially** in ascending
order: first by `tick_rate_hz`, then by `values_per_tick`, then by `qos`
(stable sort, ascending). The same `inter_qos_grace_ms` grace applies
between every consecutive pair of spawns from the same source entry —
not just QoS-pair boundaries — so socket release behaves consistently
across all expanded dimensions.

### Backward compatibility

Configs that use only scalar `tick_rate_hz`, `values_per_tick`, and `qos`
continue to work unchanged. The single-QoS integer form, the omitted-`qos`
form (expands to 1..=4), and the array `qos` form all retain their
existing semantics — the new mechanism is purely additive on the other two
dimensions.

## Known Deviations

_None yet._
