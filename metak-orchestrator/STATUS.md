# Execution Status

## Summary

| Epic | Status | Tests | Notes |
|------|--------|-------|-------|
| E0: Variant Exploration | done | — | 18 candidates researched, 5 selected |
| E1: Variant Base Crate | done | 29 (27 unit + 2 integration) | Variant trait, driver, logger, VariantDummy |
| E2: Benchmark Runner | done | 33 (29 unit + 4 integration) | Config, coordination, spawning. Post-delivery fixes applied. |
| E3a: Zenoh | done | 9 (8 unit + 1 integration) | Native Rust, Zenoh pub/sub |
| E3b: Custom UDP | done | 30 (29 unit + 1 integration) | All 4 QoS levels, NACK retransmit |
| E3c: Aeron | blocked | — | Scaffold complete, rusteron-client won't compile on Windows |
| E3d: QUIC | done | 11 (9 unit + 2 integration) | quinn, async-to-sync bridge |
| E3e: Hybrid UDP/TCP | done | 15 (13 unit + 2 integration) | UDP for QoS 1-2, TCP for QoS 3-4 |
| E4: Analysis Tool | done | 51 (42 unit + 9 integration) | Phase 1 complete: parse, cache, correlate, integrity, performance, CLI tables |
| E5-E7 | not started | — | |
| E8: Clock Sync | in planning | — | Contract drafted; awaiting user review before spawning workers |

**Total passing tests: 178** (127 Rust across 6 crates + 51 Python in analysis)

**End-to-end verified**: Two-runner coordination with custom-udp variant
on same machine — both runners discover each other, barrier-sync, spawn
variants, produce JSONL logs (255 writes + 255 receives per runner).

---

## E0: Variant Exploration — done

18 candidates researched across pub/sub frameworks, raw protocols, and
shared memory. 5 selected:

1. Zenoh (E3a) — high-level framework, native Rust, <10 us
2. Custom UDP (E3b) — raw protocol, all 4 QoS manual, 2-5 ms
3. Aeron (E3c) — finance-grade, C bindings, 21-57 us
4. QUIC (E3d) — modern protocol, native Rust, 2-12 ms
5. Hybrid UDP/TCP (E3e) — simplest correct, UDP for L1-2, TCP for L3-4

Deliverable: `metak-shared/variant-candidates.md`
Decisions: D1, D4, D5, D6

---

## E1: Variant Base Crate — done

27 unit + 2 integration tests passing. Clippy clean.

Crate provides: `Variant` trait (5 methods), `CliArgs` (clap), JSONL
`Logger` (7 event types), `SeqGenerator`, `ResourceMonitor`,
`ScalarFlood` workload, protocol driver (4 phases), `VariantDummy`
(no-network echo), `variant-dummy` binary.

---

## E2: Benchmark Runner — done (with post-delivery fixes)

29 unit + 4 integration tests passing. Clippy clean.

### Post-delivery fixes (applied by orchestrator after E2 worker completed)

**Fix 1: Discovery protocol race (Windows)**

The original broadcast-based discovery failed when two runners ran on the
same Windows machine — one runner would complete discovery and move to the
ready barrier, but the other never saw its Discover messages.

Root cause: on Windows, UDP broadcast to `255.255.255.255` is not reliably
delivered between two processes bound to the same port, even with
`SO_REUSEADDR`. Additionally, the fast runner stopped sending Discover
messages after completing discovery, leaving the slow runner stuck.

Fix:
- Each runner gets a unique port: `base_port + index_in_runners_list`
  (e.g. alice=19876, bob=19877). No port contention.
- Messages sent via both **multicast** (`239.77.66.55`) for cross-machine
  and **localhost** (`127.0.0.1`) for same-machine fallback.
- Socket joins the multicast group to receive cross-machine traffic.
- Discovery accepts any message type (Discover, Ready, Done) as proof
  of peer existence, handling the race where a fast peer moves to barriers.
- 2-second linger after discovery completes — keeps broadcasting Discover
  so slower peers can finish.

**Fix 2: CLI arg ordering**

Variant-specific args (e.g. `--multicast-group`) were placed after common
args but before runner-injected args (`--launch-ts`, `--variant`, etc.).
Clap's `trailing_var_arg` treated the unknown specific args as trailing,
absorbing the runner-injected args too.

Fix: runner-injected args now come before specific args, separated by `--`:
```
<binary> [common args] [runner-injected args] -- [specific args]
```

---

## E3: Concrete Variant Implementations

### E3a: Zenoh — done

8 unit + 1 integration tests. Zenoh peer mode, blocking API via `Wait`
trait, `FifoChannelHandler` subscriber, compact binary wire format.

### E3b: Custom UDP — done

