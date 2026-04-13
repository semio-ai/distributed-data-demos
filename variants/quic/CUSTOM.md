# QUIC Variant — Custom Instructions

## Overview

Rust binary implementing the `Variant` trait from `variant-base` using QUIC
via the quinn crate. Represents the "modern protocol" approach — built-in
encryption, multiplexed streams, congestion control.

## Tech Stack

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`variant-quic`)
- **Key dependencies**:
  - `variant-base` (path = `../../variant-base`) — Variant trait, types, CLI, driver
  - `quinn` — QUIC implementation
  - `rustls` — TLS for QUIC (self-signed certs for LAN)
  - `rcgen` — generate self-signed certificates at runtime
  - `tokio` (rt-multi-thread) — async runtime for quinn
  - `mdns-sd` — peer discovery
  - `anyhow` — error handling
- Follow `metak-shared/coding-standards.md`.

## Build and Test

```
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

## Architecture

```
variants/quic/
  src/
    main.rs       -- parse CLI, create QuicVariant, call run_protocol
    quic.rs       -- QuicVariant struct implementing Variant trait
    certs.rs      -- self-signed certificate generation
    discovery.rs  -- mDNS peer discovery
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

- `--bind-addr` (default: `0.0.0.0:0`)
- `--peers` (optional: explicit comma-separated peer addresses, skips mDNS)

### Async-to-sync bridge

Quinn is async (tokio). The `Variant` trait is sync. Strategy:
1. On `connect`, spawn a tokio runtime internally (`Runtime::new()`).
2. Use the runtime's `block_on` for connect/disconnect.
3. For `publish` and `poll_receive`, use channels:
   - `publish` sends to an mpsc channel; a background tokio task reads
     from the channel and sends over QUIC.
   - A background tokio task receives from QUIC and pushes to another
     mpsc channel; `poll_receive` does a `try_recv` on that channel.
4. On `disconnect`, shut down the runtime.

### connect

1. Generate a self-signed certificate using `rcgen`.
2. Create a Quinn endpoint with the cert.
3. Discover peers via mDNS (or use `--peers`).
4. Connect to each peer (QUIC client handshake).
5. Accept incoming connections from peers (QUIC server).
6. For each peer connection, spawn background send/receive tasks.

### QoS mapping to QUIC features

- **QoS 1-2 (best-effort / latest-value)**: Use QUIC unreliable datagrams
  (`send_datagram`). These are fire-and-forget within the QUIC connection.
  For QoS 2, include seq in header; receiver discards stale.
- **QoS 3-4 (reliable)**: Use QUIC streams (`open_uni` or `open_bi`).
  QUIC guarantees ordered, reliable delivery per stream. Open one stream
  per logical path, or multiplex with a header.

### Certificate handling

For LAN benchmarking, generate self-signed certs at startup and configure
the client to skip server cert verification (or use a shared self-signed CA).
This is a benchmark tool, not production — don't over-engineer TLS.

### Testing

- Unit test: certificate generation.
- Unit test: message serialization.
- Integration test: single-process loopback (connect to self, send/receive).
  Use `127.0.0.1` with explicit `--peers` to avoid mDNS in tests.
