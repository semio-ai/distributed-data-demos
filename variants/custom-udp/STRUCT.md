# variants/custom-udp Structure

```
variants/custom-udp/
  Cargo.toml              -- binary crate definition and dependencies
  AGENTS.md               -- agent rules for this repo
  CUSTOM.md               -- detailed design instructions
  STRUCT.md               -- this file
  src/
    main.rs               -- entry point: CLI parsing, --peers / --tcp-base-port
                             parsing, TCP port derivation, UdpVariant construction
    udp.rs                -- UdpVariant struct implementing the Variant trait
    protocol.rs           -- binary message encoding/decoding (wire format)
    qos.rs                -- QoS receive-side logic (stale discard, gap detection)
  tests/
    multicast_loopback.rs -- raw-socket multicast send/receive sanity test
    integration.rs        -- subprocess invocation of the binary with the new
                             --peers / --runner / --tcp-base-port CLI shape
                             across all four QoS levels
```

## Module Responsibilities

- **main.rs**: Parses CLI args via `variant-base::CliArgs`. Reads
  `--multicast-group`, `--buffer-size`, `--tcp-base-port` from extra args, and
  the runner-injected `--peers` / `--runner` / `--qos`. Derives this runner's
  TCP listen address and the list of peer TCP endpoints (for QoS 4) using
  `runner_stride = 1` and `qos_stride = 10`. Builds `UdpConfig` and delegates
  to `run_protocol`.
- **udp.rs**: Transport implementation. Manages UDP multicast sockets (QoS
  1-3) and TCP connections (QoS 4). UDP multicast binds the configured
  `multicast_group` directly with no stride. TCP binds the derived
  per-runner / per-qos listen address and connects to each peer's derived
  port.
- **protocol.rs**: Binary wire format. Fixed-layout header with big-endian
  integers. Also handles NACK message encoding/decoding (0xFF marker prefix).
- **qos.rs**: Receive-side QoS filtering. `LatestValueTracker` for QoS 2
  stale discard. `GapDetector` for QoS 3 gap detection.
