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

## What's next

| Epic | Status | Can start now? |
|------|--------|----------------|
| E4: Analysis Tool Phase 1 | done | -- |
| E5: Analysis Tool Phase 2 (diagrams) | not started | Yes — E4 complete |
| E6: Analysis Tool Phase 3 (time-series) | not started | After E5 |
| E7: End-to-End Validation | not started | After E4 + at least one E3 on two machines |
