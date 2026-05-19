# runner Custom Instructions

## Overview

Rust binary that coordinates benchmark execution across machines. One runner
instance per machine, all given the same TOML config file. Runners discover
each other, verify config integrity, and progress through variants in
lockstep using barrier synchronization.

The runner has **no IPC** with variant processes — it only spawns them with
CLI arguments, monitors for exit (or timeout), and records the result.

## Tech Stack and Conventions

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`runner`)
- **Key dependencies**:
  - `clap` (derive style) — CLI: `runner --name <name> --config <path.toml>`
  - `toml`, `serde`, `serde_json` — config parsing + coordination messages
  - `sha2` — SHA-256 hash of config file for cross-runner verification
  - `chrono` — RFC 3339 timestamps for `--launch-ts`
  - `anyhow` — error handling
  - `socket2` — UDP broadcast socket configuration
  - `local-ip-address` — enumerate local interface IPs for same-host
    detection during peer capture (added in E9)
- **Do NOT depend on `variant-base`**. The runner knows nothing about variant
  internals. It treats variants as opaque child processes. The interface is
  defined by the CLI contract and the TOML config.
- Follow `metak-shared/coding-standards.md` for Rust conventions.

## Build and Test

All commands run from the repo root; this is a Cargo workspace. Do **not**
`cd` into the `runner/` subfolder to build — that produces a stray
`runner/target/` directory which nothing else points at and which causes
stale-binary skew on multi-machine runs.

```
cargo build --release -p runner            # build runner binary
cargo test --release -p runner             # unit + integration tests
cargo clippy --release -p runner -- -D warnings   # lint
cargo fmt -p runner -- --check             # format check
```

Integration tests use `variant-dummy` from the `variant-base` crate. The
binary must be built first (also workspace-rooted):

```
cargo build --release -p variant-base
```

The compiled binary lives at `target/release/variant-dummy(.exe)`. The path
to `variant-dummy` in test configs is relative to the runner's CWD (the
repo root when invoking via `cargo test`).

## Integration Contracts

The runner implements three API contracts:

- **TOML config schema**: `metak-shared/api-contracts/toml-config-schema.md`
  The config file defines runners, variants, timeouts, and all variant args.

- **Variant CLI contract**: `metak-shared/api-contracts/variant-cli.md`
  Defines how the runner constructs CLI arguments for spawning variants.
  Key conversion: TOML `snake_case` keys become `--kebab-case` CLI args.
  Runner injects: `--launch-ts`, `--variant`, `--runner`, `--run`.

- **Runner coordination protocol**: `metak-shared/api-contracts/runner-coordination.md`
  UDP broadcast discovery with config-hash verification, per-variant
  ready/done barrier sync.

## Architecture

```
runner/
  src/
    main.rs          -- entry point: parse CLI, load config, run main loop
    config.rs        -- TOML config struct, parsing, validation, config hash
    cli_args.rs      -- construct variant CLI args from config sections
    spawn.rs         -- child process spawning, monitoring, timeout, exit code
    protocol.rs      -- UDP coordination: discovery, ready barrier, done barrier
    message.rs       -- coordination message types (JSON over UDP broadcast)
  tests/
    integration.rs   -- full lifecycle with variant-dummy, single and multi-runner
  Cargo.toml
```

## Design Guidance

### Single-runner mode

When `runners` contains only one name (this runner), coordination is
trivial: discovery completes immediately, barriers are no-ops. This is the
primary testing mode and should be optimized for. All integration tests
should work in single-runner mode.

### Coordination protocol messages

Use JSON over UDP broadcast. Simple, debuggable, and good enough for a
coordination protocol that runs once per variant (not in the hot path).

```json
{"type":"discover","name":"a","config_hash":"abc123..."}
{"type":"ready","name":"a","variant":"zenoh-replication"}
{"type":"done","name":"a","variant":"zenoh-replication","status":"success","exit_code":0}
```

Periodic re-broadcast every 500ms until all peers have responded. This
handles UDP packet loss without adding complexity.

### CLI arg construction

The runner reads `[variant.common]` and `[variant.specific]` as arbitrary
TOML tables. For each key-value pair, it converts `snake_case` to
`--kebab-case` and formats the value as a string. Then appends the
runner-injected args:
- `--launch-ts <RFC3339>` (recorded just before spawn)
- `--variant <name>` (from `[[variant]].name`)
- `--runner <name>` (from `--name` CLI arg)
- `--run <id>` (from top-level `run` field)

### Timeout handling

