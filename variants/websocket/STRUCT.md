# variants/websocket Structure

```
variants/websocket/
  AGENTS.md         -- agent rules for this variant
  CUSTOM.md         -- detailed design instructions
  STRUCT.md         -- this file
  Cargo.toml        -- binary crate (variant-base, tungstenite sync, socket2,
                       anyhow, clap, rand)
  src/
    main.rs         -- CLI parsing, QoS rejection guard, create
                       WebSocketVariant, call run_protocol
    websocket.rs    -- WebSocketVariant struct implementing Variant trait
                       (one tungstenite WebSocket per peer; blocking
                       writes, SO_RCVTIMEO-based read polling)
    protocol.rs     -- compact binary header (data + EOT frames,
                       same shape as hybrid/custom-udp, embedded inside
                       WebSocket binary frame body)
    pairing.rs      -- sorted-name pairing + port derivation
                       (runner_stride=1, qos_stride=10)
  tests/
    integration.rs  -- single-process loopback (bind, framing,
                       round-trip via tungstenite)
    fixtures/
      two-runner-websocket-only.toml  -- minimal two-runner fixture
                                         (qos=[3, 4], scalar-flood)
```

Only the binary `variant-websocket` is produced. No library targets, no
benchmarks (workload is driven by `variant-base`'s protocol driver).
