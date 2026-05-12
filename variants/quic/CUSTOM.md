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
3. Bind a `std::net::UdpSocket` on `0.0.0.0:my_bind_port` ourselves,
   pass it through `variant_base::tune_udp_buffers_std` to bump
   `SO_RCVBUF` / `SO_SNDBUF` to 8 MiB (T-impl.2), then hand the tuned
   socket to `quinn::Endpoint::new(EndpointConfig::default(), Some(server_config), socket, quinn::default_runtime()?)`.
   `Endpoint::server(addr)` would have bound the socket internally and
   left it on Windows' ~64 KB defaults, dropping packets at 100 K pkt/s
   same-host loads. Set the client config on the endpoint afterwards.
4. Connect to each peer (QUIC client handshake).
5. Accept incoming connections from peers (QUIC server).
6. For each peer connection, spawn background send/receive tasks.

### QoS mapping to QUIC features

- **QoS 1-2 (best-effort / latest-value)**: Use QUIC unreliable datagrams
  (`send_datagram`). These are fire-and-forget within the QUIC connection.
  For QoS 2, include seq in header; receiver discards stale.
- **QoS 3-4 (reliable)**: Use **one long-lived unidirectional QUIC
  stream per outbound connection** (T14.13). All reliable frames for
  the spawn flow over that stream as length-delimited records
  (`[u32 BE length][frame bytes]`). QUIC guarantees ordered, reliable
  byte delivery per stream, and the receiver reads the stream in a
  single task that peels off frames sequentially -- so the writer's
  send order is preserved end-to-end through to `poll_receive`.

### Reliable-stream ordering (T14.13)

Prior to T14.13 the reliable path opened a **fresh** unidirectional
stream per message (and `tokio::spawn`-ed the write) plus
spawned a per-stream `read_to_end` task on the receive side. QUIC's
per-stream ordering invariant therefore did *not* extend across the
many parallel streams, and at the E14 smoke scale
(`quic-100x100hz-multi`, 100 vpt x 100 Hz x 10 s = ~100 K msg/spawn
per direction) the analysis tool's per-(writer, receiver) prev-seq
scan flagged ~41 K out-of-order receives per direction. See the
T14.13 audit subsection in `metak-orchestrator/STATUS.md` for the
investigation log.

The fix consolidates the reliable path to a single long-lived
unidirectional stream per outbound connection. The send_loop owns
one `Option<SendStream>` per connection, lazily opens it on first
reliable send, writes length-delimited frames serially with
`write_all`, and clears the slot on error to lazily re-open later.
On `disconnect` the variant `finish()`-es every still-open stream so
the receiver sees a clean frame-aligned end-of-stream. EOT is the
final length-delimited frame on the same stream (no separate
EOT-only stream).

Wire format on a reliable stream:

```
repeat: [u32 BE frame_len][frame_len bytes of TAG_DATA/TAG_EOT frame]
```

Receive side: each accepted uni-stream gets *one* tokio task that
runs `read_reliable_stream`, which loops on
`read_exact(&mut [0u8; 4])` for the length prefix, then
`read_exact(&mut frame)` for the payload, and dispatches each frame
via the existing `dispatch_decoded` pipeline before the next
`read_exact`. A clean `FinishedEarly(0)` at a frame boundary
terminates the loop without error. The single-task ordering
guarantee is what carries QUIC's per-stream byte order through to
the variant's inbound mpsc and ultimately `poll_receive`.

This change is invisible at the `Variant` trait surface -- callers
still see the same `publish` / `try_publish` / `poll_receive`
behaviour -- but it adds a wire-format change to the reliable path
that the old variant binary cannot decode and vice-versa. Both
sides of a benchmark run must use a post-T14.13 build.

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

### Backpressure semantics (T-impl.7)

`Variant::try_publish` is implemented honestly on the QoS 1/2 datagram path
and falls through to the default `publish` for the QoS 3/4 reliable path.