When a variant exceeds its timeout:
1. Send SIGTERM (or equivalent on Windows: `TerminateProcess`)
2. Wait briefly (1-2 seconds) for graceful shutdown
3. Send SIGKILL if still alive
4. Record status as "timeout" in the done barrier

### Failure diagnostics on non-success spawn (T-impl.9)

After the status line `'<name>' finished: status=<failed|timeout>, exit_code=<n>`,
the runner prints a post-mortem block to its own stderr so an operator can
diagnose without scavenging the logs directory. Layout:

```
[runner:<name>] stderr capture: <abs path>
[runner:<name>] jsonl log:      <abs path>           # only when file exists
[runner:<name>] ---- stderr tail (last 20 lines) ----
<up to 20 lines of the capture, sourced from the last <= 64 KiB of the file>
[runner:<name>] ---- end stderr tail ----
```

If the capture file is empty (the common Windows TerminateProcess case
where the child was killed before flushing anything) the bracketed
tail block is replaced by:

```
[runner:<name>] (stderr capture is empty -- child likely killed before writing any output)
```

Successful and skipped (resume mode) spawns stay silent. The existing
status line is preserved verbatim -- the block ADDS context, never
replaces output.

Implementation lives in `print_failure_diagnostics` in `main.rs`, layered
over two helpers in `spawn.rs`:

- `spawn::jsonl_log_path` -- builds `<log_subdir>/<effective_name>-<runner_name>-<run>.jsonl`
  per `metak-shared/api-contracts/jsonl-log-schema.md`.
- `spawn::tail_stderr_file` -- bounded read of the last 20 lines / 64 KiB
  of the capture file. Returns `Ok(None)` only for a missing file (skip
  silently), `Ok(Some(""))` for an empty file (triggers the notice line),
  `Ok(Some(content))` otherwise.

Motivating incident: `configs/two-runner-websocket-qos4.toml` on bob hit
the 60s timeout, was TerminateProcess'd before flushing stderr, and the
runner's only output was the lone status line. With T-impl.9 the same
run now points the operator at the capture file + JSONL log inline.

### Per-spawn stderr capture

Every variant child's stderr is redirected to a per-spawn file so panic /
abort / OS-error messages survive even when the JSONL log was truncated
mid-write. The capture file lives next to the variant's JSONL log:

```
<log_subdir>/<effective_name>-<runner_name>-stderr.txt
```

Where `<log_subdir>` is the absolute path the variant's logs go into
(`log_dir_resolved` when the variant declared its own `log_dir`, otherwise
the run-level `run_log_dir`), `<effective_name>` is the spawn's synthesized
name (e.g. `myvariant`, `myvariant-qos3`, `v-1000x100hz-qos2`), and
`<runner_name>` is this runner's `--name`.

Semantics:

- **Truncate on every spawn.** The file is opened with create-or-truncate
  before `Command::spawn`, so a `--resume` re-spawn of the same
  `(variant, runner)` cleanly overwrites the previous attempt. The opening
  happens *before* the child is spawned, which guarantees the file exists
  on disk even if the child is killed mid-write during a timeout — nothing
  the child does can prevent file creation.
- **`Stdio::from(File::create(...))`.** The OS writes child stderr directly
  to the file. No intermediate reader thread, no deadlock risk on child
  closure, no flush from the runner needed — the kernel flushes on child
  exit or kill.
- **Runner's own stderr is untouched.** Only the variant child's stderr is
  redirected. Operators still see runner-side panics, FATAL lines, and
  coordination tracing on the runner's terminal.

Use these files first when investigating "why did the variant crash under
load?" questions — they catch the messages the JSONL log can't (panics
after the writer buffer was discarded, allocator aborts, signal-level
faults).

### Peer host capture (E9 Part A)

The discovery loop must use `recv_from` so the source `SocketAddr` of every
inbound `Discover` message is available. The runner stores
`peer_hosts: HashMap<String, String>` keyed by runner name and populated
during Phase 1.

Same-host classification: a captured source IP that is in this machine's
local interface set OR is `127.0.0.1` is stored as the literal string
`"127.0.0.1"`. Anything else is stored as the source IP's `to_string()`
form. Local interface enumeration lives in `src/local_addrs.rs`
(`local_interface_ips() -> HashSet<IpAddr>`, cached on first call).

Discovery completes only when every name in `runners` has an entry in
`peer_hosts`. Single-runner mode self-populates with `127.0.0.1`.

The map is then injected into every variant spawn as
`--peers <name1>=<host1>,<name2>=<host2>,...` (sorted by name for
determinism). See `metak-shared/api-contracts/variant-cli.md`.

