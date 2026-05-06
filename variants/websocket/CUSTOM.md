# WebSocket Variant — Custom Instructions

## Overview

Rust binary implementing the `Variant` trait from `variant-base` using
WebSocket over TCP as the transport. The variant exists to characterise
the cost of WebSocket framing on top of raw TCP — directly comparable
to the Hybrid variant's TCP path at QoS 4.

**Reliable QoS only (3-4).** For QoS 1-2 the variant returns a clear
error from `publish` and exits non-zero. The benchmark configs simply
do not spawn it at unreliable QoS levels. Do not add a UDP path; that
is Hybrid's role.

## Tech Stack

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`variant-websocket`)
- **Key dependencies**:
  - `variant-base` (path = `../../variant-base`) — Variant trait, types, CLI, driver
  - `tungstenite` — sync WebSocket implementation. **Do not** use
    `tokio-tungstenite` — we want sync to keep the measurement clean
    and to avoid runtime scheduling noise.
  - `socket2` — for `SO_SNDBUF` / `SO_RCVBUF` tuning if needed.
  - `anyhow` — error handling.
  - Optional: `url` for parsing the WS URL if helpful.
- **No tokio.**
- **No discovery library.** Peer hosts come from the runner-injected
  `--peers` (E9).
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
variants/websocket/
  src/
    main.rs       -- parse CLI, create WebSocketVariant, call run_protocol
    websocket.rs  -- WebSocketVariant struct implementing Variant trait
    protocol.rs   -- compact binary header (same format as hybrid/custom-udp)
    pairing.rs    -- sorted-name pairing logic, port derivation
  tests/
    integration.rs -- single-process loopback (bind + framing)
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

```toml
[variant.specific]
ws_base_port = 19960
```

Variant-specific CLI args:

- `--ws-base-port <u16>` — required. Base TCP port from which per-runner /
  per-qos WebSocket listen / connect ports are derived.

The variant also reads (from the standard runner-injected args, see
`metak-shared/api-contracts/variant-cli.md`):

- `--peers <name1>=<host1>,<name2>=<host2>,...` — full runner→host map.
- `--runner <name>` — this runner's name; used to look up own index.
- `--qos <N>` — concrete QoS level for this spawn (1-4). The variant
  itself only supports 3 and 4; for 1 or 2 it must error out cleanly.

### Port derivation

Identical to Hybrid TCP and QUIC, so the strides stay consistent
across the project:

```
runner_stride = 1
qos_stride    = 10

runner_index    = sorted_peer_names.position(of: --runner)
my_listen_port  = ws_base_port + runner_index * runner_stride + (qos - 1) * qos_stride

for each (name, host) in --peers where name != --runner:
    peer_index    = sorted_peer_names.position(of: name)
    peer_port     = ws_base_port + peer_index * runner_stride + (qos - 1) * qos_stride
    pair_addr     = (host, peer_port)
```

If `--runner` is not present in `--peers`, fail loudly with a clear
error — this indicates a runner/contract bug.

### Symmetric peer pairing — who connects, who accepts

Each peer pair has exactly **one** WebSocket connection, full-duplex.

- Lower-sorted-name runner is the **client**: opens TCP, performs the
  WS handshake against `ws://<peer_host>:<peer_port>/bench`.
- Higher-sorted-name runner is the **server**: binds TCP listener on
  `0.0.0.0:my_listen_port`, accepts the upgrade.

After the handshake completes, both sides treat the connection
symmetrically — either side can publish and receive at any time.

The single `/bench` URL path is sufficient; per-message routing happens
inside the binary header.

### connect

1. Parse `--peers`, `--runner`, `--qos`, `--ws-base-port`. If `--qos`
   is 1 or 2, log a clear error and exit non-zero before any I/O.
   Resolve `runner_index` and the per-peer address list.
2. For each peer where `runner_index < peer_index`: open TCP, perform
   WS handshake as client. Set `TCP_NODELAY` on the underlying socket
   immediately after `tungstenite::connect` returns.
3. For each peer where `runner_index > peer_index`: bind a TCP listener
   on `my_listen_port`, accept connections and perform the WS handshake
   as server. Set `TCP_NODELAY`.
4. Once all expected peers have either connected or accepted, connect
   completes.

