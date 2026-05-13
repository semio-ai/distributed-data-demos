# WebSocket Variant — Custom Instructions

> **T15.8 (E15 cleanup):** The on-wire EOT exchange driven by the
> `Variant::signal_end_of_test` / `Variant::poll_peer_eots` trait
> methods was removed. Sections below that describe the T14.20
> control-channel work and EOT routing in the "Threading modes"
> subsection are **historical**. End-of-operate is now driven by
> variant-base's idle detection (T15.5) and the runner-coordinated
> termination state machine (T15.4). The `eot_sent` JSONL event is
> still emitted exactly once per spawn between operate and silent
> (the marker analysis T11.5 / T14.17 consume).

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

All commands run from the repo root (Cargo workspace). Do **not** `cd` into
`variants/websocket/` to build — that produces a stray per-subfolder
`target/` directory which the configs do not point at.

```
cargo build --release -p variant-websocket
cargo test --release -p variant-websocket
cargo clippy --release -p variant-websocket -- -D warnings
cargo fmt -p variant-websocket -- --check
```

Compiled binary lives at `target/release/variant-websocket(.exe)`.

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

#### Same-host port-collision guarantee (T-impl.4)

The `runner_index * runner_stride` term in the listen-port formula
exists specifically so that two runners co-located on the same host
(alice + bob with `--peers alice=127.0.0.1,bob=127.0.0.1`) end up
binding **different** TCP ports. With `--ws-base-port 19960` and
`qos = 3`, alice binds `19980` and bob binds `19981`; neither can
ever observe an `EADDRINUSE` from the other on the same host.

This is verified by two test layers:

1. Unit test `t_impl_4_same_host_port_offset_alice_and_bob` in
   `src/pairing.rs` asserts that `bob.listen_addr.port() -
   alice.listen_addr.port() == RUNNER_STRIDE` for the canonical
   two-runner same-host peer map.
2. Ignored integration test
   `two_runner_websocket_same_host_qos3_no_port_collision` in
   `tests/two_runner_regression.rs` spawns two runner child processes
   on localhost against `tests/fixtures/two-runner-websocket-100x100hz-qos3.toml`
   and asserts both runners produce non-zero `write` AND non-zero
   cross-`receive` counts inside their operate windows. Run via:
   `cargo test --release -p variant-websocket -- --ignored two_runner_regression`.

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

## Threading modes (T14.2 + T14.10)

The variant declares `supported_threading_modes() = &[Single, Multi]`
and branches its IO model based on the `--threading-mode` CLI flag
captured at `connect` time. `start_reader_threads(mode)` /
`stop_reader_threads()` are the per-spawn lifecycle hooks called by
`variant-base`'s driver around connect/disconnect.

T14.10 reorganises the Multi-mode receive path: reader threads now
write `receive` JSONL events directly via a shared `LoggerHandle`
attached by the driver before reader-thread spawn. The bounded mpsc
that previously carried decoded `Data` frames is now a fixed-size
lifecycle-only channel (`Eot`, `PeerDropped`); data frames bypass it
entirely. See "T14.10 data flow" below for the rationale.

### When each mode is chosen

- **Single** mode: pre-E14 behaviour. The driver thread does inline
  reads + writes via tungstenite. Suitable for low-rate workloads
  (e.g. 100 vpt * 100 Hz = 10 K msg/s) where the driver can interleave
  publish + drain within the tick budget without falling behind. Single
  mode is the default and is what runs when `--threading-mode` is
  absent (T14.1 rollout phase) or set to `single`.
- **Multi** mode: per-peer OS reader thread + bounded mpsc + driver-side
  `try_recv`. Suitable for high symmetric rates (e.g. 1000 vpt * 100 Hz
  = 100 K msg/s) where Single mode's inline-read budget is exhausted
  by `publish` overhead and the driver can no longer drain the recv
  buffer in time. Multi mode is the deadlock-breaking path for the
  T-impl.10 residual failure.

### Reader-thread ownership model

For each peer, after the WS handshake completes, the variant captures
the underlying `TcpStream` and:

- In Single mode: stores the full `WebSocket<TcpStream>` inside the
  peer record. The driver thread calls `ws.read()` / `ws.send()`
  directly.
- In Multi mode: clones the underlying `TcpStream` (via
  `TcpStream::try_clone`), hands the original `WebSocket<TcpStream>`
  to a dedicated reader thread, and stores the cloned stream + a
  separate `WebSocketContext` (write-side framing state, Role-matched
  to the per-pair role) inside the peer record behind an
  `Arc<Mutex<MultiWriter>>`. The driver thread takes the writer mutex
  briefly for each `publish` call. The reader thread loops on
  `WebSocket::read` with the short SO_RCVTIMEO and pushes decoded
  frames into a shared `SyncSender<ReaderItem>`.

