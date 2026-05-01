# Hybrid UDP/TCP Variant — Custom Instructions

## Overview

Rust binary implementing the `Variant` trait from `variant-base` using UDP
for best-effort traffic (QoS 1-2) and TCP for reliable traffic (QoS 3-4).
Represents the "simplest correct" approach — no application-layer reliability
logic at all. Kernel TCP handles everything for reliable delivery.

The key benchmark question: is NACK-based reliable-UDP worth the complexity,
or does TCP perform equally well on a LAN?

## Tech Stack

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`variant-hybrid`)
- **Key dependencies**:
  - `variant-base` (path = `../../variant-base`) — Variant trait, types, CLI, driver
  - `socket2` — UDP multicast socket configuration
  - `anyhow` — error handling
- **No external libraries beyond std for TCP** — just `std::net::TcpStream`
  and `TcpListener`.
- **No discovery library**: peer hosts come from the runner-injected
  `--peers` arg (since E9). mDNS was never actually wired up; remove the
  `mdns-sd` dependency from `Cargo.toml` if it is still listed.
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
variants/hybrid/
  src/
    main.rs       -- parse CLI, create HybridVariant, call run_protocol
    hybrid.rs     -- HybridVariant struct implementing Variant trait
    udp.rs        -- UDP multicast send/receive for QoS 1-2
    tcp.rs        -- TCP connection management for QoS 3-4
    protocol.rs   -- message framing (shared between UDP and TCP)
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

As of E9, peer hosts are runner-injected via the standard `--peers`. The
variant derives its own TCP listen port and each peer's TCP connect port
from `--tcp-base-port`, the `--peers` map, and the per-spawn `--qos`. The
old variant-specific `--peers` (host:port list) and `--bind-addr` are
removed.

```toml
[variant.specific]
multicast_group = "239.0.0.1:19500"
tcp_base_port   = 19900
```

Variant-specific CLI args:

- `--multicast-group <ip:port>` — required. UDP multicast group address.
  Same value used by all runners; no runner or QoS stride applied.
- `--tcp-base-port <u16>` — required. Base port that per-runner / per-qos
  TCP ports are derived from.

The variant also reads (from the standard runner-injected args, see
`metak-shared/api-contracts/variant-cli.md`):

- `--peers <name1>=<host1>,<name2>=<host2>,...` — full runner→host map.
- `--runner <name>` — this runner's name; used to look up own index.
- `--qos <N>` — concrete QoS level for this spawn (1-4).

### Port derivation

```
runner_stride = 1
qos_stride    = 10

runner_index    = sorted_peer_names.position(of: --runner)
my_tcp_listen   = tcp_base_port + runner_index * runner_stride + (qos - 1) * qos_stride

for each (name, host) in --peers where name != --runner:
    peer_index    = sorted_peer_names.position(of: name)
    peer_tcp_port = tcp_base_port + peer_index * runner_stride + (qos - 1) * qos_stride
    connect_to    = (host, peer_tcp_port)
```

Sort `--peers` by name for stable indexing. This is the same convention
used by the QUIC variant and documented in
`metak-shared/api-contracts/toml-config-schema.md` — keep them in sync if
you change the strides.

UDP multicast: bind on `multicast_group` directly with no runner or QoS
stride. All runners join the same group. Sequential per-spawn execution
plus the runner's `silent_secs` drain phase plus `inter_qos_grace_ms`
provide cross-spawn isolation; multicast doesn't need TIME_WAIT-style
spacing.

If `--runner` is not present in `--peers`, fail loudly with a clear
error — this indicates a runner/contract bug.

### connect

1. Parse `--peers`, `--runner`, `--qos`, `--multicast-group`, `--tcp-base-port`.
   Resolve `runner_index` and derive `my_tcp_listen` and the list of
   `(peer_name, peer_host, peer_tcp_port)` tuples per "Port derivation".
2. Bind a UDP socket and join `multicast_group` (for QoS 1-2).
3. For QoS 3-4: bind a TCP listener on `0.0.0.0:my_tcp_listen` and connect
   to every peer's `(peer_host, peer_tcp_port)`.
   - Set `TCP_NODELAY` on all connections to avoid Nagle coalescing.

### publish — transport selection by QoS

- **QoS 1 (best-effort)**: UDP multicast. Fire and forget.
- **QoS 2 (latest-value)**: UDP multicast with seq in header.
- **QoS 3 (reliable-ordered)**: TCP to each peer. Kernel handles
  retransmission and ordering. No application-layer NACK logic.
- **QoS 4 (reliable-TCP)**: Same as QoS 3 — TCP to each peer.

This is the key simplification: QoS 3 and 4 use identical transport (TCP).
The custom-udp variant (E3b) implements QoS 3 with NACKs on UDP. Comparing
the two at QoS 3 directly measures whether the NACK complexity is worth it.

### poll_receive

- Check both UDP socket (non-blocking `recv_from`) and TCP streams
  (non-blocking read) for incoming data.
- Parse header, construct `ReceivedUpdate`.
- For QoS 2: track highest seq per writer, discard stale.

### Message format

Same compact binary header as custom-udp:
```
[1 byte qos | 8 bytes seq | 2 bytes path_len | N bytes path | 2 bytes writer_len | M bytes writer | payload bytes]
```

### TCP connection management

- One TCP connection per peer (bidirectional).
- Use non-blocking mode with `set_nonblocking(true)` for `poll_receive`.
- For `publish`, use blocking writes (small messages at ~1KB will fit in
  the kernel buffer and return immediately).
- Set `TCP_NODELAY` to disable Nagle algorithm — critical for latency.

### Testing

- Unit test: message serialization/deserialization.
- Unit test: QoS 2 stale-discard logic.
- Integration test: single-process loopback. Synthesize the new CLI shape:
  `--peers self=127.0.0.1`, `--runner self`, `--multicast-group 239.0.0.1:<port>`,
  `--tcp-base-port <port>`, `--qos <1..4>`. Note that with a single-peer
  map, there are no peers to connect to (self is excluded by design); the
  test exercises bind/listen and message framing only. Cross-peer flow is
  validated end-to-end via two runners on localhost during T9.3 acceptance.
