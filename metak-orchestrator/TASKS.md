# Task Board

## Completed — E1: Variant Base Crate

All tasks done. See STATUS.md for completion report.

---

## Current Sprint — E2: Benchmark Runner

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
