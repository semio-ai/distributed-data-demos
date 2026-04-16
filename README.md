# Distributed Data Replication Demos

A benchmark suite for comparing low-latency, high-throughput distributed data
replication strategies on local networks. The system replicates a key-value
tree across nodes using different transport stacks (Zenoh, custom UDP, QUIC,
Hybrid) and measures latency, throughput, jitter, and loss under identical
conditions.

## Quick Start

```bash
# 1. Build the runner and a variant
cd runner && cargo build --release && cd ..
cd variants/custom-udp && cargo build --release && cd ../..

# 2. Run (two terminals on the same machine)
runner/target/release/runner --name alice --config configs/two-runner-test.toml
runner/target/release/runner --name bob   --config configs/two-runner-test.toml

# 3. Analyse (auto-selects the latest run)
cd analysis && python analyze.py ../logs --summary
```

Both runners must be started — they discover each other before proceeding.
See the [Usage Guide](usage-guide.md) for full configuration, multi-machine
setup, and troubleshooting.

## Project Layout

```
configs/           Benchmark config files (TOML)
logs/              Output: JSONL logs and analysis cache (gitignored)
runner/            Benchmark runner binary (Rust)
variant-base/      Shared Variant trait + VariantDummy (Rust)
variants/          Concrete variant implementations (Rust)
analysis/          Analysis tool (Python)
```

## Variants

| Variant | Transport | Status |
|---------|-----------|--------|
| Dummy | In-process (no network) | Complete |
| Zenoh | Zenoh pub/sub | Complete |
| Custom UDP | Raw UDP with sequence numbers | Complete |
| QUIC | QUIC streams | Complete |
| Hybrid | UDP + TCP fallback | Complete |
| Aeron | Aeron media driver | Blocked (Windows C FFI) |

## Documentation

- [Usage Guide](usage-guide.md) -- building, configuring, running, and troubleshooting
- [System Design](metak-shared/DESIGN.md) -- replication model, QoS tiers, data structures
- [Benchmark Design](metak-shared/BENCHMARK.md) -- runner architecture, coordination protocol
- [Analysis Design](metak-shared/ANALYSIS.md) -- metrics, integrity checks, output formats
- [Architecture](metak-shared/architecture.md) -- system boundaries and data flow
- [Variant Candidates](metak-shared/variant-candidates.md) -- transport evaluation and selection
- [JSONL Log Schema](metak-shared/api-contracts/jsonl-log-schema.md) -- log format reference
- [TOML Config Schema](metak-shared/api-contracts/toml-config-schema.md) -- config format reference
