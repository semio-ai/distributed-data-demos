# runner/ File Layout

```
runner/
  Cargo.toml              -- Binary crate: runner + sleeper + arg-echo (test helpers)
  AGENTS.md               -- Agent instructions for this repo
  CUSTOM.md               -- Repo-specific custom instructions (tech stack, design)
  STRUCT.md               -- This file
  .claude/
    CLAUDE.md             -- Worker agent configuration
  src/
    main.rs               -- Entry point: CLI parsing, config loading, main loop
                             (qos expansion, per-spawn ready/done barriers)
    config.rs             -- BenchConfig and VariantConfig structs, TOML parsing,
                             validation, SHA-256 config hash, QosSpec enum,
                             inter_qos_grace_ms field
    cli_args.rs           -- build_variant_args(): converts TOML config sections
                             into variant CLI arguments (snake_case to --kebab-case),
                             injects --peers/--qos/--variant per spawn job
    spawn.rs              -- spawn_and_monitor(): child process lifecycle with
                             timeout handling. ChildOutcome enum.
    spawn_job.rs          -- SpawnJob struct + expand_variant(): expand a
                             [[variant]] entry into one job per QoS level,
                             producing the synthesized <name>-qosN effective name
    protocol.rs           -- Coordinator: UDP broadcast discovery (with
                             recv_from for peer source IP capture and
                             same-host classification), ready/done barriers.
                             Single-runner optimization. Exposes peer_hosts().
    local_addrs.rs        -- local_interface_ips() (cached set of this
                             machine's interface IPs) + canonical_peer_host()
                             (collapses local/loopback sources to "127.0.0.1").
    message.rs            -- Message enum (Discover, Ready, Done) with JSON
                             serialization for the coordination protocol.
  tests/
    integration.rs        -- End-to-end tests: single-runner lifecycle, config
                             validation, multi-variant execution, timeout
                             handling, qos expansion (array + omitted forms),
                             --peers injection verification via arg-echo
    fixtures/
      single-runner.toml  -- Config with one runner, one variant (variant-dummy)
      multi-variant.toml  -- Config with one runner, two variants (variant-dummy)
      bad-name.toml       -- Config for testing --name validation rejection
      qos-array.toml      -- Single runner with qos = [1, 2] (expansion test)
      qos-omitted.toml    -- Single runner with qos omitted (expand to all 4)
    helpers/
      sleeper.rs          -- Tiny binary that sleeps forever (test timeout target)
      arg_echo.rs         -- Tiny binary that writes its CLI args to a JSON file
                             (used to verify runner-injected args like --peers)
```

## Module Responsibilities

- **main.rs**: Parses CLI (--name, --config, --port), loads and validates config,
  checks binary paths exist, creates Coordinator, runs discovery, snapshots
  the captured peer_hosts map, then for each variant: expand into per-QoS
  spawn jobs and execute each job sequentially (ready barrier, build args,
  spawn and monitor, done barrier) with a small inter-job grace period
  between consecutive QoS spawns. Prints summary table. Exits non-zero if
  any variant failed/timed out.

- **config.rs**: Deserializes TOML config into typed structs. Validates run
  ID, runners list, variant uniqueness, binary paths, qos range/form, timeout
  values. Computes SHA-256 hash of raw config bytes for cross-runner
  verification. Defines `QosSpec` (Single/Multi/All), parsed lazily from the
  `[variant.common].qos` field via `VariantConfig::qos_spec()`. Top-level
  `inter_qos_grace_ms` field controls the inter-job sleep between consecutive
  per-QoS spawn jobs (default 250 ms).

- **cli_args.rs**: Converts TOML key-value pairs to CLI argument vectors.
  snake_case keys become --kebab-case flags. Skips the common-section `qos`
  key (the per-spawn concrete level is injected as `--qos` instead). Appends
  runner-injected args (--qos, --launch-ts, --variant, --runner, --run,
  --peers) before the `--` separator that fences `[variant.specific]`
  trailing args. `--peers` is comma-separated `name=host` pairs sorted by
  name for determinism.

- **spawn.rs**: Spawns variant binary as child process, polls for exit with
  configurable timeout. On timeout, kills the child and returns Timeout
  outcome. Returns Success (exit 0) or Failed(code) for normal exits.

- **spawn_job.rs**: Defines `SpawnJob` (effective_name, qos, source_index)
  and `expand_variant()` which expands one `[[variant]]` entry into one
  job per concrete QoS level. Single-level entries keep the original
  `variant.name`; multi-level entries synthesize `<name>-qosN`.

- **protocol.rs**: Manages leaderless UDP coordination. Broadcasts messages
  every 500ms. Discovery uses `recv_from` so peer source IPs can be
  captured into `peer_hosts: HashMap<String, String>` (keyed by runner
  name, collapsing local/loopback sources to `"127.0.0.1"` via
  `local_addrs::canonical_peer_host`). Ready and Done barriers wait for
  all expected runners. Single-runner mode skips all network I/O and
  self-populates `peer_hosts` with `"127.0.0.1"`.

- **local_addrs.rs**: Enumerates this machine's IPv4/IPv6 interface
  addresses via `local-ip-address::list_afinet_netifas`, cached on first
  call, always including loopback. `canonical_peer_host()` maps any
  observed peer source IP that belongs to this machine (or is loopback)
  to the literal `"127.0.0.1"`; remote IPs pass through as
  `to_string()`.

- **message.rs**: Defines the three coordination message types serialized
  as tagged JSON objects for UDP broadcast.
