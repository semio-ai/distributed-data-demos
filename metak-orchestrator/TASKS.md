# Task Board

## Current Sprint — E9: Peer Discovery Injection + QoS Expansion

See `EPICS.md` E9 for the rationale. Two tasks: T9.1 lands the runner
changes (new contract surface), T9.2 migrates the QUIC variant to consume
the new `--peers` arg and `--qos` per-spawn.

### T9.1: Runner — peer source IP capture, --peers injection, qos expansion

**Repo**: `runner/`
**Status**: pending
**Depends on**: contract updates in `metak-shared/api-contracts/` (already
landed: `runner-coordination.md` Phase 1 changes, `variant-cli.md`
`--peers` and `--qos` semantics, `toml-config-schema.md` qos optional/list
form + `<name>-qosN` expansion).

Bundles two coupled runner changes. They share spawn-construction code, so
it's cheaper to land them in one task than to land peer-injection and then
re-touch the same call sites for qos-expansion.

#### Part A — Peer source IP capture and --peers injection

Scope:
1. In `src/protocol.rs` discovery loop: switch from `recv` to `recv_from` so
   the source `SocketAddr` is available. Store `peer_hosts: HashMap<String, String>`
   on the `Coordinator` keyed by runner name; populate as `Discover` messages
   arrive.
2. New helper `src/local_addrs.rs`:
   - `pub fn local_interface_ips() -> HashSet<IpAddr>` — enumerate this
     machine's interface addresses. Use the `local-ip-address` crate
     (`list_afinet_netifas`) or `if-addrs`. Always include `127.0.0.1`.
   - Cache on first call.
3. Same-host detection: when storing a peer host, if the captured source IP
   is in `local_interface_ips()` OR equals `127.0.0.1`, store the string
   `"127.0.0.1"`. Otherwise store the source IP's string form.
4. Discovery completion criterion (in addition to existing checks): every
   name in `runners` has an entry in `peer_hosts`. Single-runner mode:
   self-populate with `127.0.0.1` and complete immediately.
5. Pass `peer_hosts` from `Coordinator` through to the spawn call site in
   `src/main.rs`. Format as `--peers name1=host1,name2=host2,...` (sorted by
   name for determinism). Inject in `src/cli_args.rs::build_variant_args`
   alongside the existing `--launch-ts`/`--variant`/`--runner`/`--run`.
6. Add Cargo dependency: `local-ip-address = "0.6"` (or `if-addrs = "0.13"`
   if preferred — pick one and document the choice in CUSTOM.md).

Tests:
- Unit: `local_interface_ips()` returns a non-empty set including
  `127.0.0.1`.
- Unit: same-host classifier correctly maps a local IP and a `127.0.0.1`
  source to `"127.0.0.1"`, and an arbitrary `192.168.x.y` to itself.
- Unit: `build_variant_args` includes `--peers` with sorted name=host pairs
  given a populated map.
- Integration (extend existing two-coordinator-on-localhost test in
  `tests/integration.rs`): after discovery, verify each coordinator's
  `peer_hosts` map contains every expected name and every value is
  `"127.0.0.1"` (since both run on the same host).
- Integration: end-to-end single-runner lifecycle with `variant-dummy` —
  verify the variant is invoked with a `--peers <self>=127.0.0.1` argument
  even though there are no peers.

#### Part B — qos expansion

Scope:
1. In `src/config.rs`: change `[variant.common].qos` field from `u8` to a
   custom enum/typed-form that accepts integer, array, or omission. Suggest:
   `pub enum QosSpec { Single(u8), Multi(Vec<u8>), All }` with a serde
   helper that maps `qos = N → Single(N)`, `qos = [..] → Multi(..)`,
   missing key → `All`. Validate elements are 1..=4.
2. New helper `pub fn QosSpec::levels(&self) -> Vec<u8>`:
   - `Single(n)` → `vec![n]`
   - `Multi(v)` → sorted unique copy of `v`
   - `All` → `vec![1, 2, 3, 4]`
