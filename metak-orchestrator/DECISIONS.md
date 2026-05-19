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

## D9: T-coord.1 — diagnosis of the 2026-05-07 mid-run hang

**Date**: 2026-05-07
**Context**: T-coord.1 investigation. During the 2026-05-07 Hybrid full-
matrix benchmark on alice + bob (commits `6d9a53e` / `16476d3+dirty`),
both runners completed every spawn through `hybrid-100x1000hz-qos4`
successfully, then deadlocked at the transition to spawn N+1
(`hybrid-100x100hz-qos1`). Alice's last log line was
`[runner:alice] ready barrier for spawn 'hybrid-100x100hz-qos1' ...`;
bob's last log line was
`[runner:bob] 'hybrid-100x1000hz-qos4' finished: status=success, exit_code=0`.

### Root cause

**H1 (fast peer stops broadcasting Done) is confirmed.** The done-barrier
loop in `runner/src/protocol.rs` (lines 372-462) re-broadcasts `Done`
on every iteration of the wait loop, then runs a 2-second linger after
`results.len() == self.expected.len()` (lines 446-461) before returning.
After that, the runner enters `ready_barrier` for the next variant
(`runner/src/main.rs` line 503: `coordinator.ready_barrier(&job.effective_name)?`).
**`ready_barrier` only matches `Ready`/`ProbeRequest` and silently drops
any inbound `Done` message** (lines 327-368, the trailing `_ => {}` arm).
There is no recovery path on the fast peer once the done-N linger expires.

The code paths trace as follows. On bob:

