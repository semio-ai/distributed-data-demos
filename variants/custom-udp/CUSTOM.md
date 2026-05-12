# Custom UDP Variant — Custom Instructions

## Overview

Rust binary implementing the `Variant` trait from `variant-base` using raw
UDP sockets with a custom protocol. Represents the "from scratch" approach —
full manual control over transport, implementing all four QoS levels.

## Tech Stack

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`variant-custom-udp`)
- **Key dependencies**:
  - `variant-base` (path = `../../variant-base`) — Variant trait, types, CLI, driver
  - `socket2` — advanced socket configuration (SO_BROADCAST, SO_REUSEADDR, multicast)
  - `anyhow` — error handling
- Follow `metak-shared/coding-standards.md`.
- **No discovery library**: peer hosts come from the runner-injected
  `--peers` arg (since E9). mDNS was never wired up in code; remove the
  `mdns-sd` dependency from `Cargo.toml` if it is still listed.

## Build and Test

All commands run from the repo root (Cargo workspace). Do **not** `cd` into
`variants/custom-udp/` to build — that produces a stray per-subfolder
`target/` directory which the configs do not point at.

```
cargo build --release -p variant-custom-udp
cargo test --release -p variant-custom-udp
cargo clippy --release -p variant-custom-udp -- -D warnings
cargo fmt -p variant-custom-udp -- --check
```

Compiled binary lives at `target/release/variant-custom-udp(.exe)`.

## Architecture

```
variants/custom-udp/
  src/
    main.rs       -- parse CLI, create UdpVariant, call run_protocol
    udp.rs        -- UdpVariant struct implementing Variant trait
    protocol.rs   -- message framing, header serialization
    qos.rs        -- QoS-specific send/receive logic
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

As of E9, peer hosts are runner-injected via the standard `--peers`. The
old variant-specific `--peers` (host:port list) and `--bind-addr` are
removed. The variant derives its own TCP listen port (for QoS 4) and each
peer's TCP connect port from `--tcp-base-port`, the `--peers` map, and
the per-spawn `--qos`.

```toml
[variant.specific]
multicast_group   = "239.0.0.1:19500"
buffer_size       = 65536
tcp_base_port     = 19800
control_base_port = 20000  # T14.18
```

Variant-specific CLI args:

- `--multicast-group <ip:port>` — required. UDP multicast group address.
  Same value used by all runners; no runner or QoS stride applied.
- `--buffer-size <bytes>` — required. UDP receive buffer size.
- `--tcp-base-port <u16>` — required. Base port that per-runner / per-qos
  TCP ports are derived from (used only at QoS 4).
- `--control-base-port <u16>` — **required (T14.18)**. Base port for the
  per-peer-pair TCP control side-channel that carries EOT frames
  independently of the data path. See "Control side-channel (T14.18)"
  below for the derivation formula and rationale.

The variant also reads (from the standard runner-injected args, see
`metak-shared/api-contracts/variant-cli.md`):

- `--peers <name1>=<host1>,<name2>=<host2>,...` — full runner→host map.
  Sort by name for stable indexing.
- `--runner <name>` — this runner's name; used to look up own index.
- `--qos <N>` — concrete QoS level for this spawn (1-4).

### Port derivation

For QoS 1-3 (UDP-only paths): bind on `multicast_group` directly. No
runner stride, no QoS stride. All runners join the same group; sequential
per-spawn execution + `silent_secs` drain + `inter_qos_grace_ms` provide
cross-spawn isolation.

For QoS 4 (TCP):
```
runner_stride = 1
qos_stride    = 10  // (qos - 1) * stride; for qos=4 this is 30

runner_index    = sorted_peer_names.position(of: --runner)
my_tcp_listen   = tcp_base_port + runner_index * runner_stride + (qos - 1) * qos_stride

for each (name, host) in --peers where name != --runner:
    peer_index    = sorted_peer_names.position(of: name)
    peer_tcp_port = tcp_base_port + peer_index * runner_stride + (qos - 1) * qos_stride
    connect_to    = (host, peer_tcp_port)