3. In the main loop (`src/main.rs`), expand each `[[variant]]` entry by
   `qos_spec.levels()` into one or more "spawn jobs". A spawn job carries:
   - `effective_name`: original `name` if `levels.len() == 1`, otherwise
     `format!("{}-qos{}", name, qos)`.
   - `qos`: the concrete level for this spawn.
   - All other fields (binary, timeouts, common minus qos, specific) shared
     from the source entry.
4. Ready/done barriers and timeouts use `effective_name`. Each spawn is
   independent and runs the full stabilize/operate/silent cycle.
5. CLI arg construction in `src/cli_args.rs`: when building args for a spawn
   job, override `--qos` with the job's concrete level (so `[variant.common]`
   no longer needs to carry it as an integer) and use `effective_name` as
   `--variant`.
6. Sequential execution: spawn jobs from one source `[[variant]]` entry run
   in ascending QoS order before moving on to the next entry. Insert a small
   inter-job grace period (e.g. 250 ms) to let TCP/UDP sockets fully release
   before the next QoS spawn binds the same `base_port + qos_stride` port.
   Make this configurable via top-level `inter_qos_grace_ms` (optional,
   default 250).

Tests:
- Unit: `QosSpec` deserialization for all three forms (integer, array,
  omitted).
- Unit: `QosSpec::levels()` produces expected vectors; `Multi([3, 1, 1, 4])`
  → `[1, 3, 4]` (sorted unique).
- Unit: spawn-job expansion: a fixture with one entry having `qos = [1, 3]`
  produces 2 jobs with `effective_name` = `<name>-qos1` and `<name>-qos3`,
  both with the original `binary` and remaining common fields.
- Unit: a fixture with `qos = 2` (single integer) produces 1 job with
  unchanged `effective_name`.
- Unit: a fixture with `qos` omitted produces 4 jobs.
- Integration: build a single-runner config with `qos = [1, 2]` against
  `variant-dummy`, run end-to-end, verify two JSONL log files are
  produced (one per qos), each with the expected `qos` field in records.

#### Part C — Wiring + docs

1. Update `runner/CUSTOM.md` (orchestrator does this part — see below).
   Worker should re-read it after orchestrator updates and follow the
   guidance.
2. Update `runner/STRUCT.md` to reflect new `local_addrs.rs` module.

#### Validation against reality

- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Run `runner --name local --config <config-with-qos-omitted>` against a
  config that uses `variant-dummy` with `qos` omitted, and verify 4 log
  files appear with the correct `-qosN` naming.
- Run two runner instances on localhost in two terminals against the same
  config; verify both progress through all per-qos spawns in lockstep and
  both peer maps contain `"127.0.0.1"` for each peer.

#### Acceptance criteria

- [ ] `Coordinator` captures peer source IPs into `peer_hosts`
- [ ] Same-host detection collapses local-interface IPs and `127.0.0.1`
      sources to `"127.0.0.1"`
- [ ] `--peers <sorted name=host pairs>` injected into every variant spawn
- [ ] `QosSpec` accepts integer, array, or omitted; validation rejects
      out-of-range values
- [ ] Spawn-job expansion produces one job per QoS level; single-level
      keeps the original variant name
- [ ] Effective spawn name `<name>-qosN` used for `--variant`, ready/done
      barriers, log files
- [ ] Inter-job grace period applied between consecutive QoS spawns
- [ ] All unit tests for the new logic pass
- [ ] Integration test with `qos = [1, 2]` produces 2 distinct log files
- [ ] Two-runner-on-localhost integration still passes
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.2: QUIC variant — consume --peers, derive ports from base_port

**Repo**: `variants/quic/`
**Status**: pending
**Depends on**: T9.1 (needs the runner to inject `--peers` and pass `--qos`
per spawn).

Migrate the QUIC variant from explicit `bind_addr` and `peers` config
fields to a single `base_port` config field, with bind/connect addresses
derived at runtime from the runner-injected `--peers` and `--runner` args
plus the per-spawn `--qos`.

Scope:
1. CLI parsing (likely in `src/main.rs` or wherever QUIC's specific args
   are parsed): replace `--bind-addr` and `--peers` (the old variant-
   specific peers) with `--base-port <u16>`. Continue to accept the
   runner-injected `--peers <name=host,...>` (string).
