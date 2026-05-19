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
  Defines the lifecycle-only structured log format. Every line must
  include `ts`, `variant`, `runner`, `run`, `event`. Event types:
  `connected`, `phase`, `eot_sent`, `eot_received`, `eot_timeout`,
  `resource`. Per-event observations (`write`, `receive`,
  `backpressure_skipped`, `gap_*`) live in the sibling compact-Parquet
  file (see `metak-shared/api-contracts/compact-log-schema.md`) post-T19.10.

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

### Write timestamp capture (T16.2)

The driver's operate-phase loop captures `write_ts = Utc::now()`
**before** calling `variant.try_publish(...)`, then passes that
timestamp to `Logger::log_write_at(...)` after the publish returns
`Ok(true)`. The capture order is load-bearing on same-host benchmarks:
multi-mode reader threads (websocket-multi, hybrid-multi, custom-udp-multi)
share a single QPC-backed `Utc::now()` source machine-wide with the
peer's writer thread. If `write_ts` is captured AFTER `try_publish`
returns, the peer's reader thread can read the bytes off the loopback
socket and log `receive_ts` for ~50% of seqs **before** the writer
thread reaches `log_write`, producing `receive_ts < write_ts` on the
analysis side and violating the schema contract that
`receive_ts >= write_ts`. The `Ok(false)` (backpressure-skip) and
error paths intentionally do NOT reuse the pre-publish `write_ts` --
those events are correctly timestamped at-event-time, not
at-attempt-time. `Logger::log_write` is preserved as a thin wrapper
around `log_write_at(Utc::now(), ...)` for any non-driver caller that
does not need pre-publish ordering. See
`metak-shared/api-contracts/jsonl-log-schema.md` for the
externally-visible contract.

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

### Strict no-skip contract for QoS 3 / QoS 4 (T17.2)

Per `metak-shared/DESIGN.md` § 6.5 (and the § 6.6 summary table),
QoS 3 (Reliable-UDP) and QoS 4 (Reliable-TCP) prioritise **delivery
over throughput**. Variants MUST deliver 100% of accepted writes and
MUST NOT silently drop messages at the publish path; under sustained
overload the acceptable failure mode is throughput collapse, not
delivery shortfall.

The driver enforces this rule by branching the per-tick publish loop
on the QoS level (`driver::is_strict_delivery_qos`):

- **QoS 1 / QoS 2** -- unchanged. `try_publish` returning `Ok(false)`
  produces exactly one `backpressure_skipped` JSONL event; under
  `max-throughput` workload the two-tier self-pacing back-off (T-impl.8)
  applies.
- **QoS 3 / QoS 4** -- the driver loops on `try_publish` until it
  returns `Ok(true)` (or `Err`, which propagates). The first
  consecutive `Ok(false)` issues a `std::thread::yield_now()`; later
  consecutive `Ok(false)` results issue
  `std::thread::sleep(Duration::from_micros(100))`. Critically the
  driver does NOT emit `backpressure_skipped` at QoS 3/4: that event is
  restricted to QoS 1/2 by `metak-shared/api-contracts/jsonl-log-schema.md`
  and the analyzer (T17.9) flags any QoS 3/4 occurrence as a variant
  bug.

When a variant returns `Ok(false)` at QoS 3/4 (a contract violation
under DESIGN.md § 6.5), the driver emits the one-shot stderr line

```
QoS 3/4 contract violation: try_publish returned Ok(false); see DESIGN.md § 6.5
```

exactly once per spawn (guarded by a process-static `AtomicBool` that
the driver resets at the top of `run_protocol`). The warning surfaces
the misbehaviour to the operator without spamming stderr; the
analyzer's T17.9 check is the load-bearing detector.

**`max-throughput` + QoS 3/4 is rejected at startup.** The
`max-throughput` workload removes the inter-tick sleep and relies on
`Ok(false)` skips to self-pace; QoS 3/4 forbids skips by contract.
The combination produces an unbounded busy loop with no throttle, so
the driver returns an `Err` from `run_protocol` BEFORE any
phase/logger emission. Use `scalar-flood` with QoS 3/4, or QoS 1/2
with `max-throughput`.

Concrete variants enforce no-skip by blocking inside their own
`try_publish` at QoS 3/4 (kernel TCP back-pressure, bounded
application-level queues, peer-acknowledged credit windows, etc.) --
see T17.3..T17.8 for the per-variant implementations.

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