29 unit + 1 integration tests. All 4 QoS levels: fire-and-forget (L1),
stale-discard (L2), NACK retransmit with 10K buffer (L3), TCP (L4).
Compact big-endian wire format.

### E3c: Aeron — blocked

Full scaffold committed (trait impl, message codec, 11 unit tests in
source). Build fails: `rusteron-client` C FFI compilation fails on
Windows due to LLVM MinGW/MSVC toolchain mismatch in bindgen. Expected
to build on Linux. See DECISIONS.md for unblock steps.

### E3d: QUIC — done

9 unit + 2 integration tests. Async-to-sync bridge via internal tokio
runtime + mpsc channels. QoS 1-2 via QUIC unreliable datagrams, QoS 3-4
via reliable streams. Self-signed certs via rcgen.

### E3e: Hybrid UDP/TCP — done

13 unit + 2 integration tests. UDP multicast for QoS 1-2, TCP with
TCP_NODELAY for QoS 3-4. Zero application-layer reliability logic.
Directly tests whether NACK-based reliable-UDP (E3b) is worth the
complexity vs kernel TCP.

---

## E4: Analysis Tool Phase 1 — done

51 tests (42 unit + 9 integration). ruff format and ruff check clean.

Python analysis tool that ingests JSONL log files, caches parsed data in
a pickle file, correlates write-receive events, runs integrity verification,
computes performance metrics, and prints CLI summary tables.

Modules: `analyze.py` (CLI), `cache.py` (pickle caching with mtime-based
change detection), `parse.py` (JSONL parsing, Event/DeliveryRecord dataclasses),
`correlate.py` (write-receive correlation), `integrity.py` (QoS-aware integrity
checks: completeness, ordering, duplicates, gap recovery), `performance.py`
(connection time, latency percentiles, throughput, jitter, loss, resource usage),
`tables.py` (CLI table formatting).

Verified against real logs from two-runner-logs/: custom-udp (alice+bob,
bidirectional, 255 writes each, 100% delivery) and dummy (alice loopback,
255 writes, near-zero latency). Diagrams placeholder for E5.

---

## E9: Peer Discovery Injection + QoS Expansion

### T9.1: Runner — peer source IP capture, --peers injection, qos expansion — done

Runner-side changes from E9 contract updates landed and verified end-to-end
against `variant-dummy` on Windows. All acceptance criteria met.

#### What was implemented

Part A — peer source IP capture and `--peers` injection:
- New `src/local_addrs.rs` with `local_interface_ips()` (cached set of this
  machine's interface IPs from `local-ip-address` crate, always including
  IPv4/IPv6 loopback) and `canonical_peer_host()` (collapses any local or
  loopback source IP to the literal `"127.0.0.1"`; passes remote IPs through
  as `to_string()`).
- `Coordinator` switched discovery from `recv` to `recv_from`. New
  `peer_hosts: Mutex<HashMap<String, String>>` populated as `Discover`
  messages arrive (skipping self-loopback echoes; self is pre-populated to
  `"127.0.0.1"` at construction). Discovery completion now requires every
  expected runner to also have an entry in `peer_hosts`. New public method
  `peer_hosts() -> HashMap<String, String>`.
- `cli_args::build_variant_args` extended with two new parameters
  (`effective_variant_name`, `effective_qos`) and a `peer_hosts` map.
  Injects `--peers` as comma-separated `name=host` pairs sorted by name.
  Skips the common-section `qos` key in favor of the per-spawn `--qos`.
- `main.rs` snapshots `peer_hosts` after discovery and threads it through
  every spawn.
- Cargo dep added: `local-ip-address = "0.6"` (chosen over `if-addrs` for
  more active maintenance).

Part B — QoS expansion:
- New `QosSpec` enum (`Single(u8)`, `Multi(Vec<u8>)`, `All`) with
  `levels()` (sorted, deduped) and `validate()` (1..=4 range, non-empty
  arrays). Parsed lazily via `VariantConfig::qos_spec()` from the
  `[variant.common].qos` field — accepts integer, array, or omission.
- New `src/spawn_job.rs` with `SpawnJob { effective_name, qos, source_index }`
  and `expand_variant()` that turns one `[[variant]]` entry into one job
  per concrete level. Single-level entries keep the original `variant.name`;
  multi-level entries synthesize `<name>-qosN`.
- `main.rs` main loop iterates entries, expands each into jobs, and runs
  jobs sequentially in ascending QoS order with a `Duration::from_millis(...)`
  grace sleep between consecutive QoS spawns from the same entry. Top-level
  `inter_qos_grace_ms` field (default 250 ms) controls the sleep.
