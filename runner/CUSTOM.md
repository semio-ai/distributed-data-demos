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
- **Do NOT depend on `variant-base`**. The runner knows nothing about variant
  internals. It treats variants as opaque child processes. The interface is
  defined by the CLI contract and the TOML config.
- Follow `metak-shared/coding-standards.md` for Rust conventions.

## Build and Test

```
cargo build                   # build runner binary
cargo test                    # unit + integration tests
cargo clippy -- -D warnings   # lint
cargo fmt -- --check          # format check
```

Integration tests use `variant-dummy` from the `variant-base` crate. The
binary must be built first:

```
cd ../variant-base && cargo build --release
```

The path to `variant-dummy` in test configs is relative to the runner's CWD.

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
