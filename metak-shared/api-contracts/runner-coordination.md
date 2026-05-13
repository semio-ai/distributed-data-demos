# API Contract: Runner Coordination Protocol

Defines how runner instances discover each other and synchronize through
benchmark phases.

Source: BENCHMARK.md S3.

## Overview

Runners are leaderless. They progress through the benchmark config in lockstep
using symmetric barrier synchronization over UDP broadcast on the local
network.

## Phase 1: Discovery and Handshake

1. Each runner broadcasts a discovery message containing:
   - `name`: this runner's identity (must match a name in the config)
   - `config_hash`: hash of the config file contents
   - `log_subdir`: the proposed log subfolder name (see "Log subfolder
     selection" below for the resume-mode rule)
   - `resume`: boolean — whether this runner was launched with `--resume`

2. Each runner listens for discovery messages from others.

3. **Config hash mismatch**: if any received `config_hash` does not match this
   runner's hash, ALL runners abort with a clear error. This catches
   mismatched configs before anything is launched.

4. **Peer address capture**: when a discovery message arrives, the runner
   records the source address of the UDP packet (via `recv_from`) as that
   peer's address. This populates a `peer_hosts: HashMap<String, String>`
   keyed by runner name.

5. **Same-host detection**: for each captured peer source IP, compare against
   the local interfaces of this runner (enumerate via `local-ip-address` or
   `if-addrs`). If the peer source IP appears in the local interface set,
   OR the source IP is `127.0.0.1`, the peer is treated as same-host and
   stored as `127.0.0.1`. Otherwise, store the source IP as observed.
   Rationale: on Windows, multicast/broadcast loopback can deliver packets
   with either the LAN interface IP or `127.0.0.1` as source — both must
   resolve to loopback for same-host inter-variant communication. The runner
   already has localhost-fallback behaviour for its own multicast; this
   keeps variant peer addresses consistent with that.

6. Discovery completes when all runner names listed in the config's `runners`
   array have been seen, their config hashes match, AND a host address has
   been captured for each.

7. The captured `peer_hosts` map is retained for the rest of the run and
   passed into spawned variants via the `--peers` runner-injected CLI arg
   (see `variant-cli.md`).

8. **Resume-mode agreement**: every runner's `resume` flag in the discover
   message must match. If any peer reports a different value than this
   runner, ALL runners abort with a clear error. Resume is an all-or-nothing
   property of the run.

### Log subfolder selection

The discover message includes a `log_subdir` proposal. The first runner in
the config's `runners` array is the leader; its proposal wins for the run.

**Fresh mode** (`--resume` absent): each runner proposes
`<bench_config.run>-<ts>` where `ts` is the current UTC time formatted
`YYYYMMDD_HHMMSS`. The leader's `ts` becomes the run's `log_subdir`.

**Resume mode** (`--resume` present on every runner):
- Each runner enumerates its resolved `<base_log_dir>/` for entries whose
  name starts with `<bench_config.run>-` and selects the lexicographically
  greatest one (the timestamp suffix sorts correctly). Empty folders are
  fine; only existence is required.
- That selection becomes the runner's proposed `log_subdir` in discover.
- The leader's proposal still wins. Each follower must have a directory of
  that exact name on disk; if not, it aborts with a clear error before
  proceeding to Phase 1.25.
- If a runner finds NO matching folder locally, it aborts immediately
  before sending discover (resume requires a previous run to exist).

### Discovery responds to late-arriving discoveries

Discovery's exit condition admits any inbound message type — `Discover`,
`Ready`, `Done`, `ResumeManifest` — as proof that the sending peer
exists. Only `Discover` carries the `log_subdir` field. A non-leader
that joins late (so the leader has already advanced into a Phase 2
barrier and stopped broadcasting `Discover`) can therefore observe
`seen == expected && hosts_known` while never having received the
leader's `log_subdir`.

To prevent this from being a fatal panic, two cooperating rules apply:

1. **Late-discovery recovery loop in `discover()`.** Once the quorum
   condition is met but `leader_log_subdir` is still `None`, the
   non-leader keeps broadcasting `Discover` and reading inbound
   messages, bounded by an internal 30-second budget, until the
   leader's `Discover` arrives. If the budget elapses, `discover()`
   returns an `Err` describing the situation rather than panicking
   (the typical cause is an older peer binary without rule 2 below).
   This bound is internal to `discover()` and is **not** the same as
   the external `--barrier-timeout-secs` budget — discovery remains
   exempt from that timeout.

2. **Post-discovery loops re-emit `Discover` on demand.** When
   `ready_barrier`, `done_barrier`, or the Phase 1.25 ResumeManifest
   exchange receives an inbound `Discover` from a peer in the
   expected set, the runner re-broadcasts a fully-formed `Discover`
   message carrying the agreed `log_subdir`. This is best-effort
   (errors swallowed) and does not affect the active barrier's
   progress. It mirrors the "ready barrier responds to stale done
   requests" rule that protects the spawn-N → spawn-N+1 boundary on
   `Done` (T-coord.1b).

Together these rules make the discovery exit symmetric with the
barrier-linger pattern: the fast peer keeps responding to the slow
peer's earlier-phase messages long after its own discovery has
completed, and the slow peer is willing to keep listening for a
bounded period beyond quorum.

## Phase 1.25: Resume Inventory (only when `resume = true`)

After discovery completes (config hashes matched, log_subdir agreed, peer
hosts captured) and BEFORE initial clock sync, all runners exchange a
manifest of which spawn jobs they consider locally complete:

1. Each runner expands its config into the same ordered list of spawn jobs
   as Phase 2 (see `toml-config-schema.md` "Variant Templates" and "Array
   Expansion" — same `effective_name` set).

2. For each `effective_name`, the runner checks whether
   `<log_subdir>/<effective_name>-<self_name>-<run>.jsonl` exists and is
   non-empty. **Empty files are deleted at this point** (they represent
   crashed/aborted prior attempts and must be re-run cleanly).

3. Each runner exchanges a `ResumeManifest` payload with every peer via
   a per-peer-pair TCP connection (see "Manifest exchange transport
   (T14.24)" below). The payload carries:
   - `name`: this runner's identity
   - `run`: the bench config's run id (echoed for safety)
   - `complete_jobs`: sorted, deduplicated array of `effective_name`
     strings for which the local log file exists and is non-empty

4. Each runner waits until it has received a manifest from every peer
   listed in `runners` (the TCP exchange handles loss and ordering via
   TCP's own retransmit; per-pair connect attempts retry until the peer's
   listener binds, bounded by `--barrier-timeout-secs`).

5. **Intersection rule**: a job is considered "fully complete" for the run
   iff its `effective_name` appears in every runner's manifest (including
   this runner's own). Jobs not fully complete are "incomplete".

6. **Cleanup of incomplete jobs**: for each incomplete job, every runner
   deletes its own `<effective_name>-<self_name>-<run>.jsonl` if present
   (regardless of size). This guarantees that the upcoming spawn writes
   into a clean file and that no stale partial data survives.

7. The fully-complete set is retained for the rest of the run as the
   "skip set". In Phase 2, jobs in the skip set bypass their ready
   barrier, spawn, and done barrier entirely. Jobs not in the skip set
   run normally.

Single-runner mode in resume: the inventory step still runs, but the
intersection is just "self". A non-empty self-log = skip; otherwise run.
No network exchange is needed; the cleanup rule still applies (delete
empty files; partial-from-self is treated as complete-for-self).

### Manifest exchange transport (T14.24)

The manifest exchange uses **per-peer-pair TCP**, not UDP multicast. The
ready / done barriers remain on UDP multicast for now; only Phase 1.25's
ResumeManifest moved to TCP.

Motivation: at full-matrix scale (e.g. all-variants-01 with ~192 spawn
jobs), the serialised `ResumeManifest` JSON exceeds the runner's 4 KB
UDP recv buffer. The pre-T14.24 UDP implementation silently truncated
oversized datagrams, the receiver's parse returned `None`, and every
500 ms re-broadcast resent the same over-sized payload. Both runners
timed out at 120 s waiting for a manifest the kernel had already dropped.
TCP per-peer-pair eliminates the truncation class of failure entirely
and inherits TCP retransmit for free.

Wire details:

- **Pairing.** For each unordered peer pair `(a, b)` with
  `a < b` lexicographically (by runner name), `a` accepts and `b`
  connects. Self-loops do not exchange; only inter-peer pairs.
- **Port derivation.** Each runner listens on
  `--port + 32 + runner_index`, where `runner_index` is the runner's
  position in the config's `runners` array (zero-based). The UDP
  coordination range remains at `--port + runner_index`. The constant
  `32` is the `RESUME_MANIFEST_TCP_OFFSET` in `runner/src/protocol.rs`
  — chosen large enough to leave headroom for many-runner experiments
  while staying inside the same low-numbered ephemeral region operators
  already need to permit through the firewall for UDP coordination.
- **Framing.** Each direction sends one length-prefixed frame:
  `[u32 BE length][JSON bytes]`. The JSON is the same `ResumeManifest`
  message wire-shape as the older UDP version. The accept side reads
  first, then writes; the connect side writes first, then reads — this
  matches by design and avoids any chance of a write-write deadlock.
- **Reliability.** The connecting side retries its connect attempt
  until the accepting side's listener has bound (process-startup skew
  tolerance), bounded by the overall `--barrier-timeout-secs` deadline.
  Once connected, TCP retransmit handles in-flight loss; an I/O error
  fails that attempt and the connector retries.
- **Defensive bounds.** Manifest payloads above 4 MiB
  (`RESUME_MANIFEST_MAX_BYTES`) are rejected on read; this is well
  above any plausible benchmark scale and prevents an unbounded
  allocation on a peer that lies about its length prefix.

On overall timeout (no convergence within `--barrier-timeout-secs`) the
runner exits with code 75 (`EX_TEMPFAIL`) just like the ready / done
barriers, and the wrapper scripts re-launch with `--resume` appended.

The on-disk artefacts and intersection / cleanup semantics are
**unchanged**; only the transport differs. A peer-pair where one side
runs an older binary that still expects UDP multicast for ResumeManifest
is not interoperable — but this is an internal coordination protocol on
a single coordination port range, so all peers must run the same binary
version anyway (the discovery `config_hash` check already enforces this
indirectly via mismatched compiled config layouts).

## Phase 1.5: Initial Clock Sync

After discovery completes (config hashes match) and before the first ready
barrier, each runner measures pairwise clock offsets against every other
runner using the protocol defined in `clock-sync.md`. Results are written
to `<runner>-clock-sync-<run>.jsonl`.

Single-runner runs skip this phase entirely.

**In resume mode**: clock sync is always performed (initial and per-variant
resyncs both run for non-skipped jobs). The clock-sync log file is opened
in append mode so prior measurements are preserved. New entries are
appended for the resume execution.

## Phase 2: Per-Variant Execution

For each spawn job derived from the variant expansion (in order):

**Resume skip**: if the job's `effective_name` is in the skip set computed
in Phase 1.25, ALL barriers, the spawn, and the per-variant clock resync
are bypassed for that job. The runner emits an informational log line
(`[runner:<name>] skipping '<effective_name>' (resume: complete on all peers)`)
and proceeds to the next job. The skip is consistent across runners
because the skip set was derived from a network-exchanged manifest under
the intersection rule.

### Ready Barrier

- Each runner broadcasts: `ready for variant <name>`
- Waits until all runners have signaled ready for this variant.
- **Transport (T15.10):** Ready frames travel over the dedicated TCP
  barrier channel introduced in "Ready/Done barrier transport
  (T15.10)" below. The legacy UDP multicast path is retained as a
  fallback exercised only by the in-process unit tests that do not
  install the TCP transport; production runners always install it
  before the first ready barrier.

### Per-Variant Clock Resync

After the ready barrier and before launch, each runner re-measures clock
offsets against every other runner (same protocol as Phase 1.5). This
catches drift across the run. Logged with `variant = <name>` so analysis
picks the most recent measurement preceding the variant's writes.

### Launch

- Each runner spawns the variant binary as a child process.
- CLI arguments are constructed from the config (see `variant-cli.md`).
- The runner records `launch_ts` immediately before spawning and passes it
  as `--launch-ts`.

### Monitor

- The runner waits for the child process to exit (`waitpid` or equivalent).
- **No IPC** with the child — only exit status observation.
- If the child does not exit within `timeout_secs` (per-variant or
  `default_timeout_secs`), the runner kills it and records a timeout.

### Done Barrier

- Each runner broadcasts: `done with variant <name>` along with exit status
  (success / failure / timeout).
- Waits until all runners have reported done.
- Proceeds to the next variant, or finishes if all variants are complete.
- **Transport (T15.10):** Done frames travel over the same TCP
  barrier channel as Ready (see "Ready/Done barrier transport
  (T15.10)" below).

### Ready/Done barrier transport (T15.10)

Before T15.10 the ready/done barriers shared the UDP coordination
socket with discovery, clock-sync, and probe responses. Under
symmetric same-host stress (e.g. `configs/two-runner-stress-e14.toml`
at 1000 vpt x 100 Hz x 2 directions ~~ 200K msgs/s on the variant
data plane plus multicast loopback on the coord socket) the kernel's
per-socket UDP recv buffer overflowed during the variant-transition
window between spawns, dropping `Ready` / `Done` datagrams. With no
application-level retransmit and the receiver's 2 s linger having
already expired, the local barrier waited the full
`--barrier-timeout-secs` (default 120 s) and exited 75.

T15.10 moves the ready/done quorum signal to a dedicated long-lived
TCP-per-peer-pair channel mirroring T14.24's resume_manifest pattern
and T15.3's progress_coord pattern. Wire details:

- **Pairing.** Same rule as T14.24 / T15.3: for peer pair `(a, b)`
  with `a < b` lexicographically by runner name, `a` accepts and `b`
  connects. Self-loops do not exchange.
- **Port derivation.** Each runner listens on
  `--port + 96 + runner_index`, where `runner_index` is the runner's
  position in the config's `runners` array (zero-based). The
  constant `96` is `BARRIER_TCP_OFFSET` in
  `runner/src/barrier_coord.rs`. Full layout:
  - UDP coord: `base + index`
  - Resume manifest TCP (T14.24): `base + 32 + index`
  - Progress TCP (T15.3): `base + 64 + index`
  - Barrier TCP (T15.10): `base + 96 + index`

  All four ranges sit inside the same low ephemeral region operators
  already permit for UDP coordination, so no new firewall rules are
  required.
- **Handshake.** The connecting side writes a single length-prefixed
  UTF-8 frame carrying its runner name (`[u32 BE length][name bytes]`,
  with a 256-byte cap on `length`). The accepting side reads this
  frame, verifies the name is in the expected runners set, and
  installs the stream as the peer's writer. There is no reverse
  handshake -- peer identity is established by the connect side
  only.
- **Framing.** Each subsequent barrier frame is one length-prefixed
  JSON encoding of `Message::Ready` or `Message::Done`:
  `[u32 BE length][JSON bytes]`. Frames above
  `BARRIER_FRAME_MAX_BYTES` (16 KiB) are rejected on read; the
  payload today is a few hundred bytes, so this cap is purely
  defensive against a peer that lies about its length prefix.
- **Lifecycle.** The coordinator's `start()` runs after discovery
  (which populates `peer_hosts`). It accepts inbound connections and
  makes outbound connect attempts in parallel, retrying connects
  every ~500 ms until the peer's listener has bound (bounded by an
  internal `BARRIER_STARTUP_BUDGET` of 15 s). Once a pair has
  connected, the connection remains open across every Phase 2 spawn.
  A reader thread per peer folds incoming Ready/Done frames into a
  per-peer inbox; the barrier loops drain the inbox on every poll.
  `shutdown()` flips an atomic stop flag, closes all streams (which
  the peers observe as EOF), and joins the reader threads. **Failure
  to bind the TCP listener at runner startup is fatal** (the
  runner exits with a clear error, not EX_TEMPFAIL) -- a bind
  failure indicates a port collision or firewall rule, not a
  transient peer condition, so the wrapper should not retry.
- **Single-runner mode.** No transport is set up; the broadcast and
  poll helpers short-circuit and the barrier methods return
  immediately as they did pre-T15.10.

**Defensive isolation from UDP coordination.** Ready/Done frames
received on the UDP coord socket while a TCP barrier is in progress
do NOT count toward quorum -- the TCP path is the sole source of
the quorum signal once installed. The UDP socket continues to
service clock-sync probes, late-discovery re-emission (T-coord.3),
and stale-Done re-emission (T-coord.1b); those messages were never
the quorum signal, they are recovery aids for cross-phase races.

**Backwards compatibility.** The on-disk artefacts and the
`Message::Ready` / `Message::Done` wire shapes are unchanged. A
peer running an older binary that expects ready/done on UDP
multicast is not interoperable with a peer running T15.10 -- but
this is an internal coordination protocol on a single coordination
port range, so all peers must run the same binary version anyway
(the discovery `config_hash` check already enforces this indirectly
via mismatched compiled config layouts).

On overall barrier timeout (no convergence within
`--barrier-timeout-secs`) the runner still exits with code 75
(`EX_TEMPFAIL`) and the wrapper script re-launches with `--resume`,
identical to the pre-T15.10 contract.

### Per-Spawn Progress Exchange (T15.3, E15)

While a variant child is running, each runner broadcasts a per-spawn
progress snapshot to every other runner once per second. The receivers
maintain a `RemoteProgressView { peer_runner -> spawn -> snapshot }`
that T15.4's phase-aware termination state machine consults alongside
the local progress tracker.

**Message.** A new coordination-message type is added:

```json
{
  "type": "progress_update",
  "runner": "alice",
  "spawn": "dummy-qos2",
  "phase": "operate",
  "sent": 1234,
  "received": 5678,
  "eot_sent": false,
  "eot_received": false,
  "ts": "2026-05-11T00:00:00.000000000Z"
}
```

Fields mirror the variant's `event=progress` stdout schema (see
`variant-cli.md` "E15 additions"). `runner` is the sender's identity;
`spawn` is the variant's `effective_name`; counters are monotonic
per-spawn aggregates across all peers; `eot_sent` / `eot_received` are
sticky flags; `phase` reflects the variant's current protocol-driver
phase as observed at the most recent stdout event; `ts` is the
sender's wall-clock at snapshot time (RFC 3339 nanoseconds).

The receiver indexes incoming snapshots by `(runner, spawn)` and
updates counters monotonically (a later frame whose counters are
smaller than the stored snapshot is ignored defensively). T15.4
consumes the receiver-side `last_update_ts` (not the wire `ts`) when
deciding "have we heard recently from peer X about spawn Y".

**Cadence.** Approximately one snapshot per active spawn per second
per peer. The publisher reads the local `LocalProgressTracker` on each
tick and broadcasts to every connected peer. Snapshots stop when the
local variant child exits (the spawn loop drops the broadcaster
closure between spawns).

**Transport.** Long-lived per-peer-pair TCP, **distinct from the
T14.24 resume-manifest TCP channel** even though it mirrors that
pattern. Rationale: resume_manifest is a one-shot exchange that closes
after a single round-trip during Phase 1.25; reusing it for the
continuous-stream Phase 2 case would force a complicated lifecycle
across both. A dedicated channel keeps both protocols simple and
independent. Wire details:

- **Pairing.** For each unordered peer pair `(a, b)` with `a < b`
  lexicographically, `a` accepts and `b` connects -- same rule as
  T14.24's resume_manifest exchange. Self-loops do not exchange.
- **Port derivation.** Each runner listens on
  `--port + 64 + runner_index`, where `runner_index` is the runner's
  position in the config's `runners` array (zero-based). The
  constant `64` is `PROGRESS_TCP_OFFSET` in
  `runner/src/progress_coord.rs`. Layout summary:
  - UDP coord: `base + index`
  - Resume manifest TCP (T14.24): `base + 32 + index`
  - Progress TCP (T15.3): `base + 64 + index`

  All three ranges sit inside the same low ephemeral region operators
  already permit for UDP coordination, so no new firewall rules are
  required.
- **Handshake.** The connecting side writes a single length-prefixed
  UTF-8 frame carrying its runner name (`[u32 BE length][name bytes]`,
  with a 256-byte cap on `length`). The accepting side reads this
  frame, verifies the name is in the expected runners set, and
  installs the stream as the peer's writer. There is no reverse
  handshake -- a peer's identity is established by the connect side
  only.
- **Framing.** Each subsequent `progress_update` is one length-prefixed
  JSON frame: `[u32 BE length][JSON bytes]`. Frames above
  `PROGRESS_FRAME_MAX_BYTES` (64 KiB) are rejected on read; the
  payload today is a few hundred bytes, so this cap is purely
  defensive against a peer that lies about its length prefix.
- **Lifecycle.** The coordinator's `start()` runs after discovery
  (which populates `peer_hosts`). It accepts inbound connections and
  makes outbound connect attempts in parallel, retrying connects
  every ~500 ms until the peer's listener has bound (bounded by an
  internal `PROGRESS_STARTUP_BUDGET` of 15 s). Once a pair has
  connected, the connection remains open across every Phase 2 spawn.
  A reader thread per peer folds incoming frames into the shared
  `RemoteProgressView`. `shutdown()` flips an atomic stop flag,
  closes all streams (which the peers observe as EOF), and joins the
  reader threads. The progress channel is best-effort: per-peer
  write errors mark that pair unhealthy (it is removed from the
  writer map) and the runner continues with the peers it still has.
  Losing the channel does **not** abort the run -- T15.4's safety-net
  `max_spawn_secs` still bounds a stuck spawn.
