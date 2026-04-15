# Analysis Tool — Custom Instructions

## Overview

Python script that ingests JSONL log files from benchmark runs, verifies
data integrity, computes performance metrics, and produces CLI summary
tables. This is E4 (Phase 1 — Foundation). Diagrams come later in E5-E6.

The full spec is in `metak-shared/ANALYSIS.md`. This epic covers sections
1-6 of that document (everything except diagrams).

## Tech Stack

- **Language**: Python 3.10+
- **Type hints**: required throughout
- **Formatting**: `ruff format`
- **Linting**: `ruff check`
- **Testing**: `pytest`
- **Dependencies**: standard library only for Phase 1. No pandas, no
  matplotlib (those come in E5). Use `dataclasses`, `json`, `pickle`,
  `pathlib`, `statistics`, `argparse`.
- Follow `metak-shared/coding-standards.md` (Python section).

## Build and Test

```
cd analysis
python -m pytest tests/ -v
ruff format --check .
ruff check .
```

No build step — it's a Python script.

## Integration Contracts

Consumes JSONL log files per `metak-shared/api-contracts/jsonl-log-schema.md`.

Key fields on every line: `ts`, `variant`, `runner`, `run`, `event`.
Event types: `connected`, `phase`, `write`, `receive`, `gap_detected`,
`gap_filled`, `resource`.

## Test Data

Real JSONL logs are available at `../two-runner-logs/`:
- `custom-udp-alice-local-test-01.jsonl` — 540 lines, runner "alice"
- `custom-udp-bob-local-test-01.jsonl` — 540 lines, runner "bob"
- `dummy-alice-local-test-01.jsonl` — 540 lines, single runner

Use these for integration tests. Also create small synthetic JSONL
fixtures for unit tests.

## Architecture

```
analysis/
  analyze.py          -- CLI entry point
  cache.py            -- pickle caching pipeline
  parse.py            -- JSONL parsing, data model (dataclasses)
  correlate.py        -- write-receive correlation, delivery records
  integrity.py        -- integrity verification (completeness, ordering, dupes, gaps)
  performance.py      -- performance analysis (latency, throughput, jitter, loss, resources)
  tables.py           -- CLI summary table formatting
  tests/
    test_parse.py
    test_correlate.py
    test_integrity.py
    test_performance.py
    test_cache.py
    test_integration.py   -- end-to-end with real logs
    fixtures/             -- small synthetic JSONL files for unit tests
```

## Design Guidance

### Data Model

After parsing, the data is held as lists of dataclass instances:

```python
@dataclass
class Event:
    ts: datetime
    variant: str
    runner: str
    run: str
    event: str
    # event-specific fields stored as a dict
    data: dict

@dataclass
class DeliveryRecord:
    variant: str
    run: str
    path: str
    seq: int
    qos: int
    writer: str
    receiver: str
    write_ts: datetime
    receive_ts: datetime
    latency_ms: float
```

### Caching Pipeline

1. Check for `<logs-dir>/.analysis_cache.pkl`
2. Scan `*.jsonl` files, compare mtime against cache
3. Parse new/changed files, merge into cache
4. Write updated cache
5. `--clear` deletes the pickle and rebuilds from scratch

### Correlation

Join `write` events with `receive` events on `(variant, run, seq, path)`
where `receive.writer == write.runner`. Produces one `DeliveryRecord` per
(write, receiver) pair.

For VariantDummy (single-runner loopback), the writer and receiver are the
same runner — this is expected and should still produce valid delivery
records with near-zero latency.

### Integrity Verification

Per (variant, run, writer -> receiver) pair:
- **Completeness**: every write has a receive (QoS 3-4 only; 1-2 are loss-tolerant)
- **Ordering**: receives have non-decreasing seq (QoS 2-4)
- **Duplicates**: same (writer, seq, path) received twice (flag for QoS 3-4)
- **Gap recovery**: every gap_detected has gap_filled (QoS 3 only)

### Performance Analysis

All derived from delivery records and event timestamps:
- **Connection time**: from `connected` events (`elapsed_ms`)
- **Latency**: p50, p95, p99, max from `latency_ms` on delivery records
- **Throughput**: writes/sec and receives/sec from event counts and operate duration
- **Jitter**: std-dev of latency within sliding 1-second windows
- **Packet loss**: missing receives / total writes (QoS 2-4)
- **Resource usage**: mean and peak CPU/memory from `resource` events

### CLI Output

Two tables printed to stdout:

```
Integrity Report
---
Variant              Run          QoS  Delivery%  Out-of-order  Dupes  Gaps
custom-udp           local-test   1    99.8%      0             0      -
dummy                local-test   1    100.0%     0             0      -

Performance Report
---
Variant              Run          Connect(ms)  Latency p50  p95     p99     Writes/s  Loss%
custom-udp           local-test   12.3         0.5ms        1.2ms   2.3ms   50        0.2%
dummy                local-test   0.1          0.01ms       0.02ms  0.03ms  50        0.0%
```
