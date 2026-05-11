# variant-base Custom Instructions

## Overview

Rust library crate providing the shared foundation for all benchmark variant
implementations. Concrete variants (Zenoh, custom UDP, etc.) depend on this
crate and only implement transport-specific logic. This crate handles
everything else.

## Tech Stack and Conventions

- **Language**: Rust (2021 edition)
- **Crate type**: library (`lib`), plus a binary target `variant-dummy`
- **Key dependencies**:
  - `clap` (derive style) — CLI argument parsing
  - `serde`, `serde_json` — JSONL log serialization
  - `chrono` — RFC 3339 timestamps with nanosecond precision
  - `anyhow` — error handling (binary target); `thiserror` for the library's
    public error types
  - `sysinfo` — CPU/memory sampling for resource monitor
- **Do NOT depend on `arora_types`** yet. The workload currently generates
  synthetic `Value`-like payloads (byte vectors of configurable size). The
  real `arora_types::Value` integration comes when concrete variants land.
  This keeps the base crate buildable without pulling in the full arora
  dependency chain during early development.
- Follow `metak-shared/coding-standards.md` for Rust conventions (cargo fmt,
  cargo clippy --deny warnings, etc.)

## Build and Test

All commands run from the repo root (Cargo workspace). Do **not** `cd` into
`variant-base/` to build — that produces a stray per-subfolder `target/`
directory which the configs and the runner integration tests do not point
at.

```
cargo build --release -p variant-base       # build library + variant-dummy binary
cargo test --release -p variant-base        # unit + integration tests
cargo clippy --release -p variant-base -- -D warnings
cargo fmt -p variant-base -- --check
```

Compiled `variant-dummy` lives at `target/release/variant-dummy(.exe)`.

## Integration Contracts

This crate implements the variant side of two API contracts:

- **Variant CLI contract**: `metak-shared/api-contracts/variant-cli.md`
  Defines common CLI arguments, runner-injected arguments, specific argument
  pass-through, and exit code semantics.

- **JSONL log schema**: `metak-shared/api-contracts/jsonl-log-schema.md`
  Defines the structured log format. Every line must include `ts`, `variant`,
  `runner`, `run`, `event`. Event types: `connected`, `phase`, `write`,
  `receive`, `gap_detected`, `gap_filled`, `resource`.

## Architecture

```
variant-base/
  src/
    lib.rs           -- public API re-exports
    trait.rs         -- Variant trait definition
    types.rs         -- shared types (QoS, Phase, ReceivedUpdate, etc.)
    cli.rs           -- common CLI arg parsing (clap)
    logger.rs        -- JSONL structured logger
    driver.rs        -- test protocol driver (4 phases)
    workload.rs      -- workload profile trait + scalar-flood impl
    seq.rs           -- sequence number generator
    resource.rs      -- CPU/memory resource monitor
  bin/
    variant_dummy.rs -- VariantDummy binary entry point
  tests/
    integration.rs   -- full protocol driver test with VariantDummy
```

Module layout is a suggestion, not a mandate. Organize naturally as the code
evolves, but keep the public API surface clean.

## Design Guidance

### Variant Trait

The trait is the core of this crate. It must be:
- **Generic enough** to accommodate different transport models (pub/sub,
  direct UDP, TCP connections, shared memory).
- **Minimal** — only the transport-specific operations. Everything else
  (phases, logging, workload, CLI) lives outside the trait.
- **Synchronous-first** — use blocking APIs. Async can be introduced later
  if needed, but avoid it in the initial design to keep things simple.

### VariantDummy

- Implements the `Variant` trait with no networking.
- `connect` is a no-op (immediate success).
- `publish` writes to an in-process data structure (e.g. a `VecDeque`).
- `poll_receive` reads from that same structure, simulating instant local
  delivery. Since there is only one process (no real peers), the dummy
  "receives" its own writes. This is intentional — it exercises the full
  write/receive logging path.
