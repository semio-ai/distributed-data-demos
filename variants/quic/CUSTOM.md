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

All commands run from the repo root (Cargo workspace). Do **not** `cd` into
`variants/quic/` to build — that produces a stray per-subfolder `target/`
directory which the configs do not point at.

```
cargo build --release -p variant-quic
cargo test --release -p variant-quic
cargo clippy --release -p variant-quic -- -D warnings
cargo fmt -p variant-quic -- --check
```

Compiled binary lives at `target/release/variant-quic(.exe)`.

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

As of E9, the QUIC variant derives its bind and connect addresses from the
runner-injected `--peers` plus the per-spawn `--qos` and a single
config-supplied `--base-port`. The variant-specific config in TOML is just:

```toml
[variant.specific]
base_port = 19930
```

Variant-specific CLI args:

- `--base-port <u16>` — required. The base port that all per-runner /
  per-qos ports are derived from.

The variant also reads (from the standard runner-injected args, see
`metak-shared/api-contracts/variant-cli.md`):

- `--peers <name1>=<host1>,<name2>=<host2>,...` — full runner→host map.
- `--runner <name>` — this runner's name; used to look up own index.
- `--qos <N>` — concrete QoS level for this spawn (1-4).

Old `--bind-addr` and the variant-specific `--peers` (explicit
comma-separated peer addresses) have been removed. mDNS discovery in this
variant is also retired in favour of runner-driven discovery.

### Port derivation

```
runner_stride = 1
qos_stride    = 10

runner_index = sorted_peer_names.position(of: --runner)
my_bind_port = base_port + runner_index * runner_stride + (qos - 1) * qos_stride

for each (name, host) in --peers where name != --runner:
    peer_index   = sorted_peer_names.position(of: name)
    peer_port    = base_port + peer_index * runner_stride + (qos - 1) * qos_stride
    connect_to   = (host, peer_port)
```

Sort `--peers` by name for stable indexing. Bind on `0.0.0.0:my_bind_port`.
Connect to every peer except self. The same convention is documented in
`metak-shared/api-contracts/toml-config-schema.md` — keep them in sync if
you change the strides.

If `--runner` is not present in `--peers`, fail loudly with a clear
error — this indicates a runner/contract bug.

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

1. Parse `--peers`, `--runner`, `--qos`, `--base-port`. Derive `my_bind_port`
   and the list of `(peer_name, peer_host, peer_port)` tuples per the
   "Port derivation" section above.
2. Generate a self-signed certificate using `rcgen`.
3. Create a Quinn endpoint bound to `0.0.0.0:my_bind_port` with the cert.
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
  Synthesize the new CLI shape: `--peers self=127.0.0.1`, `--runner self`,
  `--base-port <free port>`, `--qos 1` (or whichever level the test
  exercises).
