# Distributed Data Replication System — Design Requirements

## 1. Overview

A multi-node distributed system for very low latency, high throughput data
replication over a local network. The system uses a leaderless, self-organized
topology where write conflicts are eliminated by design through single-writer
subtree ownership.

The data layer is built around `arora_types::Value` — a rich enum of 35
variants covering primitives, typed arrays, structures, enumerations, and
nested key-value trees.

Source: https://github.com/semio-ai/arora-types

## 2. Performance Targets

| Metric | Target |
|---|---|
| Tick rate | 100 Hz (10 ms per step) |
| Replication latency | < 10 ms on a local network |
| Write throughput | ~1,000 atomic value sets per step per node |
| Aggregate update rate | ~100,000 value updates/sec (across all nodes) |

## 3. Data Model

### 3.1 Value Type

All replicated data uses `arora_types::Value`. Most values are single
primitives (scalars, small arrays); a small fraction are complex nested
structures.

Representative variants:

- **Scalars**: `Unit`, `Boolean`, `U8`..`U64`, `I8`..`I64`, `F32`, `F64`,
  `String`, `Uuid`
- **Containers**: `Option`, `Structure`, `Enumeration`, `KeyValue`
- **Typed arrays**: `ArrayBoolean`, `ArrayU8`..`ArrayF64`, `ArrayString`,
  `ArrayValue`, `ArrayStructure`, `ArrayEnumeration`

### 3.2 Key-Value Tree

Data is organized as a nested key-value tree:

```
root
├── sensors/            (owned by node A)
│   ├── lidar: Value
│   ├── camera: Value
│   └── imu/            (owned by node B — overrides A within this subtree)
│       ├── accel: Value
│       └── gyro: Value
├── actuators/          (owned by node C)
│   ├── left_wheel: Value
│   └── right_wheel: Value
└── planner/            (owned by node D)
    └── trajectory: Value
```

Each node in the tree is a `KeyValue` identified by a UUID, containing named
fields (`KeyValueField`) that hold optional `Value` payloads.

## 4. Ownership Model

### 4.1 Single-Writer Subtrees

- A node that wants to write data **registers** a new key-value node (root or
  nested) that is not yet registered by another node.
- The registering node becomes the **owner** and sole writer of that subtree.
- All other nodes may **read** any part of the tree.
- This eliminates write-write conflicts entirely — no consensus protocols or
  CRDTs are needed.

### 4.2 Descendant Override

Ownership applies to a subtree, but a different node may register a new
key-value node **within** an existing owner's subtree. The new node then owns
that nested subtree, overriding the parent's write authority for that branch
only.

```
/sensors/          owner: A   (A can write here)
/sensors/imu/      owner: B   (B can write here, overriding A for this branch)
```

The parent owner retains write access to the rest of its subtree.

## 5. Replication Model

### 5.1 Push-Based Convergent Consistency

- Each writer **pushes** updates to all other nodes immediately upon write.
- Because each subtree has exactly one writer, updates are **totally ordered**
  per writer using a monotonically increasing sequence number.
- No vector clocks are needed — a simple `(writer_id, sequence_number)` pair
  uniquely identifies every update.
- The system is **convergent**: stronger than eventual consistency because
  there are no conflicting writes. Every received update is authoritative.

### 5.2 Update Format

Each update message contains:

- `writer_id`: UUID of the writing node
- `sequence`: monotonic sequence number for this writer
- `path`: key path within the tree (e.g. `/sensors/lidar`)
- `value`: the new `Value` payload
- `qos`: the QoS level for this subtree branch

## 6. Quality of Service (QoS)

Four QoS levels are supported. QoS is configured **per subtree branch** by the
branch owner. A descendant owner may override the QoS for its own sub-branch.

### 6.1 Level 1 — Best-Effort (UDP, unordered, fault-tolerant)

- Fire-and-forget UDP datagrams.
- No sequence tracking at the receiver.
- Packets may arrive in any order; missing packets are ignored.
- **Use case**: High-frequency telemetry where only the most recent value
  matters and occasional loss is acceptable.

### 6.2 Level 2 — Latest-Value (UDP, ordered, fault-tolerant)

- Each message carries a per-writer sequence number.
- Receiver tracks the highest-seen sequence per writer and **discards**
  anything with a lower or equal sequence number.
- Missing packets are tolerated — the receiver simply jumps to the latest.
- **Use case**: State that is continuously overwritten (joint positions, sensor
  readings) where a stale value is worse than a skipped one.

