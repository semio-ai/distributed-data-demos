# API Contract: Variant CLI Interface

Defines how the runner spawns variant processes and what arguments they receive.

Source: BENCHMARK.md S4, S5.

## Invocation

The runner constructs a CLI command from the TOML config and spawns the variant
as a child process. There is no IPC after launch.

```
<binary> [common args...] [specific args...] --launch-ts <RFC3339>
```

## Common Arguments (from `[variant.common]`)

All key-value pairs in the `[variant.common]` TOML section are passed as
`--key value` CLI arguments. The following keys are defined by the benchmark
design:

| Argument | Type | Description |
|----------|------|-------------|
| `--tick-rate-hz` | integer | Target tick rate in Hz (e.g. 100) |
| `--stabilize-secs` | integer | Duration of the stabilize phase in seconds |
| `--operate-secs` | integer | Duration of the operate phase in seconds |
| `--silent-secs` | integer | Duration of the silent/drain phase in seconds |
| `--workload` | string | Workload profile name (e.g. `scalar-flood`) |
| `--values-per-tick` | integer | Number of value updates per tick |
| `--qos` | integer | QoS level (1-4). Always a single integer at the variant CLI level — when the TOML omits `qos` or specifies a list, the runner expands the entry into multiple per-QoS spawn invocations and passes one concrete level per spawn. |
| `--log-dir` | path | Directory for JSONL output |

## Runner-Injected Arguments

These are added by the runner itself, not from the config file:

| Argument | Type | Description |
|----------|------|-------------|
| `--launch-ts` | RFC 3339 timestamp | Wall-clock time recorded by the runner immediately before spawn. Used by the variant to compute connection time. |
| `--variant` | string | The variant name from config (e.g. `zenoh-replication`). May include a `-qosN` suffix when the runner expands a `qos`-omitted entry into per-QoS runs (see TOML config schema). Used in log entries. |
| `--runner` | string | The runner's name (e.g. `a`). Used in log entries. |
| `--run` | string | The run identifier from config (e.g. `run01`). Used in log entries. |
| `--peers` | string | Comma-separated `name=host` pairs for ALL runners in the config (including this one), e.g. `alice=192.168.1.10,bob=192.168.1.11`. Hosts are derived by the runner during discovery (Phase 1, see `runner-coordination.md`). For same-host peers the value is `127.0.0.1`. Variants that need explicit peer addresses (e.g. QUIC) use this; variants that do their own discovery (Zenoh, mDNS-based variants) may ignore it. |

## Specific Arguments (from `[variant.specific]`)

All key-value pairs in the `[variant.specific]` TOML section are passed as
`--key value` CLI arguments. These are implementation-defined. Examples:

**Zenoh variant**: `--zenoh-mode peer --zenoh-listen udp/0.0.0.0:7447`

**Custom UDP variant**: `--buffer-size 65536 --multicast-group 239.0.0.1:9000`

## Exit Code

| Code | Meaning |
|------|---------|
| 0 | Success — all phases completed, logs flushed |
| Non-zero | Failure — runner records the code |

The runner kills the variant if it does not exit within the per-variant
`timeout_secs` (or `default_timeout_secs`) and records a timeout.

## Key Convention

TOML keys use `snake_case`. CLI arguments use `kebab-case` with `--` prefix.
The runner converts `snake_case` TOML keys to `--kebab-case` CLI args
(e.g. `tick_rate_hz` becomes `--tick-rate-hz`).