2. Identity resolution: parse `--peers`, look up `--runner` to find
   `runner_index` (0-based, sorted by name). Treat any peer with the same
   name as self as the local entry. Fail loudly if `--runner` not in
   `--peers`.
3. Port derivation:
   - `runner_stride = 1`, `qos_stride = 10` (constants in code, documented
     in CUSTOM.md as matching the convention in `toml-config-schema.md`).
   - `my_bind_port = base_port + runner_index * runner_stride + (qos - 1) * qos_stride`
   - For each peer (excluding self): `peer_port = base_port + peer_index * runner_stride + (qos - 1) * qos_stride`
   - Bind on `0.0.0.0:my_bind_port`; connect to `<peer_host>:peer_port` for
     every other peer.
4. Update `quic.rs` connect logic accordingly. Remove any code that depends
   on the old `--bind-addr` or variant-specific `--peers`.
5. Update `variants/quic/CUSTOM.md` to document `base_port` config and the
   port-derivation rules. Orchestrator will draft this — worker should
   re-read after the orchestrator commits the update.
6. Update the QUIC entries in `configs/two-runner-all-variants.toml`:
   - Remove `bind_addr` and `peers`.
   - Add `base_port = 19930`.
   - Drop the explicit `qos = 3` to let the runner expand to all 4 levels
     (the all-variants config is meant to be comprehensive). If the user
     wants only a subset, they can use `qos = [3]` form.
   - Keep the rest unchanged.

Tests:
- Unit: identity resolution from `--peers alice=127.0.0.1,bob=127.0.0.1`
  with `--runner alice` returns index 0; `--runner bob` returns index 1.
- Unit: port derivation with `base_port=19930`, `runner_index=1`, `qos=3`
  returns `19930 + 1 + 20 = 19951`.
- Unit: `--runner` not in `--peers` returns a clear error.
- Loopback integration test: update the existing `tests/loopback.rs` to use
  the new CLI shape (synthesize `--peers self=127.0.0.1`, `--runner self`,
  `--base-port <free>`, `--qos 1` etc.). Verify the variant binds, connects
  to itself, and exchanges a message.

#### Validation against reality

- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Build the runner from T9.1 and the new QUIC binary. Run the updated
  `configs/two-runner-all-variants.toml` for QUIC entries only (e.g. comment
  out the others or use a small QUIC-only test config) on a single machine
  with two runners. Verify both runners cycle through QoS levels 1-4 and
  produce 8 JSONL log files (4 per runner).
- Spot-check a generated log file to confirm `qos` field matches the spawn
  name suffix.

#### Acceptance criteria

- [ ] QUIC `[variant.specific]` reduced to `base_port` (no `bind_addr`, no
      `peers` field)
- [ ] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [ ] Bind/connect ports computed per the convention; off-by-one errors
      checked
- [ ] Same-host loopback test still passes with new CLI shape
- [ ] `configs/two-runner-all-variants.toml` QUIC entries updated to
      `base_port`-only with no explicit `qos`
- [ ] Two-runner end-to-end QUIC run produces correctly-named per-QoS
      JSONL files
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.3: Hybrid variant — consume --peers, derive TCP ports from tcp_base_port

**Repo**: `variants/hybrid/`
**Status**: pending
**Depends on**: T9.1 (runner injects `--peers` and `--qos` per spawn).

Same migration shape as T9.2 (QUIC), applied to the Hybrid variant. Hybrid
currently takes an explicit `peers = "host:port,..."` field in TOML, which
breaks any inter-machine run (peer IPs hard-coded as `127.0.0.1`). With
T9.1 landed, the runner already injects `--peers <name=host,...>`; Hybrid
just needs to consume it and derive its own TCP ports.

UDP multicast is left as-is — same group on every runner, no QoS stride
needed (sequential per-spawn execution + `silent_secs` drain +
`inter_qos_grace_ms` provide cross-spawn isolation, and multicast doesn't
have TIME_WAIT). Only TCP gets per-runner / per-qos port derivation.

