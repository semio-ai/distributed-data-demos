# Task Board

## Current Sprint — E8: Application-Level Clock Synchronization

Cross-machine latency cannot be measured without correcting for clock skew
between runner machines. See `metak-shared/api-contracts/clock-sync.md`
for the full protocol.

### T8.1: Runner — clock-sync protocol implementation

**Repo**: `runner/`
**Status**: pending
**Depends on**: contract review by user

Implement the NTP-style offset measurement and persist results to a JSONL
log file. Variants are NOT touched.

Scope:
1. New module `src/clock_sync.rs`:
   - Add `ProbeRequest { from, to, id, t1 }` and
     `ProbeResponse { from, to, id, t1, t2, t3 }` variants to the existing
     `Message` enum in `src/message.rs`.
   - `pub struct ClockSyncEngine` holding the existing UDP socket handle
     plus a counter for probe IDs.
   - `pub fn measure_offsets(&self, peers: &[String], n_samples: usize) -> HashMap<String, OffsetMeasurement>`
     where `OffsetMeasurement` carries `offset_ms`, `rtt_ms`, `samples`,
     `min_rtt_ms`, `max_rtt_ms`.
   - Per peer: send `n_samples` `ProbeRequest`s with 5 ms inter-sample
     delay. Wait up to 100 ms per response. Pick the sample with smallest
     RTT. Compute offset and rtt as defined in `clock-sync.md`.
   - Always-respond logic: when the runner's UDP receive loop sees a
     `ProbeRequest` addressed to it, immediately send back a
     `ProbeResponse` with `t2` (receive time) and `t3` (send time). This
     must work even when the runner is in a barrier — add probe handling
     to the existing loops in `src/protocol.rs`.

2. New JSONL writer `src/clock_sync_log.rs`:
   - `pub fn open_clock_sync_log(log_dir: &Path, runner: &str, run: &str) -> ClockSyncLogger`.
   - File name: `<runner>-clock-sync-<run>.jsonl`. Same dir as variant logs.
   - `pub fn write(&mut self, variant: &str, peer: &str, m: &OffsetMeasurement)`.
   - JSONL schema per `jsonl-log-schema.md` `clock_sync` event.

3. Wire-in in `src/main.rs`:
   - After discovery completes, before first ready barrier: call
     `measure_offsets` for all peers, write each result with `variant=""`.
   - For each variant after its ready barrier and before spawn: call
     `measure_offsets` again, write with `variant=<name>`.
   - Single-runner runs skip both calls (no peers).

4. Tests:
   - Unit: offset math given known timestamps → expected `offset_ms` and
     `rtt_ms`. Min-RTT selection picks the right sample.
   - Unit: serialize/deserialize `ProbeRequest`/`ProbeResponse`.
   - Integration: two `ClockSyncEngine` instances on localhost — same
     machine so true offset is 0. Verify `|offset_ms| < 1.0` and
     `rtt_ms > 0`.
   - Integration: end-to-end runner launch on localhost (same machine) —
     verify the clock-sync JSONL file appears, has the expected number
     of entries (1 initial + N variants per peer), and offset is near zero.

Acceptance criteria:
- [ ] `Message` enum includes `ProbeRequest` and `ProbeResponse`
- [ ] `ClockSyncEngine::measure_offsets` returns per-peer measurements
- [ ] Probe responses are sent promptly even while in barrier loops
- [ ] `<runner>-clock-sync-<run>.jsonl` written with one entry per
      (peer, measurement_event)
- [ ] Initial sync runs after discovery; per-variant sync runs after each
      ready barrier
- [ ] Single-runner mode produces no clock-sync log (or an empty file)
- [ ] Localhost integration test: `|offset_ms| < 1.0`
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean

---

### T8.2: Analysis — apply offsets when computing cross-machine latency

**Repo**: `analysis/`
**Status**: pending
**Depends on**: contract review by user (can run in parallel with T8.1
because analysis can be tested with synthetic clock-sync JSONL fixtures
before T8.1 lands)

Read clock-sync log files and adjust cross-machine latency calculations.

Scope:
1. Update `parse.py`:
   - Add `clock_sync` to the recognized event types.
   - Parsed event-specific fields: `peer`, `offset_ms`, `rtt_ms`, `samples`,
     `min_rtt_ms`, `max_rtt_ms`. Stored alongside other events in `Event.data`.