### Threading-mode dimension (T14.1)

The `Variant` trait now carries a `ThreadingMode { Single, Multi }`
dimension. The runner injects `--threading-mode` (E14, see
`metak-shared/api-contracts/variant-cli.md` "E14 additions"); the
driver passes the chosen mode to `Variant::connect` and to a pair of
new lifecycle hooks. Each variant decides what the mode means inside
its own implementation. T14.1 ships the trait surface, CLI plumbing,
JSONL field, and a `[Single, Multi]`-capable `VariantDummy`;
T14.2-T14.7 add real `Multi` support per variant; T14.8 teaches the
runner to inject the arg and expand the `threading_modes` config
dimension.

**Trait surface (in `variant_trait.rs`)**

- `fn supported_threading_modes(&self) -> &'static [ThreadingMode]`
  -- default `&[ThreadingMode::Single]`. Variants override to declare
  multi-mode support.
- `fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()>`
  -- the breaking signature change. Variants that don't branch on mode
  may accept it and ignore.
- `fn start_reader_threads(&mut self, mode: ThreadingMode) -> Result<()>`
  -- default no-op. Called by the driver immediately AFTER
  `connect` returns Ok. Variants that spawn per-peer reader threads
  do it here, not inside `connect`, so the `connected` event is
  emitted before any reader thread starts running.
- `fn stop_reader_threads(&mut self) -> Result<()>` -- default
  no-op. Called by the driver BEFORE `disconnect`, so reader threads
  can drain pending receives cleanly before the transport tears down.

**Driver wiring (in `driver::run_protocol`)**

The connect path now reads:

```
variant.connect(config.threading_mode)?;
variant.start_reader_threads(config.threading_mode)?;
logger.log_connected(..., threading_mode, recv_buffer_kb)?;
```

and the disconnect path reads:

```
variant.stop_reader_threads()?;
variant.disconnect()?;
```

If a variant supports only Single mode, both hooks remain default
no-ops and the driver behaviour is identical to pre-E14.

**CLI args (in `cli::CliArgs`)**

- `--threading-mode <single|multi>` -- parsed via `FromStr` on
  `ThreadingMode`. Defaults to `single` during the E14 rollout
  because T14.1 ships before T14.8: the runner does not yet inject
  the arg, and the default keeps existing runner integration tests
  passing without modification. Once T14.8 lands, the runner always
  injects it and the default becomes a fallback for ad-hoc manual
  invocations.
- `--recv-buffer-kb <u32>` -- optional, default `4096` (4 MiB),
  range `64..=65536` (64 KiB to 64 MiB). Variants apply
  `setsockopt(SO_RCVBUF, recv_buffer_kb * 1024)` on every recv-side
  socket they own. Async-only variants whose transport library hides
  the socket may treat this as advisory but must still record the
  value in the `connected` event.

**JSONL impact**

The `connected` event gains two fields, `threading_mode` and
`recv_buffer_kb`. Both are optional during the E14 rollout (pre-T14.8
logs may omit them) and become required once T14.8 lands. See
`metak-shared/api-contracts/jsonl-log-schema.md`.

**`VariantDummy` capabilities**

`VariantDummy` overrides `supported_threading_modes()` to return
`&[ThreadingMode::Single, ThreadingMode::Multi]`. The dummy has no
real I/O, so both modes do the same thing internally; the point is
to exercise the new threading-mode infrastructure end-to-end (in
unit tests, integration tests, and runner smoke runs) regardless of
which mode the runner picks. The dummy records the mode it was
asked for so tests can confirm propagation -- this is a test hook,
not part of the trait surface.

**Open concern for T14.2 follow-up**

`stop_reader_threads` is called BEFORE `disconnect` deliberately so
reader threads can drain in-flight messages from the underlying
sockets before those sockets close. A real reader thread will be
blocking inside `WebSocket::read_message` (or the equivalent) at the
moment `stop_reader_threads` runs; the natural way to signal it to
wake up and exit is to either (a) set an `AtomicBool` and rely on a
short `SO_RCVTIMEO` to surface a periodic poll, or (b) shutdown the
socket on the variant side first. The trait does not prescribe a
mechanism -- T14.2 (websocket) will set the precedent. The current
ordering is intentional: variants that need to issue a peer-side
shutdown (e.g. send a close frame) want a still-live transport to
do it from, so `stop_reader_threads` cannot itself tear the socket
down.