Scope:
1. CLI parsing (`src/hybrid.rs::HybridConfig::from_extra_args` and/or
   `src/main.rs`):
   - Remove `--bind-addr` and the variant-specific `--peers` (the
     comma-separated `host:port` list).
   - Keep `--multicast-group` and `--tcp-base-port`.
   - Parse the runner-injected `--peers <name=host,...>` from extra args.
2. Identity resolution: parse `--peers`, look up `--runner` to find
   `runner_index` (0-based, sorted by name). Fail loudly if `--runner`
   is not in `--peers`.
3. Port derivation (constants in code, mirror QUIC convention):
   - `runner_stride = 1`, `qos_stride = 10`
   - `my_tcp_listen = tcp_base_port + runner_index * runner_stride + (qos - 1) * qos_stride`
   - For each non-self peer:
     `peer_tcp_port = tcp_base_port + peer_index * runner_stride + (qos - 1) * qos_stride`
   - Bind TCP listener on `0.0.0.0:my_tcp_listen`. Connect to
     `(peer_host, peer_tcp_port)` for every peer except self.
4. UDP multicast: bind on `multicast_group` directly (no stride). All
   runners share the group.
5. Remove the now-dead `Cargo.toml` `mdns-sd` dependency if it's still
   listed (no `discovery.rs` exists, but the dep may be hanging on).
6. Update `variants/hybrid/CUSTOM.md` reference to `discovery.rs` if any
   linger — orchestrator already cleaned the main spec, but worker should
   re-read after orchestrator commits and align if needed.
7. Update the Hybrid entries in `configs/two-runner-all-variants.toml`:
   - Remove `peers` lines.
   - Drop the explicit `qos = 2` so the runner expands to all 4 levels
     (the all-variants config is meant to be comprehensive — and Hybrid
     is exactly the variant where comparing UDP-path QoS 1-2 vs TCP-path
     QoS 3-4 is most interesting).
   - Keep `multicast_group` and `tcp_base_port`.

Tests:
- Unit: identity resolution from `--peers alice=127.0.0.1,bob=127.0.0.1`
  with `--runner alice` returns index 0; `--runner bob` returns index 1.
- Unit: port derivation with `tcp_base_port=19900`, `runner_index=1`,
  `qos=4` returns `19900 + 1 + 30 = 19931`.
- Unit: `--runner` not in `--peers` returns a clear error.
- Existing integration test: update to the new CLI shape with
  `--peers self=127.0.0.1`, `--runner self`, `--qos <1..4>`. With a
  single-peer map there are no peers to connect to (self excluded by
  design); the test now exercises bind/listen + message framing only.
  Cross-peer flow is covered by the manual two-runner validation below.

Validation against reality:
- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Use a small Hybrid-only test fixture (or temporarily comment out the
  non-Hybrid entries in `two-runner-all-variants.toml`) and run two
  runners on localhost in two terminals.
- Verify both runners cycle through all 4 QoS levels and produce 8 JSONL
  log files (4 per runner, named e.g. `hybrid-1000x100hz-qos1...`
  through `...-qos4...`).
- Spot-check at least one QoS 1-2 file (UDP path) and one QoS 3-4 file
  (TCP path) to confirm the `qos` field matches the spawn-name suffix
  and that records are present (i.e. cross-runner delivery worked on both
  transport paths).

Acceptance criteria:
- [ ] Hybrid `[variant.specific]` reduced to `multicast_group` + `tcp_base_port`
      (no `peers`, no `bind_addr`)
- [ ] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [ ] TCP bind/connect ports computed per the convention; off-by-one
      errors checked
- [ ] UDP multicast still binds the configured group with no stride
- [ ] Loopback test passes with new CLI shape
- [ ] `mdns-sd` dependency removed from `Cargo.toml` if present
- [ ] `configs/two-runner-all-variants.toml` Hybrid entries updated:
      `peers` removed, `qos` removed
- [ ] Two-runner end-to-end Hybrid run produces correctly-named per-QoS
      JSONL files; both UDP and TCP paths verified
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.4a: Zenoh variant — make extra-arg parser lenient

