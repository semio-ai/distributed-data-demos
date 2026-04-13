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

---

## E3: Concrete Variant Implementations

| Variant | Status | Worker | Notes |
|---------|--------|--------|-------|
| E3a: Zenoh | in-progress | worker-zenoh | Native Rust, Zenoh pub/sub |
| E3b: Custom UDP | done | worker-custom-udp | Raw UDP, all 4 QoS levels implemented |
| E3c: Aeron | blocked | worker-aeron | Scaffold complete; rusteron-client build fails (see below) |
| E3d: QUIC | done | worker-quic | quinn crate, async-to-sync bridge |
| E3e: Hybrid UDP/TCP | done | worker-hybrid | UDP for QoS 1-2, TCP for QoS 3-4 |

### E3c: Aeron -- Blocker Report

**Status:** blocked -- scaffold committed, build fails on `rusteron-client` C FFI compilation.

**What was implemented (scaffold):**

- `Cargo.toml`: binary crate depending on `variant-base`, `rusteron-client`, `anyhow`.
- `src/main.rs`: CLI extra-arg parsing (`--aeron-dir`, `--channel`, `--stream-id` with defaults), constructs `AeronVariant`, calls `run_protocol`. Includes 5 unit tests for arg parsing.
- `src/aeron.rs`: `AeronVariant` struct implementing `Variant` trait with full `connect`, `publish`, `poll_receive`, `disconnect` methods. `FragmentReceiver` implementing `AeronFragmentHandlerCallback`. Message serialization/deserialization (compact binary wire format). Includes 6 unit tests for serialization and struct construction.
- `STRUCT.md`: file layout documentation.

**Build error:**

`rusteron-client v0.1.162` fails during its build script. The crate uses `bindgen` to generate Rust FFI bindings from the Aeron C headers, and `cmake` to compile the Aeron C library from source. On this Windows machine, `bindgen`/`clang-sys` picks up an LLVM MinGW toolchain (`llvm-mingw-20260324-ucrt-x86_64`) whose headers are incompatible with the MSVC target (`x86_64-pc-windows-msvc`). The MinGW `stdlib.h` uses GCC-specific `__attribute__` syntax that the bindgen clang parser rejects, producing hundreds of parse errors and ultimately "fatal error: too many errors emitted."

**Root cause:** Toolchain mismatch -- bindgen's clang is resolving system headers from an LLVM MinGW installation rather than the MSVC Windows SDK headers. This is an environment configuration issue, not a code issue.

**To unblock:**

1. Ensure `LIBCLANG_PATH` points to an MSVC-compatible LLVM/Clang installation (e.g., LLVM from Visual Studio or the official LLVM release for Windows).
2. Alternatively, set `BINDGEN_EXTRA_CLANG_ARGS` to include the correct MSVC SDK include paths (e.g., `--target=x86_64-pc-windows-msvc -isystem "C:/Program Files/.../include"`).
3. Or remove/relocate the MinGW toolchain from the PATH so bindgen does not find it.
4. On Linux/macOS, this crate is expected to build without issues if clang and cmake are installed.

### E3b: Custom UDP -- Completion Report

**What was implemented:**

- `Cargo.toml`: binary crate depending on `variant-base` (path), `socket2`, `anyhow`, `clap`.
- `src/main.rs`: CLI parsing, constructs `UdpConfig` from extra args, creates `UdpVariant`, calls `run_protocol`.
- `src/protocol.rs`: Compact binary message encoding/decoding. Wire format: `[4B total_len][1B qos][8B seq][2B path_len][NB path][2B writer_len][MB writer][payload]`. All multi-byte integers big-endian. NACK message format with 0xFF marker prefix.
- `src/qos.rs`: Receive-side QoS filtering. `LatestValueTracker` for QoS 2 (tracks highest seq per writer+path, discards stale). `GapDetector` for QoS 3 (detects sequence gaps, returns missing seq list).
- `src/udp.rs`: `UdpVariant` implementing the `Variant` trait. Supports all four QoS levels:
  - QoS 1 (BestEffort): UDP multicast fire-and-forget.
  - QoS 2 (LatestValue): UDP multicast with stale-discard on receive.
  - QoS 3 (ReliableUdp): UDP multicast with send buffer (10K messages) and NACK-based retransmit. Receiver detects gaps and sends NACK to multicast group.
  - QoS 4 (ReliableTcp): TCP connections to explicit peers. Non-blocking accept/read.
- `tests/multicast_loopback.rs`: Integration test verifying single-process multicast send/receive.
- `STRUCT.md`: File layout documentation.

**CLI extra args:** `--multicast-group` (default 239.0.0.1:9000), `--buffer-size` (default 65536), `--peers` (comma-separated addresses for QoS 4 TCP).

**Test results:**

- 29 unit tests pass (protocol: 11, qos: 12, udp: 6)
- 1 integration test passes (multicast loopback)
- `cargo clippy -- -D warnings`: clean
- `cargo fmt -- --check`: clean

**Design decisions:**

- Own messages filtered out by comparing `writer` field to `config.runner` in recv_udp.
- Multicast loopback enabled so nodes can receive their own messages (needed for testing).
- QoS 3 NACK retransmit buffer capped at 10,000 messages to bound memory usage.
- mDNS discovery deferred (as instructed); `--peers` provides explicit peer addresses.

**Open concerns:**

- Fragmentation for large payloads (>1472 bytes) not yet implemented. The scalar-flood workload (8-byte payloads) fits in a single datagram.
- QoS 4 TCP requires peers to be pre-configured via `--peers` and does not auto-discover.
- QoS 3 NACK recovery is basic: NACKs sent to multicast group, original sender retransmits if message still buffered.

### E3d: QUIC -- Completion Report

**What was implemented:**

- `Cargo.toml`: binary crate depending on `variant-base`, `quinn`, `rustls`, `rcgen`, `tokio`, `clap`, `anyhow`.
- `src/main.rs`: CLI extra-arg parsing (`--bind-addr`, `--peers`), constructs `QuicVariant`, calls `run_protocol`. Includes 3 unit tests for arg parsing.
- `src/quic.rs`: `QuicVariant` struct implementing `Variant` trait with full async-to-sync bridge. Tokio runtime spawned on `connect`; mpsc channels bridge sync trait methods to background tokio tasks. QoS 1-2 use QUIC unreliable datagrams (`send_datagram`); QoS 3-4 use QUIC unidirectional streams (`open_uni`). Custom binary wire format with writer/path/qos/seq/payload encoding. `SkipServerVerification` for LAN benchmarking. Background accept task handles incoming connections. Includes 5 unit tests for message encoding/decoding and struct construction.
- `src/certs.rs`: self-signed certificate generation using `rcgen`. Includes 1 unit test.
- `tests/loopback.rs`: 2 integration tests -- no-peer binary exit test, and self-connect loopback verifying write+receive log entries.
- `STRUCT.md`: file layout documentation.

**Test results:**

- 9 unit tests pass (quic: 5, main: 3, certs: 1)
- 2 integration tests pass (no-peer run, self-connect loopback)
- `cargo clippy --all-targets -- -D warnings`: clean
- `cargo fmt -- --check`: clean

**Deviations from spec:**

- `discovery.rs` (mDNS) not implemented per instructions ("skip mDNS for now"). Peer addresses are provided via `--peers` CLI arg.
- No separate lib target; all code is in the binary crate with `mod` declarations. Integration tests use subprocess testing via `env!("CARGO_BIN_EXE_variant-quic")`.

**Open concerns:**

- None. All acceptance criteria met.