- Effective spawn name flows through `--variant`, ready/done barriers, and
  the variant log filename (variant-base writes `<variant>-<runner>-<run>.jsonl`).

#### Tests run and results

- `cargo test`: 54 unit + 7 integration = 61 tests, all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo fmt --check`: clean.

New unit tests:
- `local_addrs`: 5 tests covering loopback inclusion, non-empty result,
  caching, and the local/loopback/remote IP classification.
- `config::QosSpec`: 5 tests covering single integer, array form, omitted,
  out-of-range rejection (single + array), and empty-array rejection.
  Plus 2 tests for `inter_qos_grace_ms` default and override.
- `spawn_job`: 5 tests covering single integer, array expansion with
  suffix, omitted-expands-to-4, deduplication, single-element array keeps
  original name.
- `cli_args`: extended `build_args_includes_all_sections` to assert
  `--peers` appears with sorted single-runner value; added
  `format_peers_arg_sorts_by_name`, `format_peers_arg_single_entry`,
  `build_args_uses_effective_variant_name_and_qos` (verifies effective
  name overrides `--variant`, and `--qos` appears exactly once with the
  per-spawn level), `build_args_includes_peers_pairs_sorted` (multi-peer
  ordering).
- `protocol::two_runner_localhost_coordination` extended to assert each
  coordinator's `peer_hosts` contains both runner names mapped to
  `"127.0.0.1"`.
- `protocol::single_runner_discover_is_immediate` extended to assert
  self-population.

New integration tests (in `tests/integration.rs`):
- `qos_array_produces_per_qos_log_files`: runs end-to-end against
  variant-dummy with `qos = [1, 2]`; verifies summary mentions
  `dummy-qos1` + `dummy-qos2`, exactly 2 JSONL files appear with the
  correct names, and each file's `qos` field matches the suffix.
- `qos_omitted_produces_four_log_files`: same with `qos` omitted; verifies
  4 JSONL files (`dummy-qos1` … `dummy-qos4`) appear.
- `single_runner_injects_peers_arg_with_self_loopback`: spawns a new
  `arg-echo` test helper as the variant binary; the helper writes its
  argv to a JSON file. Asserts `--peers self=127.0.0.1` is present in the
  captured args, `--variant` uses the original name (single-QoS),
  `--qos 1` is the runner-injected value.

End-to-end validation (manually executed, in addition to integration
tests above):
- `runner --name local --config tests/fixtures/qos-array.toml`: produced
  `dummy-qos1-local-qosarr.jsonl` and `dummy-qos2-local-qosarr.jsonl`,
  with `"qos":1` and `"qos":2` in their records respectively.
- `runner --name local --config tests/fixtures/qos-omitted.toml`: produced
  4 JSONL files (`dummy-qos1` … `dummy-qos4`), all with the correct qos
  field.
- `runner --name local --config tests/fixtures/single-runner.toml`
  (single integer qos = 1): backward compatible — produced `dummy` (no
  `-qos1` suffix) as expected.

#### Notes / minor deviations

- Pre-existing clippy warning `clippy::approx_constant` was tickled by a
  lurking `3.14` literal in a `cli_args` test (unrelated to this task);
  changed to `2.5` to keep `cargo clippy -- -D warnings` clean.
- `--peers` injection works against `variant-dummy` because variant-base's
  CLI uses `trailing_var_arg = true, allow_hyphen_values = true` on the
  `extra: Vec<String>` field, which absorbs unknown flags. Variants that
  want to consume `--peers` will need to parse it from `extra` (current
  pattern in QUIC) or update variant-base to expose it as a typed field.
  This is expected and aligned with T9.2 (which migrates QUIC to the new
  `--peers`).
- `STRUCT.md` updated to document `local_addrs.rs`, `spawn_job.rs`, the
  new fixtures, and the `arg-echo` helper binary.

#### Acceptance criteria

All ticked:
- [x] `Coordinator` captures peer source IPs into `peer_hosts`
- [x] Same-host detection collapses local-interface IPs and `127.0.0.1`
      sources to `"127.0.0.1"`
- [x] `--peers <sorted name=host pairs>` injected into every variant spawn
- [x] `QosSpec` accepts integer, array, or omitted; validation rejects
      out-of-range values
- [x] Spawn-job expansion produces one job per QoS level; single-level
      keeps the original variant name
- [x] Effective spawn name `<name>-qosN` used for `--variant`, ready/done
      barriers, log files
- [x] Inter-job grace period applied between consecutive QoS spawns
- [x] All unit tests for the new logic pass
- [x] Integration test with `qos = [1, 2]` produces 2 distinct log files
- [x] Two-runner-on-localhost integration still passes
- [x] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` clean
- [x] STATUS.md updated

---

