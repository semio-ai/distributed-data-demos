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
| `--variant` | string | The effective spawn name from the runner. Equals the `[[variant]].name` (post-template-resolution) when the entry expands to a single spawn. May carry suffixes when the entry expands across multiple dimensions: `-<vpt>x<hz>hz` when `values_per_tick` or `tick_rate_hz` was an array, and/or `-qos<N>` when `qos` was an array or omitted. See TOML config schema "Array Expansion". Used in log entries. |
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

---

## E14 additions: threading mode and recv buffer

Two new runner-injected CLI args, added by T14.1 + T14.8 (approved 2026-05-12):

### `--threading-mode <single|multi>`

Required. Set by the runner from the expanded `threading_modes` dimension
in TOML config (see `toml-config-schema.md` DRAFT section). Tells the
variant which execution model to use:

- `single` -- single-threaded, fully synchronous. No tokio. Variants
  that fundamentally rely on async runtimes (QUIC, WebRTC, Zenoh)
  must reject this mode at `connect` time with a clear error and exit
  non-zero before any I/O. TCP-family variants (websocket, hybrid,
  custom-udp) must support this mode. Single-threaded mode is the
  WASM-compatible mode -- adopting any Rust crate or pattern that
  forces async into this mode is a violation of the WASM compilation
  goal.
- `multi` -- multi-threaded. Variants may spawn OS threads (typically
  one per peer connection on the recv side) to decouple per-message
  parse cost from the driver's poll cadence. May still avoid tokio if
  the transport library doesn't require it; "multi" does NOT imply
  "async". All seven variants must support this mode.

Capability is per-variant and declared via the Variant trait's
`supported_threading_modes() -> &[ThreadingMode]`. The runner consults
this declaration (mechanism: static TOML field OR `--print-capabilities`
probe -- T14.8 chooses) and silently skips spawns whose threading_mode
the variant does not support.

### `--recv-buffer-kb <u32>`

Optional, default `4096` (4 MiB). Range `64..=65536` (64 KiB to 64 MiB).
Sized to be safe on a Raspberry Pi 4 with 4 GB RAM under a 2-peer
benchmark. Variants must call `setsockopt(SO_RCVBUF, recv_buffer_kb * 1024)`
on every recv-side socket they own. Variants whose transport library
does not expose the underlying socket (Zenoh, webrtc-rs) must document
why and may treat this arg as advisory.

### JSONL log impact

The `connected` event gains a `threading_mode` field whose value is one
of `"single"` / `"multi"`. The field is optional during the E14 rollout
(pre-T14.8 logs may omit it) and becomes required once T14.8 lands.
The `recv_buffer_kb` value is recorded in the same `connected` event
as a separate field, for offline reproducibility.

See also `jsonl-log-schema.md` DRAFT section (T14.1 will write that
update).