### Internal-stall watchdog (T15.11)

A second variant-side monitor thread complements the T15.5 inline
idle detector. The inline detector lives inside the operate loop and
fires only when the driver thread reaches each iteration's bookkeeping
check; if the driver thread is blocked inside a transport library
call (the Zenoh qos3/qos4-multi failure mode under symmetric flood),
the inline detector never runs. The watchdog runs on its OWN OS
thread, polls the same `sent` / `received` counters the
`ProgressEmitter` already maintains, and converts that wedged-driver
shape from "runner kills the variant + truncated JSONL" to "variant
self-exits cleanly + flushed JSONL".

**Module**: `variant-base/src/watchdog.rs`.

**Behaviour**:

- Watches `ProgressEmitter`'s shared `(sent, received, phase)` state
  via a cloned `Arc<ProgressState>`. No second bookkeeping path.
- Wakes once per second. Captures a snapshot.
- Gating: only fires when `phase == "operate"`. Outside operate, the
  threshold timer is anchored to "now" each tick so a stabilize or
  silent phase with frozen counters does not accumulate threshold.
- Fire condition: BOTH `sent` and `received` unchanged for
  `watchdog_secs` consecutive seconds during operate phase.
- On fire: invoke the injected `on_fire` callback (the driver binds
  this to `logger.flush()` -- see "Flush before exit" below), write
  the line `[variant] watchdog: no progress in <N>s during operate
  phase -- internal stall; self-exiting` to stderr, then call
  `std::process::exit(WATCHDOG_EXIT_CODE)`.

**CLI arg**: `--watchdog-secs <u32>` (default `30`, `0` disables).
The default is chosen to win the race against typical runner
safety-net deadlines: the existing stress fixtures set
`default_timeout_secs = 60`, and the watchdog must fire well before
that to leave the JSONL flush time to complete. Thirty seconds is
far longer than any cooperative phase budget in the existing fixture
set (stabilize + operate + silent ~12 s) yet comfortably under the
shortest reasonable runner deadline. Operators running fixtures
with longer per-spawn timeouts may raise this. When `0`, no thread
is spawned and `Watchdog::is_enabled()` returns false.

**Exit-code choice**: `WATCHDOG_EXIT_CODE = 2`.

- `0` is reserved for clean success.
- `1` is the `eprintln + exit(1)` path in `variant_dummy.rs::main`'s
  top-level error handler (and the same pattern in every other
  variant binary). The watchdog must produce a DIFFERENT code so
  the analysis classifier can disambiguate watchdog self-exits from
  generic error exits without having to parse stderr to confirm.
- `2` is the conventional "command-line misuse" code in many Unix
  tools, but the variant binaries do not currently use it for
  anything else. Reusing it for the watchdog keeps the meaningful
  exit-code space small and matches the T15.11 task recommendation.

The analysis pipeline (`analysis/timeout_classification.py`)
substring-searches the stderr capture for `watchdog: no progress`
to classify the row as `variant_self_killed_idle`. It does NOT key
on the exit code today -- the stderr signature is the load-bearing
signal -- so the exit code is an aid for operators reading the raw
runner output, not part of the classifier's contract.

**Flush before exit**: `std::process::exit` does NOT run
destructors. The variant's `Logger` wraps a `BufWriter<File>` and
has no `Drop` impl that would flush. Without an explicit flush, the
watchdog would produce the EXACT truncation shape it is supposed
to eliminate. The driver binds the watchdog's `on_fire` callback to
`logger_handle.lock().flush()` so the JSONL is fully written to
disk before the process exits. The watchdog module is intentionally
decoupled from the logger -- it sees only a `FnMut()` callback --
so logger evolution does not propagate into the monitor thread.

**Why a second monitor thread instead of a flag-and-park approach
on the driver thread?** A flag-and-park approach can only fire when
the driver thread runs. The whole point of T15.11 is to detect the
case where the driver thread has stopped running. A separate OS
thread is the smallest correct design.

**Why not Tokio / async?** The variant-base crate is sync-first
(see "Variant Trait" above). Adding a runtime just for the watchdog
would be disproportionate; `std::thread::Builder::new().spawn(...)`
with a 1 Hz sleep loop is sufficient and matches the existing
`ProgressEmitter` thread's style.