- **Single-runner mode.** No transport is set up; the publisher is a
  quick early return and the `RemoteProgressView` stays empty.

**Defensive isolation from UDP coordination.** The UDP coordination
parser intentionally drops any inbound `progress_update` it observes on
the multicast port (a stray frame from a buggy peer cannot perturb
discovery / ready / done barriers). `progress_update` is **only**
valid on the dedicated TCP channel.

### Ready barrier responds to stale done requests

The done barrier's 2-second linger covers slow peers that arrive at done-N
within ~2 s of the fast peer reaching quorum, but it does **not** cover
the case where the slow peer arrives later (per-machine runtime skew on a
high-rate variant plus UDP receive-buffer pressure on the slow peer's
runner can push that gap past the linger). After the linger expires, the
fast peer enters `ready_barrier` for spawn N+1 and would otherwise drop
inbound `Done` messages on the floor — leaving the slow peer's done-N
loop with no message any peer will ever send that satisfies its
barrier-completion condition (see DECISIONS.md D9).

To prevent this longer-tail hang, the post-done coordination phases
re-emit a cached `Done` on demand:

1. **Most-recent-completed cache.** `Coordinator` maintains a single-entry
   cache of the most recently completed `done_barrier` outcome —
   `(variant, run, status, exit_code)`. The cache is written at the tail
   of `done_barrier` just before returning, only on the success path; the
   timeout-error branch leaves it untouched (we did not complete that
   variant's coordination cleanly, so re-emitting a `Done` for it would
   misrepresent the outcome). The cache is **bounded to one entry by
   design**: a slow peer never asks for a `Done` from any spawn earlier
   than the immediately preceding one — older variants intentionally
   time out via the post-discovery barrier-timeout safety net (T-coord.2).