**Repo**: `variants/zenoh/`
**Status**: pending
**Depends on**: T9.1 (runner injects `--peers` to every variant).

Zenoh's `ZenohArgs::parse` at `src/zenoh.rs` currently bails on any
unknown `--<name>` token in extra args. Since T9.1, the runner injects
`--peers <name=host,...>` into every variant. Zenoh has its own discovery
(Zenoh scouting) and does not need peer info — but the strict parser now
breaks every Zenoh run that goes through the runner.

This was missed during T9.1 because validation only covered the
already-migrated variants. Caught when the user tried the full
`two-runner-all-variants.toml` on two real machines and saw Zenoh
(and custom-udp) fail.

Scope:
1. In `src/zenoh.rs::ZenohArgs::parse`, replace the `bail!("unknown
   Zenoh argument: ...")` arm with a lenient skip:
   - When an unknown `--<name>` token is seen, advance past it AND the
     following token (treat as a `--name value` pair so the parser stays
     in sync with the standard convention used by the runner). If the
     unknown token doesn't start with `--`, just skip it.
2. Update the existing test that asserts `parse(&["--unknown"])` errors
   so it now succeeds (returns defaults). Add a new test that asserts
   `parse(&["--peers", "alice=127.0.0.1,bob=192.168.1.10"])` succeeds and
   leaves `mode`/`listen` at defaults.

Tests:
- `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean.
- The existing single-process Zenoh integration test still passes.

Acceptance criteria:
- [ ] `ZenohArgs::parse` ignores unknown `--<name> <value>` pairs without
      erroring
- [ ] Test for unknown-arg pass-through added
- [ ] Existing tests pass with the updated `--unknown` expectation
- [ ] `cargo test`, `cargo clippy`, `cargo fmt --check` clean
- [ ] STATUS.md updated

---

### T9.4b: Custom UDP variant — consume --peers, derive TCP port from tcp_base_port

**Repo**: `variants/custom-udp/`
**Status**: pending
**Depends on**: T9.1 (runner injects `--peers` and `--qos` per spawn).
Independent of T9.4a — can run in parallel.

Same migration shape as T9.3 (Hybrid), applied to the Custom UDP variant.
Custom UDP currently has its own `--peers` parser at `src/udp.rs:56-65`
expecting old-style `host:port,host:port`. Now that the runner injects
`--peers <name=host,...>`, that parser fails on the new format ("invalid
peer address: invalid socket address syntax"). It triggers regardless of
QoS because the parser runs unconditionally during config build.

Custom UDP uses TCP for QoS 4 only — for QoS 1-3 it's UDP-only and
peer-host info is not needed at the transport layer. So peer info is only
load-bearing for QoS 4, but the parser must succeed for ALL QoS values
(parse runs before connect).

UDP multicast is left as-is — same group on every runner, no QoS stride
needed. Only TCP gets per-runner / per-qos port derivation.

Scope:
1. In `src/udp.rs::UdpConfig::from_extra` (and any related main.rs
   plumbing):
   - Remove the old `--peers` (host:port list) and `--bind-addr` arg
     handling.
   - Add `--tcp-base-port <u16>` parsing (variant-specific, required).
   - Parse the runner-injected `--peers <name=host,...>` from extra args.
2. Identity resolution: parse `--peers`, look up `--runner` to find
   `runner_index` (0-based, sorted by name). Fail loudly if `--runner`
   is not in `--peers`.
3. TCP port derivation per spawn (only consumed at QoS 4):
   - `runner_stride = 1`, `qos_stride = 10`
   - `my_tcp_listen = tcp_base_port + runner_index * runner_stride + (qos - 1) * qos_stride`
   - For each non-self peer:
     `peer_tcp_port = tcp_base_port + peer_index * runner_stride + (qos - 1) * qos_stride`
   - Bind TCP listener on `0.0.0.0:my_tcp_listen`. Connect to
     `(peer_host, peer_tcp_port)` for every peer except self.
4. UDP multicast: bind on `multicast_group` directly. NO runner stride,
   NO QoS stride. All runners share the group.
5. Remove `mdns-sd` from `Cargo.toml` if present.
6. Update the Custom UDP entries in `configs/two-runner-all-variants.toml`
   (8 entries):
   - Add `tcp_base_port = 19800` (or pick another free base — keep it
     distinct from Hybrid's `19900`).
   - The existing `multicast_group` and `buffer_size` stay.
   - `qos` stays omitted (T9.3 already removed it; runner expands to all
     four levels).
7. Update the loopback integration test to use the new CLI shape:
   `--peers self=127.0.0.1`, `--runner self`, `--multicast-group ...`,
   `--buffer-size ...`, `--tcp-base-port ...`, `--qos <1..4>`. With a
   single-peer map there are no peers to connect to; test exercises
   bind/listen + framing only.

Tests:
- Unit: identity resolution from `--peers alice=127.0.0.1,bob=127.0.0.1`
  with `--runner alice` returns index 0; `--runner bob` returns index 1.
- Unit: port derivation with `tcp_base_port=19800`, `runner_index=1`,
  `qos=4` returns `19800 + 1 + 30 = 19831`.
- Unit: `--runner` not in `--peers` returns a clear error.
- Unit: parse succeeds at QoS 1 even though peer info is unused.
- Existing integration test: update to new CLI shape.

Validation against reality (orchestrator will run the cross-variant smoke
test in T9.4c — worker only needs to confirm same-machine):
- Run `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
  clean.
