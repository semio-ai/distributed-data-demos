# variants/hybrid Structure

```
variants/hybrid/
  AGENTS.md         -- agent rules for this variant
  CUSTOM.md         -- detailed design instructions
  STRUCT.md         -- this file
  Cargo.toml        -- binary crate depending on variant-base, socket2, anyhow
  src/
    main.rs         -- CLI parsing, create HybridVariant, call run_protocol
    hybrid.rs       -- HybridVariant struct implementing Variant trait
    protocol.rs     -- compact binary message encoding/decoding (shared by UDP/TCP)
    udp.rs          -- UDP multicast transport for QoS 1-2
    tcp.rs          -- TCP connection management for QoS 3-4
  tests/
    integration.rs  -- single-process loopback tests (UDP multicast, TCP)
```
