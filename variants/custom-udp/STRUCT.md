# variants/custom-udp Structure

```
variants/custom-udp/
  Cargo.toml              -- binary crate definition and dependencies
  AGENTS.md               -- agent rules for this repo
  CUSTOM.md               -- detailed design instructions
  STRUCT.md               -- this file
  src/
    main.rs               -- entry point: CLI parsing, construct UdpVariant, run driver
    udp.rs                -- UdpVariant struct implementing the Variant trait
    protocol.rs           -- binary message encoding/decoding (wire format)
    qos.rs                -- QoS receive-side logic (stale discard, gap detection)
  tests/
    multicast_loopback.rs -- integration test: single-process multicast send/receive
```

## Module Responsibilities

- **main.rs**: Parses CLI args via `variant-base::CliArgs`, constructs `UdpConfig` from extra args, creates `UdpVariant`, and delegates to `run_protocol`.
- **udp.rs**: Transport implementation. Manages UDP multicast sockets (QoS 1-3) and TCP connections (QoS 4). Handles socket setup, multicast join/leave, send/receive, and NACK retransmit buffering.
- **protocol.rs**: Binary wire format. Fixed-layout header with big-endian integers. Also handles NACK message encoding/decoding (0xFF marker prefix).
- **qos.rs**: Receive-side QoS filtering. `LatestValueTracker` for QoS 2 stale discard. `GapDetector` for QoS 3 gap detection.