2. **Re-emit hook in post-done loops.** When `ready_barrier`,
   `done_barrier` (cross-spawn case: inbound `Done` for a different
   variant than the current one), or the Phase 1.25 ResumeManifest
   exchange receives an inbound `Done` from a peer in the expected set,
   it consults the cache. If the cache is `Some((variant, run, …))` and
   the inbound `(variant, run)` matches, the runner broadcasts its own
   cached `Done` for that variant. Otherwise (cache empty, older variant,
   or mismatched run id) the inbound message is dropped without effect on
   the active barrier — the helper is a strict no-op outside the matching
   case.

3. **Best-effort.** Send errors from the re-emit are swallowed; this is a
   recovery-only path running inside the hot loop of another barrier and
   must not abort the active barrier on a transient failure. The active
   barrier's progress (inserts into `seen` / `results`, the `expected`-set
   completion check, the overall deadline) is unaffected.

What this rule does **not** cover:

- **Older variants.** Bob asking for `Done` on spawn N-1 (or earlier)
  while alice's cache holds spawn N gets nothing. The bounded cache is
  intentional — chained mid-run hangs across multiple spawns are out of
  scope; the barrier-timeout safety net catches them.
- **Cache empty.** A runner that has not yet completed any `done_barrier`
  has nothing to re-emit; a stale `Done` request received during the
  Phase 1.5 / Phase 1.25 windows of its first variant is dropped silently.
  In practice this is structurally inert: a peer that has already entered
  `done_barrier` for variant X must itself have seen this runner reach
  the same point in `ready_barrier` for X, which means this runner has
  not yet completed any `done_barrier` to cache.