### Clock synchronization (E8)

Cross-machine latency measurement requires correcting `receive_ts -
write_ts` for inter-machine clock skew. The runner — not variants — is
responsible for measuring pairwise offsets and writing them to a sibling
JSONL file.

Full protocol: `metak-shared/api-contracts/clock-sync.md`. Summary:

- NTP-style 4-timestamp exchange (t1 send, t2 peer-receive, t3 peer-reply,
  t4 receive). N=32 samples per peer. Pick the sample with smallest RTT.
  Compute `offset = ((t2 - t1) + (t3 - t4)) / 2` (peer.clock − self.clock).
- Inter-sample delay: 5 ms. Per-sample timeout: 100 ms.
- Reuses the existing UDP coordination socket. Two new `Message` variants:
  `ProbeRequest { from, to, id, t1 }` and
  `ProbeResponse { from, to, id, t1, t2, t3 }`. Probe traffic is filtered
  by `to` so each probe targets exactly one peer.
- Probe responses must be sent promptly even while the runner is in a
  barrier loop. Add probe handling alongside the barrier loops in
  `protocol.rs`.

Run the protocol twice per variant cycle:
1. **Initial sync** — once after discovery completes, before the first
   ready barrier. Logged with `variant=""`.
2. **Per-variant resync** — after each variant's ready barrier, before
   spawning. Logged with `variant=<name>`.

Output: `<runner>-clock-sync-<run>.jsonl` in the same directory as variant
log files. One JSONL line per (peer, measurement_event). Schema in
`metak-shared/api-contracts/jsonl-log-schema.md` (`clock_sync` event).

Single-runner runs skip clock sync entirely (no peers).

### QoS expansion (E9 Part B)

`[variant.common].qos` is now optional and may also be an array. Parse it
via a `QosSpec` enum:

```rust
enum QosSpec {
    Single(u8),     // qos = N
    Multi(Vec<u8>), // qos = [..]
    All,            // qos omitted
}
```

`QosSpec::levels()` returns the concrete QoS levels to run, in ascending
order, deduplicated. `All` expands to `[1, 2, 3, 4]`.

The main loop expands each `[[variant]]` entry into one or more "spawn
jobs". Each job has a synthesized `effective_name`: the original `name`
when there's only one level, or `<name>-qosN` when there are multiple.
The job's `--variant` arg, ready/done barrier identifier, and JSONL log
filename all use `effective_name`.

Spawn jobs from one source entry run sequentially in ascending QoS order
with a small inter-job grace period (default 250 ms, configurable via
top-level `inter_qos_grace_ms`) to let TCP/UDP sockets release before the
next QoS spawn binds the same port. Then move on to the next source entry.

The runner does NOT manipulate ports inside `[variant.specific]`. Variants
that need QoS-disjoint ports use the `base_port + runner_index * runner_stride + (qos - 1) * qos_stride`
convention documented in `metak-shared/api-contracts/toml-config-schema.md`,
which they compute themselves from `--peers`, `--runner`, and `--qos`.

### Resume mode (`--resume`)

Optional CLI flag on the runner. Lets an interrupted multi-machine
benchmark be picked up where it left off without redoing completed
spawns.

Behavior:

1. **Resume is all-or-nothing across runners.** The flag's value is
   carried in the discover message (`resume: bool` field). If runners
   disagree, the run aborts during discovery.

2. **Log subfolder selection.** Instead of generating
   `<run>-<now-ts>`, each runner picks the lexicographically greatest
   subfolder under its resolved `<base_log_dir>/` that starts with
   `<bench_config.run>-`. That selection is the runner's discover
   `log_subdir` proposal. The leader's proposal still wins. Followers
   must have a folder of that exact name on disk; if not, abort.
   If a runner finds no matching folder locally, abort before sending
   discover.

3. **Phase 1.25: ResumeManifest exchange.** A new coordination phase
   runs after discovery and before initial clock sync (only when
   `resume = true`). Each runner inventories its local log files
   against the spawn-job expansion of the current config:
   - `<log_subdir>/<effective_name>-<self_name>-<run>.jsonl`
     non-empty → include in `complete_jobs`.
   - Empty file → delete it now (crashed prior attempt) and exclude.
   Each runner broadcasts its `ResumeManifest`, waits for one from
   every peer (periodic re-broadcast every 500 ms, same loss-recovery
   pattern as discovery), then computes the intersection: the "skip
   set" is the set of `effective_name`s that appear in every peer's
   manifest. For incomplete jobs, every runner deletes its own
   matching log file (any size) before proceeding.

