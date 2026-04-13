# System Architecture

## Overview

The system has four components that run across two or more machines on a local
network. The runner coordinates benchmark execution; variant processes are the
systems under test; log files are the interchange format; the analysis tool
consumes logs offline.

```
  Machine A                              Machine B
 +---------------------------+          +---------------------------+
 |  runner (name: "a")       |<--UDP--->|  runner (name: "b")       |
 |    |                      |  coord   |    |                      |
 |    +-- spawns --> variant  |          |    +-- spawns --> variant  |
 |         (no IPC, CLI only)|          |         (no IPC, CLI only)|
 |         writes .jsonl     |          |         writes .jsonl     |
 +---------------------------+          +---------------------------+

              Log files collected post-run
                        |
                        v
              +-------------------+
              | analysis (Python) |
              | reads all .jsonl  |
              +-------------------+
```

## Service Map

### runner (Rust binary)

- **Repo**: `runner/`
- **Responsibility**: Coordinates benchmark execution across machines.
  Discovers peers via UDP broadcast, verifies config hashes match, progresses
  through variants in lockstep using barrier sync, spawns variant child
  processes, monitors them for exit/timeout, reports status.
- **Interfaces**:
  - Runner-to-runner: UDP broadcast (discovery + barrier protocol).
  - Runner-to-variant: child process spawn with CLI arguments. No IPC.
- **Input**: `--name <name> --config <path.toml>`
- **Output**: exit code per variant (success/failure/timeout).

### variant (Rust binary, one per implementation)

- **Repos**: `variants/zenoh/`, `variants/custom-udp/`, etc.
- **Responsibility**: Implements the distributed data replication system
  described in DESIGN.md. Connects to peers, runs the test protocol
  (connect -> stabilize -> operate -> silent), logs all events, exits.
- **Interfaces**:
  - CLI arguments from the runner (common + specific config).
  - Peer-to-peer: implementation-specific networking (Zenoh, raw UDP, etc.).
  - Output: JSONL log file.
- **Input**: CLI args derived from TOML config (common + specific sections)
  plus `--launch-ts <RFC3339>`.
- **Output**: `<variant>-<runner>-<run>.jsonl`, exit code 0 on success.

### analysis (Python script)

- **Repo**: `analysis/`
- **Responsibility**: Offline analysis of benchmark results. Parses JSONL
  logs, caches in pickle, verifies integrity (delivery completeness, ordering,
  duplicates, gap recovery), computes performance metrics (latency percentiles,
  throughput, jitter, loss, resource usage), produces CLI tables and PNG
  diagrams.
- **Interfaces**:
  - Input: directory of `.jsonl` files.
  - Output: CLI summary tables, CSV, PNG diagrams.
- **Input**: `python analyze.py <logs-dir> [--clear] [--summary] [--diagrams] [--output <dir>]`

### metak-shared/ (documentation, not a service)

- Shared design documents, API contracts, glossary, coding standards.
- Read-only for worker agents; maintained by the orchestrator.

## Data Flow

```
1. Config file (TOML) --> copied to all machines
2. runner reads config, discovers peers, barrier syncs
3. For each variant in config:
   a. runner spawns variant binary with CLI args from config
   b. variant connects to peers (implementation-specific discovery)
   c. variant runs: connect -> stabilize -> operate -> silent
   d. variant writes events to <variant>-<runner>-<run>.jsonl
   e. variant exits (0 = success)
   f. runner records exit status, barrier syncs "done"
4. Log files collected from all machines into one directory
5. analyze.py ingests all .jsonl files, produces reports
```

## Tech Stack

| Component | Language | Key dependencies |
|-----------|----------|------------------|
| runner | Rust | `arora_types`, UDP sockets, TOML parsing |
| variants | Rust | `arora_types`, variant-specific libs (e.g. Zenoh) |
| analysis | Python | Standard lib, matplotlib (diagrams), pickle (cache) |

## Key Design Decisions

### No IPC between runner and variant

The runner spawns variants as child processes and passes all configuration via
CLI arguments. There is no shared memory, pipe, or socket between them. This
ensures the runner cannot interfere with variant measurements.

### Single-writer ownership eliminates conflicts

Each subtree in the key-value tree has exactly one writer. No consensus
protocols or CRDTs are needed. Updates are totally ordered per writer using
a simple `(writer_id, sequence_number)` pair.

### Four QoS tiers

QoS is per-subtree-branch, configured by the branch owner. Levels range from
fire-and-forget UDP (level 1) to reliable TCP (level 4). This allows mixed
reliability within a single tree.

### Leaderless topology

Both the runner coordination protocol and the replication system itself are
leaderless. No node is special. This simplifies deployment and avoids
single-point-of-failure concerns.

## ADRs

_None yet. Decisions will be logged in `metak-orchestrator/DECISIONS.md` as
implementation proceeds._