A short discovery / handshake timeout (e.g. 30 s) avoids deadlock if
one peer never arrives. On timeout: log clearly and exit non-zero.

### publish — transport selection by QoS

- **QoS 1, 2**: must NOT happen at this point — `connect` already
  rejected them. Defensively return an error if seen.
- **QoS 3 (reliable-ordered)**: write a binary WS frame to every peer
  connection. Kernel TCP handles retransmission and ordering;
  WebSocket adds a length-prefixed binary-frame header on top. No
  application-layer NACK logic.
- **QoS 4 (reliable-TCP)**: same as QoS 3.

### poll_receive

- For each peer's WebSocket: poll the read side. Use the same
  `SO_RCVTIMEO`-based polling trick as Hybrid's TCP path so writes
  remain fully blocking (back-pressure measurement) while reads are
  short-deadline non-blocking.
- For each fully received WS frame, parse the binary header and
  construct a `ReceivedUpdate`.

### Message format

The same compact binary header used by `custom-udp` and `hybrid` lives
**inside** the WebSocket binary frame body:

```
WS binary frame body:
[1 byte qos | 8 bytes seq | 2 bytes path_len | N bytes path | 2 bytes writer_len | M bytes writer | payload bytes]
```

WebSocket adds its own frame header on top; that overhead is exactly
what the variant exists to measure.

### Connection management

- One WebSocket connection per peer pair. Bidirectional.
- Set `TCP_NODELAY` on every underlying socket — critical for latency.
- Keep the underlying TCP socket in **blocking mode** for writes (same
  rationale as Hybrid: kernel back-pressure is the signal we want to
  measure). For reads, use a short `SO_RCVTIMEO` (~1 ms) on a cloned
  read handle so `poll_receive` interleaves peers without stalling.
- WebSocket close frames at `disconnect` time: send a clean close, but
  do not block the spawn forever — give peers a small grace window
  then drop the underlying TCP.
- Per-peer fault tolerance: if one peer's connection returns an
  unrecoverable error (closed, reset), drop that peer from the active
  set and continue with the surviving peers. Same rule as Hybrid TCP.

### EOT (End-of-Test) — E12 protocol

Per `metak-shared/api-contracts/eot-protocol.md`, the WebSocket
variant's EOT looks like Hybrid's TCP-frame variant: at end-of-operate,
broadcast an `eot_sent` marker as a binary WS frame to every connected
peer. Recognise incoming `eot_sent` frames from peers. The marker
distinguishes itself from data via a reserved value in the header
(see `eot-protocol.md` for the on-wire encoding — the worker should
follow that contract verbatim).

Implement `signal_end_of_test` and `poll_peer_eots` from
`variant-base`'s `Variant` trait. The trait defaults are no-op, so
anything you don't override silently turns into an `eot_timeout`
diagnostic.

### Testing

- Unit test: message-header serialization / deserialization.
- Unit test: pairing / port-derivation logic given a few `--peers`
  shapes.
- Unit test: `publish` at QoS 1 or 2 returns an error.
- Integration test: single-process loopback. Synthesize the new CLI
  shape: `--peers self=127.0.0.1`, `--runner self`,
  `--ws-base-port <free port>`, `--qos 3` (or `--qos 4`). With a
  single-peer map there are no peers to connect to (self is excluded
  by design); the test exercises bind/listen, the role-decision logic,
  and message framing in isolation.
- Cross-peer flow is validated end-to-end via two runners on localhost
  during the variant's regression-test task (T3f.4).

### Validation against reality

After implementation:

- `cargo test --release -p variant-websocket` — all-green.
- `cargo clippy --release -p variant-websocket --all-targets -- -D warnings`.
- `cargo fmt -p variant-websocket -- --check`.
- Build the binary, then run an end-to-end two-runner localhost test
  using a TOML config that spawns websocket at QoS 4 (analogous to
  `configs/two-runner-hybrid-all.toml`). Verify the produced JSONL log
  has the expected `connected` / `phase` / `eot_sent` / `eot_received`
  events and that delivery is ≥ 99% over the operate window.

## Out of scope

- TLS / `wss://`.
- WebSocket subprotocols, extensions (compression, etc.).
- HTTP/2 WebSockets (RFC 8441).
- QoS 1 and 2 over UDP — this is Hybrid's role.
- mDNS or any peer discovery beyond `--peers`.