4. **Phase 2 with skips.** For each spawn job, if its
   `effective_name` is in the skip set, bypass ready barrier, spawn,
   per-variant clock resync, and done barrier. Emit one informational
   line. Otherwise run normally. The skip set is a property of the
   run, not per-runner — the network exchange guarantees consistency.

5. **Clock-sync logging.** Always re-run clock sync (initial and
   per-variant) in resume mode. Open `<runner>-clock-sync-<run>.jsonl`
   in append mode so prior measurements are preserved.

6. **Single-runner resume.** Same logic; the manifest exchange becomes
   a no-op (intersection over a single set = the set itself). Empty
   self-files still get deleted.

Reference contract: `metak-shared/api-contracts/runner-coordination.md`
(Phase 1 updates, Phase 1.25 added, Phase 2 skip rule, ResumeManifest
message format).

### Coordination barrier timeouts (T-coord.2)

The post-discovery coordination barriers (ready, done, Phase 1.25
ResumeManifest) wait on UDP messages from peers. A peer crashing mid-run
would otherwise leave every other runner blocked forever. Each barrier
honours a per-call timeout configured via `--barrier-timeout-secs`
(default **120 s**); when the timeout fires the runner exits with code
**75 (`EX_TEMPFAIL` from `<sysexits.h>`)** and a single descriptive
stderr line so the wrapper script can detect the case and re-launch with
`--resume`.

Why these specific choices:

- **120 s default.** Long enough to absorb realistic worst-case variant
  cleanup (zenoh shutdown can take ~30 s under load; some QUIC
  configurations stall up to 60 s in the linger-and-flush path before a
  Done message is broadcast) without papering over a true peer death.
  Anything shorter would falsely trip during normal high-load runs;
  anything longer would defeat the point on small fixtures. Configurable
  via `--barrier-timeout-secs` for stress tests, CI, and slow-LAN
  scenarios.
- **Exit 75.** `EX_TEMPFAIL` is the standardised "service unavailable,
  retry later" code from BSD `sysexits.h`. Picked specifically because
  (a) it is unambiguous about the retry intent, (b) it does not collide
  with any code variant binaries currently produce (variants use 0/1/2),
  and (c) it gives the wrapper script a single, clear signal to gate the
  re-launch on. Any other non-zero exit (panic, config error, variant
  failure, child timeout) propagates as-is and stops the wrapper loop.
- **Discovery is excluded.** A stuck discovery means mismatched runner
  names, blocked UDP multicast, or hardware NIC offline — none of which
  retrying with `--resume` can fix. Discovery already has its own
  loss-recovery loop (re-broadcast every 500 ms) that handles transient
  packet drops; if a peer never appears the operator must intervene.
  Keeping discovery un-timed-out also means the wrapper script will not
  spin on a config typo.
- **Clock-resync is implicitly bounded.** `ClockSyncEngine::measure_one`
  sends `N=32` probes with a 100 ms per-sample timeout each — at most
  ~3.4 s per peer. We do not wrap a separate timeout around it; if a
  resync produces zero samples for some peer, that is a soft warning
  (the most recent successful initial-sync measurement remains
  available). The fail-fast on the *initial* sync is unchanged from
  T8.5.
- **No self-exec / auto-restart inside the runner process.** The runner
  exits cleanly on timeout and lets the wrapper handle the loop. This
  keeps the runner's state machine simple and allows operators to set
  the wrapper's retry-attempt cap independently.

Wrapper scripts live at `scripts/runner-resume.{sh,ps1}`. They re-launch
the runner with `--resume` appended ONLY on exit 75; every other exit
propagates immediately. The PowerShell wrapper is written for
Windows PowerShell 5.1 (no `??` / ternary / `?.`). Manual smoke-test
recipes are in `scripts/README.md`.

In-flight child cleanup on timeout: the spawn-and-monitor loop is
synchronous, so by the time a barrier is being held the variant child
is always either not yet spawned (ready barrier) or already collected
(done barrier). There is no orphan to kill on timeout exit. If this
ever changes (e.g. async-spawn refactor), revisit the cleanup path in
`main::run` where the `BarrierTimeoutError` is caught.

### Late-arriving discovery handling (T-coord.3)

A non-leader that joins late — leader has already exited its discovery
linger and advanced into a Phase 2 barrier — can populate its `seen`
set via `Ready`/`Done`/`ResumeManifest` from the leader without ever
seeing the leader's `Discover`, leaving `leader_log_subdir = None` at
the discovery exit point. Two cooperating mechanisms cover this:

