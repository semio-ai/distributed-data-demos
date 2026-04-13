# Execution Status

## E1: Variant Base Crate

| Task | Status | Worker | Notes |
|------|--------|--------|-------|
| T1: Core types, trait, scaffold | done | worker-e1 | All types, trait, CLI parsing implemented |
| T2: JSONL logger | done | worker-e1 | All 7 event types, RFC 3339 nanosecond timestamps |
| T3: Seq gen, resource monitor, workload | done | worker-e1 | SeqGenerator, ResourceMonitor, ScalarFlood workload |
| T4: Test protocol driver | done | worker-e1 | 4-phase driver with tick loop, resource sampling |
| T5: VariantDummy + integration tests | done | worker-e1 | VecDeque echo, binary target, 2 integration tests |

### Completion Report

**What was implemented:**

- T1: Cargo.toml with lib + variant-dummy binary targets. Qos, Phase, ReceivedUpdate types. Variant trait with 5 methods. CliArgs struct with all common, runner-injected, and pass-through arguments.
- T2: Logger struct with BufWriter<File>, all 7 event type methods (connected, phase, write, receive, gap_detected, gap_filled, resource). File naming follows `<variant>-<runner>-<run>.jsonl`. Timestamps use RFC 3339 with nanosecond precision.
- T3: SeqGenerator (monotonic counter from 1), ResourceMonitor (sysinfo-based CPU/memory sampling), ScalarFlood workload (generates N writes to /bench/0..N with 8-byte f64 payloads), create_workload factory function.
- T4: run_protocol driver function executing connect, stabilize (sleep), operate (tick loop with workload + resource sampling every ~100ms), silent (drain + flush) phases.
- T5: VariantDummy with VecDeque echoing writes as receives. variant-dummy binary entry point. Integration tests: full protocol pipeline test and binary subprocess exit-code test.

**Test results:**

- 27 unit tests pass (cli: 2, dummy: 5, logger: 11, seq: 3, resource: 1, workload: 5)
- 2 integration tests pass (full protocol pipeline, binary subprocess)
- cargo clippy -- -D warnings: clean
- cargo fmt -- --check: clean

**Deviations from task spec:**

- SeqGenerator method named `next_seq()` instead of `next()` to avoid clippy warning about shadowing `Iterator::next`.
- Trait file named `variant_trait.rs` instead of `trait.rs` since `trait` is a Rust keyword and cannot be a module name.

**Open concerns:**

- None. All acceptance criteria met.

---

## E0: Variant Exploration

| Task | Status | Notes |
|------|--------|-------|
| Research: pub/sub and middleware frameworks | done | Zenoh, CycloneDDS, RTI Connext, ZeroMQ, NATS, Redis |
| Research: raw protocol approaches | done | UDP multicast/unicast, raw TCP, QUIC, io_uring, mio |
| Research: shared memory and IPC | done | Iceoryx2, Aeron, Dust DDS, DPDK, Cap'n Proto, shared_memory |
| Synthesize into variant-candidates.md | done | 18 candidates evaluated, 4 selected |
| Update EPICS.md E3+ with final list | done | E3a Zenoh, E3b Custom UDP, E3c Aeron, E3d QUIC |
| Review E1 trait compatibility | done | No changes needed (D5) |

### Completion Report

**Deliverables:**
- `metak-shared/variant-candidates.md` — full research with per-candidate
  assessment, comparison matrix, and E1 trait compatibility analysis.
- EPICS.md updated with concrete E3a-E3d variant epics.
- Decisions D4 (variant selection) and D5 (trait unchanged) logged.

**Selected variants:**
1. Zenoh (E3a) — framework, native Rust, <10 us
2. Custom UDP (E3b) — raw protocol, all 4 QoS, 2-5 ms
3. Aeron (E3c) — finance-grade, C bindings, 21-57 us
4. QUIC/quinn (E3d) — modern protocol, native Rust, 2-12 ms

**E1 impact:** None. Sync trait works for all four candidates.

---

## E2: Benchmark Runner

| Task | Status | Worker | Notes |
|------|--------|--------|-------|
| T1: Crate scaffold, config parsing, CLI arg builder | done | worker-e2 | All config fields, validation, SHA-256 hash, CLI arg builder |
| T2: Child process spawning and monitoring | done | worker-e2 | spawn_and_monitor with timeout, ChildOutcome enum |
| T3: UDP coordination protocol | done | worker-e2 | Coordinator with discover, ready, done barriers; single-runner optimization |
| T4: Main loop + integration tests | done | worker-e2 | Full lifecycle, 4 integration tests, summary table |

### Completion Report

**What was implemented:**

- T1: Cargo.toml with all required dependencies (clap, toml, serde, serde_json, sha2, chrono, anyhow, socket2). CLI struct with --name, --config, --port args. BenchConfig/VariantConfig structs matching TOML schema contract. Validation for all rules (empty run, empty runners, duplicate variant names, empty binary, qos range 1-4, positive timeouts). SHA-256 config hash (hex-encoded). CLI arg builder converting snake_case to --kebab-case with runner-injected args appended.
- T2: spawn_and_monitor() function with try_wait polling loop. ChildOutcome enum (Success, Failed(i32), Timeout). Binary path validation before spawn. Timeout kills child via child.kill(). Platform tested on Windows.
- T3: Message enum (Discover, Ready, Done) with tagged JSON serde. Coordinator struct with UDP broadcast socket (socket2, SO_BROADCAST, SO_REUSEADDR). 500ms re-broadcast interval for UDP loss resilience. Config hash mismatch detection and abort. Single-runner mode: no socket created, all methods return immediately.
- T4: Main loop wires config loading, discovery, per-variant ready/spawn/monitor/done barriers. Summary table printed to stdout. Exit code 1 if any variant failed/timed out. Integration tests: single-runner lifecycle with variant-dummy, config validation (bad --name), multi-variant sequential execution (2 variants), timeout handling (sleeper binary killed after 3s). STRUCT.md created.

**Test results:**

- 29 unit tests pass (config: 10, cli_args: 4, message: 6, protocol: 5, spawn: 4)
- 4 integration tests pass (single-runner lifecycle, config validation, multi-variant, timeout)
- cargo clippy -- -D warnings: clean
- cargo fmt -- --check: clean

**Deviations from task spec:**

- The `variant` field in BenchConfig uses `#[serde(default)]` so that configs without `[[variant]]` entries still parse (allows testing top-level validation without needing variant entries).
- Timeout integration test uses a custom `sleeper` test binary (tiny Rust binary that sleeps forever) instead of a shell script, because shell scripts on Windows do not get properly killed by child.kill().
- The `sleeper` binary is declared as a `[[bin]]` target in Cargo.toml for test helper purposes.
- launch_ts is computed in main.rs before calling spawn_and_monitor (not inside spawn_and_monitor) since the timestamp must be passed as a CLI arg to the variant before spawning.

**Open concerns:**

- UDP broadcast tests use `AtomicU16` port allocation to avoid conflicts when tests run in parallel. If tests are run across multiple concurrent cargo test invocations, port conflicts could still occur.
- The two-runner localhost coordination test depends on UDP broadcast working on the test machine. Some CI environments may block UDP broadcast.