**Limitations -- crashes vs stalls.** The watchdog only converts
STALLS (the driver thread blocked inside a library call). If the
transport library panics or aborts the process within the
watchdog's threshold (observed empirically on Zenoh qos3 multi
under sustained flood, where Zenoh occasionally aborts via
`STATUS_CONTROL_C_EXIT` / `None`-from-`status.code()` within ~2 s
of operate-phase start), the watchdog has no chance to fire. The
analysis pipeline still distinguishes the two outcomes: a crash
keeps the legacy `deadlock` classification (no `eot_sent` + no
`phase=silent` + stderr lacking the watchdog substring), a stall
becomes `variant_self_killed_idle`. Both still land as "not
completed" warnings, but the operator can read the JSONL cleanness
and stderr signature to tell which library failure mode produced
the row.

### Compact-log Parquet output (T18.1 + T18.2 / E18)

E18 attacks the 60+ GB log volumes that a 100 K msg/s x 30 s x 200
spawn campaign would otherwise produce with per-event JSONL. The
target is a 30-50x reduction with no loss of the rows the analysis
pipeline actually consumes.

**Two new modules** in `variant-base/src/`:

- `compact.rs` -- in-memory columnar event buffers
  (`CompactBuffers`) with lazy interning of paths (`u32` indices,
  capped at `u32::MAX`) and peer/writer names (`u8` indices, capped
  at `MAX_PEERS = 254` so `u8::MAX = 255` is the `PEER_SELF`
  sentinel). One row per per-event observation across seven
  parallel `Vec`s: `ts_ns: i64`, `kind: u8`, `seq: u64`,
  `path_idx: u32`, `peer_idx: u8`, `qos: u8`, `bytes: u32`.
  `EventKind` discriminants are pinned (Write=0, Receive=1,
  BackpressureSkipped=2, GapDetected=3, GapFilled=4) and form part
  of the on-disk Parquet wire format -- renumbering would break
  analysis that has already parsed previous files.

- `compact_writer.rs` -- serialises a `CompactBuffers` instance to
  `<log_dir>/<variant>-<runner>-<run>.compact.parquet` via the
  `parquet = "53"` crate. One row group, seven primitive columns
  (small unsigned fields widened to the smallest signed Parquet
  type that holds them losslessly), snappy compression by default
  (zstd-3 buys ~5% file-size at ~3x CPU cost; not worth it for the
  digest-phase budget). Intern dictionaries + spawn identifiers go
  into the file's KV metadata block so the analysis tool can
  decode `path_idx` / `peer_idx` back to strings without a
  side-car file.

**Digest phase** (the new `Phase::Digest`): runs after `Silent` and
after `Variant::disconnect`. The driver emits one `phase=digest`
JSONL marker, writes the Parquet file, prints a single
`[variant] digest: wrote <path> (<rows>, <bytes>)` line to stderr
for operator visibility, then flushes and exits. The marker is
how the runner distinguishes a clean run-with-compact-output from
one that died before it could finalise.

**Single-source EventSink** (T19.10): per-event observations
(`write`, `receive`, `backpressure_skipped`, `gap_*`) are pushed
exclusively into the compact buffers — there is no dual-emission gate
and no `--legacy-jsonl-events` flag. The driver's lifecycle events
(`phase`, `connected`, `eot_sent`, `eot_received`, `eot_timeout`,
`resource`) flow directly to the JSONL stream because they are
low-volume and the runner consumes them out-of-band (E15 progress
streaming, T11.5 analysis markers); the same lifecycle rows are also
mirrored into the compact buffers (T18.2b) so an analyzer that reads
only the compact-Parquet file has full coverage. Per-event JSONL
emission was removed in the E19 follow-up cleanup; old pre-T18.2
JSONL datasets that carry per-event rows are no longer supported.

**Memory ceilings** (`--digest-mem-soft-mb` / `--digest-mem-hard-mb`,
defaults 1024 / 2048):

- Soft ceiling: once `CompactBuffers::approx_bytes()` exceeds the
  threshold, the driver prints one stderr warning per spawn and
  continues. A sticky once-flag inside the `EventSink` prevents
  the warning from oscillating if the footprint hovers around the
  threshold.
- Hard ceiling: once exceeded, the operate loop returns an error
  with a descriptive message naming both the current footprint and
  the threshold. The spawn aborts cleanly; the JSONL has the full
  phase trail up to the abort point.
- The check fires **once per outer iteration** of the operate
  loop, not per drained message -- the `approx_bytes` math is
  cheap, but keeping it off the hot path is free.