- **`Coordinator::last_log_subdir`** caches the agreed `log_subdir`
  once `discover()` succeeds (every runner — leader writes its own
  proposal, non-leaders write the leader's proposal). Pre-populated
  in single-runner mode at construction; otherwise stays `None` until
  `discover()` returns.
- **`Coordinator::maybe_reemit_discover`** broadcasts our cached
  `Discover` (with the agreed `log_subdir`) when invoked. It is
  invoked from a `Some(Message::Discover { name, .. })` arm in
  `ready_barrier`, `done_barrier`, and `exchange_resume_manifest`,
  gated on `self.expected.contains(&name)`. Best-effort: send errors
  are swallowed because the active barrier must not be aborted by a
  transient network failure.
- **`Coordinator::discover` late-recovery loop**: once
  `seen == expected && hosts_known` becomes true but
  `leader_log_subdir.is_none()`, the call keeps broadcasting
  `Discover` and reading inbound for up to **30 seconds** (constant
  `LATE_DISCOVER_RECOVERY_BUDGET`) before bailing with a clear
  message. With the `maybe_reemit_discover` rule active on every
  runner, the leader answers within one re-broadcast cycle (~500 ms)
  and the loop terminates immediately after.

The 30-second budget is internal to `discover()` and **not** the
external `--barrier-timeout-secs` budget — discovery as a whole is
still exempt from external timeouts (a stuck discovery is a config /
firewall problem; auto-resume cannot fix it). The 30 s value is
calibrated for the realistic case where the leader is in a long
variant's `done_barrier` linger plus `clock_resync` plus the next
variant's ready barrier broadcast — comfortably above one full
post-discovery barrier cycle but below any sane operator's "is this
hung?" patience threshold. If a future workload routinely triggers
the bail!, raise the constant or add a CLI knob.

The original `.expect("leader log_subdir should be known after
discovery")` panic at the discover-return site is gone. A late
non-leader that observes the recovery branch but never receives the
leader's `Discover` (e.g. an old peer binary running in mixed-version
mode without the re-emit rule) gets a controlled `bail!` rather than a
process panic.

### Stale-done recovery (T-coord.1b)

A slow peer (bob) entering `done_barrier` for spawn N after the fast
peer (alice) has exited her `done_barrier` linger and advanced into
`ready_barrier(spawn_n_plus_1)` previously had no path to forward
progress: alice was no longer broadcasting `Done` for spawn N AND her
`ready_barrier` silently dropped inbound `Done` messages, leaving bob
to loop forever on his own `Done` rebroadcasts. The barrier-timeout
safety net (T-coord.2) eventually exits with code 75 and the wrapper
restarts with `--resume`, but a multi-hour Hybrid full-matrix benchmark
loses 30-90 seconds of wall-clock to that recovery path. T-coord.1b
closes the surgical gap so the slow peer recovers in one re-broadcast
cycle. Two cooperating mechanisms:

- **`Coordinator::last_completed`** caches the most recently completed
  `done_barrier` outcome — `(variant, run, status, exit_code)` — at the
  tail of `done_barrier` just before returning. Written **only on the
  success path**; the timeout-error branch leaves the cache untouched
  so a stale `Done` for a coordination-failed variant is never
  re-emitted. The cache is **bounded to one entry by design**: bob only
  ever asks for the immediately preceding variant's `Done`, and chained
  cross-spawn hangs are deliberately routed to the barrier-timeout
  safety net rather than a multi-entry retry log.
- **`Coordinator::maybe_reemit_stale_done`** broadcasts our cached
  `Done` when the inbound `(variant, run)` matches the cache. It is
  invoked from a `Some(Message::Done { … })` arm in `ready_barrier`
  (the post-done case the bug originally hit), the cross-spawn branch
  of `done_barrier` (so a peer who is itself the slow peer of spawn
  N+1 still answers stale spawn-N requests), and
  `exchange_resume_manifest` (so the same recovery applies in resume
  mode). Each call site is gated on `self.expected.contains(&name)`.
  Best-effort: send errors are swallowed because the active barrier
  must not be aborted by a transient network failure.

The cache is intentionally **not** wired into the discovery linger.
Reasoning: `last_completed` is constructor-initialized to `None` and
is only ever written by `done_barrier`. On a fresh process,
`discover()` runs before any `done_barrier`, so the cache is `None`
during the discovery linger. On a `--resume` process, the previous
instance exited (cache was per-process), so the cache is also `None`
on entry to the new process's `discover()`. Therefore a hook in the
discovery linger would be structurally inert; we omit it to keep the
recovery surface minimal and document the omission explicitly here so
future maintainers do not "fix" the asymmetry by accident.