- **The discovery linger.** In principle the discovery linger could also
  re-emit cached `Done` on inbound `Done`. In practice the cache is
  always `None` at that point (no `done_barrier` has run yet on a fresh
  process, and resume-mode runs reset the cache through a process
  restart), so the wiring is omitted as structurally inert. Documented
  here for symmetry with the discovery-recovery rule above.

This rule mirrors the "discovery responds to late-arriving discoveries"
rule (Phase 1, T-coord.3): the fast peer keeps a small piece of state
about its last completed coordination event and is willing to replay it
in response to a slow peer's stale request, bounded so the cost is O(1).

## Message Format

_To be defined during implementation. The protocol must be simple and
resilient to UDP packet loss (e.g. periodic re-broadcast until acknowledged)._

The resume protocol adds one new message type:

```json
{"type":"resume_manifest","name":"a","run":"two-runner-test","complete_jobs":["zenoh-1x10hz-qos1","custom-udp-1x10hz-qos1"]}
```

The discover message gains two fields documented in Phase 1: `log_subdir`
(string) and `resume` (bool).

The `resume_manifest` message wire-shape is unchanged, but as of T14.24
it is exchanged over a per-peer-pair TCP connection (length-prefixed
JSON frame), not UDP multicast. See "Manifest exchange transport
(T14.24)" under Phase 1.25 for the framing and pairing rules.

