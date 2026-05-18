# Custom UDP Variant — Custom Instructions

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

- **Data channel** (`data_tx` / `data_rx`): **T16.4 made this
  unbounded** (`mpsc::channel`). Pre-T16.4 it was a bounded
  `sync_channel` of size `4 * values_per_tick * (peer_count + 1)` with
  drop-on-full. The drop-on-full rationale was "QoS 3/4 protocols on
  the driver thread are unchanged because the dropped item simply
  never lands in `pending`", but that reasoning was incomplete: at
  1000 paths x 100 Hz QoS 3 the bound (4000) overflowed almost
  immediately, dropped data frames triggered the receiver's gap
  detector to fire NACKs, and the resulting retransmits ALSO
  overflowed the same channel -- a NACK-storm feedback loop that
  collapsed multi delivery to ~10 % vs single's ~56 % (logs
  `same-machine-all-variants-01-20260514_084636/`, T16.4). After T16.4
  the channel is unbounded; the kernel UDP `SO_RCVBUF` (8 MiB after
  `tune_udp_buffers`) remains the only natural bound on the receive
  side, and the driver's per-iteration drain budget
  (`4 * values_per_tick`) keeps the channel's resident set bounded
  under sustained load (~4x headroom over a 1-peer 100-Hz 1000-vpt
  workload). The reader thread additionally filters out self-echoes
  (writer == runner) before enqueueing, removing ~50 % of channel
  pressure caused by multicast loopback. The
  `[custom-udp] multi: data channel full -- dropping Data frame` log
  line no longer exists; no replacement log line is added because the
  failure mode it indicated is gone.
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
  guaranteeing EOT / NACK / PeerDropped observations are surfaced
  ahead of data. Post-T16.4 the priority guarantee is unchanged --
  even though the data channel can no longer overflow, lifecycle
  draining FIRST still costs nothing and matches the T14.16 wiring.
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

### Startup retry (T14.22)

Same-host two-runner startup is a known TCP race: both runners hit
the ready barrier and call `connect()` to each other's QoS-4 TCP
listen port near simultaneously; the peer's `listen()` may not yet
be accepting and the kernel returns `ConnectionRefused` on the first
attempt. Pre-T14.22, custom-udp's `setup_tcp` made a single
non-retrying `TcpStream::connect` call: a single refusal silently
dropped that peer from the broadcast set, and the spawn proceeded in
asymmetric disconnected state (the listening side timed out waiting
for the inbound TCP peer; the dialing side accumulated writes into
the void). Reproduced in `logs/all-variants-01-20260512_152156/`
for the `custom-udp-100x1000hz-qos4-multi` spawn.

`src/udp.rs::connect_qos4_with_retry` ports the
`variants/hybrid/src/tcp.rs::connect_with_retry` pattern (the T14.4
prior art) to custom-udp's qos4 outbound TCP setup. Same shape:

