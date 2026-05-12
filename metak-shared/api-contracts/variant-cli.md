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

Required from T14.8 onward (the runner-side change that injects the arg
unconditionally). During the T14.1 -> T14.8 rollout window the arg is
optional with default `single` so existing runner integration tests that
spawn variants directly continue to work. Tells the variant which
execution model to use:

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
benchmark.

**Semantics (clarified 2026-05-12 after T14.3 + T14.4 implementation
discovery):** `--recv-buffer-kb` is a **minimum floor**, not an
absolute value. Variants must ensure each recv-side socket they own
has `SO_RCVBUF` at least as large as `recv_buffer_kb * 1024`; if the
variant has its own tuning logic that sets a larger value (e.g.
custom-udp's T-impl.2 8 MiB `tune_udp_buffers`, hybrid's same pattern),
that larger value is preserved. The original strict-`setsockopt(SO_RCVBUF,
recv_buffer_kb * 1024)` wording would have *shrunk* the existing T-impl.2
buffer to 4 MiB on default and regressed the qos1 100 K msg/s same-host
fixture; treating the arg as a floor rather than a setter keeps the
contract's intent ("at least this large") while preserving variant-side
tuning.

Variants whose transport library does not expose the underlying socket
(Zenoh, webrtc-rs) must document why and may treat this arg as advisory.

### JSONL log impact

The `connected` event gains a `threading_mode` field whose value is one
of `"single"` / `"multi"`. The field is optional during the E14 rollout
(pre-T14.8 logs may omit it) and becomes required once T14.8 lands.
The `recv_buffer_kb` value is recorded in the same `connected` event
as a separate field, for offline reproducibility.

See also `jsonl-log-schema.md` DRAFT section (T14.1 will write that
update).

---

## E15 additions: stdout progress emission

One new common CLI arg, added by T15.1 (approved 2026-05-11):

### `--progress-stdout-interval-ms <u32>`

Optional, default `1000` (one progress line per second). `0` disables
emission entirely -- the back-compat path for callers that pre-date
E15. Range is `0..=u32::MAX`; sane runner-side values sit in
`100..=5000`.

When `> 0`, the variant emits one JSON line to **stdout** per interval
with the schema below. The line is the **only** stdout output the
variant produces: all banners, warnings, and other diagnostic text go
to stderr. This invariant is what makes the runner's stdout reader
(T15.2) able to parse the stream as line-delimited JSON.

### Stdout JSON schema (one line per interval)

```
{"event":"progress","ts":"<RFC 3339 with ns>","phase":"<phase>","sent":<u64>,"received":<u64>,"eot_sent":<bool>,"eot_received":<bool>}
```

Field semantics:

| Field | Type | Description |
|-------|------|-------------|
| `event` | string | Always the literal `"progress"`. |
| `ts` | RFC 3339 with nanoseconds | Wall-clock timestamp at which the line was emitted (UTC). |
| `phase` | string | One of `"connect"`, `"stabilize"`, `"operate"`, `"eot"`, `"silent"`, `"done"`. Reflects the variant-side protocol driver phase at emission time; `"done"` is the terminal label the variant uses after the driver has torn the transport down and the binary is about to exit. |
| `sent` | u64 | Monotonic per-spawn aggregate count of successful `try_publish` outcomes (`Ok(true)`). Aggregated across all peers; per-peer breakdown remains in the JSONL `write` events. |
| `received` | u64 | Monotonic per-spawn aggregate count of `receive` events observed via the driver's `poll_receive` drain loops. Aggregated across all peers; per-peer breakdown remains in the JSONL `receive` events. |
| `eot_sent` | bool | Sticky: flips to `true` after the variant emits its `eot_sent` JSONL event. |
| `eot_received` | bool | Sticky: flips to `true` once every expected peer EOT has been observed (or immediately, if the expected-peer set is empty -- e.g. single-runner self-loopback). |

Atomicity: the variant writes one complete JSON object followed by a
single `'\n'` per emission, then flushes stdout. Line splitters on the
runner side may rely on `'\n'` as the record separator.

Terminal line: the variant emits one final line synchronously during
driver shutdown so the runner observes the `done` transition exactly
once even if the interval boundary has not yet fired. The thread is
joined before the binary exits, so no further stdout output can be
generated past the last `done` line.

### Disabled path

`--progress-stdout-interval-ms 0` results in **zero** stdout writes
from the variant. The variant-base driver still maintains the
in-memory counters (so tests and future hooks have access) but the
emitter thread is not spawned. Pre-E15 runner integration tests work
unchanged because they do not pass the new flag, and clap's default
for the arg is `1000` -- which is a no-op for tests that never read
the child's stdout.
