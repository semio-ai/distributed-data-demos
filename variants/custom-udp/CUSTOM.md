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
  - `mdns-sd` — mDNS peer discovery (or manual peer list via CLI args)
  - `anyhow` — error handling
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
variants/custom-udp/
  src/
    main.rs       -- parse CLI, create UdpVariant, call run_protocol
    udp.rs        -- UdpVariant struct implementing Variant trait
    protocol.rs   -- message framing, header serialization
    discovery.rs  -- mDNS peer discovery
    qos.rs        -- QoS-specific send/receive logic
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

Expected in `extra` pass-through args:
- `--multicast-group` (default: `239.0.0.1:9000`)
- `--buffer-size` (default: `65536`)
- `--bind-addr` (default: `0.0.0.0:0`)
- `--peers` (optional: explicit comma-separated peer addresses, skips mDNS)

### connect

1. Bind a UDP socket (multicast-capable via socket2).
2. Join the multicast group.
3. Run mDNS discovery to find peers (or use `--peers` if provided).
4. For QoS 4 (TCP): also open TCP listeners/connections to each peer.

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
- Integration test: single-process (publish to multicast, receive own messages).
- The binary should work with the runner.
