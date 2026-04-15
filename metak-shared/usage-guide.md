# Usage Guide

How to build, configure, and run the benchmark system.

## Prerequisites

- Rust toolchain (1.75+): `rustup`, `cargo`
- Python 3.10+ (for the analysis tool, when available)
- Two or more machines on the same LAN subnet (for multi-machine runs)
- UDP broadcast must not be blocked by firewall (for multi-runner coordination)

## Building

From the repository root:

```bash
# Build the variant-dummy (zero-network test variant)
cd variant-base
cargo build --release

# Build the benchmark runner
cd ../runner
cargo build --release
```

Binaries are produced at:
- `variant-base/target/release/variant-dummy` (`.exe` on Windows)
- `runner/target/release/runner` (`.exe` on Windows)

## Configuration

The benchmark is driven by a single TOML config file. This is the only file
that needs to be copied to each machine.

### Minimal example (single machine)

```toml
run = "my-first-run"
runners = ["local"]
default_timeout_secs = 60

[[variant]]
name = "dummy"
binary = "../variant-base/target/release/variant-dummy.exe"  # adjust path

  [variant.common]
  tick_rate_hz = 100
  stabilize_secs = 2
  operate_secs = 10
  silent_secs = 1
  workload = "scalar-flood"
  values_per_tick = 1000
  qos = 2
  log_dir = "./logs"

  [variant.specific]
```

### Multi-machine example

```toml
run = "lan-bench-01"
runners = ["machine-a", "machine-b"]
default_timeout_secs = 120

[[variant]]
name = "dummy"
binary = "./variant-dummy"
timeout_secs = 60

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
```

Copy the same config file and the same binaries to both machines. Use
identical paths or adjust `binary` to match each machine's layout.

### Multiple variants in one run

```toml
run = "comparison-01"
runners = ["local"]
default_timeout_secs = 120

[[variant]]
name = "dummy-qos1"
binary = "./variant-dummy.exe"

  [variant.common]
  tick_rate_hz = 100
  stabilize_secs = 2
  operate_secs = 10
  silent_secs = 1
  workload = "scalar-flood"
  values_per_tick = 1000
  qos = 1
  log_dir = "./logs"

  [variant.specific]

[[variant]]
name = "dummy-qos2"
binary = "./variant-dummy.exe"

  [variant.common]
  tick_rate_hz = 100
  stabilize_secs = 2
  operate_secs = 10
  silent_secs = 1
  workload = "scalar-flood"
  values_per_tick = 1000
  qos = 2
  log_dir = "./logs"

  [variant.specific]
```

Variants are executed sequentially in the order they appear in the config.

### Config reference

| Field | Required | Description |
|-------|----------|-------------|
| `run` | yes | Unique run ID. Appears in every log line. |
| `runners` | yes | List of runner names. Discovery waits for all. |
| `default_timeout_secs` | yes | Default child process timeout (seconds). |

Per `[[variant]]`:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Unique variant name. Used in log filenames. |
| `binary` | yes | Path to variant executable (relative to runner CWD). |
| `timeout_secs` | no | Override timeout for this variant. |

`[variant.common]` — passed to all variant instances as CLI args:

| Field | Description |
|-------|-------------|
| `tick_rate_hz` | Target tick rate in Hz (e.g. 100) |
| `stabilize_secs` | Quiet period after connection (seconds) |
| `operate_secs` | Active measurement phase duration (seconds) |
| `silent_secs` | Drain period before exit (seconds) |
| `workload` | Workload profile name (currently: `scalar-flood`) |
| `values_per_tick` | Number of value writes per tick |
| `qos` | QoS level 1-4 |
| `log_dir` | Directory for JSONL output files |

`[variant.specific]` — variant-specific options passed as extra CLI args.
Currently unused by `variant-dummy`.

## Project Layout

```
configs/           -- benchmark config files (checked into git)
logs/              -- benchmark output: JSONL logs, analysis cache (gitignored)
runner/            -- runner binary (Rust)
variant-base/      -- shared Variant trait + VariantDummy (Rust)
variants/          -- concrete variant implementations (Rust)
analysis/          -- analysis tool (Python)
```

Configs are inputs you version-control. Logs are artifacts you regenerate.

## Running

All commands are run from the **repo root**.

### Single machine

```bash
runner/target/release/runner --name local --config configs/my-config.toml
```

