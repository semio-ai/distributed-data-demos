# Task Board

## Current Sprint — E1: Variant Base Crate

### T1: Core types, Variant trait, and crate scaffold

**Repo**: `variant-base/`
**Status**: pending
**Depends on**: nothing

Scaffold the Rust crate and implement the foundational types and trait.

Scope:
1. Initialize `Cargo.toml` with library + `variant-dummy` binary targets.
   Add dependencies: `clap` (derive), `serde`, `serde_json`, `chrono`,
   `anyhow`, `thiserror`, `sysinfo`.
2. Define shared types in `src/types.rs`:
   - `Qos` enum (BestEffort=1, LatestValue=2, ReliableUdp=3, ReliableTcp=4)
   - `Phase` enum (Connect, Stabilize, Operate, Silent)
   - `ReceivedUpdate` struct (writer: String, seq: u64, path: String,
     qos: Qos, payload: Vec<u8>)
3. Define `Variant` trait in `src/trait.rs`:
   - `fn name(&self) -> &str` — human-readable name for logging
   - `fn connect(&mut self) -> Result<()>` — establish transport
   - `fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()>`
   - `fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>>`
   - `fn disconnect(&mut self) -> Result<()>`
4. Define common CLI args struct in `src/cli.rs` using clap derive:
   - All common args from `api-contracts/variant-cli.md`
   - Runner-injected args (`--launch-ts`, `--variant`, `--runner`, `--run`)
   - A pass-through mechanism for variant-specific args (e.g.
     `Vec<String>` trailing args, or `clap` allow_external_subcommands)
5. Wire up `src/lib.rs` with public re-exports.
6. `cargo build` and `cargo clippy -- -D warnings` must pass.

Acceptance criteria:
- [ ] `Cargo.toml` has lib + bin targets, all listed dependencies
- [ ] `Qos`, `Phase`, `ReceivedUpdate` types are defined and public
- [ ] `Variant` trait is defined and public
- [ ] CLI args struct parses all common + runner-injected args
- [ ] `cargo build` succeeds, `cargo clippy -- -D warnings` is clean

---

### T2: JSONL logger

**Repo**: `variant-base/`
**Status**: pending
**Depends on**: T1 (needs types)

Implement the structured JSONL log writer per `api-contracts/jsonl-log-schema.md`.

Scope:
1. Create `src/logger.rs`.
2. `Logger` struct that holds a `BufWriter<File>` and the identity fields
   (variant, runner, run).
3. Constructor takes log_dir, variant name, runner name, run id. Creates
   the output file as `<variant>-<runner>-<run>.jsonl`.
4. Methods for each event type:
   - `log_connected(launch_ts: &str, elapsed_ms: f64)`
   - `log_phase(phase: Phase, profile: Option<&str>)`
   - `log_write(seq: u64, path: &str, qos: Qos, bytes: usize)`
   - `log_receive(writer: &str, seq: u64, path: &str, qos: Qos, bytes: usize)`
   - `log_gap_detected(writer: &str, missing_seq: u64)`
   - `log_gap_filled(writer: &str, recovered_seq: u64)`
   - `log_resource(cpu_percent: f64, memory_mb: f64)`
5. Each method serializes a JSON object with:
   - `ts`: current wall-clock time (RFC 3339, nanosecond precision)
   - `variant`, `runner`, `run`: from the stored identity
   - `event`: the event type string
   - Event-specific fields
6. `flush()` method to force-flush the writer.
7. Unit tests: write events, read back the JSONL, verify all fields present
   and correctly typed.

Acceptance criteria:
- [ ] All 7 event types produce valid JSONL matching the schema contract
- [ ] `ts` field uses RFC 3339 with nanosecond precision
- [ ] Every line contains `ts`, `variant`, `runner`, `run`, `event`
- [ ] File is named `<variant>-<runner>-<run>.jsonl`
- [ ] Unit tests pass, cargo clippy clean

---

### T3: Sequence generator, resource monitor, workload profiles

**Repo**: `variant-base/`
**Status**: pending
**Depends on**: T1 (needs types), T2 (resource monitor uses logger)

Implement the three smaller support modules.

Scope:
1. **Sequence generator** (`src/seq.rs`):
   - `SeqGenerator` struct with `next() -> u64` returning monotonically
     increasing values starting from 1.
   - Simple, no-frills. Just an atomic or plain counter.

2. **Resource monitor** (`src/resource.rs`):
   - Uses `sysinfo` crate to sample current process CPU% and memory (MB).
   - `ResourceMonitor` struct with `sample() -> (f64, f64)` returning
     `(cpu_percent, memory_mb)`.
   - The driver will call this periodically and pass results to the logger.

