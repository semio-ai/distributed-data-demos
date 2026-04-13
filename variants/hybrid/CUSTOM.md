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
  - `mdns-sd` — peer discovery
  - `anyhow` — error handling
- **No external libraries beyond std for TCP** — just `std::net::TcpStream`
  and `TcpListener`.
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
    discovery.rs  -- mDNS peer discovery
  Cargo.toml
```

## Design Guidance

### CLI args (variant-specific)

- `--multicast-group` (default: `239.0.0.1:9000`)
- `--tcp-base-port` (default: `19900`)
- `--bind-addr` (default: `0.0.0.0`)
- `--peers` (optional: explicit comma-separated peer addresses, skips mDNS)

### connect

1. Bind a UDP socket and join the multicast group (for QoS 1-2).
2. Discover peers via mDNS (or use `--peers`).
3. For QoS 3-4: establish TCP connections to each peer.
   - Each node listens on `tcp_base_port`.
   - Connect to each discovered peer's TCP port.
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
- Integration test: single-process loopback (multicast to self for UDP,
  connect to self for TCP). Short durations.
