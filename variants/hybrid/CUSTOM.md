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
- For each peer's `TcpStream`:
  - The socket stays in **blocking mode** (we never call
    `set_nonblocking(true)` on it). `publish` writes are truly blocking:
    when the kernel send buffer fills, the writer thread pauses until
    the receiver drains it. This is the back-pressure signal we want to
    measure for the benchmark; bypassing it (e.g. with non-blocking
    writes plus app-side retry-and-drop) would distort the comparison
    against `custom-udp`'s NACK approach.
  - To make reads pollable without flipping the socket-wide `FIONBIO`
    flag, the read handle (obtained via `try_clone`) gets a short
    `SO_RCVTIMEO` via `TcpStream::set_read_timeout` (~1 ms). Reads then
    return `WouldBlock` (Unix) or `TimedOut` (Windows) when no data is
    in flight, allowing `poll_receive` to interleave UDP and other
    peers' reads without stalling the protocol loop. Writes are
    unaffected by `SO_RCVTIMEO` and remain blocking.
- **Why not just `set_nonblocking(true)` on the read clone?** Because
  `set_nonblocking` calls `ioctlsocket(FIONBIO,...)` which is
  socket-wide; the cloned read handle's `FIONBIO` flag is shared with
  the write handle and would silently un-block the write side too,
  defeating the back-pressure goal.
- The variant also keeps a defence-in-depth `write_with_retry` wrapper
  with a generous wall-clock budget (10 s) that catches any
  `WouldBlock` it might somehow see — under normal operation the socket
  is blocking and `write` never returns `WouldBlock`, but the wrapper
  protects against accidental future regressions of the blocking flag.
- Set `TCP_NODELAY` on every connection to disable Nagle — critical for
  latency.

### TCP read loop — per-peer fault tolerance

At cross-machine high throughput, individual peer streams may return
`ConnectionAborted` / `ConnectionReset` or unexpected EOF — typically as
a downstream effect of one side bailing on a `WouldBlock`. The TCP
read loop in `try_recv` MUST NOT propagate such errors up: it logs a
single `eprintln!` warning, drops that peer's stream from the active
set, and continues polling the surviving peers. The whole spawn does
NOT fail on a single peer drop; the protocol-driver layer must still
complete its phases. The same fault-tolerance rule is applied to write
errors during `broadcast`: a per-peer write failure drops the offending
peer and the broadcast continues with the rest.

### UDP send — bounded WouldBlock retry

The UDP socket is non-blocking because `poll_receive` needs `recv_from`
to be non-blocking so the variant's poll loop can interleave UDP and
TCP reads without one starving the other. `set_nonblocking(true)` sets
the flag for the entire socket — there is no per-direction toggle on a
UDP socket — so `send_to` is also non-blocking and can return
`WouldBlock` when the kernel send buffer fills. Making the UDP socket
fully blocking is therefore awkward: it would force `recv_from` to
block too, breaking the polled receive path.

To keep the receive path non-blocking while still tolerating transient
send pressure, `publish` retries on `WouldBlock` with a bounded
wall-clock budget (~1 ms via `std::thread::yield_now()` between
attempts). If the budget is exhausted while still hitting `WouldBlock`,
the send returns an error so the caller surfaces back-pressure rather
than silently dropping data. We also bump `SO_SNDBUF` (~4 MB) at socket
creation to reduce how often the retry actually triggers under high
multicast rates on Windows.

### Testing

- Unit test: message serialization/deserialization.
- Unit test: QoS 2 stale-discard logic.
- Integration test: single-process loopback. Synthesize the new CLI shape:
  `--peers self=127.0.0.1`, `--runner self`, `--multicast-group 239.0.0.1:<port>`,
  `--tcp-base-port <port>`, `--qos <1..4>`. Note that with a single-peer
  map, there are no peers to connect to (self is excluded by design); the
  test exercises bind/listen and message framing only. Cross-peer flow is
  validated end-to-end via two runners on localhost during T9.3 acceptance.
