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

All commands run from the repo root (Cargo workspace). Do **not** `cd` into
`variants/hybrid/` to build — that produces a stray per-subfolder `target/`
directory which the configs do not point at.

```
cargo build --release -p variant-hybrid
cargo test --release -p variant-hybrid
cargo clippy --release -p variant-hybrid -- -D warnings
cargo fmt -p variant-hybrid -- --check
```

Compiled binary lives at `target/release/variant-hybrid(.exe)`.

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
than silently dropping data. The UDP socket is also tuned via
`variant_base::tune_udp_buffers` at creation time to bump both
`SO_RCVBUF` and `SO_SNDBUF` to 8 MiB (T-impl.2) — the previous 4 MiB
send-only bump was insufficient because the receive path also clipped
at Windows-default ~64 KB buffers during 100 K pkt/s same-host runs.
The TCP path is untouched: TCP back-pressure is the protocol-level
signal we deliberately measure.

### Backpressure semantics (T-impl.7)

Hybrid overrides `Variant::try_publish` (see `src/hybrid.rs`,
`impl Variant for HybridVariant::try_publish`) so the driver gets
honest backpressure signalling instead of always seeing `Ok(true)`.
The driver assigns and consumes a seq number BEFORE calling
`try_publish`, so any `Ok(false)` return creates a receiver-visible
gap in the seq stream. This is fine for some QoS levels and
catastrophic for others:

- **QoS 1 (BestEffort)** — single non-blocking `UdpSocket::send_to`
  via `UdpTransport::try_send_nonblocking` (see `src/udp.rs`).
  `WouldBlock` -> `Ok(false)`. The driver logs `backpressure_skipped`;
  the receiver tolerates loss by definition. No retry, no kernel-
  buffer spin — the existing `send_with_retry` loop (1 ms budget,
  used by `publish`) is bypassed because we want the driver to see
  the immediate backpressure signal, not absorb it.
- **QoS 2 (LatestValue)** — same as QoS 1. Gap-tolerant by design;
  the receiver's stale-discard logic only cares about the highest
  seen seq.
- **QoS 3 (ReliableUdp / TCP path)** — blocking `TcpTransport::
  broadcast`. ALWAYS `Ok(true)`. TCP receivers expect strictly
  contiguous framed messages; a gap would corrupt the per-peer
  reader state. The outbound TCP socket is in blocking mode (see
  "TCP connection management" above), so `write_all` blocks under
  kernel back-pressure — exactly the signal we want to measure for
  the QoS-3 NACK-vs-TCP comparison.
- **QoS 4 (ReliableTcp / TCP path)** — identical to QoS 3 in this
  variant. Always `Ok(true)`.

Where it lives:

- `src/udp.rs::UdpTransport::try_send_nonblocking` — single attempt,
  `WouldBlock` surfaces as `Ok(false)`.
- `src/hybrid.rs::Variant::try_publish` — dispatches by QoS.
- `src/hybrid.rs::Variant::publish` — unchanged; keeps the
  spin-on-WouldBlock UDP send and the blocking TCP broadcast.

### Threading modes (T14.4 + T14.16)

Hybrid declares
`supported_threading_modes() = &[Single, Multi]`. The driver chooses
which mode to use per spawn via `--threading-mode` (runner-injected,
T14.1) and the variant branches its receive path accordingly.

**Audit (T14.4 first step)**: before T14.4, Hybrid was fully inline —
zero `thread::spawn`, zero `mpsc`, zero `JoinHandle` in `src/`. The
TCP path used per-peer `SO_RCVTIMEO`-driven polled reads (1 ms
timeout, fault-tolerant on per-peer error) and the UDP path used
non-blocking `recv_from`. The high-rate fixture (100 K msg/s) passes
this way thanks to kernel back-pressure on the blocking TCP writes,
not because of reader threads. See
`metak-orchestrator/STATUS.md` "T14.4 -- variants/hybrid audit
(2026-05-11)" for the full audit report.

**Single mode** (`ThreadingMode::Single`): unchanged inline behaviour.
`poll_receive` does a bounded loop probing UDP (`UdpTransport::try_recv`,
non-blocking) and every TCP peer (`TcpPeer::try_recv_framed`, polled
with `SO_RCVTIMEO = 1 ms` via the read clone). Each iteration
consumes at most one frame; the loop returns Data, returns None
(idle), or strictly drains a buffered frame. Single mode at high
symmetric rates may saturate the driver thread — the variant has
to interleave publish and receive on one OS thread — and the
delivery measurement is the point of the dimension.

**Multi mode** (`ThreadingMode::Multi`): `start_reader_threads(Multi)`
spawns:

- one UDP recv thread (`src/reader.rs::spawn_udp_reader`) that owns a
  dedicated blocking recv-side `UdpSocket`. The primary UDP socket
  stays non-blocking for `try_send_nonblocking` (the back-pressure
  signal for QoS 1/2); the recv socket is a SECOND socket joined to
  the same multicast group via `SO_REUSEADDR` + `join_multicast_v4`,
  in BLOCKING mode with a short `SO_RCVTIMEO`
  (`reader::UDP_READER_TIMEOUT = 200 ms`) so the reader can poll the
  shutdown flag between attempts. Built by
  `UdpTransport::make_blocking_recv_socket`.