What this rule does **not** cover:

- A request for an older variant (spawn N-1 or earlier) — bounded cache
  by design; the barrier-timeout safety net catches multi-spawn hangs.
- A peer that crashed mid-`done_barrier` and never reached the cache
  write — same thing: barrier-timeout + `--resume` is the recovery
  path.

This rule mirrors the late-arriving-discovery rule above (T-coord.3):
the fast peer keeps a small piece of state about its last completed
coordination event and is willing to replay it in response to a slow
peer's stale request, bounded so the cost is O(1).

### Variant templates + multi-dimensional expansion (T-config.2)

Two further config-side mechanisms layer on top of QoS expansion:

1. **`[[variant_template]]`** — a top-level array of reusable defaults.
   `[[variant]]` entries with `template = "<name>"` inherit the template's
   `binary`, `timeout_secs`, `[variant.common]`, and `[variant.specific]`,
   with the variant entry's keys winning on conflict. Templates do not
   spawn. Resolution happens in `config.rs` after parse, before validation.

2. **Array expansion for `tick_rate_hz` and `values_per_tick`** — same
   pattern as `QosSpec`. Add `tick_rate_spec()` / `values_per_tick_spec()`
   helpers returning ascending-deduped concrete values. The expansion
   in `spawn_job.rs` becomes a triple-nested Cartesian product in stable
   order: `tick_rate_hz` (outer) → `values_per_tick` (middle) → `qos`
   (inner). `SpawnJob` carries the per-spawn scalar values; `cli_args.rs`
   emits `--tick-rate-hz`, `--values-per-tick`, `--qos` from the SpawnJob,
   not from `[variant.common]`.

Auto-naming after expansion:

```
<post-template-name>[-<vpt>x<hz>hz][-qos<N>]
```

The hz/vpt suffix appears whenever either dimension expanded (multiple
effective values). Both numbers always show in the suffix even if only
one dimension expanded, so spawn names are always unique within a parent
entry. `inter_qos_grace_ms` becomes "inter-spawn grace" — applied
between every consecutive pair of spawns derived from one source entry,
not just QoS-pair boundaries.

Full spec: `metak-shared/api-contracts/toml-config-schema.md` "Variant
Templates" and "Array Expansion" sections. Worker brief:
`metak-orchestrator/TASKS.md` T-config.2.

### Threading-mode dimension (T14.8)

E14 adds a fourth expansion dimension on top of qos / tick_rate_hz /
values_per_tick: `threading_modes`. The runner cross-products it with
the existing dimensions and injects `--threading-mode <mode>` plus
`--recv-buffer-kb <N>` into every spawned variant. Full spec lives in
`metak-shared/api-contracts/toml-config-schema.md` ("E14 additions")
and `variant-cli.md` ("E14 additions: threading mode and recv buffer").
Code paths to mind when revisiting:

- `config.rs`: `ThreadingMode` enum (string-only -- the runner does
  NOT depend on `variant-base`), `ThreadingModesSpec` (mirrors the
  `QosSpec` / `PositiveSpec` shape), `recv_buffer_kb()` accessor with
  parse-time range validation (`64..=65536`), and the new
  `[[variant]] supported_modes` field via `supported_modes_resolved()`.
- `spawn_job.rs`: cross-product expansion now four-deep
  (`tick_rate_hz` -> `values_per_tick` -> `qos` -> `threading_mode`).
  Naming suffix `-<mode>` appears AFTER `-qos<N>` only when more than
  one mode was requested. Sort order matches the contract: alphabetical,
  so `multi` comes before `single`.
- `cli_args.rs`: emits `--threading-mode <mode>` and
  `--recv-buffer-kb <N>` unconditionally on every spawn (the
  rollout-window default in `variant-base`'s clap is `single` /
  4096, but from T14.8 onward the runner does not rely on those
  defaults).
- `main.rs::expand_and_gate_jobs`: applies capability gating before
  resume-inventory and Phase 2 so the skip set / barrier ids / log
  filenames are all aligned on the post-gating job list.

#### Capability mechanism: Option A (static TOML declaration)

T14.8 picks **Option A** over Option B (`--print-capabilities`
runtime probe). Reasoning:

- **Simpler.** No per-binary startup probe; no JSON-shape contract
  for the probe response; no caching of probe results in the runner.
  The runner stays unaware of how variants implement threading;
  it just reads a list of strings from the config.
- **Faster.** No process spawn per variant binary at startup. On
  multi-host benchmarks that is N variants times one extra fork
  each per runner -- not catastrophic, but wasteful.