### 6.3 Level 3 — Reliable-UDP (UDP, ordered, fault-intolerant)

- Sequence numbers with **gap detection**.
- Receiver buffers out-of-order packets and **NACKs** the sender for missing
  ones.
- The application-visible stream **lags** while gaps are being recovered.
- Avoids TCP's head-of-line blocking: a lost packet for one key path does not
  stall delivery of unrelated key paths.
- **Use case**: Event streams and command sequences where every update must be
  processed in order.

### 6.4 Level 4 — Reliable-TCP (TCP, ordered, fault-intolerant)

- Standard TCP connection per node pair (or multiplexed).
- The kernel handles ordering, retransmission, and flow control.
- Head-of-line blocking applies: one lost segment stalls the entire connection
  until recovered.
- On a local network, packet loss is rare, so this tradeoff is often
  acceptable.
- **Use case**: Configuration state, registration events, and data where
  implementation simplicity is preferred over per-path independence.

### 6.5 QoS Summary

| Level | Transport | Ordering | Loss | Complexity | Latency |
|---|---|---|---|---|---|
| Best-Effort | UDP | None | Tolerant | Minimal | Lowest |
| Latest-Value | UDP | Latest-wins | Tolerant | Low | Low |
| Reliable-UDP | UDP | Strict | Intolerant | High | Variable (lags on loss) |
| Reliable-TCP | TCP | Strict | Intolerant | Low (kernel) | Low (HOL on loss) |

## 7. Topology

- **Leaderless**: No distinguished coordinator or primary node.
- **Self-organized**: Nodes discover each other and establish connections
  autonomously on the local network.
- **Fully connected reads**: Every node can read every part of the tree.
- **Single-writer paths**: Write traffic flows only from owner to readers,
  never between non-owners.

## 8. Benchmark System

The goal is to implement multiple variants of the replication system and
compare them empirically. The benchmark system has two layers: a **runner**
layer that coordinates the benchmark across machines, and the **variant**
processes that are the actual implementations being tested.

### 8.1 Architecture Overview

```
  Machine A                           Machine B
 ┌─────────────────────┐            ┌─────────────────────┐
 │  Runner (name: "a") │◄──coord──►│  Runner (name: "b") │
 │    │                 │            │    │                 │
 │    ├─ spawns ──► variant-1       │    ├─ spawns ──► variant-1
 │    │  (monitor, no IPC)          │    │  (monitor, no IPC)
 │    │                 │            │    │                 │
 │    ├─ spawns ──► variant-2       │    ├─ spawns ──► variant-2
 │    │  ...            │            │    │  ...            │
 └─────────────────────┘            └─────────────────────┘
```

- One **runner** binary per machine, all given the same config file.
- Runners coordinate with each other (discovery, barrier sync).
- Runners have **no IPC** with variant processes — they only spawn, monitor
  for exit (or timeout), and collect the exit code. This avoids any
  measurement interference.
- Variant processes are fully self-contained: they receive their configuration
  via CLI arguments, connect to their peers independently, execute their
  workload, log locally, and exit.

### 8.2 Runner CLI

```
runner --name <runner-name> --config <path-to-config.toml>
```

- `--name`: This runner's identity. Must match one of the names listed in the
  config file.
- `--config`: Path to the single benchmark config file.

### 8.3 Runner Coordination Protocol

Runners are **leaderless** — no runner is special. They progress through the
config in lockstep using symmetric barrier synchronization.

#### Phase 1: Discovery and Handshake

1. Runners discover each other on the local network via UDP broadcast (or a
   third-party discovery library). Each runner announces its name.
2. During handshake, each runner also broadcasts a **hash of the config file
   contents**. If any runner's hash does not match, all runners abort with a
   clear error before anything is launched. This catches mismatched configs
   from incomplete copies.
3. Discovery completes when all runner names listed in the config have been
   seen and their config hashes match.

#### Phase 2: Per-Variant Execution (repeated for each variant in config)

```
  All runners                All runners               All runners
 ┌──────────┐  barrier     ┌──────────┐  barrier     ┌──────────┐
 │  Ready   │────────────►│  Launch  │────────────►│  Done    │──► next
 │  for V_i │  (all ACK)   │  V_i     │  (all ACK)   │  with V_i│     variant
 └──────────┘              └──────────┘              └──────────┘
```

1. **Ready barrier**: Each runner broadcasts "ready for variant V_i". Waits
   until all runners have signaled ready.