**Why snappy over zstd.** Both codecs are supported by the `parquet`
crate (the `compression` field on `CompactWriterOptions` is the
single switch). Snappy was chosen because:

1. The dominant columns are mostly non-redundant numerics: `seq` is
   strictly increasing, `ts_ns` is approximately so, `path_idx` and
   `peer_idx` cycle through small dictionaries. Snappy LZ77 picks
   up the dictionary repetition; zstd's entropy coder adds at most
   ~5% over that on this column shape.
2. Digest runs inside the spawn budget -- a 3x CPU saving on the
   encode path is more valuable than ~5% smaller files. Empirically
   on a 2 s scalar-flood at 1000 Hz x 100 vpt the digest write
   finishes in 200..500 ms with snappy versus 800 ms..1.5 s with
   zstd-3.
3. The cross-task analysis reader is happy with either codec (the
   `parquet` crate's reader auto-detects); changing the codec later
   does not break older files.

**Acceptance evidence.** The new `test_compact_parquet_at_least_10x_smaller_than_jsonl`
integration test runs a 2 s scalar-flood at 1000 Hz x 100 vpt =
~400 K events, produces both files, asserts the Parquet file is at
least 10x smaller than the JSONL (10x is conservative slack on the
30-50x epic target). Local Windows measurement: jsonl ~15.1 MB vs
parquet ~930 KB -- 16.3x. Release mode is expected to push this
toward the upper end of the 10-30x range as the JSONL serialisation
becomes the bottleneck rather than the Parquet writer.

### Workload-shape dimension (T19.2 + T19.3 / E19)

E19 introduces two new workload profiles alongside `scalar-flood` and
`max-throughput`:

- `block-flood` — emits `vpt / blob_size` WriteOps per tick, each
  carrying a `blob_size`-element block of scalars. Stresses the
  serialization path and large-message transport handling.
- `mixed-types` — emits a heterogeneous mix of scalar / array /
  nested-struct WriteOps per tick, summing to exactly `vpt` total
  leaves. Stresses the full serialization path including nested
  `KeyValue` structures.

**`WriteOp` extension** (`workload.rs`): the existing
`{ path, payload }` tuple gains:

- `leaf_count: u32` — number of scalar leaves in `payload`. Scalar =
  1; array = N; struct = total leaves in the tree.
- `shape: WriteShape` — enum `{ Scalar, Array, Struct }`.

**Logger emission**: every `write` row (compact Parquet) carries
`leaf_count` and `shape`. Defaults `leaf_count = 1, shape = Scalar`
for backward compat with `scalar-flood` and `max-throughput`. There
is no JSONL emission for the `write` event post-T19.10.

**No trait-surface change**: the `Variant` trait's
`publish(path, &[u8], qos, seq)` signature is unchanged. Payloads
remain opaque bytes; the wire layer doesn't know or care about leaf
structure. The `leaf_count` / `shape` metadata lives only on the log
events.

**Receive side**: receivers do NOT log `leaf_count` / `shape`. The
analyzer correlates receives with their matching write event by
`(writer, seq, path)` and inherits the metadata from the write side.
This keeps receive-side handling minimal and consistent with the
opaque-blob wire model.

**Latency canonical unit**: per-WriteOp everywhere. Block-flood /
mixed-types report one latency sample per published block (one
timestamp pair per `try_publish` call). Scalar-flood preserves its
per-leaf granularity as a coincidence — its WriteOps each carry one
leaf, so 1 op = 1 leaf.

**Mixed-types RNG determinism**: the new `--workload-seed` arg
(optional) seeds the generator deterministically. When omitted, the
seed is derived from `--variant + --run` so re-runs with identical
config produce identical workload sequences. Use the `rand` crate's
`StdRng`.

**Mixed-types termination guarantee**: recursion depth is bounded at
`log_2(vpt) + 4`; if the bound is reached during dict-tree expansion,
the remaining leaves are emitted as a flat dict at that level. This
prevents pathological infinite recursion when the random branching
factor repeatedly picks 1.

**Validation at startup** (in `run_protocol`): the driver checks
workload-param constraints (see
`metak-shared/api-contracts/variant-cli.md` E19 additions) and returns
Err with a descriptive message BEFORE any phase event if the
constraints are violated. The `max-throughput + qos 3/4` rejection
pattern (T17.2) is the template.