T15.3 adds the `progress_update` message:

```json
{"type":"progress_update","runner":"alice","spawn":"dummy-qos2",
 "phase":"operate","sent":1234,"received":5678,
 "eot_sent":false,"eot_received":false,
 "ts":"2026-05-11T00:00:00.000000000Z"}
```

This message is **only** exchanged on the dedicated TCP per-peer-pair
channel introduced in "Per-Spawn Progress Exchange (T15.3, E15)" under
Phase 2; the UDP coordination parser drops it defensively if it ever
appears there.

## Barrier Timeout

Each post-discovery coordination barrier — the ready barrier, the done
barrier, and the Phase 1.25 ResumeManifest exchange — is bounded by a
per-call timeout. The runner CLI exposes this as `--barrier-timeout-secs`
with a default of **120 seconds**.

When a barrier fails to reach quorum within its timeout the runner:

1. Stops broadcasting on the affected barrier and emits a single stderr
   line in the form
   `[runner:<name>] FATAL: barrier '<kind>' for variant '<v>' timed out after <t>s waiting for peer(s): [<missing>] — exiting 75 (EX_TEMPFAIL); wrapper should retry with --resume`.
2. Exits with code **75** (`EX_TEMPFAIL` from `<sysexits.h>`). This code
   is the single signal the auto-resume wrapper scripts gate on; every
   other non-zero exit (panic, config error, variant failure, child
   timeout) propagates as-is and stops the wrapper loop.
