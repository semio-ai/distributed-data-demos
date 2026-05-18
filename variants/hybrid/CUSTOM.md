# Hybrid UDP/TCP Variant — Custom Instructions

> **T15.8 (E15 cleanup):** The on-wire EOT exchange and the dedicated
> control TCP side-channel (T14.18) were removed. Sections below that
> describe `signal_end_of_test`, `poll_peer_eots`, `--control-base-port`,
> `control_base_port`, the `controltcp` module, and EOT routing in the
> "Threading modes" subsection are **historical**. End-of-operate is
> now driven by variant-base's idle detection (T15.5) and the runner-
> coordinated termination state machine (T15.4). The `eot_sent` JSONL
> event is still emitted exactly once per spawn between operate and
> silent (the marker analysis T11.5 / T14.17 consume).

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
tcp_base_port     = 19900
control_base_port = 20100  # T14.18
```

Variant-specific CLI args:

- `--multicast-group <ip:port>` — required. UDP multicast group address.
  Same value used by all runners; no runner or QoS stride applied.
- `--tcp-base-port <u16>` — required. Base port that per-runner / per-qos
  TCP ports are derived from.
- `--control-base-port <u16>` — **required (T14.18)**. Base port for the
  per-peer-pair TCP control side-channel that carries EOT frames
  independently of the data path. See "Control side-channel (T14.18)"
  below for the derivation formula and rationale.

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
  - Since **T17.4**, the socket is in **non-blocking** mode
    (`set_nonblocking(true)`). Both clones of the stream share the
    `FIONBIO` flag (it is socket-wide on Windows), so reads AND writes
    return `WouldBlock` / `TimedOut` immediately when the kernel
    would otherwise stall. The strict-delivery write loop in
    `TcpTransport::broadcast` retries indefinitely on `WouldBlock`
    -- a transient back-pressure is the application-level signal the
    benchmark exists to measure (DESIGN.md § 6.5). Only true I/O
    errors (`ConnectionReset`, `BrokenPipe`, etc.) drop the peer.
  - In **single mode** the broadcast loop runs an inline
    read-drain pass between write retries (`inline_drain_into_pending`).
    This breaks the symmetric-saturation wedge that previously
    forced the T16.3 SO_SNDTIMEO + peer-drop workaround: both
    peers can spend the publish phase inside the loop, but each
    drains its own incoming frames while waiting for its own
    writes to flush. Drained frames are stashed on
    `TcpTransport::pending_drained` and surfaced by the next
    `try_recv` call (the variant's `poll_receive` sees them in
    arrival order).
  - In **multi mode** the per-peer reader thread drains in
    parallel; the broadcast loop only retries on `WouldBlock`
    without running an inline drain. The reader thread now uses
    `push_data_or_block` (blocking-on-full) on the data channel
    instead of the pre-T17.4 drop-on-full -- a full bounded
    channel back-pressures the kernel TCP recv buffer, which
    back-pressures the peer's writes, which surfaces as the same
    application-level signal as in single mode.
  - The read handle (obtained via `try_clone`) keeps a short
    `SO_RCVTIMEO` for the rare path that re-enables blocking
    mode during teardown. With the socket non-blocking, the
    timeout is effectively advisory: reads return `WouldBlock`
    immediately.
- **Why non-blocking writes?** Without them, both peers can stall
  inside `write_all` while neither calls `poll_receive` to drain
  the kernel recv buffer. The pre-T17.4 fix dropped the offending
  peer after a `SO_SNDTIMEO` fired, but dropping a peer at QoS 3/4
  loses every undelivered message to it -- a violation of DESIGN.md
  § 6.5. Non-blocking writes + inline drain (single) / blocking
  channel send (multi) instead absorb the back-pressure with zero
  message loss.
- Set `TCP_NODELAY` on every connection to disable Nagle -- critical for
  latency.

### TCP read — buffered-frame fast path (T16.3)

`TcpPeer::try_recv_framed` extracts a buffered frame from
`read_buf` **before** issuing another `read()` syscall. Each
read syscall costs up to `READ_POLL_TIMEOUT` (1 ms) of wall
clock when the kernel recv buffer is empty (because the read
handle has `SO_RCVTIMEO`); doing that syscall while we already
have a complete frame in the internal buffer was burning 1 ms
per buffered message under load. At 1 000 msg/s symmetric on
QoS 3/4 in Single mode this capped the drain at ~1 000
calls/s and starved the receive path — the exact mechanism
behind the original T16.3 catastrophic-delivery report
(2.62 % at 100x100hz pre-fix). With the fast path, the driver
drains a batch of frames the kernel delivered in a single
read in microseconds and returns to publish promptly.

The slow path (read more from the kernel first when the
internal buffer is empty or only holds a partial frame) is
unchanged. The frame-extraction code is shared between both
paths via the `take_buffered_frame` helper.

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

### Backpressure semantics (T-impl.7 / T17.4)

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
- **QoS 3 (ReliableUdp / TCP path)** — `TcpTransport::broadcast`,
  which under T17.4 is non-blocking-with-internal-strict-retry.
  ALWAYS `Ok(true)`. Per DESIGN.md § 6.5, QoS 3/4 forbids
  skipping; the broadcast loop retries on `WouldBlock`
  indefinitely (while either running the inline drain pass in
  single mode or relying on the per-peer reader thread + blocking
  channel send in multi mode to drain on the receiver side),
  yielding 100 % delivery at the cost of throttled throughput.
- **QoS 4 (ReliableTcp / TCP path)** — identical to QoS 3 in this
  variant. Always `Ok(true)`.

Where it lives:

- `src/udp.rs::UdpTransport::try_send_nonblocking` — single attempt,
  `WouldBlock` surfaces as `Ok(false)`.
- `src/hybrid.rs::Variant::try_publish` — dispatches by QoS.
- `src/tcp.rs::TcpTransport::broadcast` — non-blocking writes,
  strict retry on `WouldBlock`, inline drain pass in single mode.
- `src/reader.rs::push_data_or_block` — TCP reader thread (multi
  mode) blocks on a full data channel instead of dropping.
- `src/hybrid.rs::Variant::publish` — unchanged; same path as
  `try_publish` for QoS 3/4.

### Control side-channel (T14.18)

Hybrid establishes a **per-peer-pair TCP control connection** at
`connect()` time, separate from the data path (UDP multicast for
qos1-2 and TCP per-pair for qos3-4). EOT markers are routed
exclusively over this control connection regardless of QoS. Source:
T14.18, after the all-variants 100K msg/s repro
(`logs/all-variants-01-20260512_093124/`) showed `eot_lost` on
Single mode under symmetric UDP saturation — the kernel UDP recv
buffer fills faster than userspace can drain, and the EOT datagram is
dropped at the kernel level. A separate TCP socket is the only way
to make EOT robust to data-path saturation in Single mode (we can't
just say "use Multi" because Single is a first-class WASM
requirement).

**Port derivation** (matches `derive_control_endpoints` in
`src/main.rs`):

```
runner_stride = 1
my_control_listen = control_base_port + runner_index * runner_stride
                                              # NO QoS stride.