Output:
```
[runner:local] config loaded: run=my-first-run, 1 variant(s), 1 runner(s), hash=9685b7e25f3f
[runner:local] starting discovery...
[runner:local] discovery complete
[runner:local] ready barrier for variant 'dummy'
[runner:local] spawning variant 'dummy' (timeout: 60s)
[runner:local] variant 'dummy' finished: status=success, exit_code=0
Benchmark run: my-first-run
Variant                  Runner   Status    Exit
dummy                    local    success   0
```

JSONL log files appear in the configured `log_dir`.

### Multiple machines

On each machine, run the runner with the same config file but a different
`--name`:

```bash
# Machine A
runner/target/release/runner --name machine-a --config configs/bench.toml

# Machine B
runner/target/release/runner --name machine-b --config configs/bench.toml
```

Runners discover each other via UDP multicast on port 19876 (configurable
with `--port`). They verify that all machines have identical config files
(SHA-256 hash check). Once all runners are discovered, they proceed through
variants in lockstep.

Each machine produces its own JSONL log files. Collect all log files into
a single directory for analysis.

### Running variant-dummy directly (without the runner)

For quick testing, you can run the variant binary directly:

```bash
cd variant-base
./target/release/variant-dummy \
  --tick-rate-hz 100 \
  --stabilize-secs 2 \
  --operate-secs 5 \
  --silent-secs 1 \
  --workload scalar-flood \
  --values-per-tick 1000 \
  --qos 2 \
  --log-dir ./logs \
  --launch-ts "$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)" \
  --variant dummy \
  --runner local \
  --run test01
```

## Output

### JSONL log files

Each variant process produces one structured log file:
`<variant>-<runner>-<run>.jsonl`

Example filename: `dummy-local-my-first-run.jsonl`

Each line is a self-describing JSON object with fields: `ts`, `variant`,
`runner`, `run`, `event`, plus event-specific fields. Event types:

| Event | Description |
|-------|-------------|
| `phase` | Start of a protocol phase (connect, stabilize, operate, silent) |
| `connected` | All peers connected, includes `elapsed_ms` from launch |
| `write` | A value was written (includes `seq`, `path`, `qos`, `bytes`) |
| `receive` | A replicated value was received |
| `resource` | CPU and memory sample |
| `gap_detected` | Sequence gap found (QoS 3 only) |
| `gap_filled` | Gap recovered (QoS 3 only) |

### Runner summary table

The runner prints a summary table to stdout after all variants complete:

```
Benchmark run: my-first-run
Variant                  Runner   Status    Exit
dummy                    local    success   0
```

Exit code: 0 if all variants succeeded, 1 if any failed or timed out.

### Analysing results

```bash
cd analysis
python analyze.py ../logs --summary
```

Add `--clear` to force a full re-parse if you regenerate logs. The pickle
cache (`logs/.analysis_cache.pkl`) makes repeated runs instant.

## Tuning parameters

| Parameter | Effect | Typical range |
|-----------|--------|---------------|
| `tick_rate_hz` | How often the writer publishes | 10-1000 Hz |
| `values_per_tick` | Writes per tick | 1-10000 |
| `operate_secs` | Measurement duration | 5-300 seconds |
| `qos` | Reliability level | 1 (fire-and-forget) to 4 (reliable TCP) |
| `stabilize_secs` | Warm-up before measurement | 2-10 seconds |
| `silent_secs` | Drain time after measurement | 1-10 seconds |

Total write rate = `tick_rate_hz * values_per_tick`. For example, 100 Hz
with 1000 values/tick = 100,000 writes/sec.

## Troubleshooting

**"variant binary not found"**: The `binary` path in the config is relative
to the runner's working directory, not the config file location. Check your
CWD when running the runner.

**"runner name 'X' is not in the config runners list"**: The `--name` you
passed doesn't match any entry in the `runners` array in the config.

**"config hash mismatch"**: The config files on different machines are not
identical. Copy the exact same file to all machines (byte-for-byte).

**Runner hangs at discovery**: The other runner(s) haven't started yet, or
UDP multicast is blocked by a firewall. Check that all runners are on the
same subnet and UDP port 19876+ is open. Each runner uses port
`base_port + index` (e.g. alice=19876, bob=19877).

**Windows Firewall**: On first run, Windows will prompt to allow
`runner.exe` and variant binaries through the firewall. You must allow
them for both same-machine and cross-machine operation.

**Variant times out**: The variant didn't exit within `timeout_secs`. The
runner kills it and reports "timeout". Increase the timeout or reduce the
workload.
