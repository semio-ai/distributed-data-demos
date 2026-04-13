# variants/zenoh -- File Layout

```
variants/zenoh/
  Cargo.toml                -- Binary crate depending on variant-base, zenoh, anyhow
  AGENTS.md                 -- Agent rules for this repo
  CUSTOM.md                 -- Detailed instructions (tech stack, design guidance)
  STRUCT.md                 -- This file
  src/
    main.rs                 -- CLI parsing, ZenohVariant construction, run_protocol call
    zenoh.rs                -- ZenohVariant struct, Variant trait impl, MessageCodec, ZenohArgs
  tests/
    loopback.rs             -- Integration test: full protocol driver over real Zenoh loopback
```
