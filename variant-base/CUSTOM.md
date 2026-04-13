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

```
cargo build                   # build library + variant-dummy binary
cargo test                    # unit + integration tests
cargo clippy -- -D warnings   # lint
cargo fmt -- --check          # format check
```

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
- Ships as `variant-dummy` binary that the runner can spawn like any other
  variant.

### Test Protocol Driver

The driver is a function (not a trait) that takes a `&dyn Variant` (or
generic `impl Variant`) and the parsed CLI config, then runs:
1. Connect phase
2. Stabilize phase (sleep)
3. Operate phase (tick loop with workload)
4. Silent phase (drain + flush)

The driver owns the logger and calls it directly. Variants never touch
the logger — they only do transport work through the trait methods.