- one per-peer TCP reader thread (`src/reader.rs::spawn_tcp_reader`).
  Each thread owns the read clone taken from a `TcpPeer` via
  `TcpPeer::take_read_stream`, raised to `SO_RCVTIMEO = 200 ms` so
  the reader wakes periodically to check shutdown.

Both reader-thread families pre-decode `Frame`s and route the
decoded item onto one of two channels per the T14.16 split:

- `HubDataMessage::Data` -> bounded `data_tx` (an
  `mpsc::sync_channel` of capacity
  `reader::READER_CHANNEL_CAPACITY = 4096`). `try_send` (no blocking)
  drops on full-channel rather than blocking the reader. Drop-on-full
  is acceptable: QoS 1/2 tolerate loss by definition; QoS 3/4 (TCP)
  receivers depend on kernel TCP, which applies its own back-pressure
  before the recv buffer can fill pathologically. The warning line
  for an overrun is
  `[variant-hybrid] data channel full (... slots) -- dropping Data frame (receiver saturated)`
  -- disambiguated from the pre-T14.16 wording so operators can be
  sure lifecycle items (EOT) were NOT lost when this line appears.
- `HubLifecycleMessage::Eot` -> unbounded `lifecycle_tx` (a
  `std::sync::mpsc::channel`). Lifecycle items must NEVER drop: losing
  an EOT forces the peer's driver to wait the full `eot_timeout`,
  defeating the EOT contract. Hybrid has no NACK protocol so the
  lifecycle channel currently only carries `Eot`; per-peer drop
  signalling is handled inline by the reader thread's local
  exit-on-error code path (it does not push a separate
  `PeerDropped` lifecycle item -- the connection-close on the peer
  side suffices for the driver's downstream logic).

The driver drains both channels via `try_recv` inside
`HybridVariant::poll_receive_multi`. T14.16 priority: the lifecycle
channel is drained FIRST on every call (loop until empty), then the
bounded data channel is drained up to `POLL_BUDGET = 256` items per
call. This guarantees that EOT observations are never starved by a
saturated data channel.

Lifecycle:

- `start_reader_threads` runs RIGHT AFTER `connect` returns. It
  first calls `accept_pending_with_buffer` in a 5 s busy-wait so
  inbound TCP connections are present before their reader threads
  spawn (the stabilize phase gives the other runner time to dial).
- `stop_reader_threads` is called by the driver BEFORE `disconnect`.
  It flips the shared `AtomicBool`, calls `shutdown(Both)` on each
  per-peer TCP read-side handle (to wake blocked reads), flips the
  UDP recv socket to non-blocking (to wake the blocked recv), and
  joins handles with a 2 s budget per handle via
  `JoinHandle::is_finished` polling (detaches anything still
  blocked).

**`--recv-buffer-kb` plumbing (T14.1 / T14.4)**: every recv-side
socket gets `SO_RCVBUF = recv_buffer_kb * 1024` applied:

- primary UDP socket via `UdpTransport::apply_recv_buffer_kb` in
  `connect`;
- dedicated Multi-mode UDP recv socket at creation time
  (`make_blocking_recv_socket`);
- every outbound TCP socket inside `TcpTransport::connect_to_peer`;
- every inbound TCP socket inside `TcpTransport::accept_pending`
  (and the Multi-mode `accept_pending_with_buffer`).

The user-tunable knob OVERRIDES the implicit 8 MiB target from
`variant_base::tune_udp_buffers` (which is the default). A
warning is emitted if the achieved `SO_RCVBUF` lands below the
requested value (e.g. Windows silently clamping).

**TCP connect race fix**: both runners pass the ready barrier and
call `connect_to_peer` near-simultaneously; either side's listener
may not be bound yet. `TcpTransport::connect_to_peer` now retries
on `ConnectionRefused` / `TimedOut` / `WouldBlock` for a bounded
5 s budget (mirrors the websocket variant's `ws_client_connect`).
Without this retry, the qos4-multi spawn flakes ~50% of the time
on Windows even on localhost.

### Testing

- Unit test: message serialization/deserialization.
- Unit test: QoS 2 stale-discard logic.
- Unit test (T14.4 `reader.rs`): `ReaderHub::new`,
  `stop_and_join` on empty hub, `push_or_drop` on
  disconnected / full channels.
- Integration test: single-process loopback. Synthesize the new CLI shape:
  `--peers self=127.0.0.1`, `--runner self`, `--multicast-group 239.0.0.1:<port>`,
  `--tcp-base-port <port>`, `--qos <1..4>`. Note that with a single-peer
  map, there are no peers to connect to (self is excluded by design); the
  test exercises bind/listen and message framing only. Cross-peer flow is
  validated end-to-end via two runners on localhost during T9.3 acceptance.
- Two-runner regression (existing): `two_runner_regression_correctness_sweep`,
  `two_runner_regression_highrate_no_cascade` (Single mode only, the
  current behaviour these were written against).
- Two-runner regression (T14.4): `two_runner_threading_modes_qos4_both_modes`
  in `tests/two_runner_threading_modes.rs` exercises the
  `threading_modes = ["single", "multi"]` expansion at
  100 vpt * 100 Hz = 10 K msg/s symmetric on QoS 4. Asserts non-zero
  cross-receives in both modes; does NOT assert a delivery threshold
  for Single mode (per the T14.4 task spec, the actual figure is
  just recorded in the test output).