2. **Launch**: Each runner spawns the variant binary as a child process,
   passing configuration via CLI arguments (common section + variant-specific
   section from the config).
3. **Monitor**: The runner waits for the child to exit. No IPC — just
   `waitpid` (or equivalent). If the child does not exit within the
   per-variant timeout specified in the config, the runner kills it and
   records a timeout.
4. **Done barrier**: Each runner broadcasts "done with variant V_i" along
   with the exit status (success / failure / timeout). Waits until all
   runners have reported done.
5. Proceed to the next variant, or finish if all variants are complete.

### 8.4 Config File

A single TOML file represents a complete benchmark run. It is the only file
that needs to be copied to each machine.

```toml
# Unique identifier for this benchmark run (e.g. "run01", "run02").
# Included in every log line so repeated runs are distinguishable.
run = "run01"

# Runners expected in this benchmark
runners = ["a", "b", "c"]

# Default timeout for variants (can be overridden per variant)
default_timeout_secs = 120

# Variant definitions — executed in order
[[variant]]
name = "zenoh-replication"
binary = "./variants/zenoh-variant"
timeout_secs = 180                     # override default

  # Common section — passed to all variant instances
  [variant.common]
  tick_rate_hz = 100
  stabilize_secs = 3
  operate_secs = 30
  silent_secs = 5
  workload = "scalar-flood"
  values_per_tick = 1000
  qos = 2
  log_dir = "./logs"

  # Variant-specific options — only this implementation uses these
  [variant.specific]
  zenoh_mode = "peer"
  zenoh_listen = "udp/0.0.0.0:7447"

[[variant]]
name = "custom-udp-replication"
binary = "./variants/custom-variant"

  [variant.common]
  tick_rate_hz = 100
  stabilize_secs = 3
  operate_secs = 30
  silent_secs = 5
  workload = "scalar-flood"
  values_per_tick = 1000
  qos = 2
  log_dir = "./logs"

  [variant.specific]
  buffer_size = 65536
  multicast_group = "239.0.0.1:9000"
```

The runner parses the config, and for each variant, constructs CLI arguments
from both `variant.common` and `variant.specific` to pass to the child
process.

### 8.5 Variant Process Contract

Each variant binary is a standalone executable that:

1. **Receives** all configuration via CLI arguments (derived from the config
   file by the runner).
2. **Discovers or connects** to its peers autonomously — how it does so is
   implementation-specific (zero-conf, explicit addresses in specific config,
   etc.).
3. **Executes** the test protocol phases internally:

```
┌───────────┐   ┌──────────────┐   ┌───────────┐   ┌─────────┐
│ Connect   │──▶│ Stabilize    │──▶│ Operate   │──▶│ Silent  │──▶ exit 0
│           │   │ (e.g. 3-5s)  │   │ (measured) │   │ (drain) │
└───────────┘   └──────────────┘   └───────────┘   └─────────┘
```

   - **Connect** — find peers, establish channels.
   - **Stabilize** — quiet period (duration from config). No writes.
   - **Operate** — run the workload, log all events.
   - **Silent** — drain in-flight data, flush logs.

4. **Logs** all events to a local JSONL file (see §8.8).
5. **Exits 0** on success. Non-zero indicates failure. The runner records
   the exit code.

### 8.6 Workload Profiles

Each operation phase runs a named workload profile. Planned profiles:

| Profile | Description |
|---|---|
| `scalar-flood` | Single writer, 1,000 scalar `Value` updates per tick at 100 Hz. Measures raw throughput and baseline latency. |
| `multi-writer` | N nodes each own a subtree, all writing concurrently. Measures fan-out and cross-traffic interference. |
| `mixed-types` | Mix of scalar, array, and nested `KeyValue` updates. Measures serialization cost variance. |
| `burst-recovery` | Sustained writes followed by a deliberate pause, then a burst. Measures buffering and recovery behavior under load spikes. |
| `qos-ladder` | Same data written under each QoS level sequentially. Directly compares QoS latency/loss characteristics. |

### 8.7 Metrics

Each node measures locally (no coordination required during the operation
phase):