- **QoS 1 / QoS 2 (best-effort / latest-value, datagrams)**: the variant's
  main thread bypasses the send_loop channel and calls
  `quinn::Connection::send_datagram` directly. Before each send it inspects
  every established connection's `datagram_send_buffer_space()` and, if
  *no* connection currently has room for the encoded message, returns
  `Ok(false)` so the driver logs a `backpressure_skipped` event. A
  receiver-visible seq gap is acceptable here per the QoS contract.

  **Why polling buffer space rather than matching on a `Blocked` error
  variant**: quinn 0.11's `Connection::send_datagram` always forwards to
  `proto::Datagrams::send(data, drop=true)`, which makes the
  `proto::SendDatagramError::Blocked` discriminant `unreachable!()` inside
  the wrapper. The error variants actually surfaced are `UnsupportedByPeer`,
  `Disabled`, `TooLarge`, and `ConnectionLost`. With `drop=true`, a full
  outgoing-datagram queue causes quinn to silently evict the oldest queued
  datagram to make room for the new one — which would inflate our delivery
  rate metric and hide real backpressure. Polling `datagram_send_buffer_space`
  and refusing to send when it is below the message length is therefore the
  honest signal. (Quinn does offer `send_datagram_wait` for the blocking
  variant, which we deliberately do *not* use for QoS 1/2 because blocking
  would introduce unbounded latency without producing the gap the driver's
  `backpressure_skipped` event is designed to count.)

  The exact quinn error variant we match for "connection went away mid-burst"
  is `quinn::SendDatagramError::ConnectionLost(_)`; we ignore it on a single
  connection and let the rest of the fan-out continue. Other hard errors
  propagate as `anyhow::Error` to the driver.

- **QoS 3 / QoS 4 (reliable streams)**: `try_publish` delegates to
  `publish`, which enqueues an `OutboundMessage::reliable=true` onto the
  send_loop's unbounded mpsc channel. The send_loop opens a fresh
  unidirectional QUIC stream per message and awaits
  `SendStream::write_all`. QUIC streams flow-control inside quinn (the
  `write_all` future stalls until peer-side credit is available), so
  backpressure is absorbed at the stream layer rather than producing a
  seq gap. `try_publish` therefore always returns `Ok(true)` on the
  reliable path.

The unit test `test_try_publish_qos1_reports_backpressure_under_burst`
verifies the QoS 1 path by spinning up a loopback Quinn pair and bursting
~1 KiB datagrams from one variant; without ever yielding back to the
runtime, the outgoing datagram buffer fills and `try_publish` flips to
`Ok(false)` within seconds. `test_try_publish_qos3_and_qos4_never_backpressure`
verifies the reliable path returns `Ok(true)` across hundreds of bursts.

## Threading modes (T14.5)

The QUIC variant declares `supported_threading_modes() -> &[Multi]`. It
does not support `ThreadingMode::Single` and `connect(Single)` returns
an `Err` before any socket I/O is attempted. The runner consults this
declaration (per T14.8) and silently skips any
`<name>-qos<n>-single` spawn the matrix expansion would otherwise
produce.

### Why no Single mode

quinn is fundamentally async: every meaningful operation
(`Endpoint::accept`, `Connection::open_uni`, `SendStream::write_all`,
`Datagrams::read`) is a future that needs a runtime to drive its
sockets and timers. Even a "single-threaded" tokio current-thread
runtime is still an async runtime -- and that is precisely the dependency
the E14 single-threaded mode exists to remove for WASM-friendly
deployments (browser-WASM cannot host a multi-threaded tokio at all,
and WASI's threading is restricted).

Hand-rolling a synchronous, runtime-free QUIC client on top of `mio`
or raw sockets would be a multi-week rewrite -- duplicating quinn's
loss recovery, congestion control, and TLS state machines -- and would
no longer be measuring "what does the off-the-shelf quinn stack
cost?", which is the explicit purpose of this variant per E3d. Out
of scope.

Downstream consumers that genuinely need single-threaded QUIC in WASM
would use a different transport (raw UDP-over-WebTransport or HTTP/3
fetch from the browser side), or wait for a sans-IO Rust QUIC
implementation to mature. Neither lives in this benchmark's scope.

### `--recv-buffer-kb`

The variant already binds its own `std::net::UdpSocket` before handing
it to `quinn::Endpoint::new`, and calls `variant_base::tune_udp_buffers_std`
to push both `SO_RCVBUF` and `SO_SNDBUF` to a fixed 8 MiB target
(T-impl.2). The runner-injected `--recv-buffer-kb` arg is currently
**advisory** for this variant: the trait method `Variant::connect`
does not receive it (the driver consumes it for the `connected`
event only), so the variant cannot honour a per-spawn override without
a trait extension. Operators tuning kernel buffers for QUIC should
edit `variant_base/src/socket.rs::TARGET_UDP_BUFFER_BYTES` rather than
the runner config until that plumbing exists.