2. Update `cache.py` (if needed) to include `*-clock-sync-*.jsonl` in the
   ingestion glob — should already work since cache scans `*.jsonl` but
   verify.

3. New module `analysis/clock_sync.py`:
   - `class OffsetTable` mapping `(run, self_runner, peer_runner) → list[ClockSyncEvent]`
     sorted by `ts`.
   - `def lookup(self, run, receiver, writer, before_ts) -> float | None`:
     return `offset_ms` from the most recent measurement on the receiver's
     side with `peer=writer` and `ts <= before_ts`. Falls back to the
     initial sync (variant="") if no per-variant entry exists.

4. Update `correlate.py` and/or `performance.py`:
   - After delivery records are built, apply correction for cross-runner
     records: `latency_ms = (receive_ts − write_ts).total_seconds() * 1000 + offset_ms`.
   - Same-runner records: no adjustment.
   - If no offset is available (e.g. clock-sync file missing), log a
     warning to stderr and emit latency without correction. Add a
     `corrected: bool` flag to the delivery record so the report can
     surface this.

5. Update `tables.py`:
   - When any record in a row is uncorrected, append `(uncorrected)` to the
     latency cells, OR add a column to the integrity report flagging it.
     (Pick whichever is least invasive — just make it visible.)

6. Tests:
   - Unit: `OffsetTable.lookup` returns the most recent measurement
     preceding the given timestamp; falls back to initial sync.
   - Unit: cross-runner record gets correction applied; same-runner does not.
   - Unit: missing offset → warning + uncorrected flag.
   - Integration: synthetic JSONL fixtures with a known +50 ms clock skew
     between two runners — verify reported latency is the true value, not
     50 ms higher.

Acceptance criteria:
- [ ] `parse.py` recognizes `clock_sync` event
- [ ] `OffsetTable` lookup respects `before_ts` and falls back to initial sync
- [ ] Cross-runner records get offset correction
- [ ] Same-runner records are unaffected
- [ ] Missing-offset case emits warning and flags as uncorrected
- [ ] Integration test with +50 ms synthetic skew passes
- [ ] All existing tests still pass
- [ ] `ruff format --check` and `ruff check` clean on touched files

---

### T8.3: End-to-end two-machine validation

**Repo**: top-level (no code; runs binaries and analysis)
**Status**: pending
**Depends on**: T8.1, T8.2

Validate that on a real two-machine setup, latency numbers are no longer
dominated by clock skew.

Scope:
1. Run a short benchmark on two machines (e.g. `two-runner-quic-10x100.toml`).
2. Verify clock-sync log files are produced on both runners.
3. Verify reported `offset_ms` is reasonable (single-digit ms or less on a
   quiet LAN; large but consistent if clocks are far apart).
4. Verify reported latency is in the expected range (< 10 ms target per
   DESIGN.md), as opposed to the previous behavior where it would reflect
   the clock skew.
5. Document findings in `metak-shared/LEARNED.md`.

Acceptance criteria:
- [ ] Two-machine benchmark completes successfully
- [ ] Clock-sync logs produced on both machines
- [ ] Cross-machine latency in reasonable range; not dominated by skew
- [ ] Findings documented in LEARNED.md

---

## Completed — E1: Variant Base Crate

All tasks done. See STATUS.md for completion report.

---

## Completed — E2: Benchmark Runner

### T1: Crate scaffold + TOML config parsing + CLI arg construction

**Repo**: `runner/`
**Status**: pending
**Depends on**: nothing

Scaffold the Rust binary crate and implement config parsing and the CLI arg
builder that converts TOML config sections into variant CLI arguments.

Scope:
1. Initialize `Cargo.toml` as a binary crate. Add dependencies: `clap`
   (derive), `toml`, `serde`, `serde_json`, `sha2`, `chrono`, `anyhow`,
   `socket2`.
2. CLI (`src/main.rs`): `runner --name <name> --config <path.toml>`.
   Validate that `--name` matches one of the runner names in the config.
3. Config struct (`src/config.rs`):
   - Top-level: `run` (String), `runners` (Vec<String>),
     `default_timeout_secs` (u64).
   - `[[variant]]`: `name` (String), `binary` (String),
     `timeout_secs` (Option<u64>), `common` (toml::Table),
     `specific` (Option<toml::Table>).
   - Parse from TOML file path. Run validation rules from the
     `toml-config-schema.md` contract.
   - `config_hash()` method: SHA-256 of the raw file bytes, hex-encoded.
