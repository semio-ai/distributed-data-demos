# Decisions Log

## D1: Variant exploration before implementation

**Date**: 2026-04-13
**Context**: The original epics assumed Zenoh and custom-UDP as the two
variants. Before committing to implementation, we want to know what the
landscape actually looks like.
**Decision**: Add E0 (Variant Exploration) as a research-only epic that
surveys transport libraries and protocols, documents each candidate's fit
with our design criteria, and produces a shortlist. Concrete variant epics
(E3+) are defined after E0 completes.
**Rationale**: Avoids premature commitment to specific transport stacks.
The exploration output also informs the variant base trait design (E1) by
revealing what capabilities the trait must accommodate.

---

## D2: Shared variant base crate with Variant trait

**Date**: 2026-04-13
**Context**: All variants share identical logic: common CLI parsing, test
protocol phases (connect, stabilize, operate, silent), JSONL logging,
resource monitoring, workload execution, sequence numbering. Only the
transport layer differs.
**Decision**: Extract shared logic into a `variant-base` library crate that
defines a `Variant` trait. Each concrete variant is a thin binary that
implements the trait and provides only transport-specific code.
**Rationale**: Ensures all variants follow the same protocol and produce
identically structured logs. Reduces duplication. Makes it easy to add new
variants — implement the trait, and everything else works automatically.
The trait also serves as a compile-time contract in addition to the
documentation-level API contracts.

---

## D3: Variant base before runner; VariantDummy included

**Date**: 2026-04-13
**Context**: The runner spawns variant binaries and collects results. The
variant base defines the trait, protocol driver, and logging that all
variants share. Originally the runner was E1 and the base was E2.
**Decision**: Swap the order — variant base is now E1, runner is E2. The
base crate also includes a `VariantDummy` implementation: a no-network
variant that uses an in-process data board.
**Rationale**: Building and testing the base crate first surfaces any issues
with the trait design, CLI contract, or log format before the runner is
written. Findings may feed back into runner design or API contracts.
`VariantDummy` serves three purposes: (1) unit/integration testing of the
base crate without network dependencies, (2) harness testing for the runner
on a single machine (spawn dummy, verify CLI arg passing, timeout handling,
log collection), (3) zero-network performance baseline measuring overhead
of everything except the transport layer.

---

## D4: E0 variant selection — four candidates

**Date**: 2026-04-13
**Context**: E0 research surveyed 18 candidates across pub/sub frameworks,
raw protocols, and shared memory / niche transports. See
`metak-shared/variant-candidates.md` for the full analysis.
**Decision**: Four variants selected for the benchmark:
1. **Zenoh** (E3a) — high-level framework, native Rust, <10 us latency
2. **Custom UDP** (E3b) — raw protocol, full manual control, all 4 QoS levels
3. **Aeron** (E3c) — finance-grade messaging, C bindings, 21-57 us latency
4. **QUIC via quinn** (E3d) — modern protocol, native Rust, multiplexed streams
**Rationale**: Each represents a different approach to the same problem:
mature framework (Zenoh), ground-up implementation (custom UDP), purpose-built
high-perf system (Aeron), modern standard protocol (QUIC). Together they
cover the design space and provide meaningful comparison data.

Eliminated: NATS and Redis (broker required), RTI Connext (experimental Rust,
commercial), io_uring and DPDK (Linux only), iceoryx2 (same-machine only),
CycloneDDS (redundant with Zenoh, C bindings), ZeroMQ (no discovery without
Zyre), Cap'n Proto (RPC not transport).

---

## D5: E1 Variant trait unchanged after E0

**Date**: 2026-04-13
**Context**: E0 identified four variant candidates. Two (Zenoh, QUIC) are
async-first. One (Aeron) uses C bindings with a callback model. One
(custom UDP) is naturally synchronous.
**Decision**: Keep the synchronous `Variant` trait unchanged. No breaking
modifications to `variant-base`.
**Rationale**: All four candidates can implement the sync trait:
- Custom UDP: maps directly to sync socket APIs.
- Zenoh: has blocking wrappers; use `try_recv` for non-blocking poll.
- Aeron: buffer callbacks into internal queue, drain via `poll_receive`.
- QUIC: spawn internal tokio runtime, bridge to sync via channels.
For benchmarking, a sync driver with a controlled tick loop is preferable —
it eliminates async runtime scheduling noise from measurements.

---

## D6: Hybrid UDP/TCP variant added (E3e)

**Date**: 2026-04-13
**Context**: DESIGN.md defines QoS 3 as reliable-UDP with NACK-based gap
recovery (application-layer reliability), while QoS 4 uses TCP (kernel
reliability). The custom UDP variant (E3b) implements the NACK protocol
for QoS 3, which is the most complex piece of that variant.
**Decision**: Add a fifth variant (E3e) that uses UDP for QoS 1-2 and TCP
for QoS 3-4. No application-layer reliability at all — the kernel TCP
stack handles ordering, retransmission, and flow control for reliable
delivery.
**Rationale**: Directly tests the design hypothesis: "On a local network,
packet loss is rare, so [TCP head-of-line blocking] is often acceptable"
(DESIGN.md S6.4). Comparing E3b (custom NACK recovery) vs E3e (TCP for
reliable) at QoS 3 under identical workloads answers whether per-path
independence is worth the complexity. The hybrid variant is also simpler
to implement, making it a good early candidate after the runner is ready.
