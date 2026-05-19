# Glossary

| Term | Definition |
|------|-----------|
| arora_types::Value | Rich enum of 35 variants (scalars, typed arrays, structures, enumerations, nested key-value trees) used as the universal data type for all replicated values. Source: `semio-ai/arora-types`. |
| Key-Value Tree | The nested data structure that holds all replicated state. Each node is a `KeyValue` identified by a UUID, containing named `KeyValueField` entries with optional `Value` payloads. |
| Single-writer subtree | Ownership model where a node that registers a key-value node becomes its sole writer. Eliminates write-write conflicts by design. |
| Descendant override | A node may register a new key-value node within an existing owner's subtree, taking write authority for that nested branch only. |
| Writer ID | UUID identifying the node that owns and writes to a subtree. |
| Sequence number | Monotonically increasing counter per writer, used to totally order updates from that writer. |
| QoS (Quality of Service) | Per-subtree-branch setting controlling transport reliability. Four levels: Best-Effort (1), Latest-Value (2), Reliable-UDP (3), Reliable-TCP (4). |
| Best-Effort (QoS 1) | Fire-and-forget UDP. No ordering, loss-tolerant. |
| Latest-Value (QoS 2) | UDP with sequence tracking. Receiver discards stale values. Loss-tolerant. |
| Reliable-UDP (QoS 3) | UDP with gap detection and NACK-based recovery. Strict ordering, loss-intolerant. |
| Reliable-TCP (QoS 4) | Standard TCP. Strict ordering, loss-intolerant. Subject to head-of-line blocking. |
| Variant trait | Rust trait defined in `variant-base` that each concrete variant must implement. Covers connect, publish, poll_receive, and disconnect. All other behavior (phases, logging, workload) is handled by the base crate. |
| VariantDummy | A no-network `Variant` implementation included in the base crate. Uses an in-process data board instead of real networking. Used for base crate testing, runner harness testing on a single machine, and as a zero-network performance baseline. Ships as the `variant-dummy` binary. |
| variant-base | Rust library crate providing the shared foundation for all variants: `Variant` trait, CLI parsing, test protocol driver, JSONL logger, resource monitor, workload profiles, sequence generator. |
| Runner | Rust binary that coordinates benchmark execution across machines. Discovers peers, barrier-syncs, spawns variant processes, monitors exit codes. |
| Variant | A standalone Rust binary implementing the replication design using a specific stack (e.g. Zenoh, custom UDP). The system under test. |
| Barrier sync | Symmetric synchronization between runners. All runners must reach the same phase before any proceeds. Used for ready/done gates around each variant. |
| Config hash | SHA hash of the TOML config file contents. Exchanged during runner discovery to ensure all machines use identical configs. |
| Test protocol | The four phases a variant executes: Connect, Stabilize, Operate, Silent. |
| Connect phase | Variant finds peers and establishes channels. |
| Stabilize phase | Quiet period (configurable duration) after connection. No writes. Lets the system settle. |
| Operate phase | Active workload execution. All measurement events are logged during this phase. |
| Silent phase | Drain in-flight data and flush logs before exiting. |
| Workload profile | Named scenario defining what the operate phase does (e.g. `scalar-flood`, `multi-writer`, `mixed-types`, `burst-recovery`, `qos-ladder`). |
| JSONL | JSON Lines format. Each variant produces one `.jsonl` log file with one JSON object per line. Every line includes `variant`, `runner`, and `run` fields for self-describing provenance. |
| Delivery record | Analysis concept: a correlated (write, receive) pair across runners, keyed by `(variant, run, seq, path)`. Used to compute replication latency. |
| launch_ts | RFC 3339 timestamp passed by the runner to the variant via `--launch-ts` at spawn time. Used to compute connection time without IPC. |
| Pickle cache | The analysis tool caches parsed JSONL data in a `.analysis_cache.pkl` file to avoid re-parsing on repeated analysis runs. |
| PTP | Precision Time Protocol. Sub-microsecond clock sync on a LAN. Preferred method for cross-node latency measurement. |
| WriteOp | One logical unit produced by a workload profile per `Variant::try_publish` call: a `(path, payload, leaf_count, shape)` tuple. The transport ships the payload as one logical message even if the underlying socket coalesces multiple WriteOps. |
| Leaf | One scalar value (typically 8 bytes for an `f64`) within a WriteOp's payload. A scalar WriteOp has one leaf; an array WriteOp has N leaves; a nested struct WriteOp has the sum of its branch leaves. |
| Blob | Synonym for a multi-leaf WriteOp payload — used to emphasise that the wire encoding is opaque to variants and that internal leaf framing is invisible at the transport layer. |
| leaf_count | Field on the JSONL `write` event and the compact-Parquet write row recording the number of leaves carried by that WriteOp. Defaults to `1` for backward compatibility. The analysis tool's `leaves_per_sec` headline number is the sum of this field divided by operate seconds. |
| shape | Field on the JSONL `write` event and the compact-Parquet write row identifying the WriteOp shape: `scalar`, `array`, or `struct`. Used by the analysis tool to slice throughput / latency by workload shape. |
| block-flood | E19 workload profile that emits `values_per_tick / blob_size` WriteOps per tick, each carrying a `blob_size`-element block of scalars. Stresses serialization cost and large-message transport handling. |
| mixed-types | E19 workload profile that emits a heterogeneous mix of scalar / array / nested-struct WriteOps per tick, summing to exactly `values_per_tick` total leaves. Bounded by `mixed_scalars_min/max`, `mixed_arrays_min/max`, `mixed_dict_split_max`. Stresses the full serialization path including nested `KeyValue` structures. |