- `disconnect` is a no-op.
- Does NOT override `signal_end_of_test` / `poll_peer_eots`; the trait
  defaults are sufficient because the dummy is only ever spawned in
  single-runner self-loopback configurations. In that mode the driver's
  expected-peer set (peers from `--peers` minus self) is empty, so the
  EOT phase exits immediately after a single `eot_sent` event with
  `eot_id=0` — no `eot_timeout` is logged.
- Ships as `variant-dummy` binary that the runner can spawn like any other
  variant.

### Test Protocol Driver

The driver is a function (not a trait) that takes a `&dyn Variant` (or
generic `impl Variant`) and the parsed CLI config, then runs:
1. Connect phase
2. Stabilize phase (sleep)
3. Operate phase (tick loop with workload)
4. EOT phase (signal end-of-test, wait for peer EOTs, bounded by
   `--eot-timeout-secs` which defaults to `max(3 * operate_secs, 30)`
   - see `driver::default_eot_timeout_secs`)
5. Silent phase (drain + flush)

### EOT default-timeout rationale (T-impl.3)

The default formula is `max(3 * operate_secs, 30)`:

- **`3 * operate_secs`**: at 100 K writes/s on hybrid TCP transports the
  in-flight backlog at end-of-operate can take roughly the operate-phase
  wall-clock to drain. A factor of three gives headroom for late
  deliveries from peers that fell behind without permitting an unbounded
  hang.
- **30-second floor**: replaces the previous 5-second floor. Short-operate
  fixture runs (e.g. `operate_secs = 1..=10`) still need a meaningful
  drain budget - five seconds was empirically too aggressive for
  cross-machine TCP variants where socket teardown alone can take a few
  seconds. The 30-second floor matches the default per-barrier coordination
  budget the runner uses on the other side of the spawn.
- The formula has a single source of truth at `driver::default_eot_timeout_secs`.
  The CLI struct docstring (`cli::CliArgs::eot_timeout_secs`) and the runner
  contract docs reference this helper rather than re-encoding the formula.

The driver owns the logger and calls it directly. Variants never touch
the logger — they only do transport work through the trait methods.

The EOT phase uses `Variant::signal_end_of_test` (called once at phase
start; logs `eot_sent`) and `Variant::poll_peer_eots` (polled every
~10 ms; logs `eot_received` per new (writer, eot_id), with a defensive
dedup-by-writer backstop on the driver side). The expected-peer set is
derived from the runner-injected `--peers` argument (which the driver
finds in `CliArgs::extra` via `cli::parse_peer_names_from_extra`) minus
the runner's own name. If the wait expires with peers still missing,
the driver logs a single `eot_timeout` event with the missing names —
the spawn does NOT abort.

### Self-pacing in max-throughput (T-impl.8)

The operate-phase loop runs differently for the two workload profiles:

- **`scalar-flood`**: an explicit `tick_interval` sleep paces the writer
  to `tick_rate_hz`. When `try_publish` returns `Ok(false)` the driver
  emits a `backpressure_skipped` event and moves on -- the inter-tick
  sleep is the sole back-off.
- **`max-throughput`**: the inter-tick sleep is removed so each
  transport's headline rate is measured. To keep the writer from
  spinning on `Ok(false)` and starving the receiver, the operate loop
  applies a two-tier back-off using a local `consecutive_skipped: u32`
  counter:
  1. **First consecutive `Ok(false)`**: log `backpressure_skipped`, then
     `std::thread::yield_now()`. No sleep -- the yield releases the
     timeslice (cost <100 us on Windows) and the receiver thread may
     run immediately. If it drains the queue, the very next
     `try_publish` returns `Ok(true)` and the counter resets to zero.
  2. **Second or later consecutive `Ok(false)`**: log
     `backpressure_skipped`, then
     `std::thread::sleep(Duration::from_millis(1))`. The sleep already
     releases the timeslice -- no additional yield.
  3. **`Ok(true)`** resets `consecutive_skipped = 0`, so the next
     transient `Ok(false)` returns to the yield path.