```

One control port per (runner, variant binary). The TOML field
`control_base_port` is required.

**Pairing** (same convention as Hybrid TCP / QUIC / WebSocket): the
lower-sorted-name peer in a pair is the **server** (accepts on its
derived port). The higher-sorted peer is the **client** (dials the
server's port). Both sides set `TCP_NODELAY` immediately. Bounded
retry on `ConnectionRefused` (30 s budget) so the two runners can
race past the ready barrier without one's connect failing before the
other's listener is bound.

**Wire format** (length-prefixed binary, see
`src/controltcp.rs`):

```
[u32 BE length] [tag: u8] [tag-specific payload]
```

Tags:
- `0x01` — EOT marker. Payload is the existing `protocol::encode_eot`
  bytes (same `(writer, eot_id)` shape used on the pre-T14.18 data
  path).
- `0x02` — `bye` marker. No payload follows. Sent during
  `disconnect()` so the peer's read side gets a clean EOF.

Frames are capped at 4 KiB (`MAX_CONTROL_FRAME_BYTES`).

**Threading**:

- **Multi mode**: one dedicated OS reader thread per control
  connection (`src/controltcp.rs::spawn_control_reader`). The thread
  reads length-prefixed frames in a blocking loop with
  `MULTI_MODE_READ_TIMEOUT = 200 ms` and pushes decoded EOT markers
  onto the existing T14.16 `lifecycle_tx` channel as
  `HubLifecycleMessage::Eot { writer, eot_id }`. The data path is
  unchanged.
- **Single mode**: control socket is BLOCKING with a short
  `SO_RCVTIMEO` (`SINGLE_MODE_READ_TIMEOUT = 1 ms`). The variant's
  `poll_receive` polls each control peer inline via
  `ControlPeer::try_recv_frame` before draining the data path. The
  inline poll budget is microseconds per tick; no additional threads
  are spawned. **Worker chose non-blocking polling over a dedicated
  Single-mode control thread** because the per-tick cost is
  negligible and the WASM-compatibility story is cleaner (data path
  remains strictly single-threaded; the only auxiliary fd is the
  control socket polled inline).

**Lifecycle teardown** (in `disconnect`):

1. Send a `bye` frame on every surviving control peer.
2. `shutdown(Write)` the local write side so the peer's read sees
   EOF after draining in-flight frames.
3. Drain the read side until peer closes or `--eot-timeout-secs`
   elapses (default 5 s, configurable via TOML). Any EOT frame that
   arrives during the drain (typically a last EOT racing our own
   `bye`) is applied. Single mode only — Multi mode already stopped
   the control reader threads in `stop_reader_threads`.
4. Drop the stream.

**Removed**: pre-T14.18 EOT-over-multicast (qos1-2) and EOT-on-data-
TCP (qos3-4) paths. `protocol::encode_eot_framed` is retained for
tests only; `protocol::encode_eot` is still used as the inner payload
of the control EOT frame.

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
  `reader::READER_CHANNEL_CAPACITY = 4096`).
  - **UDP reader (QoS 1/2)**: uses `push_data_or_drop` (`try_send`
    with drop-on-full). QoS 1/2 tolerate loss by definition; a
    full channel is treated as best-effort drop. Warning:
    `[variant-hybrid] data channel full (... slots) -- dropping Data frame (receiver saturated)`.
  - **TCP reader (QoS 3/4, T17.4)**: uses `push_data_or_block`
    (blocking-on-full with shutdown-flag polling). Dropping
    here would silently violate the strict no-skip contract
    (DESIGN.md § 6.5). Blocking the reader propagates back-
    pressure through the kernel TCP recv buffer to the peer's
    `write_all`, which surfaces as the same application-level
    signal as in Single mode. The bounded capacity bounds
    in-memory growth on a slow driver; the kernel recv buffer
    plus the peer's send buffer absorb the rest.
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

### Strict-delivery delivery + throughput characterization (T17.4)

After the T17.4 strict-delivery fix (non-blocking writes with
inline drain in Single mode, blocking channel send in Multi mode,
no peer-drop on transient back-pressure), both threading modes
achieve **100 % delivery** at QoS 3/4 across the saturate-repro
matrix. Throughput falls under saturation -- the trade per
DESIGN.md § 6.5.

Numbers from
`variants/hybrid/tests/fixtures/two-runner-hybrid-qos4-saturate-repro.toml`
and `-pt2.toml` on Windows localhost (alice + bob in the same
machine; `silent_secs = 10` so in-flight TCP bytes can drain
before disconnect):

| Workload            | Target rate  | Mode    | Delivery | Actual writes/s (req'd, throttled) |
| ------------------- | ------------ | ------- | -------- | ---------------------------------- |
| `100x100hz`-qos3    |  10 K msg/s  | single  | 100.00 % | 20 K msg/s (no throttle needed)    |
| `100x100hz`-qos3    |  10 K msg/s  | multi   | 100.00 % | 15 K msg/s                         |
| `100x100hz`-qos4    |  10 K msg/s  | single  | 100.00 % | 18 K msg/s                         |
| `100x100hz`-qos4    |  10 K msg/s  | multi   | 100.00 % | 20 K msg/s                         |
| `1000x100hz`-qos3   | 100 K msg/s  | single  | 100.00 % | 33 K msg/s (throttled)             |
| `1000x100hz`-qos3   | 100 K msg/s  | multi   | 100.00 % | 100 K msg/s                        |
| `1000x100hz`-qos4   | 100 K msg/s  | single  | 100.00 % | 12 K msg/s (heavily throttled)     |
| `1000x100hz`-qos4   | 100 K msg/s  | multi   | 100.00 % |  80 K msg/s                        |
| `100x1000hz`-qos3   | 100 K msg/s  | single  | 100.00 % | 37 K msg/s                         |
| `100x1000hz`-qos3   | 100 K msg/s  | multi   | 100.00 % | 57 K msg/s                         |
| `100x1000hz`-qos4   | 100 K msg/s  | single  | 100.00 % | 62 K msg/s                         |
| `100x1000hz`-qos4   | 100 K msg/s  | multi   | 100.00 % | 13 K msg/s                         |

Zero `backpressure_skipped` events at QoS 3/4 across the matrix.

Pre-T17.4 the same workloads dropped 14-86 % at QoS 3/4 even in
Multi mode (kernel-recv buffer overflow into the bounded mpsc's
drop-on-full path), and Single mode either dropped peers via the
T16.3 SO_SNDTIMEO mechanism (losing every undelivered message to
the dropped peer) or fully deadlocked.

**Operational note**: under sustained saturation at QoS 3/4 the
spawn's `silent_secs` must be long enough for in-flight TCP
bytes to drain before `disconnect` runs. The reproducer fixture
uses `silent_secs = 10`; production configs targeting QoS 3/4
saturation should size `silent_secs` accordingly (rule of thumb:
~`kernel_send_buffer_size / actual_throughput * 2` -- a few
seconds for typical workloads).

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