4. CLI arg builder (`src/cli_args.rs`):
   - `fn build_variant_args(variant: &VariantConfig, run: &str, runner_name: &str, launch_ts: &str) -> Vec<String>`
   - Iterates `variant.common` table: for each key-value, converts
     `snake_case` key to `--kebab-case`, formats value as string.
   - Appends `variant.specific` entries the same way.
   - Appends runner-injected args: `--launch-ts`, `--variant`, `--runner`,
     `--run`.
   - Must match `api-contracts/variant-cli.md` exactly.
5. Unit tests:
   - Parse a sample TOML config, verify all fields.
   - Verify config hash is deterministic.
   - Verify CLI arg construction: given known config, produce expected
     arg vector. Check kebab-case conversion, value formatting, ordering.
   - Verify validation rejects: empty `run`, empty `runners`, duplicate
     variant names, missing `binary`.

Acceptance criteria:
- [ ] `Cargo.toml` with all listed dependencies
- [ ] CLI parses --name and --config, validates name is in runners list
- [ ] Config struct matches TOML schema contract exactly
- [ ] config_hash() returns deterministic SHA-256 hex
- [ ] CLI arg builder converts snake_case to --kebab-case correctly
- [ ] Runner-injected args (--launch-ts, --variant, --runner, --run) appended
- [ ] Validation catches invalid configs
- [ ] Unit tests pass, cargo clippy clean

---

### T2: Child process spawning and monitoring

**Repo**: `runner/`
**Status**: pending
**Depends on**: T1 (needs config and CLI arg builder)

Implement child process lifecycle: spawn, monitor, timeout, collect exit code.

Scope:
1. Create `src/spawn.rs`.
2. `ChildOutcome` enum: `Success`, `Failed(i32)`, `Timeout`.
3. `fn spawn_and_monitor(binary: &str, args: &[String], timeout: Duration) -> Result<ChildOutcome>`:
   - Validate binary path exists before spawning.
   - Record `launch_ts` as RFC 3339 nanosecond timestamp immediately
     before `Command::new(binary).args(args).spawn()`.
   - Return the `launch_ts` alongside the outcome (caller needs it for
     the done barrier).
   - Wait for child exit. Use a separate thread or `child.try_wait()`
     polling loop to implement timeout.
   - On timeout: kill the child process (platform-appropriate), return
     `Timeout`.
   - On normal exit: return `Success` if exit code 0, `Failed(code)`
     otherwise.
4. Unit/integration test:
   - Spawn `variant-dummy` (from variant-base) with valid args, verify
     `Success` outcome.
   - Spawn a nonexistent binary, verify error.
   - Test timeout by spawning a process that sleeps longer than the
     timeout (e.g. `sleep 999` or a small script), verify `Timeout`.
     Use a very short timeout (2-3 seconds) to keep tests fast.

Note: the `variant-dummy` binary must be pre-built. The test should check
for its existence and skip with a clear message if not found.

Acceptance criteria:
- [ ] spawn_and_monitor returns Success/Failed/Timeout correctly
- [ ] launch_ts is recorded immediately before spawn
- [ ] Binary path is validated before spawning
- [ ] Timeout kills the child process
- [ ] Integration test with variant-dummy passes
- [ ] Timeout test passes (short timeout, child killed)
- [ ] cargo clippy clean

---

### T3: UDP coordination protocol

**Repo**: `runner/`
**Status**: pending
**Depends on**: T1 (needs config for runner names and config hash)

Implement the leaderless discovery and barrier sync protocol over UDP
broadcast.

Scope:
1. Message types (`src/message.rs`):
   ```rust
   enum Message {
       Discover { name: String, config_hash: String },
       Ready { name: String, variant: String },
       Done { name: String, variant: String, status: String, exit_code: i32 },
   }
   ```
   Serialize/deserialize as JSON. Keep it simple.

2. Coordination engine (`src/protocol.rs`):
   - `Coordinator` struct holding a UDP broadcast socket (send + receive),
     this runner's name, the set of expected runner names, and the config
     hash.
   - **Port**: default 19876, configurable via `--port` CLI arg
     (add to clap struct).
   - **Bind**: `0.0.0.0:<port>`, broadcast to `255.255.255.255:<port>`.
     Use `socket2` for `SO_BROADCAST` and `SO_REUSEADDR`.

