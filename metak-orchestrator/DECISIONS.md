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

---

## D7: Zenoh path-count and max-throughput timeouts — root-cause diagnosis (T10.2)

**Date**: 2026-05-02
**Context**: Zenoh times out on 12/32 spawns of the user's two-machine
all-variants run. Failure signature: every spawn with
`values_per_tick = 1000` (regardless of `tick_rate_hz`, regardless of
QoS) times out, and every `workload = "max-throughput"` spawn (even at
`values_per_tick = 100` according to the original report; the file
`configs/two-runner-all-variants.toml` actually has 1000 there) times
out. Spawns with `values_per_tick <= 100` succeed cross-machine.

T10.2 was scoped as **investigation, not fix**. Fix is filed separately
as T10.2b in TASKS.md.

### Repro

`variants/zenoh/tests/fixtures/two-runner-zenoh-1000paths.toml` and
`two-runner-zenoh-max.toml` reproduce the timeout deterministically on
**localhost** (loopback peer-to-peer Zenoh discovery), confirming this
is not a same-host artifact (per LEARNED.md note 3, only the asymmetric
`100x10hz` was a same-host artifact — path-count failures reproduce on
loopback as expected).

Both fixtures hard-time-out at the runner's 60s spawn timeout. The
JSONL produced by `alice` contains the `connect`, `connected`,
`stabilize`, and `operate` phase records, then ~225 `write` records,
then nothing — the variant hangs mid-tick on the first operate-phase
tick. `bob`'s JSONL is 0 bytes (the variant hangs before its first
flush).

### What the diagnostic logging showed

A `--debug-trace` flag was added to `variants/zenoh/src/zenoh.rs`
(parsed from `[variant.specific].debug_trace`). When enabled it emits
flushed `[zenoh-trace]` lines on stderr for connect, every per-publish
ENTER/EXIT after a 150-call warm-up, periodic publish-rate summaries,
poll counts, and disconnect timing.

Observed pattern across multiple runs:

```
[zenoh-trace] connect: session opened in 39 ms
[zenoh-trace] connect: declare_subscriber bench/** in 0 ms
[zenoh-trace] connect: total 40 ms
[zenoh-trace] publish: count=50 avg=6 us max=42 us last_seq=50
[zenoh-trace] publish: count=100 avg=4 us max=37 us last_seq=100
[zenoh-trace] publish: ENTER seq=193 key=bench/bench/192 count=192
[zenoh-trace] publish: EXIT  seq=193 took 5 us
... (consistent ~4-12 us per put)
[zenoh-trace] publish: ENTER seq=200 key=bench/bench/199 count=199
   <silence — kept alive 25+ s, never EXIT, then taskkill>
```

Both peers stall on a single `session.put().wait()` call, somewhere in
the first tick (alice typically between publish 50-100, bob between
192-232 — the exact stall point varies but is always well before the
1000-publish first-tick boundary). They never recover.

### Root cause

