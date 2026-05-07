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

3. Each runner broadcasts a `ResumeManifest` message containing:
   - `name`: this runner's identity
   - `run`: the bench config's run id (echoed for safety)
   - `complete_jobs`: sorted, deduplicated array of `effective_name`
     strings for which the local log file exists and is non-empty

4. Each runner waits until it has received a manifest from every peer
   listed in `runners` (with periodic re-broadcast every 500 ms, identical
   to discovery's loss-recovery pattern).

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

## Message Format

_To be defined during implementation. The protocol must be simple and
resilient to UDP packet loss (e.g. periodic re-broadcast until acknowledged)._

The resume protocol adds one new message type:

```json
{"type":"resume_manifest","name":"a","run":"two-runner-test","complete_jobs":["zenoh-1x10hz-qos1","custom-udp-1x10hz-qos1"]}
```

The discover message gains two fields documented in Phase 1: `log_subdir`
(string) and `resume` (bool).

## Network Requirements

- All runners must be on the same local network subnet.
- UDP broadcast must be permitted (no firewall blocking).
- Port(s) used for coordination TBD (should be configurable or use a
  well-known default).

## Known Deviations

_None yet._
