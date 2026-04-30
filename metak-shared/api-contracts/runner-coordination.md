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

4. Discovery completes when all runner names listed in the config's `runners`
   array have been seen and their config hashes match.

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
