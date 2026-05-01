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

```
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

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
multicast_group = "239.0.0.1:19500"
buffer_size = 65536
tcp_base_port = 19800
```

Variant-specific CLI args:

- `--multicast-group <ip:port>` — required. UDP multicast group address.
  Same value used by all runners; no runner or QoS stride applied.
- `--buffer-size <bytes>` — required. UDP receive buffer size.
- `--tcp-base-port <u16>` — required. Base port that per-runner / per-qos
  TCP ports are derived from (used only at QoS 4).

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

### MTU handling

Standard Ethernet MTU = 1500 bytes. UDP payload limit = ~1472 bytes.
For messages larger than 1472 bytes, implement application-layer fragmentation:
- Fragment into chunks with a fragment header (message_id, fragment_index, total_fragments).
- Reassemble at receiver.
- For the `scalar-flood` workload (8-byte payloads), fragmentation will never trigger.

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