3. `discover(&mut self) -> Result<()>`:
   - Periodically broadcast `Discover` message (every 500ms).
   - Listen for `Discover` from other runners.
   - Verify config_hash matches; abort with clear error if mismatch.
   - Complete when all runner names seen with matching hash.
   - **Single-runner optimization**: if `runners` has only this runner's
     name, return immediately without any network I/O.

4. `ready_barrier(&mut self, variant_name: &str) -> Result<()>`:
   - Broadcast `Ready` for this variant, listen for Ready from all others.
   - Re-broadcast every 500ms until all runners have reported ready.
   - Single-runner: return immediately.

5. `done_barrier(&mut self, variant_name: &str, status: &str, exit_code: i32) -> Result<HashMap<String, (String, i32)>>`:
   - Broadcast `Done` for this variant with own status, listen for Done
     from all others.
   - Return a map of runner_name -> (status, exit_code) for reporting.
   - Single-runner: return immediately with own result.

6. Unit tests:
   - Serialize/deserialize each message type.
   - Single-runner discover, ready, done all return immediately.
   - Two-coordinator test on localhost: spawn two Coordinator instances
     on different ports (or same port with SO_REUSEADDR), verify they
     discover each other and complete barriers. Use threads.

Acceptance criteria:
- [ ] Message types serialize/deserialize correctly as JSON
- [ ] Single-runner mode completes all protocol steps without network I/O
- [ ] Discovery detects config hash mismatch and aborts
- [ ] Barriers complete when all runners have reported
- [ ] Re-broadcast handles UDP packet loss
- [ ] Two-runner localhost test passes
- [ ] cargo clippy clean

---

### T4: Main loop + integration tests

**Repo**: `runner/`
**Status**: pending
**Depends on**: T1, T2, T3

Wire everything together and validate the full runner lifecycle.

Scope:
1. Main loop (`src/main.rs`):
   - Parse CLI, load and validate config.
   - Create Coordinator, run discovery.
   - For each variant in config order:
     a. Run ready barrier.
     b. Build CLI args from config.
     c. Spawn variant binary, monitor with timeout.
     d. Run done barrier with outcome.
     e. Print summary line (variant name, status, exit code per runner).
   - Exit 0 if all variants completed, exit 1 if any failed.

2. Sample config file (`tests/fixtures/single-runner.toml`):
   ```toml
   run = "test01"
   runners = ["local"]
   default_timeout_secs = 30

   [[variant]]
   name = "dummy"
   binary = "../variant-base/target/release/variant-dummy"

     [variant.common]
     tick_rate_hz = 10
     stabilize_secs = 0
     operate_secs = 2
     silent_secs = 0
     workload = "scalar-flood"
     values_per_tick = 5
     qos = 1
     log_dir = "./test-logs"

     [variant.specific]
   ```

3. Integration tests (`tests/integration.rs`):
   - **Single-runner lifecycle**: Run runner with single-runner.toml config
     and `--name local`. Verify exit 0, JSONL file produced in test-logs/,
     file contains expected events.
   - **Timeout handling**: Config with a variant binary that hangs (e.g.
     `sleep` or a script), short timeout (3s). Verify runner reports
     timeout and exits non-zero.
   - **Config validation**: Attempt to run with --name that isn't in
     runners list, verify error message.
   - **Multi-variant config**: Config with two variant entries (both
     pointing at variant-dummy with different names). Verify runner
     executes both in order, two JSONL files produced.

4. Create STRUCT.md describing the file layout.

5. Print a summary table to stdout after all variants complete:
   ```
   Benchmark run: test01
   Variant                  Runner   Status    Exit
   dummy                    local    success   0
   ```

Acceptance criteria:
- [ ] Single-runner lifecycle test passes end-to-end
- [ ] Runner spawns variant-dummy, waits for exit, reports success
- [ ] JSONL log files are produced in the configured log_dir
- [ ] Timeout test: runner kills hung variant and reports timeout
- [ ] Config validation: bad --name is rejected with clear error
- [ ] Multi-variant: both variants executed in order
- [ ] Summary table printed to stdout
- [ ] `cargo test` passes, `cargo clippy -- -D warnings` clean, `cargo fmt -- --check` clean
- [ ] STRUCT.md exists and describes the file layout