- Bounded retry budget: `TCP_CONNECT_RETRY_BUDGET = 30 s`
  (matches hybrid's budget and `controltcp::CONTROL_CONNECT_BUDGET`).
- Per-attempt sleep: `TCP_CONNECT_RETRY_SLEEP = 50 ms`.
- Retry on `ConnectionRefused` ONLY. Every other error kind
  (including `TimedOut`) propagates IMMEDIATELY so we don't paper
  over real connectivity problems behind a 30 s delay.
- Uses the blocking `TcpStream::connect` (no per-attempt timeout)
  -- a successful connect on a healthy LAN returns within
  milliseconds. Wrapping with `connect_timeout` would risk falsely
  tripping on slow SYN-ACK scheduling at higher QoS levels (see
  hybrid CUSTOM.md "Bounded connect retry").

The retry loop is generic over a connector closure
(`connect_qos4_with_retry_inner`) so the unit tests in
`src/udp.rs::tests` (`connect_with_retry_*`) can exercise the loop
deterministically without a real TCP listener. The integration
regression
`tests/two_runner_t14_22_qos4_startup_race.rs` exercises the actual
same-host race and asserts both runners reach `status=success`.

The control-channel TCP path (`controltcp::connect_with_budget`)
already has an equivalent retry loop (it pre-dates T14.22 because
T14.18 was wired up with the retry in place). T14.22 only restores
the same property to the qos=4 data-path connect.

### TCP write retry under saturation (T17.3, supersedes T14.19)

Per `metak-shared/DESIGN.md` § 6.5 (Strict No-Skip Contract for
QoS 3 / QoS 4), the QoS 4 path MUST deliver 100% of accepted writes
under sustained overload; the acceptable failure mode is throughput
collapse, not delivery shortfall. Pre-T17.3 the variant dropped
peers on ANY `write_all` error, which lost ~55% (multi) / ~68%
(single) of writes on `custom-udp-1000x100hz-qos4` because a full
kernel send buffer surfaces as a transient `TimedOut` (Windows) /
`WouldBlock` (Unix) once `SO_SNDTIMEO` fires -- the variant treated
that as a peer-death signal and silently disconnected the peer.

**Post-T17.3 behaviour** (see `publish_encoded` in `src/udp.rs`,
`Qos::ReliableTcp` branch):

1. For each connected outbound TCP peer, loop on `write_all`:
   - on `Ok`: success, advance to next peer.
   - on transient error (`WouldBlock`, `TimedOut`, `Interrupted` --
     see `is_fatal_tcp_write_error`): retry. First retry yields,
     subsequent retries `sleep(100us)` to match the variant-base
     driver's QoS 3/4 strict-no-skip back-off.
   - on fatal error (`ConnectionReset`, `BrokenPipe`,
     `ConnectionAborted`, `NotConnected`, or anything else): drop
     the peer from `tcp_out_streams` with a
     `[custom-udp] T17.3: dropping outbound TCP peer ... after FATAL
     write error` log line. The classifier's everything-else default
     is conservative: unknown error kinds are treated as fatal
     rather than retried forever.
2. `SO_SNDTIMEO = TCP_WRITE_TIMEOUT = 500 ms` is installed on every
   outbound TCP stream in **both single AND multi modes** (pre-T17.3
   it was Single-only). The timeout is now used as a wake-from-retry
   mechanism rather than a peer-drop trigger; spurious Multi-mode
   peer-drops under transient back-pressure (the T14.19 concern that
   gated the option Single-only) are no longer a risk because
   `TimedOut` is now retry, not drop.

**Why a timeout at all?** A pure blocking `write_all` would deadlock
forever if the peer is genuinely dead (vs merely backpressured);
the retry loop needs a way to wake periodically and re-check.
`SO_SNDTIMEO` is the OS-level mechanism for that.

**Wedge resolution**: at catastrophic symmetric load (1000 vpt x
100 Hz Single mode) both publisher threads still spin inside their
retry loops while neither calls `poll_receive`. The kernel TCP send
buffers fill on both sides. The retry loop keeps re-attempting the
write; eventually one side's kernel drains a byte (a TCP keepalive,
an ACK), the retry succeeds, both sides continue. Delivery stays at
100%; throughput collapses to whatever the kernel permits under the
saturated condition (typically a few thousand msg/s instead of the
requested 100K). This is the "log everything with bad latency"
intent in its current form: operators see slow progress and lower
writes/s in the analysis table, not silent loss.

**Integration tests**:

- `tests/two_runner_t17_3_qos4_backpressure.rs` exercises the
  saturation workload via the reproducer
  `tests/fixtures/two-runner-custom-udp-qos4-saturate-repro.toml`
  (`100x100hz` qos4 in both threading modes). Asserts:
    - both spawns exit `status=success`,
    - cross-peer delivery is 100% in both directions (raw counts,
      matching `analysis/integrity.py::_check_per_pair`),
    - zero `backpressure_skipped` events with `qos == 4`.
- `tests/two_runner_t14_19_tcp_single_no_deadlock.rs` still passes:
  the spawn reaches `eot_sent` cleanly on both sides. Its
  delivery-near-zero comment in the source is historical -- under
  T17.3 the same workload now delivers 100% (no peer-drop), but
  the test deliberately does not assert delivery threshold so it
  continues to validate the "no deadlock + clean exit" property.

**Unit tests** in `src/udp.rs::tests`:

- `is_fatal_tcp_write_error_classifier` -- direct policy check on
  every error kind the classifier sees in production.
- `publish_qos4_happy_path_keeps_peer_alive` -- regression safety
  for the no-pressure path.
- `publish_qos4_drops_peer_on_fatal_write_error` -- a closed-peer
  write surfaces a fatal error and the peer is dropped.

The constant is `TCP_WRITE_TIMEOUT` in `src/udp.rs`; the classifier
is `is_fatal_tcp_write_error` next to the QoS-4 retry loop.

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
- **QoS 4 (ReliableTcp)** — blocking `write_all` **with retry on
  transient errors** (T17.3). ALWAYS `Ok(true)`. TCP receivers
  expect strictly contiguous framed messages; a gap would corrupt
  the per-peer reader state. Outbound TCP streams are kept in
  **blocking mode** (`set_nonblocking(false)` in `setup_tcp`) — only
  the inbound `tcp_in_streams` are non-blocking for polled reads, so
  there is no `FIONBIO`-is-socket-wide aliasing between the read and
  write paths. The kernel send-buffer fill makes `write_all` block,
  which is exactly the back-pressure signal we want to measure. On
  transient errors (`TimedOut` from `SO_SNDTIMEO`, `WouldBlock`,
  `Interrupted`) the variant retries the write; only fatal errors
  (`ConnectionReset`, `BrokenPipe`, `ConnectionAborted`,
  `NotConnected`) drop the peer. See "TCP write retry under
  saturation (T17.3)" below for the contract and reasoning.

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