**Why yield first, then sleep?** A yield is essentially free; if the
receiver only needs to run for a few microseconds to drain one message,
yield is plenty and we get back to publishing immediately. A sleep is
much more expensive (especially on Windows). The first-skip yield is
the optimistic case ("the receiver was just briefly preempted, give it
a chance"); the second-skip sleep is the pessimistic case ("the queue
is genuinely full, give it a real interval to drain").

**Windows timer-granularity caveat.**
`std::thread::sleep(Duration::from_millis(1))` does not sleep for 1 ms
on Windows -- it sleeps for approximately one system tick, which is
~15.6 ms by default (or ~10 ms with `timeBeginPeriod(1)`, or ~1 ms only
if some other process has bumped the timer resolution). On Linux it's
~1 ms. **This is a feature, not a bug**: the longer sleep gives the
receiver substantial drain time on Windows, which is exactly the
back-off pressure we want under sustained backpressure. The
consequence is that under a saturated transport, the max-throughput
write-rate trace becomes sawtooth-shaped (long sleep, burst of
publishes, repeat); the *aggregate* throughput converges to the
sustainable rate of the transport. We deliberately do NOT call
`timeBeginPeriod(1)` -- it would affect the whole process and is a
documented thread-scheduling hazard.

**Scalar-flood is unchanged.** The new yield/sleep is gated on
`max_throughput == true`; under `scalar-flood` the back-off counter
is never touched and the existing inter-tick sleep is the sole pacing
mechanism. See `driver::run_protocol` and the
`scalar_flood_max_throughput_path_unchanged` unit test for the guard.

### Operate-loop drain budgets (T-impl.10)

The operate-phase per-iteration receive drain is bounded by two budgets:
a message-count cap and a wallclock cap. The wallclock cap is now
computed per-iteration by `compute_operate_drain_time_budget`:

- **`scalar-flood`**:
  `drain_time_budget = max(1ms, (next_tick - now) - 1ms safety margin)`.
  If we have already overrun the tick (`next_tick - now <= 1ms`), the
  formula falls back to the 1 ms floor so the drain does not compound
  the lateness. In practice this means the drain phase fills the slack
  between the publish burst and the next tick boundary.
- **`max-throughput`**: a flat `Duration::from_millis(5)`. There is no
  tick boundary to respect, but the drain still must not be unbounded
  -- the publisher needs to run again. Five milliseconds is empirically
  long enough to drain a substantial fraction of the recv buffer
  without starving the publish path.

The message-count cap was simultaneously bumped from `2 * values_per_tick`
to `4 * values_per_tick` (floor at 1). The doubled cap costs nothing
when buffers are small (the `Ok(None)` early-exit fires immediately)
and absorbs momentary bursts at high symmetric rates.

The EOT-phase drain retains the pre-T-impl.10 budgets (2 * vpt,
1 ms wallclock) -- the failure mode the new formula addresses only
manifests during operate-phase pacing.

**Why this change exists.** A two-runner `websocket-1000x100hz`
run (100 K msg/s symmetric) deadlocked ~130 ms into the operate
phase on 2026-05-11. The 1 ms drain wallclock was too tight for
transports with expensive per-message receive cost (websocket frame
parse + client-mask XOR): the recv buffer grew monotonically each
tick at 6:1 publish-vs-receive ratio until one side's TCP window
collapsed (`WSAECONNRESET`). Transports with cheap framing (hybrid
TCP) drained in time today but sat close to the cliff. The fix is
architectural -- in the driver, not in any one variant -- because
the symptom is general to high-rate symmetric workloads. See
`driver::compute_operate_drain_time_budget` and the four T-impl.10
unit tests (`scalar_flood_drain_msg_budget_is_four_x_vpt`,
`scalar_flood_drain_does_not_overrun_tick`,
`max_throughput_drain_bounded_to_five_ms`,
`empty_queue_drain_still_early_exits`).
