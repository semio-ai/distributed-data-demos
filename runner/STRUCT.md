# runner/ File Layout

```
runner/
  Cargo.toml              -- Binary crate: runner + sleeper (test helper)
  AGENTS.md               -- Agent instructions for this repo
  CUSTOM.md               -- Repo-specific custom instructions (tech stack, design)
  STRUCT.md               -- This file
  .claude/
    CLAUDE.md             -- Worker agent configuration
  src/
    main.rs               -- Entry point: CLI parsing, config loading, main loop
    config.rs             -- BenchConfig and VariantConfig structs, TOML parsing,
                             validation, SHA-256 config hash
    cli_args.rs           -- build_variant_args(): converts TOML config sections
                             into variant CLI arguments (snake_case to --kebab-case)
    spawn.rs              -- spawn_and_monitor(): child process lifecycle with
                             timeout handling. ChildOutcome enum.
    protocol.rs           -- Coordinator: UDP broadcast discovery, ready barrier,
                             done barrier. Single-runner optimization.
    message.rs            -- Message enum (Discover, Ready, Done) with JSON
                             serialization for the coordination protocol.
  tests/
    integration.rs        -- End-to-end tests: single-runner lifecycle, config
                             validation, multi-variant execution, timeout handling
    fixtures/
      single-runner.toml  -- Config with one runner, one variant (variant-dummy)
      multi-variant.toml  -- Config with one runner, two variants (variant-dummy)
      bad-name.toml       -- Config for testing --name validation rejection
    helpers/
      sleeper.rs          -- Tiny binary that sleeps forever (test timeout target)
```

## Module Responsibilities

- **main.rs**: Parses CLI (--name, --config, --port), loads and validates config,
  checks binary paths exist, creates Coordinator, runs discovery, then for each
  variant: ready barrier, build args, spawn and monitor, done barrier. Prints
  summary table. Exits non-zero if any variant failed/timed out.

- **config.rs**: Deserializes TOML config into typed structs. Validates run ID,
  runners list, variant uniqueness, binary paths, qos range, timeout values.
  Computes SHA-256 hash of raw config bytes for cross-runner verification.

- **cli_args.rs**: Converts TOML key-value pairs to CLI argument vectors.
  snake_case keys become --kebab-case flags. Appends runner-injected args
  (--launch-ts, --variant, --runner, --run) at the end.

- **spawn.rs**: Spawns variant binary as child process, polls for exit with
  configurable timeout. On timeout, kills the child and returns Timeout outcome.
  Returns Success (exit 0) or Failed(code) for normal exits.

- **protocol.rs**: Manages leaderless UDP coordination. Broadcasts messages every
  500ms. Discovery verifies config hashes match. Ready and Done barriers wait
  for all expected runners. Single-runner mode skips all network I/O.

- **message.rs**: Defines the three coordination message types serialized as
  tagged JSON objects for UDP broadcast.
