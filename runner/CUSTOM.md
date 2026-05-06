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