3. **Workload profiles** (`src/workload.rs`):
   - `Workload` trait with `fn generate(&mut self, values_per_tick: u32) -> Vec<WriteOp>`
     where `WriteOp` is `{ path: String, payload: Vec<u8> }`.
   - `ScalarFlood` implementation: generates `values_per_tick` writes to
     paths like `/bench/0`, `/bench/1`, ... with small fixed-size payloads
     (e.g. 8 bytes representing an f64).
   - Factory function: `fn create_workload(name: &str) -> Box<dyn Workload>`
     that maps `"scalar-flood"` to `ScalarFlood`. Returns an error for
     unknown names.

4. Unit tests for all three modules.

Acceptance criteria:
- [ ] SeqGenerator produces 1, 2, 3, ... on successive calls
- [ ] ResourceMonitor returns plausible CPU/memory values
- [ ] ScalarFlood generates the correct number of WriteOps per call
- [ ] Unknown workload name returns an error
- [ ] cargo test passes, cargo clippy clean

---

### T4: Test protocol driver

**Repo**: `variant-base/`
**Status**: pending
**Depends on**: T1, T2, T3 (uses trait, logger, seq gen, resource monitor, workload)

Implement the protocol driver that orchestrates the four phases.

Scope:
1. Create `src/driver.rs`.
2. `run_protocol(variant: &mut dyn Variant, config: &CliArgs) -> Result<()>`
   function (or generic `impl Variant`).
3. Phase execution:
   - **Connect**: log `phase` event (connect), call `variant.connect()`,
     compute `elapsed_ms` from `config.launch_ts`, log `connected` event.
   - **Stabilize**: log `phase` event (stabilize), sleep for
     `config.stabilize_secs`.
   - **Operate**: log `phase` event (operate, with workload profile name).
     Run a tick loop at `config.tick_rate_hz`:
     - Each tick: call workload to generate writes, for each write call
       `variant.publish()` and `logger.log_write()`, then drain
       `variant.poll_receive()` and `logger.log_receive()` for each.
     - Every ~100ms sample resource monitor and `logger.log_resource()`.
     - Run for `config.operate_secs` total.
   - **Silent**: log `phase` event (silent). Drain remaining receives for
     `config.silent_secs`, flush logger.
4. Return `Ok(())` on success. The binary main will exit 0.

Acceptance criteria:
- [ ] All four phases execute in order
- [ ] Tick loop runs at approximately the configured rate
- [ ] Write and receive events are logged during operate phase
- [ ] Resource events are logged periodically during operate phase
- [ ] Phase events are logged at each transition
- [ ] Connected event includes launch_ts and elapsed_ms
- [ ] cargo test passes, cargo clippy clean

---

### T5: VariantDummy + integration tests

**Repo**: `variant-base/`
**Status**: pending
**Depends on**: T4 (needs the driver to run end-to-end)

Implement the dummy variant and validate the full pipeline.

Scope:
1. Create `src/dummy.rs`:
   - `VariantDummy` struct with an internal `VecDeque<ReceivedUpdate>`.
   - `connect` — no-op, returns Ok immediately.
   - `publish` — creates a `ReceivedUpdate` from the args (writer = own
     runner name) and pushes it to the internal queue.
   - `poll_receive` — pops from the queue if non-empty, else returns None.
   - `disconnect` — no-op.
2. Create `src/bin/variant_dummy.rs`:
   - Parse CLI args using the common CLI parser.
   - Instantiate `VariantDummy`.
   - Call `run_protocol(&mut dummy, &args)`.
   - Exit 0 on Ok, exit 1 on Err (print error to stderr).
3. Integration test (`tests/integration.rs`):
   - Run the protocol driver with `VariantDummy`, short durations (1s
     stabilize, 2s operate, 1s silent), low tick rate (10 Hz), small
     workload (10 values/tick).
   - Read the generated JSONL file.
   - Verify: phase events appear in order (connect, stabilize, operate,
     silent), connected event exists with elapsed_ms, write events have
     monotonic seq numbers, receive events exist for each write, resource
     events exist.
   - Verify the file can be parsed as valid JSONL (every line is valid JSON).
4. Run `variant-dummy` binary as a subprocess in a test, passing CLI args,
   verify exit code 0 and JSONL file is produced.

Acceptance criteria:
- [ ] VariantDummy implements Variant trait correctly
- [ ] `variant-dummy` binary runs to completion with exit 0
- [ ] Generated JSONL has all expected event types in correct order
- [ ] Every write has a corresponding receive (dummy echoes to itself)
- [ ] Sequence numbers are monotonically increasing
- [ ] Integration test passes
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean, `cargo fmt -- --check` clean
- [ ] STRUCT.md exists and describes the file layout
