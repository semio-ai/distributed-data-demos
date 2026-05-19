# variant-base File Structure

```
variant-base/
  .claude/
    CLAUDE.md              -- Worker agent instructions (read-only scope)
  src/
    lib.rs                 -- Public API re-exports for the library crate
    variant_trait.rs       -- Variant trait definition (connect, publish, poll_receive, disconnect, signal_end_of_test, poll_peer_eots) + PeerEot struct
    types.rs               -- Shared types: Qos enum, Phase enum (connect|stabilize|operate|eot|silent|digest), ReceivedUpdate struct
    cli.rs                 -- Common CLI argument parsing with clap derive (CliArgs struct, including --eot-timeout-secs, --digest-mem-soft-mb, --digest-mem-hard-mb); helpers for parsing extra args (--peers names)
    logger.rs              -- Lifecycle-only JSONL structured logger with methods for connected, phase, resource, eot_sent, eot_received, eot_timeout (post-T19.10: per-event observations live exclusively in compact-Parquet)
    compact.rs             -- T18.1 / T18.2: in-memory columnar event buffers (CompactBuffers) + lazy PathInterner / PeerInterner with documented caps and PEER_SELF sentinel; EventKind enum with pinned discriminants
    compact_writer.rs      -- T18.2: serialises CompactBuffers to <variant>-<runner>-<run>.compact.parquet via the `parquet` crate (snappy by default); embeds intern dictionaries + spawn identifiers in Parquet KV metadata
    driver.rs              -- Test protocol driver: runs connect, stabilize, operate, silent, digest phases. Owns a single-source EventSink that pushes per-event observations into the compact buffers (no JSONL byproduct post-T19.10).
    workload.rs            -- Workload trait + ScalarFlood implementation + factory function
    seq.rs                 -- Monotonic sequence number generator (SeqGenerator)
    resource.rs            -- CPU/memory resource monitor using sysinfo (ResourceMonitor)
    dummy.rs               -- VariantDummy: no-network Variant that echoes writes via VecDeque
    bin/
      variant_dummy.rs     -- Binary entry point for variant-dummy (parses CLI, runs protocol)
  tests/
    integration.rs         -- Integration tests: full pipeline with VariantDummy, binary subprocess test, compact-Parquet roundtrip, lifecycle-JSONL inspection, hard mem-ceiling abort
  Cargo.toml               -- Crate manifest with lib + variant-dummy binary targets
  AGENTS.md                -- Agent guide for this repo
  CUSTOM.md                -- Repo-specific custom instructions (tech stack, design guidance)
  STRUCT.md                -- This file: describes the file layout
```