| Metric | Measured at | Description |
|---|---|---|
| **Connection time** | Per node | Time from process launch to all peers connected (end of Connect phase). Logged as a `connected` event with elapsed duration. |
| **Write timestamp** | Writer | Wall-clock time when the write was committed locally |
| **Receive timestamp** | Reader | Wall-clock time when the replicated value was delivered to the application |
| **Replication latency** | Analysis | `receive_timestamp − write_timestamp` (requires synchronized clocks — see §8.9) |
| **Throughput** | Per node | Values written/sec and values received/sec |
| **Packet loss** | Reader | Gaps in sequence numbers (for QoS levels that track sequences) |
| **Recovery time** | Reader | Time from gap detection to gap fill (QoS levels 3 and 4) |
| **Jitter** | Analysis | Standard deviation of replication latency over a window |
| **CPU / memory** | Per node | Sampled periodically (e.g. every 100 ms) during operation phases |

### 8.8 Log Format

Every variant process produces a single structured log file (JSON Lines).
Every line includes `variant`, `runner`, and `run` fields so that if all
log files from all nodes, variants, and runs were concatenated into a single
file, the full dataset could be recovered by grouping on any combination of
these three keys.

```jsonl
{"ts":"2026-04-12T14:00:00.500000000Z","variant":"zenoh-replication","runner":"a","run":"run01","event":"connected","elapsed_ms":487.3}
{"ts":"2026-04-12T14:00:01.123456789Z","variant":"zenoh-replication","runner":"a","run":"run01","event":"write","seq":42,"path":"/sensors/lidar","qos":2,"bytes":128}
{"ts":"2026-04-12T14:00:01.124001234Z","variant":"zenoh-replication","runner":"b","run":"run01","event":"receive","writer":"a","seq":42,"path":"/sensors/lidar","qos":2,"bytes":128}
{"ts":"2026-04-12T14:00:01.200000000Z","variant":"zenoh-replication","runner":"b","run":"run01","event":"gap_detected","writer":"a","missing_seq":41}
{"ts":"2026-04-12T14:00:01.300000000Z","variant":"zenoh-replication","runner":"b","run":"run01","event":"gap_filled","writer":"a","recovered_seq":41}
{"ts":"2026-04-12T14:00:01.000000000Z","variant":"zenoh-replication","runner":"a","run":"run01","event":"phase","phase":"operate","profile":"scalar-flood"}
{"ts":"2026-04-12T14:00:01.100000000Z","variant":"zenoh-replication","runner":"a","run":"run01","event":"resource","cpu_percent":12.5,"memory_mb":48.3}
```

Log files are named `<variant>-<runner>-<run>.jsonl` for convenience, but the
file name is not authoritative — the fields inside each line are.

### 8.9 Clock Synchronization

Cross-node latency measurement depends on synchronized clocks. Options in
order of preference:

1. **PTP (Precision Time Protocol)** — sub-microsecond accuracy on a local
   network. Ideal but requires support on both machines.
2. **NTP with local server** — low single-digit millisecond accuracy.
   Acceptable given our 10 ms latency target, but introduces measurement
   noise.
3. **Embedded round-trip measurement** — writer sends a probe, reader echoes
   it, half-RTT approximates one-way latency. Implementation-independent
   fallback.

The analysis tool should report which synchronization method was used and flag
results where clock uncertainty exceeds a configurable threshold (e.g. > 1 ms).

### 8.10 Analysis Pipeline

After a test session, log files from all nodes (potentially across multiple
runs) are gathered into a single directory:

```
results/
├── zenoh-replication-a-run01.jsonl
├── zenoh-replication-b-run01.jsonl
├── zenoh-replication-a-run02.jsonl
├── zenoh-replication-b-run02.jsonl
├── custom-udp-replication-a-run01.jsonl
├── custom-udp-replication-b-run01.jsonl
└── ...
```

An analysis tool reads all files in the directory and produces:

- **Per-run summary**: latency percentiles (p50, p95, p99), throughput,
  loss rate, jitter, resource usage.
- **Cross-run comparison**: same implementation across runs to assess
  consistency.
- **Cross-implementation comparison**: different implementations on the same
  workload profile, side by side.
- **Output formats**: terminal summary table, CSV for further processing,
  and optionally plots (latency histograms, time-series).

## 9. Constraints and Non-Goals

### In scope

- Local network operation (single subnet, low base latency)
- Rust implementation using `arora_types::Value`
- Multiple concurrent nodes each owning distinct subtrees
- Mixed QoS within a single tree

### Out of scope (for now)

- WAN / cross-datacenter replication
- Byzantine fault tolerance
- Durable persistence / on-disk storage
- Multi-writer conflict resolution (eliminated by design)
