# WebRTC Variant — Custom Instructions

## Overview

Rust binary implementing the `Variant` trait from `variant-base` using
WebRTC DataChannels as the transport. Each peer pair establishes one
PeerConnection with four DataChannels — one per QoS level — configured
to map directly to our QoS semantics:

- L1 (best-effort): unordered, `maxRetransmits=0`
- L2 (latest-value): unordered, `maxRetransmits=0` + receiver-side seq filter
- L3 (reliable-ordered): ordered, default reliable
- L4 (reliable): ordered, default reliable (same channel config as L3)

This is the heaviest stack in the lineup (DTLS + SCTP over UDP) and the
only one that natively offers reliable + unreliable from one session.

## Tech Stack

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`variant-webrtc`)
- **Key dependencies**:
  - `variant-base` (path = `../../variant-base`) — Variant trait, types, CLI, driver
  - `webrtc` — webrtc-rs, the main Rust WebRTC implementation. Pulls in
    many transitive deps (DTLS, SCTP, ICE, SRTP, media). That weight is
    the point — we are measuring the off-the-shelf stack, not a
    minimal hand-rolled equivalent.
  - `tokio` (rt-multi-thread) — required by `webrtc`.
  - `anyhow` — error handling.
  - Optional: `serde_json` if you need to encode the SDP exchange as
    JSON envelopes over the signaling socket.
- **No mDNS, no STUN, no TURN.** LAN-only with host candidates from
  `--peers`.
- Follow `metak-shared/coding-standards.md`.

## Build and Test

```
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

**Validate the build early.** webrtc-rs has a heavy dependency tree;
on Windows, OpenSSL or ring versions may need pinning. If `cargo build`
takes more than a few minutes or fails on Windows, stop and report
back via STATUS.md before going further. We can either pin a working
combination or, in the worst case, reconsider the variant.

## Architecture

```
variants/webrtc/
  src/
    main.rs        -- parse CLI, create WebRtcVariant, call run_protocol
    webrtc.rs      -- WebRtcVariant struct implementing Variant trait (sync surface)
    runtime.rs     -- internal tokio runtime + sync-to-async bridge
    signaling.rs   -- TCP signaling channel: SDP offer/answer + ICE
    pairing.rs     -- sorted-name pairing + port derivation
    protocol.rs    -- compact binary header (shared across variants)
  tests/
    integration.rs -- single-process loopback (signaling, channel open)
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

```toml
[variant.specific]
signaling_base_port = 19980
media_base_port     = 20000
```

Variant-specific CLI args:

- `--signaling-base-port <u16>` — required. Base TCP port for the
  per-pair signaling socket. Per-runner / per-qos derivation as
  documented below.
- `--media-base-port <u16>` — required. Base UDP port for ICE host
  candidates. Per-runner / per-qos derivation as documented below.

The variant also reads (from the standard runner-injected args, see
`metak-shared/api-contracts/variant-cli.md`):

- `--peers <name1>=<host1>,<name2>=<host2>,...` — full runner→host map.
- `--runner <name>` — this runner's name; used to look up own index.
- `--qos <N>` — concrete QoS level for this spawn (1-4).

### Port derivation

```
runner_stride = 1
qos_stride    = 10

runner_index = sorted_peer_names.position(of: --runner)

my_signaling_listen = signaling_base_port + runner_index * runner_stride + (qos - 1) * qos_stride
my_media_listen     = media_base_port     + runner_index * runner_stride + (qos - 1) * qos_stride

for each (name, host) in --peers where name != --runner:
    peer_index            = sorted_peer_names.position(of: name)
    peer_signaling_port   = signaling_base_port + peer_index * runner_stride + (qos - 1) * qos_stride
    peer_media_port       = media_base_port     + peer_index * runner_stride + (qos - 1) * qos_stride
```

If `--runner` is not present in `--peers`, fail loudly with a clear
error.

### Signaling — variant-to-variant TCP

The runner does NOT carry SDP. Each peer pair brings up a small TCP
signaling socket on the derived signaling ports.

For each peer pair (sorted by name):
- Lower-sorted runner is the **signaling initiator**: opens TCP to the
  higher peer's signaling port. Sends its SDP offer. Receives an SDP
  answer. Streams ICE candidates as they are discovered locally and
  applies remote ICE candidates as they arrive.
- Higher-sorted runner is the **signaling responder**: binds and
  accepts on its signaling port. Receives the offer, generates the
  answer, sends it back. Same ICE candidate streaming.

Frame format on the signaling socket: length-prefixed JSON envelopes,
e.g. `{"kind":"offer","sdp":"..."}`, `{"kind":"answer","sdp":"..."}`,
`{"kind":"candidate","candidate":"..."}`, `{"kind":"done"}`. Keep it
simple — do not invent a generic protocol.

Close the signaling socket once both sides see all four DataChannels
report `open`. After that, the PeerConnection is the only channel.

### ICE candidate policy

- **Host candidates only.** Disable the STUN, TURN, and mDNS providers
  in webrtc-rs's `ICEGatherer` configuration so it only emits host
  candidates.
- Bind ICE on `0.0.0.0:my_media_listen` so candidates use the OS's
  default interfaces. The peer host comes from `--peers`.

