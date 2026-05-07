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