Zenoh peer-to-peer mode shares its tokio multi-thread runtime
(`ZRuntime::Application`) between TX, RX, and the routing tables
(`tables_lock`). `session.put().wait()` calls
`session.resolve_put(...)` → `face.send_push_consume(...)` →
`route_data(...)` which acquires `zread!(tables_ref.tables)` (a parking
read lock from `parking_lot`) and synchronously routes the message to
each destination face's TX channel. This call is **not async** —
`Wait::wait()` for `PublicationBuilder<PublisherBuilder, PublicationBuilderPut>`
is a direct synchronous chain that does not use `block_on`. So the
default `CongestionControl::DEFAULT = Drop` does *not* save us via
async backpressure release; if the TX path stalls inside
`route_data`, the calling thread (the variant's main thread) stalls.

The repeating pattern of "first ~50-200 publishes succeed at 4-6 us
each, then a single put hangs forever" combined with **both peers
stalling simultaneously** strongly indicates a deadlock involving the
shared tables lock and the tokio runtime, with high probability:

1. **Thread starvation in Zenoh's tokio runtime**. `route_data` posts
   work onto the destination face's transport TX queue, which a tokio
   worker task is supposed to drain to the actual socket. Both
   variant processes run a tight loop of 1000 `put().wait()` calls per
   tick on the main thread, each holding `zread!(tables)` briefly. On
   loopback, both peers produce massive cross-traffic; the local RX
   side also wants `zread!(tables)` to dispatch incoming samples to
   the subscriber's FIFO. In peer mode there is no broker thread —
   the same runtime juggles TX, RX, routing, and the keep-alive HLC.
   When the variant's main thread is hogging the routing path with a
   tight publish loop, the RX side falls behind, the peer's TX side
   sees its socket buffer fill, and the inter-peer congestion control
   inside `zenoh-transport` eventually stops accepting new pushes —
   but our per-publish trace shows no return from `put().wait()`,
   which means the synchronous `route_data` is itself stuck (most
   likely on a `parking_lot` lock inside the transport multiplex or
   on an `await` inside an async path that the synchronous wrapper
   gates on).

2. **Compounded by `session.put()` re-declaring an ad-hoc publisher
   per call**. `Session::put` is implemented as
   `SessionPutBuilder { publisher: self.declare_publisher(key_expr), ... }`
   — every put declares a fresh `PublisherBuilder` (no actual session
   declaration to the network, but it does call `apply_qos_overwrites`
   and instantiates a fresh `PublisherBuilder` struct). At 1000
   distinct keys per tick, that's 1000 short-lived publisher builders
   per tick per peer. Each `route_data` call also invokes
   `get_data_route(...)` which does a wire-expr lookup against the
   tables. With 1000 distinct keys, the route cache is cold on the
   first tick and every put pays the full lookup cost.

The dominant cause is (1); (2) is a contributory aggravator that
makes the deadlock window arrive sooner with more keys.

### Why is this distinct-path-count-sensitive (and not pure throughput)?

Because **distinct paths drive Zenoh's per-key route resolution and
keyexpr declaration cost**, and that cost is paid synchronously inside
`route_data` while holding the tables read lock. With 10 paths
(repeated every tick), the route cache stays warm and one tick's worth
of puts completes in under 100 us total — the runtime catches up
before the next tick. With 100 paths, still under the threshold for
deadlock on any reasonably fast host. With 1000 paths, a single tick
takes long enough that the route resolution + per-key lookup cost
combined with the symmetric peer producing its own 1000-path tick
saturates whatever lock or channel both threads need.

The `max-throughput` workload uses the same `ScalarFlood` profile
(same 1000 paths in our config) but with **no inter-tick sleep**, so
it hits the same wall even faster — the trace shows `bob` stalling
after only ~100 publishes (vs ~200 for the rate-limited 10 Hz
fixture) because it never gets a 100ms breather to let the runtime
catch up.

So **path-count failure and max-throughput failure share the same
root cause**: synchronous Zenoh routing + symmetric high-fanout
publishing exhausts the runtime's ability to make progress on the RX
side. Max-throughput just removes the safety valve.

### What would fix it (scope of T10.2b)

Three options, in increasing order of effort:

**Option A: cache per-path Publishers** — declare a `Publisher` once
per distinct key on first publish, store in a `HashMap<String, Publisher>`
on the variant, reuse on subsequent publishes. Eliminates repeated
`PublisherBuilder` construction and lets Zenoh keep route cache
entries warm against a stable publisher set. **Estimated effort: ~30
lines in `src/zenoh.rs`, no new tests required (existing loopback
test exercises the path).** This alone may not be sufficient — the
deadlock hypothesis above suggests the lock contention persists — but
it's cheap, strictly improves the per-call cost, and is the
recommended next step in Zenoh's own docs for high-rate fixed-key
publishing.

**Option B: drive the Zenoh API from a dedicated tokio runtime via
the async API + bridging channels** — instead of using `Wait::wait()`
on the main thread, spawn an internal multi-thread tokio runtime,
shuttle publish requests over a bounded `tokio::sync::mpsc::channel`
to a tokio task that calls `session.put(...).await`, and shuttle
received samples over another channel for `poll_receive` to drain.
The variant's main thread never blocks Zenoh's runtime. **Estimated
effort: ~120-180 lines, requires reworking `connect/publish/poll_receive/disconnect`
significantly, and adds one new dependency on `tokio` proper (currently
only pulled in transitively by Zenoh).** This matches the QUIC
variant's bridge pattern and would likely fully resolve the deadlock.

**Option C: switch to `client` mode against a real Zenoh router**
— the user would run `zenohd` (the broker) on one machine and both
variant processes connect as clients. Removes peer-to-peer mesh
concerns entirely. **Adds operational complexity (separate broker
process) and changes what the benchmark is measuring** (broker-mediated
vs peer-to-peer). Not recommended unless A and B both fail.

**Recommendation**: T10.2b ships Option A first (cheap, strictly
positive). If Option A alone doesn't make the 1000-paths fixture pass
on localhost, escalate to Option B in the same task. Do not pursue
Option C — it changes the benchmark's identity.

### Decision on diagnostic logging

**Keep in place behind the `--debug-trace` flag.** Justification:

1. The investigation is going to need to validate the fix lands —
   re-running with `--debug-trace` will be the obvious confirmation
   step ("publish counts go past 1000 without an ENTER/EXIT gap").
2. The macro is a hard `if enabled` no-op when the flag is off; zero
   runtime overhead in the default path.
3. The flag is parsed lenient-style alongside the existing
   `--zenoh-mode` / `--zenoh-listen` and follows the same TOML →
   `[variant.specific]` → CLI surface, so no new contract burden.
4. Future high-rate Zenoh debugging will want the same hooks.
5. Removing it costs ~100 lines for zero forward-debugging value.

The two repro fixtures
(`variants/zenoh/tests/fixtures/two-runner-zenoh-{1000paths,max}.toml`)
ship with `debug_trace` commented out by default, with a comment
pointing at how to enable.

### Files touched in this investigation

- `variants/zenoh/src/zenoh.rs` — added `--debug-trace` flag, two
  trace macros (`trace_if!`, `trace_now!`) with explicit stderr
  flushing, and per-phase instrumentation. Two new unit tests
  (`test_zenoh_args_defaults` updated to assert `debug_trace=false`,
  new `test_zenoh_args_debug_trace_flag`).
- `variants/zenoh/tests/fixtures/two-runner-zenoh-1000paths.toml` —
  comment line for `debug_trace = true` opt-in.
- `variants/zenoh/tests/fixtures/two-runner-zenoh-max.toml` — same.
- `metak-orchestrator/DECISIONS.md` — this entry.
- `metak-orchestrator/STATUS.md` — investigation outcome.
- `metak-orchestrator/TASKS.md` — T10.2b filed.

### Validation

- `cargo test --release` clean (10 unit + 1 integration, 11/11 pass).
- `cargo clippy --release -- -D warnings` clean.
- `cargo fmt -- --check` clean.
- The single-process loopback integration test (`tests/loopback.rs`)
  still passes with the new code path — confirms the no-trace default
  path is unchanged in behaviour.

### One incidental finding (not in scope for T10.2b but worth noting)

Zenoh keys end up double-prefixed: the workload generates paths like
`/bench/0`, the publish code does
`if let Some(stripped) = path.strip_prefix('/') { format!("bench/{stripped}") }`
which produces `bench/bench/0` (the `stripped` is `bench/0`, not `0`).
Subscriber on `bench/**` still matches due to wildcard, so it works
end-to-end. Not the cause of the timeout (the timeout reproduces
regardless of key format). Worth a one-line fix in the same patch
as T10.2b but not load-bearing for the deadlock fix.

---

## D8: Application-level NTP-style clock sync (E8)

**Date**: 2026-05-03
**Context**: Cross-machine latency measurement was dominated by OS clock
skew. Real PTP needs OS + NIC support we don't control; OS-level NTP
(Windows w32time) syncs once per week and is accurate to seconds, not ms.
The benchmark target is 10 ms latency.
**Decision**: Implement application-level NTP-style 4-timestamp offset
measurement in the runner (E8 / T8.1). N=32 samples per peer, best-by-min-RTT
selection. Run once after discovery and once per variant before launch.
Results emitted to `<runner>-clock-sync-<run>.jsonl`. Analysis (T8.2)
applies offsets via polars asof join (per-variant resync preferred,
initial sync as fallback). Variants are not modified.
**Rationale**: This is the only option achievable in our binaries. Matches
BENCHMARK.md S9 option 3 (embedded round-trip). Achieves sub-ms accuracy
on a quiet LAN — three orders of magnitude better than OS clocks, and well
under the 10 ms target. Variants stay untouched, so all five existing
variant implementations continue to work without changes. Schema columns
were already reserved in E11, so no cache rebuild was needed.
**Validation**: localhost two-runner smoke 2026-05-03 produced clock-sync
JSONL with offsets in ±0.3 ms range; analysis applied them automatically.

### Outlier follow-up (T8.4 — resolved 2026-05-04)

The -387 ms outlier flagged at T8.1 closeout was investigated under T8.4.
Hypothesis 1 (stale ProbeResponse cross-talk) was eliminated by audit:
the `(from, to, id)` triple uniquely identifies an exchange and the ID
counter is monotonic per-engine. Hypothesis 2 (Windows clock
quantization) and 3 (transient OS time correction) are indistinguishable
at sample level — both would manifest as a single sample with a
plausible RTT but a wildly wrong offset. Mitigation: outlier rejection
in `pick_best`. If the min-RTT sample's offset deviates from the median
by more than 5σ, fall back to the median offset of the three samples
with the lowest RTTs. New `outlier_rejected: bool` field on the
canonical JSONL line. Per-sample raw timestamps are now also written to
a sibling `<runner>-clock-sync-debug-<run>.jsonl` for offline diagnosis.
Defense-in-depth `t1` echo verification added to `wait_for_response`.

Validation: stress harness (100 iter × 32 samples per direction) showed
mean ≈ 0.006 ms, stddev ≈ 0.003 ms, max 0.022 ms, zero outliers
triggered. Smoke re-run on `configs/smoke-all-variants.toml`: all 10
measurements between -0.073 and +0.057 ms, zero outliers. The
previously-failing alice→bob `smoke-quic` measurement is now -0.060 ms.

---
