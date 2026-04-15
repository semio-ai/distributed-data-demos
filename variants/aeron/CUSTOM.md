# Aeron Variant — Custom Instructions

## Overview

Rust binary implementing the `Variant` trait from `variant-base` using
Adaptive Aeron for transport. Represents the "finance-grade" performance
ceiling — purpose-built for ultra-low-latency messaging.

**Important**: This variant uses C bindings via `rusteron`. It requires the
Aeron media driver to be running on each machine. If the media driver cannot
be installed or the crate fails to build, report the blocker in STATUS.md
rather than spending excessive time debugging C FFI issues.

## Tech Stack

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`variant-aeron`)
- **Key dependencies**:
  - `variant-base` (path = `../../variant-base`) — Variant trait, types, CLI, driver
  - `rusteron-client` — Rust bindings to Aeron C client library
  - `anyhow` — error handling
- **External requirement**: Aeron media driver must be running.
  Install from https://github.com/real-logic/aeron or use the Java driver.
- Follow `metak-shared/coding-standards.md`.

## Build and Test

```
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

If `rusteron-client` fails to build (C library not found), document the
error in STATUS.md and mark the variant as blocked.

## Architecture

```
variants/aeron/
  src/
    main.rs       -- parse CLI, create AeronVariant, call run_protocol
    aeron.rs      -- AeronVariant struct implementing Variant trait
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

- `--aeron-dir` (default: platform-specific temp dir for Aeron media driver)
- `--channel` (default: `aeron:udp?endpoint=239.0.0.1:40456`)
- `--stream-id` (default: `1001`)

### connect

1. Connect to the Aeron media driver via the shared memory interface.
2. Create a Publication on the configured channel + stream.
3. Create a Subscription on the same channel + stream.
4. Wait for the publication to be connected.

### publish

- Offer the serialized message buffer to the Aeron Publication.
- Aeron handles fan-out to all subscribers via the media driver.
- If `offer` returns BACK_PRESSURED, retry (spin briefly).

### poll_receive

- Poll the Aeron Subscription for fragments.
- Aeron delivers fragments via a callback (FragmentHandler).
- Buffer received fragments into an internal `VecDeque<ReceivedUpdate>`.
- Return one from the queue per `poll_receive` call.

### Message format

Same compact binary format as the custom-udp variant (or simpler, since
Aeron handles framing). Serialize: writer, seq, qos, path, payload.

### Testing

- Unit test: verify message serialization.
- Integration test: if the media driver is available, run a short
  single-process test (publish + subscribe on same channel).
- If the media driver is NOT available, skip the integration test with a
  clear message (don't fail the build).