The mutex serialises outbound frames so two concurrent publishers
never interleave WebSocket frame bytes on the wire (illegal framing).
The reader thread never writes to the shared TCP socket from
publish-bytes -- only tungstenite-internal auto-pong responses go
through the read-side socket, which is the same kernel TCP connection
as the write-side clone so pongs reach the peer correctly.

### T14.10 data flow -- log-from-reader

Pre-T14.10 the reader thread pushed decoded `Data` frames into the
shared bounded mpsc and the driver thread popped them off to call
`Logger::log_receive`. At high symmetric rates (1000 vpt at 100 Hz,
i.e. 100 K msg/s per direction) the driver's publish path consumed
nearly the entire 10 ms per-tick budget, leaving microseconds to
drain the channel. The bound filled, the reader dropped data items
(drop-on-full), and JSONL `receive` counts collapsed to ~28 % even
though every frame had been parsed off the wire. This violates the
project's stated goal that **every received message must be logged**
(`metak-shared/overview.md` "Cross-cutting goals").

T14.10 moves `receive` logging onto the reader thread:

- `variant-base` introduces a `LoggerHandle` type
  (`Arc<Mutex<Logger>>` wrapper) and a `Variant::attach_logger` trait
  hook. The driver wraps its `Logger` in a `LoggerHandle` and calls
  `variant.attach_logger(handle.clone())` between `connect` and
  `start_reader_threads`.
- The websocket variant stores the handle and clones it into each
  reader thread at spawn time.
- Inside `reader_thread_main`, after `protocol::decode_frame(...)`
  produces a `Frame::Data(update)`, the reader calls
  `logger.log_receive(...)` directly. The frame is then forgotten;
  it never touches the mpsc.
- The mpsc is now **lifecycle-only**: it carries `ReaderItem::Eot`
  and `ReaderItem::PeerDropped`. A fixed `LIFECYCLE_CHANNEL_CAPACITY`
  of 256 is comfortably larger than the worst-case lifecycle event
  count per spawn (`peer_count` Eot markers + optional drops).

Logger mutex contention is the new bottleneck cliff. Each
`log_receive` call serialises one JSONL line write (microseconds on
the buffered file path) under the shared mutex. Empirically this
moves the cliff far above 100 K msg/s symmetric -- the workload
that originally exposed T-impl.10.

### Bounded-channel rationale (lifecycle-only, post-T14.10)

The shared mpsc is sized to a fixed `LIFECYCLE_CHANNEL_CAPACITY`
(256). Drop-on-full is no longer relevant: the channel never sees
high-rate data items.

- **EOT items use blocking-send with shutdown escape.** EOT markers
  are critical for the end-of-test synchronization; dropping one
  forces the peer's driver to wait the full `eot_timeout`. The
  channel has more than enough headroom that blocking would never
  fire in practice, but the safety remains for malformed inputs.
- **PeerDropped items use blocking-send.** Best-effort delivery is
  fine; if the variant is being torn down the driver no longer cares.

### Ordering and observability under T14.10

Receive events from N reader threads now interleave in the JSONL
file. The downstream analysis groups events by `(variant, run,
writer, seq, path)`, so wall-clock ordering across writers does not
matter (it was already non-deterministic on a multicore machine).
Within a single writer's stream the reader thread processes frames
sequentially, so per-writer JSONL ordering is preserved.

Driver-side events (`phase`, `write`, `eot_sent`, `eot_received`,
`resource`, `eot_timeout`) interleave with reader-side `receive`
events under the same shared mutex. The lock is held only for the
duration of one `write_line` call, so contention is minimal.

### `stop_reader_threads` semantics

Called by the driver immediately before `disconnect`. Sets an
`AtomicBool` shutdown flag and drops the variant's retained sender so
the receiver will eventually observe `Disconnected` on drain. Each
reader thread checks the flag every ~1 ms (between SO_RCVTIMEO-bounded
reads) and exits on the next iteration. The join uses a 2 s wallclock
budget per thread; if a reader is wedged inside a long Windows
overlapped-recv beyond that, we log a warning and abandon the thread
-- Rust will tear it down at process exit. This is the documented
Windows caveat from T-impl.8.

### `SO_RCVBUF` tuning

In BOTH modes, immediately after the WS handshake completes, the
variant calls `setsockopt(SO_RCVBUF, recv_buffer_kb * 1024)` on the
underlying TCP socket via `socket2::Socket`. On failure (some
OSes silently cap very large requests) the variant logs a warning
and continues with the kernel default. `recv_buffer_kb` is plumbed
through from `variant-base`'s `--recv-buffer-kb` CLI arg (default
4096 / 4 MiB).

### Single-mode TCP wedge safety net (T14.19)