### connect

1. Parse CLI, derive ports.
2. Stand up the internal tokio runtime.
3. Build a single `RTCPeerConnection` per peer (so one `PeerConnection`
   per peer, not per QoS — a single connection multiplexes all four
   DataChannels for that peer).
4. Per peer: open the four DataChannels with the QoS-appropriate
   options (the active spawn's `--qos` determines which channel that
   spawn primarily writes to; receivers must accept on all four
   channels regardless, because peers may write at any QoS).
5. Run signaling: initiator sends offer, responder sends answer,
   trickle ICE both ways.
6. Wait until all expected peers' connections are `connected` AND all
   expected DataChannels are `open`.
7. Wire each open DataChannel's `on_message` to push into a single
   shared `tokio::sync::mpsc::Receiver` that `poll_receive` drains via
   `try_recv`.

### publish — DataChannel selection by QoS

The active spawn always has a single `--qos` value (the runner expands
multi-QoS configs into per-QoS spawns, see E9). So each spawn writes to
exactly **one** channel:

- L1: send on the unordered, `maxRetransmits=0` channel.
- L2: same channel as L1; the binary header carries the seq, receiver
  filters stale.
- L3: send on the ordered reliable channel.
- L4: send on the ordered reliable channel (same as L3).

For each peer, look up the appropriate `RTCDataChannel` and call
`send` (or `send_text` for binary — verify which API the webrtc crate
uses for binary frames).

### poll_receive

- `try_recv` from the shared mpsc receiver fed by all DataChannel
  `on_message` handlers.
- For QoS 2: track highest seq per writer, discard stale.

### Message format

The same compact binary header used by `custom-udp`, `hybrid`, and
`websocket` lives **inside** the DataChannel message body:

```
DataChannel message body:
[1 byte qos | 8 bytes seq | 2 bytes path_len | N bytes path | 2 bytes writer_len | M bytes writer | payload bytes]
```

DataChannel framing is provided by SCTP; we do not add length prefixes.

### Sync-to-async bridge

`webrtc-rs` is async. The `Variant` trait surface is sync. Strategy
(mirrors `variants/quic/CUSTOM.md`):

1. On `connect`, spawn a multi-threaded tokio runtime
   (`Runtime::new()`).
2. Use `Runtime::block_on` for `connect`, `signal_end_of_test`,
   `poll_peer_eots`, `disconnect`.
3. For `publish`: send to an mpsc channel; a per-peer background tokio
   task drains the channel and calls `RTCDataChannel::send`. (Do NOT
   `block_on` from `publish` — at our tick rate, the runtime entry/exit
   overhead would dominate measurements.)
4. For `poll_receive`: `try_recv` against a shared mpsc populated by
   `on_message` callbacks.
5. On `disconnect`, gracefully close DataChannels, then the
   PeerConnections, then `runtime.shutdown_timeout(...)`.

### EOT (End-of-Test) — E12 protocol

Per `metak-shared/api-contracts/eot-protocol.md`. The DataChannel-based
variant of EOT closely resembles QUIC's stream-end approach:

- `signal_end_of_test`: send the EOT marker on the **reliable**
  DataChannel (L3/L4) to every peer. Use a reserved header value as
  documented in `eot-protocol.md`. The marker MUST go on the reliable
  channel even if the spawn's primary `--qos` is unreliable —
  otherwise an EOT packet loss would deadlock the wait.
- `poll_peer_eots`: drain incoming EOT markers from the reliable
  channel's receive queue; return the new (writer, eot_id) pairs to
  the driver.

### Testing

- Unit test: pairing / port-derivation logic given a few `--peers`
  shapes.
- Unit test: signaling envelope encode / decode.
- Unit test: message-header serialization.
- Integration test: single-process loopback. Synthesize the CLI shape
  `--peers self=127.0.0.1`, `--runner self`, `--qos 1`,
  `--signaling-base-port <free>`, `--media-base-port <free>`. With a
  single-peer map there are no peers to connect to (self is excluded
  by design); the test exercises CLI parsing, port derivation, and
  the runtime startup path. Full end-to-end DataChannel exchange is
  validated by the cross-machine regression task (T3g.4).
- Build smoke test: `cargo build --release` completes on Windows.

### Validation against reality

After implementation:

- `cargo test --release -p variant-webrtc` — all-green.
- `cargo clippy --release -p variant-webrtc --all-targets -- -D warnings`.
- `cargo fmt -p variant-webrtc -- --check`.
- Build the binary, then run an end-to-end two-runner localhost test
  using a TOML config that spawns webrtc across all four QoS levels
  (analogous to `configs/two-runner-quic-all.toml`). Verify the JSONL
  logs show `connected`, the EOT phase, and delivery ≥ 95% on
  reliable channels. Unreliable channels at high rate may show some
  loss — record what you measure.

## Out of scope

- STUN / TURN.
- mDNS ICE candidates.
- Browser interop (no SDP-munging).
- DTLS certificate pinning.
- Multiple PeerConnections per peer pair.
- Re-negotiation after the initial offer/answer.
- Recovery after a PeerConnection drop mid-spawn — log clearly and let
  the spawn fail. Cross-spawn isolation is the runner's responsibility.