### T9.2: QUIC variant — consume --peers, derive ports from base_port — done

QUIC variant migrated from explicit `bind_addr`/`peers` config fields to a
single `base_port` field, with bind/connect addresses computed at runtime
from the runner-injected `--peers`, `--runner`, and per-spawn `--qos`. All
acceptance criteria met. Two-runner-on-localhost validation produced 8
JSONL files (4 QoS × 2 runners) with the correct `qos` field and
bidirectional message flow on every level.

#### What was implemented

- `variants/quic/src/main.rs` rewritten:
  - Removed the old runner-name-to-letter-index hack and the
    `--bind-addr` / variant-specific `--peers` extraction.
  - New `parse_peers(raw)` parses `name=host,...` strings into a
    sorted-by-name `Vec<(String, String)>`. Trims whitespace, rejects
    empty/malformed entries.
  - New `derive_endpoints(peer_map, runner, base_port, qos)` returns a
    `DerivedEndpoints { bind_addr, peers }` per the convention in
    `metak-shared/api-contracts/toml-config-schema.md`:
    - `RUNNER_STRIDE = 1`, `QOS_STRIDE = 10` (constants documented).
    - `my_bind_port = base_port + runner_index + (qos - 1) * 10`.
    - `peer_port = base_port + peer_index + (qos - 1) * 10`.
    - Bind on `0.0.0.0:my_bind_port`; connect to `<peer_host>:peer_port`
      for every peer except self.
    - Fails loudly with a clear "runner '<x>' not present in --peers
      (have: ...)" error when `--runner` is missing from `--peers`.
    - All arithmetic uses `checked_add` / `checked_mul` to prevent silent
      overflow.
  - Required-arg parsing via `parse_required_extra_arg` for
    `--base-port` and `--peers`; clear error messages on absence or
    invalid u16.
- mDNS discovery code: not present in this crate (already retired in a
  previous pass; `discovery.rs` does not exist). No work needed.
- `variants/quic/tests/loopback.rs` rewritten to the new CLI shape:
  - `test_binary_loopback_exits_successfully`: synthesizes
    `--peers self=127.0.0.1`, `--runner self`, `--base-port 19440`,
    `--qos 1`. With self-only peers and self excluded, the variant binds
    and runs to completion (verifying lifecycle + log file production).
  - `test_binary_runner_not_in_peers_fails`: synthesizes a multi-peer
    `--peers` map with a `--runner` value that isn't in it; asserts
    non-zero exit and stderr mentions both the missing runner name and
    "not present".
  - `test_binary_missing_base_port_fails`: omits `--base-port`; asserts
    non-zero exit and stderr mentions "base-port".
- `configs/two-runner-all-variants.toml` QUIC entries (8 of them)
  updated:
  - `[variant.specific]` reduced to `base_port = 19930` (was
    `bind_addr` + `peers`).
  - `qos = 3` removed from `[variant.common]` so the runner expands to
    all 4 QoS levels (4 spawns per entry; 32 total spawns across 8
    QUIC entries).
  - All other fields (binary, timing, workload, values_per_tick,
    log_dir) unchanged.
  - Added a header comment block on the first QUIC entry documenting
    QoS expansion and the port-derivation convention.
- `variants/quic/STRUCT.md` updated to describe the new `main.rs`
  responsibilities and the rewritten `loopback.rs` tests; added the
  `tests/fixtures/` entry.

#### Tests run and results