1. `runner/src/spawn.rs::spawn_and_monitor` returns success.
2. `runner/src/main.rs:580` prints `'<name>' finished: status=success, exit_code=0` (the last line in bob's log).
3. `runner/src/main.rs:586` calls `coordinator.done_barrier(&job.effective_name, status, exit_code)`.
4. Inside `done_barrier` (lines 372-462) bob broadcasts its own Done at every iteration, but never receives alice's Done — alice has already lingered out and moved on. Bob loops indefinitely.

On alice (from her side of the timeline):

1. Alice's variant exits earlier than bob's (per-machine runtime skew).
2. Alice enters `done_barrier`. Alice broadcasts Done. Alice waits.
3. Eventually bob's variant exits and bob broadcasts its first Done. Alice receives it on one of her recv windows. `results.len() == self.expected.len()` is true.
4. Alice enters the 2-second linger (lines 446-461), broadcasting Done at 500 ms intervals (~5 broadcasts).
5. Alice's linger ends. Alice returns from `done_barrier`.
6. Alice runs the inter-spawn grace (default 250 ms, `runner/src/main.rs:497-503`).
7. Alice prints `[runner:alice] ready barrier for spawn '<next>' ...` (line 499).
8. Alice enters `ready_barrier`. From this moment forward, **alice only re-broadcasts `Ready`, never `Done`**. Inbound `Done` messages from bob are silently dropped (line 360, `_ => {}`).

For bob to hang, bob's recv windows during steps 4-8 above must miss every one of alice's Done broadcasts. The 2-second linger on a quiet LAN is normally enough. What pushes the field run past it:

- **Per-machine runtime skew at high-rate variants**. `hybrid-100x1000hz-qos4` runs the workload at 100 000 values/sec for the configured operate window. On the slower of the two machines, the variant binary itself takes meaningfully longer to finish than on the faster one. Alice can have been waiting for bob's Done for tens of seconds — long enough that bob's UDP receive buffer (Windows default ~64 KB) accumulates a backlog of alice's Done broadcasts during her wait.
- **OS UDP receive buffer pressure**. While bob's variant is running, bob's runner thread is in `child.try_wait()`/sleep loops — it is NOT draining the coordination socket. Alice's Done broadcasts (~150 bytes, 2 fan-out addresses, every 500 ms) accumulate. With a long-running variant, the receive buffer fills and subsequent datagrams are dropped at the kernel level. If alice's linger broadcasts land in that "dropped" window, bob never sees them even after entering done_barrier.
- **No defense against this loss pattern**. The linger is the only recovery mechanism. Once it expires, alice's state machine has no way to re-engage with a stale Done request from bob. Bob hangs forever.

### Verdict on the four hypotheses

- **H1 — fast peer stops broadcasting Done: CONFIRMED.** Code-path
  analysis above. `done_barrier`'s 2-second linger is the only window in
  which alice will respond to a Done request for spawn N. After that,
  `ready_barrier(spawn_n_plus_1)` silently discards inbound `Done`
  messages, leaving bob with no message any peer will ever send that
  satisfies its barrier-completion condition. Reproducer:
  `runner/src/protocol.rs::done_barrier_hang_repro_when_peer_already_advanced`
  (asserts that bob's done_barrier is still hung 6 seconds after alice
  has parked in `ready_barrier(spawn_n_plus_1)`).

- **H2 — variant-name / message-type filter mismatch: RULED OUT.** Both
  runners derive `effective_name` deterministically from the same TOML
  config (config_hash mismatch would have aborted in Phase 1 discovery,
  see `runner/src/protocol.rs:215-217`). The done_barrier filter is
  `variant == variant_name && run == self.run && self.expected.contains(&name)`
  (lines 415-419) which alice's broadcast satisfies for any value bob
  expects. The bug is not in the matching predicate; it is in the
  lifetime of the broadcasts.

- **H3 — receive-window race / "post-N limbo" state: RULED OUT.** The
  done_barrier code path either still has unmet expectations
  (`results.len() < self.expected.len()`) and therefore continues to
  loop and re-send, or it transitions to the linger and then returns
  cleanly. There is no "exited the loop but hasn't yet returned"
  intermediate state. The actual hang is firmly inside the "still
  has unmet expectations" loop, not after.

- **H4 — Windows socket-state side effect (variant TCP teardown affecting
  runner UDP socket): RULED OUT.** The runner's coordination socket
  (`runner/src/protocol.rs:603-625`) is created with
  `socket2::Socket::new(Domain::IPV4, Type::DGRAM, ...)` and is owned
  exclusively by the runner process. `runner/src/spawn.rs:48-51`
  invokes `Command::new(...).spawn()` with no inheritance flags;
  Rust's default on Windows does not pass file/socket handles to the
  child by default. The `os error 997` ("Overlapped I/O operation in
  progress") observed during variant teardown is on the variant's
  TCP/UDP transport sockets, not the runner's UDP coordination socket.
  The two are decoupled.

### Proposed fix (T-coord.1b)

The fix should re-engage the fast peer with stale Done requests from a
slow peer. Three viable options, in increasing order of invasiveness:

1. **Re-broadcast Done from `ready_barrier` on demand.** When
   `ready_barrier` receives a `Done` whose `(name, variant, run)` matches
   a recently-completed spawn, re-emit our own `Done` for that variant.
   Smallest change; uses an O(1) most-recent-completed cache. ~30 lines
   in `runner/src/protocol.rs`.

2. **Extend the done-barrier linger to cover an entire spawn duration.**
   Replace the fixed 2-second linger with one that runs concurrently
   with the next variant's spawn (or at least the next variant's ready
   barrier). Larger refactor — the current shape is sequential.

3. **Replace the linger with a sticky "completed Done" state**: keep a
   `HashSet<(variant, run)>` of completed spawns and have every barrier
   loop re-broadcast a Done in response to any inbound Done request for
   a variant in that set. Cleanest semantics, modest implementation
   complexity. ~50-80 lines.

**Recommendation**: option 1. It surgically patches the failure mode
without restructuring the state machine, and the most-recent-completed
cache is bounded (one entry — we only ever care about the immediately
preceding variant). T-coord.2 (barrier timeouts + auto-resume wrapper)
lands in parallel and provides the safety net regardless. Filed as
T-coord.1b; see TASKS.md.

The fix can defer to T-coord.2 ONLY if the operator-experience cost of
"barrier timeout fires, runner exits, wrapper restarts with --resume,
30-90 seconds of wall-clock are wasted" is acceptable. For the user's
multi-hour Hybrid full-matrix runs, that would mean the run loses one
spawn-cycle every time the skew + buffer-pressure pattern triggers. We
estimate this happens ~once every several full-matrix runs based on the
2026-05-07 incident being the first observed case in many prior runs;
T-coord.2's safety net is strictly necessary, but T-coord.1b's
surgical fix is preferable for runs where bisect-reproducibility of
results matters. Land both.

### Files touched in this investigation

- `runner/src/protocol.rs` — added `set_verbose_coord(bool)` /
  `verbose_coord_enabled()` toggle and per-message verbose tracing in
  `ready_barrier` and `done_barrier` (default-off, gated by the static
  `VERBOSE_COORD` `AtomicBool`). Added the
  `done_barrier_hang_repro_when_peer_already_advanced` unit test in
  `mod tests` that demonstrates the hang.
- `runner/src/main.rs` — added the `--verbose-coord` CLI flag wired to
  `protocol::set_verbose_coord`. Default `false`. No change to the
  default execution path.
- `metak-orchestrator/DECISIONS.md` — this entry.
- `metak-orchestrator/TASKS.md` — T-coord.1b filed.
- `metak-orchestrator/STATUS.md` — completion report.

### Validation

- `cargo build --release -p runner` clean.
- `cargo test --release -p runner` — 120 unit + 10 integration + 1
  stress test, all green. The new reproducer test passes (asserting
  the hang occurs); the existing `barrier_linger_prevents_slow_peer_hang`
  test continues to pass (the legitimate slow-peer linger pattern is
  unaffected — that test's delay is 800 ms, well within the 2-second
  linger window).
- `cargo clippy --release -p runner --all-targets -- -D warnings` clean.
- `cargo fmt -p runner -- --check` clean.

The reproducer test is intentionally an "asserts the bug" test — when
T-coord.1b lands, the maintainer must invert the assertion. The test's
panic message spells this out: "REGRESSION: bob's done_barrier
completed within 6 seconds ... Invert this assertion to lock in the
fixed behaviour."

---

## D10: T-impl.5 — WebRTC signaling fragility, investigation and disposition

**Date**: 2026-05-07
**Context**: T-impl.5 was filed against the observation that many
`webrtc-*` rows in the all-variants matrix were producing 0 writes /
0 ms wall-time, suggesting the DataChannel handshake had not
completed before `operate` began. The task asked for an investigation
phase first, then a fix only if the diagnosis pointed to an
actionable, in-scope issue.

**Investigation**: I read `variants/webrtc/src/webrtc.rs` end-to-end
to map the signaling and DataChannel-open path, then spawned the
exact rate / QoS shape called out in the task — `webrtc-100x100hz-qos1`
two-runner same-host — three times in a row with verbose stderr capture.

The variant's `connect` path already contains the await-all-channels
behaviour the task hypothesized was missing:

1. `handle_peer_pair` (`variants/webrtc/src/webrtc.rs` lines ~339-467)
   spins up an `open_tx` / `open_rx` mpsc and registers `on_open`
   callbacks for every DataChannel — on both the initiator side
   (lines ~389-423) and the responder side via `on_data_channel`
   (lines ~343-386).
2. After `run_signaling` returns, the function blocks in a
   `tokio::select!` that drains `open_rx` until **all four**
   DataChannels for the peer have reported `Open`
   (lines ~444-467). The wait is bounded by `CONNECT_TIMEOUT = 15 s`.
3. Only after every expected peer's 4 channels are open does
   `WebRtcVariant::connect` return, which means
   `variant_base::driver::run_protocol` cannot enter `stabilize` /
   `operate` until the data path is ready.

The signaling exchange itself is a small length-prefixed JSON envelope
protocol (`Offer`/`Answer`/`Candidate`/`Done`) on a per-pair TCP socket
derived from `signaling_base_port + runner_index + (qos-1) * qos_stride`,
with `SIGNALING_CONNECT_TIMEOUT = 10 s` and a 500 ms grace timer that
sends a final `Done` once SDP is exchanged. ICE is host-candidates-only
(STUN/TURN/mDNS disabled in `build_api`).

**Empirical result**: three back-to-back same-host runs of
`webrtc-100x100hz-qos1` at `values_per_tick=100`, `operate_secs=10`
produced (alice writes, alice receives, bob writes, bob receives):

- Run 1: 100100, 100100, 100100, 100100
- Run 2: 100000, 100100, 100100, 100000
- Run 3: 100100, 100100, 100100, 100100

All three runs produced non-zero writes AND receives on both sides
(100% of the smoke acceptance bar of "2 of 3 produce non-zero
write AND receive"). An additional `webrtc-1000x100hz-qos1` smoke run
at 10x the load (100k ticks per side over 10 s) also completed end-to-end
with ~82-86% delivery on the unreliable QoS-1 channel — exactly the
shape expected for an unreliable transport under sustained pressure
without application-layer reliability code.

**Diagnosis**: There is no signaling-fragility bug in the WebRTC
variant at the rates and shapes the task targets. The `connect` phase
already awaits `data_channel_open` for every expected peer (with a
15-second cap), the signaling path is bounded by sane timeouts, and
ICE host-candidate gathering completes well within the 500 ms grace
timer the variant already applies. Earlier matrix runs that showed
"0 writes / 0 ms" rows for webrtc were almost certainly produced by
a now-resolved upstream coordination issue (D9 / T-coord.1b's
done-barrier hang) rather than by the WebRTC variant itself, since
the same observation would surface across multiple variants in a
single matrix run — which the historical data shows.

**Disposition**: **No fix required.** The acceptance criterion in
T-impl.5 — "at least 2 of 3 same-host smoke runs produce non-zero
write AND receive counts" — is met by the current implementation
without modification (3 of 3 actually pass). The matching note is
added to `variants/webrtc/CUSTOM.md` so future maintainers see this
diagnosis at the variant boundary.

If a future run does surface 0-write / 0-receive rows for webrtc
specifically — independent of other variants in the same matrix —
the most likely first investigation step is to bump `CONNECT_TIMEOUT`
from 15 s to ~30 s and re-instrument the DataChannel open path; that
remains the cheapest reversible knob.

### Files touched in this investigation

- `metak-orchestrator/DECISIONS.md` — this entry.
- `variants/webrtc/CUSTOM.md` — added a "Signaling robustness
  characterisation" note pointing at this entry.
- `variants/websocket/src/pairing.rs` — added the T-impl.4 unit
  test that asserts same-host port offset.
- `variants/websocket/tests/two_runner_regression.rs` — added the
  two-runner same-host smoke regression test.
- `variants/websocket/tests/fixtures/two-runner-websocket-100x100hz-qos3.toml`
  — fixture for the new regression test.
- `variants/websocket/CUSTOM.md` — added a "Same-host port-collision
  guarantee" subsection documenting the T-impl.4 verification chain.

---

## D? (open): is the strict-no-skip QoS3/QoS4 contract right for Zenoh?

**Filed 2026-05-19** after walking the Zenoh QoS3/QoS4 stall path in
`smoke-01-20260519_143351`. Not a decision yet — the user is about to
launch a cross-machine smoke that will inform it. Capturing the
question while context is fresh.

### The question

DESIGN.md § 6.5 mandates **100 % delivery, zero
`backpressure_skipped`** for reliable QoS tiers (QoS3 / QoS4). For
the variants we built ourselves (custom-udp, QUIC, hybrid,
websocket) this is straightforward: we control the reliability
layer and can shape it to fit the contract.

For Zenoh, reliability lives inside the framework as
`CongestionControl::Block`. Native CC=Block alone deadlocks
asymmetrically at sustained ≥ 50K msg/s (T16.12), so we layered an
application-level credit/window protocol over it (T17.8) — receivers
publish max-seq watermarks on a side channel, senders gate on a
condvar when any peer falls > 2048 messages behind. **We are
effectively re-implementing flow control above the framework that's
supposed to handle it.**

Three concerns:

1. **Fairness of comparison**: the headline metric is **receive
   throughput** (per overview.md "cross-cutting goals"). Zenoh's
   reliable path runs at whatever rate the credit/window allows,
   which is set by our window size + ack interval, NOT by what
   Zenoh's transport can natively sustain. Other variants' reliable
   numbers come out of their actual transport. We may be measuring
   "how well our window protocol throttles Zenoh" rather than
   "Zenoh's reliable throughput".
2. **Localhost vs cross-machine variance**: the credit/window still
   loses on localhost at ≥ 100 keys (T15.11 watchdog fires —
   smoke-01 evidence). CUSTOM.md treats T17.10 (cross-machine) as
   the canonical gate. If the cross-machine run still shows wide
   variance, the contract may be unmeetable in practice.
3. **Conceptual coherence**: variants that fundamentally can't meet
   strict-no-skip without re-implementing flow control are arguably
   in a different category from those that meet it natively. The
   honest analysis output may be a separate "framework-mediated
   reliable" column rather than treating Zenoh QoS3/QoS4 as the same
   measurement as custom-udp QoS3/QoS4.

### Options to choose between (after cross-machine results)

- **A. Keep strict-no-skip as-is.** Accept that Zenoh's reliable
  numbers reflect the credit/window's throttle, not native Zenoh.
  Lowest churn; preserves uniform contract.
- **B. Relax to "≥ 99 % delivery, log skips honestly".** Drop the
  credit/window layer entirely; let `CC=Block` apply native pressure
  and `try_publish` return `Ok(false)` when the bridge fills. Adds a
  `backpressure_skipped` count to Zenoh's reliable rows that other
  variants don't show, but measures the native transport. Analysis
  comparison plots get an honesty footnote.
- **C. Split the reliable category.** Two distinct contracts in
  DESIGN.md § 6.5: "natively reliable" (custom-udp / QUIC / hybrid /
  websocket — strict no-skip) and "framework-reliable" (Zenoh —
  best-effort within framework's flow control + bounded skip).
  Plots and tables show the two categories separately.

### Inputs needed before deciding

- Cross-machine smoke result for `two-runner-smoke.toml` Zenoh
  QoS3/QoS4 rows (about to be launched).
- If cross-machine still stalls at ≥ 100 keys: option B or C is
  forced. If cross-machine is clean: A is viable.
- Sanity-check whether other framework-mediated transports we add
  later (NATS, MQTT, Kafka if any) would face the same question.
  If yes, C generalises better than A or B.

### Out of scope right now

- Tuning `QOS_STRICT_WINDOW` / `ACK_EMIT_INTERVAL` — only worth it
  if option A is the chosen direction.
- Revisiting whether Zenoh belongs in the variant matrix at all —
  it does (per overview.md "high-level framework approach").

---