```

This is the same convention used by Hybrid (and QUIC for its own bind/connect
ports). Documented in `metak-shared/api-contracts/toml-config-schema.md` —
keep the strides consistent if they ever change.

If `--runner` is not present in `--peers`, fail loudly with a clear error.

For QoS 3 (NACK-based reliable UDP): NACKs and retransmits travel on the
existing UDP socket — no peer-host knowledge required from `--peers`.

### connect

1. Parse `--peers`, `--runner`, `--qos`, `--multicast-group`, `--buffer-size`,
   `--tcp-base-port`. Resolve `runner_index` and (only at QoS 4) derive
   `my_tcp_listen` and the list of `(peer_name, peer_host, peer_tcp_port)`
   tuples per "Port derivation".
2. Bind a UDP socket (multicast-capable via socket2).
3. Join the multicast group.
4. For QoS 4 (TCP): bind a TCP listener on `0.0.0.0:my_tcp_listen` and
   connect to every peer's `(peer_host, peer_tcp_port)`.

### publish

- **QoS 1 (best-effort)**: Send to multicast group. Fire and forget.
- **QoS 2 (latest-value)**: Same as QoS 1 but include seq in header.
- **QoS 3 (reliable-UDP)**: Send to multicast + buffer the message for
  potential retransmit. Listen for NACKs from receivers.
- **QoS 4 (reliable-TCP)**: Send over the TCP connection to each peer.

### poll_receive

- Check the UDP socket for incoming datagrams (non-blocking `recv_from`).
- Parse the header to extract writer, seq, path, qos, payload.
- **QoS 2**: Track highest seq per writer, discard stale.
- **QoS 3**: Detect gaps, send NACK to writer, buffer out-of-order.
- **QoS 4**: Read from TCP streams.
- Return one `ReceivedUpdate` per call, or `None` if nothing pending.

### Message format

```
[header: 4 bytes total_len | 1 byte qos | 8 bytes seq | 2 bytes path_len | N bytes path | 2 bytes writer_len | M bytes writer] [payload bytes]
```

Keep it compact — these are small messages at 100K/sec. Avoid serde for the
wire format; manual byte packing is faster and simpler for fixed-layout headers.

The minimum-valid-frame size is 17 bytes (`HEADER_FIXED_SIZE` in
`src/protocol.rs` = 4 + 1 + 8 + 2 + 2). Any frame smaller than that cannot
contain a complete header and is invalid by construction.

### Framing safety

Any length-prefixed reader (today the QoS-4 TCP path in
`src/udp.rs::read_framed_message`, but the rule applies to any future
length-prefixed transport) MUST validate that the declared length
`total_len` from the wire satisfies:

```
HEADER_FIXED_SIZE <= total_len <= max_buffer_size
```

before allocating a buffer of that size. Anything else from the wire is a
peer protocol violation (or a torn cross-machine read masquerading as one)
and MUST be handled by:

1. Logging a single `eprintln!` with a short reason.
2. Dropping that peer's stream.
3. Continuing — never panic, never propagate the error up to the spawn
   driver.

Why: on loopback the kernel atomically tears down both ends of a TCP
connection, so `read_exact` either delivers a complete frame or returns
EOF. Across the network there is a real window where `read_exact` returns
`Ok(())` with stale or zero bytes that decode as a 0..=3 length prefix.
Without the bounds check, `vec![0u8; total_len]` followed by
`msg_buf[..4].copy_from_slice(&len_buf)` panics for `total_len < 4`. This
is the regression that hit the user on the cross-machine `custom-udp-
10x1000hz-qos4` spawn (LEARNED.md "Cross-machine validation reveals
failures invisible on localhost"; TASKS.md T10.4).

Treat reads of `total_len > max_buffer_size` the same way (drop peer): a
peer that asks us to allocate more than `--buffer-size` bytes is buggy
or hostile, and silent truncation is worse than dropping the stream.

### Control side-channel (T14.18)

Custom-UDP establishes a **per-peer-pair TCP control connection** at
`connect()` time, separate from the data path (multicast UDP for
QoS 1-3 / TCP per-pair for QoS 4). EOT markers are routed exclusively
over this control connection regardless of QoS. Source: T14.18, after
the all-variants run (`logs/all-variants-01-20260512_093124/`) showed
custom-udp Single mode losing EOT under symmetric UDP saturation at
100K msg/s — the kernel UDP recv buffer fills faster than the inline
`poll_receive` can drain it, and the EOT datagram is dropped at the
kernel level before the application sees it. A separate TCP socket is
the only way to make EOT robust to data-path saturation in Single
mode (Multi mode would also benefit, but the data path is fixed to
single-threaded in Single mode by the WASM-compatibility goal).

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
retry on `ConnectionRefused` (30 s budget) absorbs the race past the
ready barrier.

**Wire format** (length-prefixed binary, see
`src/controltcp.rs`):

```
[u32 BE length] [tag: u8] [tag-specific payload]
```

Tags:
- `0x01` — EOT marker. Payload is the existing
  `protocol::encode_eot` bytes (same `(writer, eot_id)` shape used on
  the pre-T14.18 data path).
- `0x02` — `bye` marker. No payload follows. Sent during
  `disconnect()` for a clean EOF on the peer's read side.

Frames are capped at 4 KiB (`MAX_CONTROL_FRAME_BYTES`).

**Threading**:

- **Multi mode**: one dedicated OS reader thread per control
  connection (`src/controltcp.rs::spawn_control_reader`). The thread
  reads length-prefixed frames in a blocking loop with
  `MULTI_MODE_READ_TIMEOUT = 200 ms` and pushes decoded EOT markers
  onto the existing T14.16 `lifecycle_tx` channel as
  `ReaderLifecycleItem::Eot(EotFrame)`. The data path is unchanged.
- **Single mode**: control socket is BLOCKING with a short
  `SO_RCVTIMEO` (`SINGLE_MODE_READ_TIMEOUT = 1 ms`). The variant's
  `poll_receive` polls each control peer inline via
  `ControlPeer::try_recv_frame` before draining UDP and (at QoS 4)
  TCP data. The inline poll budget is microseconds per tick; no
  additional threads are spawned. **Worker chose non-blocking
  polling over a dedicated Single-mode control thread**: the
  per-tick cost is negligible and keeps the WASM-compatibility story
  clean (data path remains strictly single-threaded; the only
  auxiliary fd is the control socket polled inline).

**Lifecycle teardown** (in `disconnect`):

1. Send a `bye` frame on every surviving control peer.
2. `shutdown(Write)` the local write side so the peer's read sees
   EOF after draining in-flight frames.
3. Drain the read side until peer closes or `--eot-timeout-secs`
   elapses (default 5 s, configurable via TOML). Single mode only —
   Multi mode already stopped its control reader threads in
   `stop_reader_threads`. Any EOT frame that arrives during the
   drain (typically a last EOT racing our own `bye`) is applied.
4. Drop the stream.

**Removed**: pre-T14.18 EOT-over-multicast (qos1-3) and EOT-on-data-
TCP (qos4) paths. The internal `send_eot` helper is now `#[allow
(dead_code)]` and retained only as historical reference;
`signal_end_of_test` writes the marker exclusively through the
control connection. Receive-side decoding of `Frame::Eot` on the
data path is kept for defence-in-depth (a pre-T14.18 peer would
still be handled correctly) but no longer the primary path.

### Threading modes (T14.3 + T14.16)

Custom-UDP declares `[Single, Multi]` via
`Variant::supported_threading_modes`. Both modes share `connect` /
`disconnect`; the difference is on the receive side.

**Single mode** (default; pre-E14 behaviour).

- `start_reader_threads` / `stop_reader_threads` are no-ops.
- `poll_receive` reads the UDP multicast socket via non-blocking
  `recv_from` and (at QoS 4) the inbound TCP streams via lazy accept +
  short-deadline framed reads, all on the driver thread.

**Multi mode** (T14.3 addition; T14.16 channel split).

- `start_reader_threads(Multi)` spawns one OS thread per recv-side
  socket:
  - one UDP reader thread driving a *clone* of the multicast socket
    (`UdpSocket::try_clone` so the variant keeps a non-blocking handle
    for `publish`), switched to blocking with `SO_RCVTIMEO = 50 ms` so
    it can periodically wake to observe the shutdown flag.
  - at QoS 4: one thread per accepted inbound TCP peer. Inbound
    streams are *pre-accepted* synchronously during
    `start_reader_threads` (up to `tcp_peers.len()` of them, with a 30
    s timeout) and then moved into per-peer reader threads; the
    listener is dropped afterwards. The same `SO_RCVTIMEO = 50 ms`
    pattern is used.

**Two-channel architecture (T14.16).**

Pre-T14.16 the variant used a single bounded `sync_channel` for every
`ReaderItem` variant (`Data`, `Eot`, `Nack`, `TcpPeerDropped`). The
all-variants 100 K msg/s qos2 smoke surfaced an asymmetric same-host
timeout: one runner saturated its bounded reader channel under
sustained load and the saturated `try_send` dropped not only Data
frames but the peer's `Eot` marker, forcing the peer's driver to wait
the full `eot_timeout` and exit with `status=timeout`. T14.16 splits
the reader-thread mpsc into two channels so EOT can never be dropped:

- **Data channel** (`data_tx` / `data_rx`): bounded
  `mpsc::sync_channel` carrying `ReaderDataItem::Data(Message)` only.
  Bound: `4 * values_per_tick * (peer_count + 1)` floored at
  `MULTI_CHANNEL_FLOOR` (16). Drop-on-full is acceptable here -- UDP
  is best-effort by definition, and QoS 3/4 protocols on the driver
  thread are unchanged because the dropped item simply never lands in
  `pending`. Reader threads use `try_send`; on `TrySendError::Full`
  the warning line is
  `[custom-udp] multi: data channel full -- dropping Data frame (receiver saturated)`
  (renamed from the pre-T14.16 wording so operators can be sure that
  EOT was NOT lost when this line appears).
- **Lifecycle channel** (`lifecycle_tx` / `lifecycle_rx`): unbounded
  `std::sync::mpsc::channel` carrying `ReaderLifecycleItem::Eot`,
  `Nack`, and `TcpPeerDropped`. Unbounded is safe because lifecycle
  items are infrequent (O(peers) per spawn for `Eot`; O(peers) total
  for `TcpPeerDropped`; O(gaps) for `Nack` -- only fired by the
  receiver's gap detector, which is rare on same-host fixtures).
  Reader threads use the blocking-by-API `Sender::send`, which on an
  unbounded channel never actually blocks: the only failure mode is
  receiver-dropped (driver tearing down), at which point the reader
  thread exits.

**NACK disposition (T14.16).** Worker folded `Nack` into the lifecycle
channel rather than introducing a third sibling. Rationale: NACKs are
rare (only emitted by the receiver's gap detector), losing them is
catastrophic for QoS-3 reliability (the receiver would never get the
retransmit), and one extra `std::sync::mpsc` channel keeps both the
wiring and the drain path straightforward. The drain order on the
driver thread is then "all lifecycle items first, regardless of
flavour, then bounded data drain" -- which keeps the priority
guarantee uniform across `Eot`, `Nack`, and `TcpPeerDropped`.

- `poll_receive` -> `drain_multi_channel` drains
  `lifecycle_rx.try_recv()` FIRST in an unconditional loop (lifecycle
  items are rare, drain to empty), then drains `data_rx.try_recv()`
  bounded by "first staged update". This keeps the
  one-update-per-`poll_receive`-call shape used by Single mode while
  guaranteeing EOT / NACK / PeerDropped observations are never starved
  by a saturated data channel.
- `stop_reader_threads` sets an `AtomicBool` flag, drops BOTH
  receivers (closes the channels so reader threads observe
  `Disconnected`), and joins each reader thread with a 2 s deadline
  per thread. Wedged threads are logged once and abandoned -- preferred
  over deadlocking the disconnect path.

**`SO_RCVBUF`** (both modes).

Honoured per `metak-shared/api-contracts/variant-cli.md` "E14
additions: `--recv-buffer-kb`":

- **UDP**: applied via `apply_recv_buffer_kb_udp` *as an upward floor
  only*. The pre-existing `tune_udp_buffers` helper requests 8 MiB
  for high-rate same-host fixtures; honouring a smaller
  `--recv-buffer-kb` value (default 4 MiB) would silently regress
  those tests, so the helper only upsizes the buffer above the
  current achieved value. The Multi-mode UDP clone re-applies
  `recv_buffer_kb * 1024` on the cloned descriptor unconditionally
  because socket options aren't always inherited via `dup`.
- **TCP**: applied unconditionally on every TCP socket the variant
  owns -- outbound `tcp_out_streams` from `setup_tcp`, lazily-accepted
  inbound streams in Single mode (`recv_tcp`), and synchronously-
  accepted inbound streams in Multi mode
  (`multi_accept_tcp_peers`). Kernel-default TCP SO_RCVBUF on Windows
  is much smaller than 4 MiB so this is an honest upsize.

**Out of scope for T14.3.**

- Auto-tuning the channel bound at runtime based on observed
  backpressure.
- Off-thread NACK retransmission. NACK handling stays on the driver
  thread (where `send_buffer` lives); only the parse / dedup step
  moves to readers.
- Per-thread CPU affinity. The OS scheduler decides.

### Single-mode TCP wedge safety net (T14.19)

At catastrophic symmetric load on QoS 4 (the 2026-05-12 stress run
at 1000 vpt x 100 Hz = 100K msg/s symmetric on localhost), Single
mode can mutually deadlock: both runners spend the publish phase
inside `write_all` on their outbound TCP stream while neither calls
`poll_receive` to drain the peer's recv buffer, the kernel TCP send
buffers fill on both sides, and both writes wedge in the kernel.
The T14.18 control channel cannot help because the variant thread
is stuck in the data-path syscall before reaching the EOT phase.

The fix is a 5 s `SO_SNDTIMEO` (`set_write_timeout(Some(...))`)
installed on every **outbound** TCP stream in Single mode -- and
ONLY in Single mode. Multi mode runs a dedicated reader thread per
peer that drains the recv buffer in parallel with the publisher, so
the wedge does not occur; installing `SO_SNDTIMEO` in Multi mode
would only introduce spurious peer-drops under transient back-
pressure. The Single-mode branch is gated on
`self.threading_mode == ThreadingMode::Single` in `setup_tcp`.

When the timeout fires:

- the write returns `TimedOut` (Windows) / `WouldBlock` (Unix),
- the existing per-write error branch in `publish_encoded` drops the
  peer from `tcp_out_streams` with a `[custom-udp] T14.19: dropping
  outbound TCP peer ... after write error` log line,
- subsequent writes have nothing to broadcast to (no peers) but
  `try_publish` still returns `Ok(true)` per the QoS 4 contract,
- the operate phase's time-bounded outer loop exits naturally,
- the T14.18 control side-channel (a SEPARATE socket from the data
  path) routes EOT cleanly, both sides observe `eot_received` on
  the other side, both exit `status=success`.

Delivery in this scenario is near-zero (matching hybrid's empirical
0.12 % at the same workload). The T14.17 classifier marks the
spawn `completed`. This is honest "log everything with bad latency"
behaviour, not a hidden failure: operators see the peer-drop log
in stderr and the catastrophic delivery in the analysis table.

5 s is far longer than any realistic transient on a healthy LAN
(TCP_NODELAY + 4 MiB recv buffers); a timeout firing means the peer
is genuinely stuck. The constant is `TCP_SINGLE_WRITE_TIMEOUT` in
`src/udp.rs`. The integration regression
`tests/two_runner_t14_19_tcp_single_no_deadlock.rs` exercises the
deadlock workload and asserts both runners exit `status=success`
(delivery threshold deliberately NOT asserted).

### UDP buffer tuning (T-impl.2)

Every UDP socket the variant creates (today: the multicast socket in
`setup_udp`) must be passed through `variant_base::tune_udp_buffers`
immediately after `Socket::new`. The helper bumps `SO_RCVBUF` and
`SO_SNDBUF` to 8 MiB, logs a single warning if the achieved size falls
below 1 MiB, and then returns. The rationale is to absorb 100 K pkt/s
bursts on the same-host fixtures that would otherwise overrun the
Windows-default ~64 KB kernel buffer within milliseconds and produce
spurious "loss" rows. Do NOT skip the call on Linux: the helper is
no-op-safe when the kernel grants the full request.

### MTU handling

Standard Ethernet MTU = 1500 bytes. UDP payload limit = ~1472 bytes.
For messages larger than 1472 bytes, implement application-layer fragmentation:
- Fragment into chunks with a fragment header (message_id, fragment_index, total_fragments).
- Reassemble at receiver.
- For the `scalar-flood` workload (8-byte payloads), fragmentation will never trigger.

### Backpressure semantics (T-impl.7)

Custom-udp overrides `Variant::try_publish` (see `src/udp.rs`,
`impl Variant for UdpVariant::try_publish` -> `publish_encoded`) so the
driver gets honest backpressure signalling instead of always seeing
`Ok(true)`. The driver assigns and consumes a seq number BEFORE calling
`try_publish`, so any `Ok(false)` return creates a receiver-visible gap
in the seq stream. This is fine for some QoS levels and catastrophic
for others:

- **QoS 1 (BestEffort)** — non-blocking `UdpSocket::send_to`.
  `WouldBlock` -> `Ok(false)`. The driver logs `backpressure_skipped`;
  the receiver tolerates loss by definition. (No retry, no kernel-buffer
  spin: a backed-up sender skips the value and moves on.)
- **QoS 2 (LatestValue)** — same as QoS 1. Even if the receiver only
  ever cares about the newest seq, gapping is still fine because the
  stale-discard logic on the receiver runs against the highest seen
  seq.
- **QoS 3 (ReliableUdp / NACK)** — blocking `send_to` with `yield_now`
  on `WouldBlock`. ALWAYS `Ok(true)`. Rationale: the receiver's gap
  detector would NACK for any seq we drop, and the writer's send-buffer
  no longer contains the payload (we never produced it), so the
  retransmit would fail and stall the spawn. The kernel send buffer is
  the natural pacing mechanism; `publish_encoded`'s spin-on-WouldBlock
  is the same loop pre-T-impl.7 used in `publish`.
- **QoS 4 (ReliableTcp)** — blocking `write_all`. ALWAYS `Ok(true)`.
  TCP receivers expect strictly contiguous framed messages; a gap would
  corrupt the per-peer reader state. Outbound TCP streams are kept in
  **blocking mode** (`set_nonblocking(false)` in `setup_tcp`) — only
  the inbound `tcp_in_streams` are non-blocking for polled reads, so
  there is no `FIONBIO`-is-socket-wide aliasing between the read and
  write paths. The kernel send-buffer fill makes `write_all` block,
  which is exactly the back-pressure signal we want to measure.

Where it lives:

- `src/udp.rs::UdpVariant::publish_encoded` — shared core, parameterised
  by `block_on_wouldblock`.
- `src/udp.rs::Variant::try_publish` — picks `block_on_wouldblock`
  per QoS.
- `src/udp.rs::Variant::publish` — keeps the legacy "always block"
  semantics by passing `block_on_wouldblock = true`.

### Testing

- Unit tests for message serialization/deserialization.
- Unit tests for QoS 2 stale-discard logic.
- Integration test: single-process. Synthesize the new CLI shape:
  `--peers self=127.0.0.1`, `--runner self`, `--multicast-group <ip:port>`,
  `--buffer-size <bytes>`, `--tcp-base-port <port>`, `--qos <1..4>`. Note
  that with a single-peer map there are no peers to connect to (self
  excluded by design); the test exercises bind/listen + framing only.
  Cross-peer flow on QoS 1-3 (UDP) and QoS 4 (TCP) is validated end-to-end
  via two runners on localhost during T9.4 acceptance.
- The binary should work with the runner.