- `cargo test`: 20 unit + 3 integration = 23 tests, all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo fmt -- --check`: clean.

New unit tests in `main.rs::tests`:
- `test_parse_peers_single` / `test_parse_peers_sorts_by_name` /
  `test_parse_peers_trims_whitespace` / `test_parse_peers_rejects_malformed`
- `test_identity_resolution_alice_index_0` /
  `test_identity_resolution_bob_index_1` (alice=0, bob=1 from sorted
  `--peers`).
- `test_port_derivation_qos3_runner1`: `base 19930, runner_index 1,
  qos 3 → 19951`.
- `test_port_derivation_all_qos_levels_disjoint`: enumerates all
  (runner × qos) combinations and asserts 8 distinct bind ports — proves
  no collisions for the two-runner / four-qos case.
- `test_runner_not_in_peers_errors`: clear error message contains the
  missing runner name and "not present".
- `test_invalid_qos_errors`: `qos = 0` and `qos = 5` rejected.
- `test_self_only_no_peers_to_connect`: with `--peers self=127.0.0.1`
  and `--runner self`, `peers` Vec is empty.
- `test_parse_extra_arg_*`: 3 tests for the basic key/value extractor.

#### Manual two-runner-on-localhost validation

Config: `variants/quic/tests/fixtures/two-runner-quic-only.toml` (a
QUIC-only test fixture I added to keep the validation focused and fast).
Single QUIC entry with `qos` omitted, 1s stabilize / 3s operate / 1s
silent, base_port 19930.

Steps:
1. Built `runner` and `variant-quic` in release mode.
2. Spawned two runners on the same machine in parallel:
   - `runner --name alice --config variants/quic/tests/fixtures/two-runner-quic-only.toml`
   - `runner --name bob   --config variants/quic/tests/fixtures/two-runner-quic-only.toml`
3. Both runners discovered each other (peer_hosts both `"127.0.0.1"`),
   barrier-synced through all 4 QoS spawns, and exited 0.

Per-spawn binding observed:
- alice qos1 19930 → bob qos1 19931
- alice qos2 19940 → bob qos2 19941
- alice qos3 19950 → bob qos3 19951
- alice qos4 19960 → bob qos4 19961
- (bob's bind/connect symmetric on the other side)

Log files in
`logs-t92/quic-t92-validation-20260501_010741/`: exactly 8 JSONL files
(4 QoS levels × 2 runners), named per the `<name>-qosN-<runner>-<run>`
convention:
```
quic-1000x100hz-qos1-alice-quic-t92-validation.jsonl
quic-1000x100hz-qos1-bob-quic-t92-validation.jsonl
quic-1000x100hz-qos2-alice-quic-t92-validation.jsonl
quic-1000x100hz-qos2-bob-quic-t92-validation.jsonl
quic-1000x100hz-qos3-alice-quic-t92-validation.jsonl
quic-1000x100hz-qos3-bob-quic-t92-validation.jsonl
quic-1000x100hz-qos4-alice-quic-t92-validation.jsonl
quic-1000x100hz-qos4-bob-quic-t92-validation.jsonl
```

Spot-check: `qos` field in alice's `qos1`/`qos2` and bob's `qos3`/`qos4`
files matches the spawn-name suffix exactly (`"qos":1`, `"qos":2`,
`"qos":3`, `"qos":4`).

Cross-runner message flow: alice's logs show
30,100 writes and 30,100 receives at every QoS level — bidirectional
delivery is working (receives are coming from the peer, not loopback,
since self is excluded from the connection list).

#### Notes / minor deviations

- Added `variants/quic/tests/fixtures/two-runner-quic-only.toml` for
  manual two-runner validation. Kept it inside `variants/quic/` to
  respect the "stay within variants/quic/" rule.
- Made `DerivedEndpoints` derive `Debug` so test `unwrap_err` works on
  the `Result<DerivedEndpoints, _>` return.
- The new loopback test no longer exercises self-message-echo — that
  pattern is incompatible with the new identity-based contract (self is
  excluded from peer connections by design). The two-runner localhost
  validation above covers cross-peer flow.

#### Acceptance criteria

All ticked:
- [x] QUIC `[variant.specific]` reduced to `base_port` (no `bind_addr`,
      no `peers` field)
- [x] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [x] Bind/connect ports computed per the convention; off-by-one errors
      checked (8 distinct ports verified across 2×4 combinations)
- [x] Same-host loopback test still passes with new CLI shape
- [x] `configs/two-runner-all-variants.toml` QUIC entries updated to
      `base_port`-only with no explicit `qos`
- [x] Two-runner end-to-end QUIC run produces correctly-named per-QoS
      JSONL files
- [x] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean
- [x] STATUS.md updated

---

### T9.3: Hybrid variant — consume --peers, derive TCP ports from tcp_base_port — done

Worker T9.3 implemented the migration; the agent paused mid-validation
(qos1-3 logs produced, qos4 not yet attempted) before writing a completion
report. Orchestrator finished the validation directly: rebuilt the hybrid
binary in release, ran two runners (alice, bob) on localhost against
`variants/hybrid/tests/fixtures/two-runner-hybrid-only.toml` to completion.
All four QoS spawns succeeded on both runners.

Files modified by worker:
- `variants/hybrid/src/hybrid.rs` — `HybridConfig` refactored to take
  pre-derived addresses (`multicast_group`, `bind_addr`, `tcp_listen_addr`,
  `tcp_peers: Vec<SocketAddr>`); the variant no longer parses peers itself
  or knows about runner identity / QoS strides.
- `variants/hybrid/src/main.rs` — parses `--peers`, `--runner`, `--qos`,
  `--multicast-group`, `--tcp-base-port`; resolves runner index by sorted
  position of `--runner` in `--peers`; computes
  `my_tcp_listen = tcp_base + runner_index * 1 + (qos - 1) * 10` and
  `peer_tcp_port = tcp_base + peer_index * 1 + (qos - 1) * 10` for each
  non-self peer; builds `HybridConfig` and hands it to `HybridVariant`.
- `variants/hybrid/tests/integration.rs` — rewritten for new CLI shape
  (`--peers self=127.0.0.1`, `--runner self`, `--qos <N>`); single-peer
  loopback exercises bind/listen + framing only (cross-peer flow validated
  end-to-end via the two-runner fixture).
- `configs/two-runner-all-variants.toml` — Hybrid entries: removed
  `peers = ...` line, removed explicit `qos = 2` (runner now expands to
  all 4 levels). Worker also removed `qos = 2` from the custom-udp entries
  in the same file (out of strict T9.3 scope, but compatible — custom-udp
  supports all 4 QoS internally and the all-variants config is meant to be
  comprehensive); flagging for awareness.

Files added by worker:
- `variants/hybrid/tests/fixtures/two-runner-hybrid-only.toml` — hybrid-only
  two-runner-on-localhost validation fixture.

`mdns-sd` was never in `Cargo.toml` despite the old CUSTOM.md text — the
discovery code path never existed. Nothing to remove. CUSTOM.md was
already cleaned by the orchestrator before spawning T9.3.

Tests:
- `cargo test --release`: 24 unit + 7 integration, all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.

Two-runner-on-localhost validation (orchestrator-completed):
- Fixture: `variants/hybrid/tests/fixtures/two-runner-hybrid-only.toml`
  (run name `hybrid-t93-validation`, log dir `./logs-t93`, base ports
  multicast 19542, tcp 19940).
- Both `alice` and `bob` cycled through qos1→qos2→qos3→qos4 in lockstep,
  exit 0 for every spawn.
- Logs: 8 JSONL files in
  `logs-t93/hybrid-t93-validation-20260501_132351/` named
  `hybrid-t93-qos{1..4}-{alice,bob}-hybrid-t93-validation.jsonl`.
- Cross-runner delivery on UDP path: qos1 alice file 9061 records;
  receives present.
- Cross-runner delivery on TCP path: qos4 alice file 6051 records, 3010
  `event:"receive"` lines, 6020 records carry `qos:4`. TCP listen ports
  observed via runtime: alice qos4 19970, bob qos4 19971 (matches
  `19940 + runner_index + 30`).

Acceptance criteria:
- [x] Hybrid `[variant.specific]` reduced to `multicast_group` +
      `tcp_base_port` (no `peers`, no `bind_addr`)
- [x] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [x] TCP bind/connect ports computed per the convention; off-by-one
      checks in the runtime port log
- [x] UDP multicast still binds the configured group with no stride
- [x] Loopback test passes with new CLI shape
- [x] `mdns-sd` dependency confirmed absent from `Cargo.toml`
- [x] `configs/two-runner-all-variants.toml` Hybrid entries updated:
      `peers` removed, `qos` removed
- [x] Two-runner end-to-end Hybrid run produces correctly-named per-QoS
      JSONL files; both UDP (qos1-2) and TCP (qos3-4) paths verified
- [x] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean
- [x] STATUS.md updated

Open notes for orchestrator follow-up:
- Worker side-effected the custom-udp entries in
  `configs/two-runner-all-variants.toml` (dropped explicit `qos = 2`).
  Compatible and arguably more useful, but not in scope. Surface to user.

---

### T9.4a: Zenoh variant — make extra-arg parser lenient — done

`ZenohArgs::parse` in `variants/zenoh/src/zenoh.rs` previously bailed on
any unknown `--<name>` token, breaking every runner-launched Zenoh spawn
since T9.1 started injecting `--peers <name=host,...>` into all variants.

Files modified:
- `variants/zenoh/src/zenoh.rs`:
  - `ZenohArgs::parse`: replaced the `bail!("unknown Zenoh argument:
    ...")` arm with a lenient skip. When an unknown token starts with
    `--`, advance past it AND the following token (treating it as a
    `--name value` pair, matching the runner's convention). When the
    token does not start with `--`, just skip the token.
  - Test `test_zenoh_args_unknown_arg` renamed to
    `test_zenoh_args_unknown_arg_is_lenient` and inverted: now asserts
    `parse(&["--unknown"])` succeeds and returns defaults
    (`mode = "peer"`, `listen = None`).
  - New test `test_zenoh_args_peers_injection_ignored` asserts
    `parse(&["--peers", "alice=127.0.0.1,bob=192.168.1.10"])` succeeds
    and leaves `mode`/`listen` at defaults — the exact shape the runner
    now injects.

Tests:
- `cargo fmt --check`: clean.
- `cargo clippy -- -D warnings`: clean.
- `cargo test`: 9 unit (8 prior + 1 new) + 1 integration
  (`loopback_full_protocol`) — all 10 tests pass. The single-process
  Zenoh integration test still passes.

Acceptance criteria — all ticked:
- [x] `ZenohArgs::parse` ignores unknown `--<name> <value>` pairs without
      erroring
- [x] Test for unknown-arg pass-through added
      (`test_zenoh_args_peers_injection_ignored`)
- [x] Existing tests pass with the updated `--unknown` expectation
- [x] `cargo test`, `cargo clippy`, `cargo fmt --check` clean
- [x] STATUS.md updated

---

### T9.4b: Custom UDP variant — consume --peers, derive TCP port from tcp_base_port — done

The custom-udp variant had its own `--peers` parser at `src/udp.rs:56-65`
expecting old-style `host:port,host:port` and ran unconditionally during
config build. After T9.1 began injecting `--peers <name=host,...>` into
every variant spawn, that parser failed for ALL QoS levels with
`invalid peer address: invalid socket address syntax`.

This worker migrated custom-udp to the same shape as Hybrid (T9.3) and
QUIC (T9.2): the variant parses the runner-injected `--peers` map, looks
up its own index by `--runner` name, and derives per-runner / per-qos TCP
ports from a single `--tcp-base-port`. UDP multicast still binds the
configured group directly with no stride. TCP is only wired up at QoS 4
but derivation must succeed at all QoS levels because parse runs before
connect.

Files modified:

- `variants/custom-udp/src/main.rs`: rewritten to mirror
  `variants/hybrid/src/main.rs`. Adds:
  - `RUNNER_STRIDE = 1`, `QOS_STRIDE = 10` constants.
  - `parse_peers(raw)` -> sorted `Vec<(name, host)>` (sort gives stable
    cross-runner indexing).
  - `derive_endpoints(peer_map, runner, tcp_base_port, qos)` ->
    `DerivedTcpEndpoints { tcp_listen_addr, tcp_peers }`. Excludes self
    from `tcp_peers`. Errors loudly when `--runner` not in `--peers`.
  - `parse_extra_arg` / `parse_required_extra_arg` helpers (same as
    Hybrid).
  - `run()` reads `--multicast-group`, `--buffer-size`, `--tcp-base-port`,
    `--peers` from extras, derives endpoints, builds `UdpConfig`, and
    delegates to `run_protocol`.
- `variants/custom-udp/src/udp.rs`:
  - `UdpConfig` shape changed: removed `peers: Vec<SocketAddr>`, added
    `tcp_listen_addr: SocketAddr` + `tcp_peers: Vec<SocketAddr>`.
  - `UdpConfig::from_extra` removed entirely (parsing moved to main.rs).
  - `setup_tcp` now binds `self.config.tcp_listen_addr` (instead of
    `0.0.0.0:0`) and connects to each pre-derived peer in
    `self.config.tcp_peers`. Sets TCP_NODELAY on both sides.
  - Stale unit tests for the old `from_extra` shape removed; the three
    surviving unit tests cover variant name, connect/disconnect lifecycle,
    and pre-connect `poll_receive` returning None.
- `variants/custom-udp/tests/integration.rs`: NEW. Subprocess-based
  integration test mirroring `variants/hybrid/tests/integration.rs`:
  - `udp_lifecycle_qos1`, `udp_lifecycle_qos2`, `udp_lifecycle_qos3`
    (UDP path), `tcp_lifecycle_qos4` (TCP path). Each runs the binary
    with `--peers self=127.0.0.1`, `--runner self`,
    `--multicast-group <unique>`, `--buffer-size 65536`,
    `--tcp-base-port <unique>`, and asserts exit 0 + JSONL log produced.
  - `runner_not_in_peers_fails` asserts loud failure when `--runner`
    `carol` is not in `--peers`.
  - `missing_tcp_base_port_fails` and `missing_multicast_group_fails`
    assert clear error when required extras are absent.
- `variants/custom-udp/tests/multicast_loopback.rs`: untouched (still
  passes; tests raw-socket multicast wire format independent of the
  variant CLI).
- `variants/custom-udp/STRUCT.md`: updated module responsibilities to
  reflect the new main.rs surface and the new integration.rs test.
- `configs/two-runner-all-variants.toml`: added `tcp_base_port = 19800`
  to all 8 custom-udp `[variant.specific]` blocks (verified with grep:
  exactly 8 occurrences). Distinct from Hybrid's 19900. The existing
  `multicast_group` and `buffer_size` were left as-is. `qos` already
  omitted (T9.3 dropped it).

`Cargo.toml`: no `mdns-sd` was present (T9.3 noted it likely never was).
No dependency change needed.

Tests:
- `cargo fmt --check`: clean.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo test`: 41 unit + 7 integration + 1 multicast loopback = 49
  tests, all pass.
  - Unit additions cover identity resolution at index 0 and 1, port
    derivation at qos=1 and qos=4 (`19800 + 1*1 + 3*10 = 19831`
    asserted), all-qos-levels-disjoint (8 distinct ports across 2
    runners x 4 qos), runner-not-in-peers loud error, invalid qos
    rejection, self-only peer-list yielding zero peers to connect to,
    and "derive_endpoints succeeds at all QoS" (covers the
    parse-must-succeed-even-when-TCP-isn't-used requirement).

Manual two-runner-on-localhost validation:

- Built `runner.exe` (already from T9.1) and a fresh release
  `variant-custom-udp.exe`.
- Used a small custom-udp-only fixture (single `[[variant]]` entry,
  `tick_rate_hz = 50`, `values_per_tick = 5`, `operate_secs = 4`,
  `multicast_group = "239.0.0.1:19550"`, `tcp_base_port = 19800`, `qos`
  omitted -> 4 spawn expansion).
- Ran `runner.exe --name alice` and `runner.exe --name bob` in
  parallel against the fixture.
- Both runners discovered each other (`peer_hosts: {"alice":
  "127.0.0.1", "bob": "127.0.0.1"}`), cycled through all 4 QoS levels
  in lockstep, and reported `status=success exit_code=0` for every
  spawn.
- 8 JSONL log files produced (4 per runner): qos1/qos2/qos3 (UDP path)
  + qos4 (TCP path). The QoS 4 spawns also logged the expected
  `[custom-udp] TCP listener on 0.0.0.0:19830 for QoS 4` (alice,
  index 0) and `... 0.0.0.0:19831 ...` (bob, index 1) lines, confirming
  the per-runner port derivation matches the convention.
- Spot-checked all 8 files:
  - alice qos1: 1005 receive records with `writer=bob`, all
    `qos:1` -> UDP best-effort cross-runner delivery confirmed.
  - alice qos3: 1005 receives with `writer=bob`, all `qos:3` -> UDP
    NACK-reliable path confirmed.
  - alice qos4: 1005 receives with `writer=bob`, all `qos:4` -> TCP
    path confirmed.
  - bob qos1 / bob qos4: each 1005 receives with `writer=alice` -> the
    reverse direction works on both transports too.
  - 1005 = 5 paths x 201 ticks (50 Hz x ~4s operate window).
- `qos` field in every receive record matches the spawn-name suffix
  (`-qos1` -> `"qos":1`, etc.).
- Test fixture and `logs-t94b/` artifacts cleaned up afterwards (kept
  the run scoped to `variants/custom-udp/target/` and `./logs-t94b/`,
  both deleted post-validation).

Acceptance criteria - all ticked:
- [x] Custom UDP `[variant.specific]` reduced to `multicast_group` +
      `buffer_size` + `tcp_base_port` (no `peers`, no `bind_addr`)
- [x] Runner-injected `--peers` parsed; `--runner` resolved to an index
- [x] Parse succeeds for all QoS values; TCP setup only runs at QoS 4
- [x] TCP bind/connect ports computed per the convention
- [x] UDP multicast still binds the configured group with no stride
- [x] Loopback test passes with new CLI shape (added integration.rs;
      old multicast_loopback.rs still passes)
- [x] `mdns-sd` dependency removed from `Cargo.toml` if present
      (verified absent already)
- [x] `configs/two-runner-all-variants.toml` Custom UDP entries updated
      (`tcp_base_port = 19800` added to all 8)
- [x] Two-runner-on-localhost end-to-end: 8 JSONL files, both UDP and
      TCP paths verified
- [x] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`
      clean
- [x] STATUS.md updated

Open concerns / deviations:
- None. The migration mirrors T9.3 exactly per CUSTOM.md guidance,
  including using the same `runner_stride = 1` / `qos_stride = 10`
  constants. Cross-machine smoke is owned by T9.4c.

---

## What's next

| Epic | Status | Can start now? |
|------|--------|----------------|
| E4: Analysis Tool Phase 1 | done | -- |
| E5: Analysis Tool Phase 2 (diagrams) | not started | Yes — E4 complete |
| E6: Analysis Tool Phase 3 (time-series) | not started | After E5 |
| E7: End-to-End Validation | not started | After E4 + at least one E3 on two machines |
| E9: T9.1 Runner peer/qos | done | -- |
| E9: T9.2 QUIC variant migration | done | -- |
| E9: T9.3 Hybrid variant migration | done | -- |
| E9: T9.4a Zenoh lenient parser | done | -- |
| E9: T9.4b Custom UDP variant migration | done | -- |