At catastrophic symmetric load on QoS 3/4 (the 2026-05-12 stress
run at 1000 vpt x 100 Hz = 100K msg/s symmetric on localhost),
Single mode can mutually deadlock: both runners spend the publish
phase inside tungstenite's blocking `write` while neither calls
`poll_receive` to drain the peer's recv buffer, the kernel TCP send
buffers fill on both sides, and both writes wedge in the kernel.
Pre-fix the runner timed the spawn out at `default_timeout_secs`.

The fix is a 5 s `SO_SNDTIMEO`
(`TcpStream::set_write_timeout(Some(...))`) installed on every
outbound TCP stream in Single mode -- and ONLY in Single mode.
Multi mode runs a dedicated reader thread per peer that drains the
recv buffer in parallel; the wedge does not occur and installing
`SO_SNDTIMEO` in Multi would only invite spurious peer-drops under
transient back-pressure. The `start_reader_threads` path explicitly
clears the timeout on the write-clone (`set_write_timeout(None)`)
to keep Multi mode's behaviour identical.

Installation lives in the `apply_single_mode_write_timeout` helper
in `src/websocket.rs`, called from `connect` after both
`ws_client_connect` and `ws_server_accept` complete. The
`SINGLE_WRITE_TIMEOUT` constant is 5 s.

When the timeout fires:

- the write returns `TimedOut` (Windows) / `WouldBlock` (Unix)
  wrapped in `tungstenite::Error::Io`,
- `broadcast_binary` drops the offending peer with a `warning:
  dropping WS peer ... after write error` log line,
- subsequent broadcasts have nothing to send (empty `self.peers`)
  but `broadcast_binary` returns `Ok(())` rather than the pre-fix
  "all WS peers dropped after write errors" Err that would have
  cascaded into a failed spawn.

The "Ok with no peers" relaxation matters because websocket has no
separate control side-channel for EOT (unlike custom-udp and
hybrid post-T14.18); once the data peer is dropped, EOT cannot
route. The driver's EOT phase will time out waiting for the peer's
EOT and log `eot_timeout`, the silent phase passes, and the spawn
exits `status=success`. Delivery is near-zero -- matching the
"log everything with bad latency" intent. The T14.17 classifier
marks the spawn `completed` (the eot_timeout entry tells operators
exactly what happened).

5 s is far longer than any realistic transient on a healthy LAN
(TCP_NODELAY + 4 MiB recv buffers); a timeout firing means the
peer is genuinely stuck. The integration regression
`tests/two_runner_t14_19_tcp_single_no_deadlock.rs` exercises the
deadlock workload and asserts both runners exit `status=success`
(delivery threshold deliberately NOT asserted). The unit test
`t14_19_broadcast_drops_peer_on_write_timeout_and_returns_ok` in
`src/websocket.rs` is the same property tested at the broadcast
function level.

## Historical notes

### Backpressure semantics (T-impl.7) -- superseded by T14.2 for high-rate workloads

T-impl.7 added `Variant::try_publish` so transports that can cheaply
detect backpressure can return `Ok(false)` and let the driver log a
`backpressure_skipped` event instead of fire-and-forget. The WebSocket
variant intentionally does NOT override `try_publish` -- the trait's
default implementation (delegate to `publish`, return `Ok(true)`) is
what runs.

This is correct under Single mode and remains the right policy for
the publish path under Multi mode as well: reliable QoS (3, 4) must
never return `Ok(false)` (that would create a receiver-visible seq
gap). The Multi-mode contribution is on the **receive** path -- the
reader thread drains and drop-on-full guards against the symmetric-
flood deadlock that the Single-mode "block the writer" policy alone
could not break.

Rationale for keeping the default `try_publish`:

- The variant supports **only reliable QoS** (3 and 4); QoS 1 and 2
  are rejected outright at `publish` and at `connect` time. There is
  no unreliable code path that could benefit from a skip.
- For reliable QoS, returning `Ok(false)` would create a
  receiver-visible seq gap.
- Genuine connection failure is reported via the existing `Err(...)`
  return path in `publish`, not via `Ok(false)`.

### Cross-reference: T-impl.10 receive-drain widening -- still in place

T-impl.10 widened the driver's per-tick receive-drain budget in
`variant-base/src/driver.rs`. That widening still applies in both
modes and is complementary to T14.2: the driver's `try_recv` in Multi
mode is fast, but the channel can still build up if the driver is
busy publishing during the tick.

## Out of scope

- TLS / `wss://`.
- WebSocket subprotocols, extensions (compression, etc.).
- HTTP/2 WebSockets (RFC 8441).
- QoS 1 and 2 over UDP — this is Hybrid's role.
- mDNS or any peer discovery beyond `--peers`.