- **No new variant-side surface.** Option B would have required
  T14.1's variant-base + every T14.5/T14.6/T14.7 worker to emit the
  probe response, expanding the in-flight work-stream just to wire
  up gating. Option A's per-entry TOML declaration is a config-side
  responsibility -- the orchestrator can add it to each
  `[[variant]]` block independently.

The only trade-off: declarations CAN drift from the variant's actual
trait impl. Treated as an acceptable risk because (a) variants that
declare `supported_modes` wrongly will fail at `connect` time with a
clear error and the existing T-impl.9 failure-diagnostics block will
print the stderr capture inline, and (b) the trait-level
`supported_threading_modes()` is the single source of truth on the
variant side -- declarations in the TOML are an advisory layer the
runner consults, NOT the contract a variant must honour.

#### Permissive default for entries without `supported_modes`

When a `[[variant]]` entry omits `supported_modes`, the runner treats
EVERY requested threading mode as supported and emits a single
stderr note per source entry (not per spawn):

```
[runner:<name>] note: variant '<name>' has no supported_modes declared;
                     treating every requested threading_mode as supported
```

Reasoning: T14.8 lands ahead of every T14.2-T14.7 variant
capability declaration. A strict default (e.g. "supports only
single") would force the orchestrator to land every variant's
declaration before any T14.8 run could exercise its Multi mode --
serialising work that is supposed to run in parallel. The permissive
default keeps T14.8 forward-compatible.

If a variant lies and the requested mode causes `connect` to fail,
the existing failure-diagnostics block surfaces the stderr capture
in the runner's terminal. The operator sees the failure immediately
and can either fix the variant's trait impl or pin the variant's
`supported_modes` in the config.

#### Capability-gating skip notice

When the variant DOES declare `supported_modes` and the config
requests a mode that is not in the list, the runner skips the spawn
with a single stderr line and excludes it from the run summary:

```
[runner:<name>] skipping <effective_name>: variant does not support threading_mode=<mode>
```

Skipped spawns:
- Do not appear in the run summary table.
- Do not block on ready/done barriers.
- Do not produce a JSONL log file.
- Do not count as failures (the run still exits 0 if every executed
  spawn succeeded).

The exact line shape above is part of the T14.8 contract; the
`integration.rs` test
`threading_modes_capability_gating_skips_unsupported_with_notice`
pins it.

### Base log directory selection (T18.5)

The runner picks a single `base_log_dir` at startup that drives:

- The per-run session subfolder (`<base_log_dir>/<run>-<launch_ts>/`) the
  runner's own clock-sync JSONL lives in.
- Every spawned variant's `--log-dir` override, so variant JSONL files
  land next to the runner's coordination logs without the operator
  needing to set `log_dir` per variant.

Precedence (highest wins):

1. `--log-dir <path>` CLI flag.
2. `[runner] log_dir = "..."` in the TOML config.
3. The first `[variant.common].log_dir` found in any `[[variant]]`
   entry (legacy pre-T18.5 fallback; the config-side
   `log_dir = "./logs"` invariant from
   `metak-shared/coding-standards.md` makes this the typical path).
4. `./logs` (final fallback for ad-hoc single-runner runs with no
   variant `log_dir` set).

The chosen value is announced on stderr at startup:

```
[runner:<name>] base log dir: <path> (source: <one-of-the-four-above>)
```

After selection the runner runs `validate_log_dir_writable(&path)`:
`create_dir_all` the path, write a tiny `.runner-write-probe` file,
delete it. Any failure aborts the run **before discovery** with an
`anyhow::Error` describing the offending path AND the underlying I/O
error. The exit code is the standard non-zero anyhow path -- NOT 75
(`EX_TEMPFAIL`), because a non-writable shared folder is an operator
config / permissions issue that re-launching with `--resume` will not
fix.

Cross-platform notes:

- **Windows UNC paths.** `\\server\share\bench-logs` is treated as an
  opaque filesystem path -- the `[runner]` TOML accepts literal
  strings (single-quoted in TOML) so the leading backslashes survive
  without manual escaping. The probe write covers both reachable and
  permission-denied cases.
- **Linux / macOS NFS / SMB mounts.** Treated like any local path; if
  the mount is not available at runner startup the probe fails with
  the kernel's actual ENOENT / EACCES surfaced through `anyhow`.
- **Path separators in `[runner] log_dir`.** Mixed `/` and `\` are
  fine on Windows because the kernel canonicalises before
  create_dir_all. On Linux, use `/`.

The base log dir IS NOT the runner's working directory -- variants
inherit the runner's CWD as usual. Instead the runner builds the
variant's `--log-dir` argument by joining `<base_log_dir>/<log_subdir>`
and passing it as the `log_dir_resolved` override into
`cli_args::build_variant_args`. The variant therefore opens its JSONL
file at the chosen path even though its own
`[variant.common].log_dir = "./logs"` declaration is unchanged from
the coding-standards default.
### Auto-analysis after the matrix (T18.6)

When the runner is launched with `--analyze-full`, the
**lexicographically lowest-named runner** in `runners` (the typical
`alice` in an `alice` / `bob` pair) shells out to the Python analyzer
after the matrix completes:

```
python analysis/analyze.py <log-dir> --summary --dump --diagrams --output <log-dir>/analysis
```

The other runner(s) skip analysis cleanly. The lowest-sorted-name rule
matches the pair-convention used elsewhere (websocket / webrtc / hybrid
TCP pairing, resume_manifest exchange, progress channel, barrier
channel) so operators do not need to learn a new convention for the
analyzer.

**Repo-root detection.** The runner walks up from the runner binary
location (`std::env::current_exe().parent()`) looking for a directory
that contains `analysis/analyze.py`. The walkup is bounded by
`REPO_WALKUP_LIMIT = 8` to keep filesystem traversal cheap even when
the binary lives outside the workspace (e.g. an operator copying
`runner.exe` to a shared deploy folder). Expected layout:

```
<repo-root>/
  analysis/
    analyze.py     <-- the walkup target
    cache.py
    ...
  runner/
    src/
    target/release/runner(.exe)   <-- typical exe location
  target/release/runner(.exe)     <-- workspace-rooted build (also typical)
```

Both `runner/target/release/runner` and `target/release/runner` work
because the walkup checks every parent up to the limit, not just one
specific level.

**Python resolution.** The analyzer is invoked via
`Command::new(<resolved-python>)`, where `<resolved-python>` is the
first of `python3`, `python` that responds to `<exe> --version` with
any exit status (we treat the spawn-success boolean as the existence
check; the version output itself is discarded). When neither resolves,
the runner emits a `WARN` line and continues -- the matrix already
succeeded, so the analyzer not being installed is a degraded-but-
non-fatal outcome.

**Working directory.** The analyzer is spawned with
`current_dir(<repo-root>/analysis)` so any relative imports inside
`analyze.py` resolve consistently with the manual invocation pattern
documented in `analysis/AGENTS.md`.

**Non-fatal Python exit.** Any non-zero exit from the analyzer (or a
spawn error) is surfaced as a `WARN:` line on the runner's stderr and
does NOT change the runner's overall exit status. The benchmark itself
already completed; the analyzer being unable to render diagrams is a
degraded artifact, not a benchmark failure.

The contract lines the integration test
`t18_6_analyze_full_invokes_analyzer_after_matrix` pins:

- The line beginning `[runner:<name>] running analysis:` (announces
  the invocation before the spawn).
- The optional `[runner:<name>] analysis complete:` (analyzer exit 0).
- The optional `[runner:<name>] WARN: analysis exited Some(<code>) ...`
  (analyzer non-zero exit).

The pair-convention skip path emits:

```
[runner:<name>] --analyze-full set, but this runner is not the lowest-sorted name ('<lowest>'); skipping analysis
```

## Workload-shape params (T19.4 / E19)

E19 adds new optional `[variant.common]` keys that the runner must
forward to the variant CLI verbatim:

| TOML key | CLI arg | Used by |
|---|---|---|
| `blob_size` | `--blob-size` | `block-flood` |
| `mixed_scalars_min` | `--mixed-scalars-min` | `mixed-types` |
| `mixed_scalars_max` | `--mixed-scalars-max` | `mixed-types` |
| `mixed_arrays_min` | `--mixed-arrays-min` | `mixed-types` |
| `mixed_arrays_max` | `--mixed-arrays-max` | `mixed-types` |
| `mixed_dict_split_max` | `--mixed-dict-split-max` | `mixed-types` |
| `workload_seed` | `--workload-seed` | all profiles |

Follow the existing `snake_case` → `--kebab-case` forwarding
convention. The runner does NOT validate workload-param values — the
variant rejects invalid combinations at startup with a descriptive
Err.

These keys do NOT participate in the existing array-expansion
mechanism (E9 / E14). They are scalar-only. Configs that want
multiple workload shapes write multiple `[[variant]]` entries
(typically via a shared `[[variant_template]]`).

See `metak-shared/api-contracts/toml-config-schema.md` E19 additions
for the full schema and validation rules.
