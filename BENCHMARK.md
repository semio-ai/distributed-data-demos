# Distributed Data Replication System — Benchmark Design

The goal is to implement multiple variants of the replication system described
in [DESIGN.md](DESIGN.md) and compare them empirically. The benchmark system
has two layers: a **runner** layer that coordinates the benchmark across
machines, and the **variant** processes that are the actual implementations
being tested.

## 1. Architecture Overview

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

## 2. Runner CLI

```
runner --name <runner-name> --config <path-to-config.toml>
```

- `--name`: This runner's identity. Must match one of the names listed in the
  config file.
- `--config`: Path to the single benchmark config file.

## 3. Runner Coordination Protocol

Runners are **leaderless** — no runner is special. They progress through the
config in lockstep using symmetric barrier synchronization.

### Phase 1: Discovery and Handshake

1. Runners discover each other on the local network via UDP broadcast (or a
   third-party discovery library). Each runner announces its name.
2. During handshake, each runner also broadcasts a **hash of the config file
   contents**. If any runner's hash does not match, all runners abort with a
   clear error before anything is launched. This catches mismatched configs
   from incomplete copies.
3. Discovery completes when all runner names listed in the config have been
   seen and their config hashes match.

### Phase 2: Per-Variant Execution (repeated for each variant in config)

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

## 4. Config File

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

## 5. Variant Process Contract

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

4. **Logs** all events to a local JSONL file (see section 8).
5. **Exits 0** on success. Non-zero indicates failure. The runner records
   the exit code.

## 6. Workload Profiles

Each operation phase runs a named workload profile. Planned profiles:

| Profile | Description |
|---|---|
| `scalar-flood` | Single writer, 1,000 scalar `Value` updates per tick at 100 Hz. Measures raw throughput and baseline latency. |
| `multi-writer` | N nodes each own a subtree, all writing concurrently. Measures fan-out and cross-traffic interference. |
| `mixed-types` | Mix of scalar, array, and nested `KeyValue` updates. Measures serialization cost variance. |
| `burst-recovery` | Sustained writes followed by a deliberate pause, then a burst. Measures buffering and recovery behavior under load spikes. |
| `qos-ladder` | Same data written under each QoS level sequentially. Directly compares QoS latency/loss characteristics. |

## 7. Metrics

Each node measures locally (no coordination required during the operation
phase):

| Metric | Measured at | Description |
|---|---|---|
| **Connection time** | Per node | Time from process launch to all peers connected (end of Connect phase). Logged as a `connected` event with elapsed duration. |
| **Write timestamp** | Writer | Wall-clock time when the write was committed locally |
| **Receive timestamp** | Reader | Wall-clock time when the replicated value was delivered to the application |
| **Replication latency** | Analysis | `receive_timestamp − write_timestamp` (requires synchronized clocks — see section 9) |
| **Throughput** | Per node | Values written/sec and values received/sec |
| **Packet loss** | Reader | Gaps in sequence numbers (for QoS levels that track sequences) |
| **Recovery time** | Reader | Time from gap detection to gap fill (QoS levels 3 and 4) |
| **Jitter** | Analysis | Standard deviation of replication latency over a window |
| **CPU / memory** | Per node | Sampled periodically (e.g. every 100 ms) during operation phases |

## 8. Log Format

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

## 9. Clock Synchronization

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

## 10. Analysis Pipeline

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
