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

## Phase 1.5: Initial Clock Sync

After discovery completes (config hashes match) and before the first ready
barrier, each runner measures pairwise clock offsets against every other
runner using the protocol defined in `clock-sync.md`. Results are written
to `<runner>-clock-sync-<run>.jsonl`.

Single-runner runs skip this phase entirely.

## Phase 2: Per-Variant Execution

For each variant defined in the config (in order):

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

## Network Requirements

- All runners must be on the same local network subnet.
- UDP broadcast must be permitted (no firewall blocking).
- Port(s) used for coordination TBD (should be configurable or use a
  well-known default).

## Known Deviations

_None yet._
