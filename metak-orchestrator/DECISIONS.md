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
