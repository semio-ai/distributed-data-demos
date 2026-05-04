# variant-base File Structure

```
variant-base/
  .claude/
    CLAUDE.md              -- Worker agent instructions (read-only scope)
  src/
    lib.rs                 -- Public API re-exports for the library crate
    variant_trait.rs       -- Variant trait definition (connect, publish, poll_receive, disconnect, signal_end_of_test, poll_peer_eots) + PeerEot struct
    types.rs               -- Shared types: Qos enum, Phase enum (connect|stabilize|operate|eot|silent), ReceivedUpdate struct
    cli.rs                 -- Common CLI argument parsing with clap derive (CliArgs struct, including --eot-timeout-secs); helpers for parsing extra args (--peers names)
    logger.rs              -- JSONL structured logger with methods for connected, phase, write, receive, gap_detected, gap_filled, resource, eot_sent, eot_received, eot_timeout
    driver.rs              -- Test protocol driver: runs connect, stabilize, operate, eot, silent phases
    workload.rs            -- Workload trait + ScalarFlood implementation + factory function
    seq.rs                 -- Monotonic sequence number generator (SeqGenerator)
    resource.rs            -- CPU/memory resource monitor using sysinfo (ResourceMonitor)
    dummy.rs               -- VariantDummy: no-network Variant that echoes writes via VecDeque
    bin/
      variant_dummy.rs     -- Binary entry point for variant-dummy (parses CLI, runs protocol)
  tests/
    integration.rs         -- Integration tests: full pipeline with VariantDummy, binary subprocess test
  Cargo.toml               -- Crate manifest with lib + variant-dummy binary targets
  AGENTS.md                -- Agent guide for this repo
  CUSTOM.md                -- Repo-specific custom instructions (tech stack, design guidance)
  STRUCT.md                -- This file: describes the file layout
```