- Use a small Custom-UDP-only test fixture and run two runners on
  localhost. Verify both runners cycle through all 4 QoS levels and
  produce 8 JSONL log files. Spot-check at least one QoS 1-3 file (UDP
  path) and the QoS 4 file (TCP path) — the `qos` field must match the
  spawn-name suffix and there must be cross-runner receive records.

Acceptance criteria:
- [ ] Custom UDP `[variant.specific]` reduced to `multicast_group` +
      `buffer_size` + `tcp_base_port` (no `peers`, no `bind_addr`)
- [ ] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [ ] Parse succeeds for all QoS values; TCP setup only runs at QoS 4
- [ ] TCP bind/connect ports computed per the convention
- [ ] UDP multicast still binds the configured group with no stride
- [ ] Loopback test passes with new CLI shape
- [ ] `mdns-sd` dependency removed from `Cargo.toml` if present
- [ ] `configs/two-runner-all-variants.toml` Custom UDP entries updated
      (`tcp_base_port = 19800` added)
- [ ] Two-runner-on-localhost end-to-end: 8 JSONL files, both UDP and
      TCP paths verified
- [ ] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean
- [ ] STATUS.md updated

---

### T9.4c: Cross-variant smoke run on the user's two-machine setup

**Repo**: top-level (no code; runs binaries)
**Status**: pending
**Depends on**: T9.4a, T9.4b, T9.3, T9.2, T9.1 (everything in E9).

Orchestrator-owned validation. Once T9.4a and T9.4b ship, run the full
`configs/two-runner-all-variants.toml` end-to-end across the user's two
machines (alice and bob) and confirm every variant × every QoS spawn
exits successfully and produces the expected JSONL log files.

Scope (orchestrator):
1. Confirm both `runner` and all four variant binaries (custom-udp,
   hybrid, quic, zenoh) are built in release on both machines.
2. Ask the user to run two runners against
   `configs/two-runner-all-variants.toml` and capture stdout.
3. Verify all 32 variant entries × QoS expansion produced log files on
   both machines without parse errors.
4. Spot-check one log file per variant family for receive records.
5. Add an entry to `metak-shared/LEARNED.md` summarising the regression
   uncovered by this run and the fix pattern.

Acceptance criteria:
- [ ] All-variants run completes without `[variant] error: ...` lines on
      either machine
- [ ] Per-variant per-QoS JSONL files appear on both machines
- [ ] Spot-checks confirm cross-runner delivery on each variant family
- [ ] Regression + fix pattern documented in LEARNED.md

---

## Previous Sprint — E8: Application-Level Clock Synchronization

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