3. Performs no in-process self-restart. The runner is the sole source of
   truth for "did this iteration finish"; the wrapper handles the loop.

In-flight variant child cleanup: the runner's spawn-and-monitor loop is
synchronous, so the variant child has always either not been spawned
yet (ready barrier) or already exited (done barrier) by the time a
barrier timeout fires. There is no orphan child to kill on the
timeout-exit path.

**Discovery (Phase 1) is intentionally NOT subject to this timeout.** A
stuck discovery is a config or firewall problem — mismatched runner
names, blocked UDP multicast, hardware NIC offline — none of which the
auto-resume wrapper can fix by re-launching with `--resume`. Discovery
already has its own loss-recovery pattern (re-broadcast every 500 ms);
if a peer never appears, the operator must intervene. Phase 1.5 / per-
variant clock sync is similarly bounded by its own per-sample timeouts
inside `ClockSyncEngine::measure_one` (N=32 samples × 100 ms ≈ 3.4 s
upper bound per peer) and so does not need a separate barrier-style
timeout wrapped around it; per-variant zero-sample remains a soft
warning, never fatal.

The default 120 s is chosen to absorb realistic worst-case variant
cleanup (zenoh ~30 s, some QUIC linger paths up to ~60 s) without
masking real peer death. Operators running on slow LANs or stress
configurations can override via `--barrier-timeout-secs <N>`.

## Network Requirements

- All runners must be on the same local network subnet.
- UDP broadcast must be permitted (no firewall blocking).
- Port(s) used for coordination TBD (should be configurable or use a
  well-known default).

## Known Deviations

_None yet._
