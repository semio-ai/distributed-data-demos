# API Contract: Application-Level Clock Synchronization

Defines how runner instances measure pairwise clock offsets so that
cross-machine `receive_ts - write_ts` values logged by variants can be
corrected during analysis.

This is the application-level fallback from BENCHMARK.md S9 (PTP and OS-NTP
are out of scope — we depend only on what our own binaries can measure).

## Goal

Without correction, cross-machine latency = (B.receive_ts on B's clock) −
(A.write_ts on A's clock) is contaminated by the offset between A's and B's
wall clocks. On Windows with default w32time, that offset can be hundreds of
ms to seconds — orders of magnitude larger than the 10 ms latency target.

After correction, cross-machine latency error should be dominated by network
jitter and the residual offset estimation error (target: < 1 ms on a quiet
LAN).

## Algorithm — NTP-Style 4-Timestamp Exchange

For each ordered pair `(self, peer)`, `self` initiates `N` samples. Each
sample exchanges four timestamps:

```
self                                       peer
  |                                          |
  | t1 = Utc::now()                          |
  |                                          |
  |  ----- ProbeRequest { id, t1 } ------->  |
  |                                          | t2 = Utc::now() (on receive)
  |                                          | t3 = Utc::now() (on send-back)
  |  <---- ProbeResponse { id, t1, t2, t3 }  |
  | t4 = Utc::now()                          |
```

Per-sample estimates:

```
rtt    = (t4 − t1) − (t3 − t2)
offset = ((t2 − t1) + (t3 − t4)) / 2     // peer.clock − self.clock
```

`offset` is what `peer` would need to ADD to its clock to match `self`.
Equivalently, `self` SUBTRACTS `offset` from a timestamp logged by `peer` to
express it in `self`'s clock frame.

Of the `N` samples, **select the one with the smallest `rtt`** as the
initial candidate. (Min-RTT selection is the standard NTP heuristic — the
sample with the least asymmetric queueing delay produces the least biased
offset.)

Default `N = 32`. Inter-sample delay: 5 ms (so 32 samples take ~160 ms +
RTTs).

### Outlier rejection (added in T8.4)

The min-RTT heuristic alone is not sufficient on Windows: occasionally the
sample with the smallest RTT lands on a clock-quantization tick boundary
or coincides with a transient OS time correction, producing an offset that
disagrees with all other samples by hundreds of milliseconds despite a low
RTT. Observed once during T8.1 validation — see DECISIONS.md D8.

Mitigation:

1. Compute the median offset across all `N` samples and the standard
   deviation of offsets.
2. If the min-RTT sample's offset deviates from the median by more than
   `5 × stddev`, reject it.
3. Fall back to the **median offset of the three samples with the lowest
   RTTs**. This trades a tiny amount of best-case precision for robustness
   against the single-sample artefact.
4. Set `outlier_rejected = true` on the resulting `OffsetMeasurement` so
   the JSONL line records that the fallback fired.

Defense in depth: every `ProbeResponse` is verified to echo the same
`t1` string the request was sent with. Mismatches are dropped (a
defense against future protocol changes that might let a stale response
slip through despite the `(from, to, id)` matching).

### Per-sample diagnostic log (added in T8.4, extended in T8.5)

Each runner additionally writes a sibling
`<runner>-clock-sync-debug-<run>.jsonl` containing one line per probe
**attempted**, regardless of whether a response was received. This is
for offline diagnosis and is not consumed by analysis.

Per-line fields: `ts`, `runner`, `run`, `variant`, `peer`,
`sample_index`, `t1_ns`, `t2_ns`, `t3_ns`, `t4_ns`, `accepted: bool`
(marks the sample that fed the canonical measurement, or `false` if
no sample was chosen / outlier path fired), `outlier_rejected: bool`,
and `result: string` — see below.

The numeric `offset_ms` and `rtt_ms` fields are present **only** for
`result == "ok"` rows. For non-`ok` rows they are absent (the sample
quad was never completed) and consumers must check `result` first.

#### `result` values (T8.5)

| Value | Meaning |
|-------|---------|
| `"ok"` | A matching `ProbeResponse` arrived in time; `(t1, t2, t3, t4)` are all populated and the derived `offset_ms`/`rtt_ms` fields are present. |
| `"timeout"` | No matching `ProbeResponse` arrived within the per-sample 100 ms window. `t1_ns` is preserved (we know when we sent); `t2_ns`/`t3_ns`/`t4_ns` are 0; numeric fields absent. |
| `"rejected_filter"` | One or more datagrams arrived during the wait window but their `(from, to, id)` did not match the in-flight probe. Strong signal that per-runner port routing is misconfigured (e.g. a peer is binding the wrong port, or a stale process is still listening). |
| `"rejected_t1"` | A `ProbeResponse` matched on `(from, to, id)` but its echoed `t1` string did not match the request's `t1`. Should be impossible in production; only fires under protocol-version skew or extreme stale-response scenarios. |
| `"parse_error"` | One or more datagrams arrived during the wait window but failed to parse as a `Message`. Indicates either an unrelated process bound on the coordination port or a serializer-version skew. |

The motivating regression: pre-T8.5, this file only got a row when a
sample completed successfully. In a cross-machine run where 100% of
probes silently failed (T8.5 field report), the file was 0 bytes —
giving the operator no signal to distinguish "engine never ran" from
"every probe was filter-rejected." With the new schema, even a
total-failure cohort produces N debug rows whose `result` field
identifies which failure mode dominated.

## When to Run

Twice in the runner lifecycle:

1. **Initial sync**: after discovery completes and config hashes match,
   before the first `ready` barrier. This gives a baseline offset for each
   peer.

   **Fail-fast (T8.5).** If the initial sync produces zero samples for any
   listed peer, the runner aborts with a non-zero exit *before* the first
   ready barrier. Cross-machine latency without an offset measurement is
   statistically meaningless; we must never silently produce a benchmark
   run whose cross-machine numbers are uncorrected. The peer side is also
   forced to abort because it never sees its peer reach the ready barrier.

   The fail-fast applies only to the initial sync. Per-variant resyncs
   that produce zero samples remain soft warnings: analysis falls back to
   the most recent valid measurement (the initial sync, in the worst
   case).

2. **Per-variant resync**: immediately after each variant's `ready` barrier
   completes, before spawning the variant child. This catches clock drift
   over long runs (16 variants × 30 s ≈ 8 min total operate time, during
   which Windows w32time can drift several ms).

## Message Format

Sent over the existing runner coordination UDP socket (multicast +
localhost). New `Message` variants:

```rust
ProbeRequest  { from: String, to: String, id: u64, t1: String }
ProbeResponse { from: String, to: String, id: u64, t1: String, t2: String, t3: String }
```

- `from`/`to`: runner names (filter messages addressed to this runner).
- `id`: unique per `(from, to)` exchange. 64-bit incrementing counter.
- Timestamps: RFC 3339 nanosecond strings (same format as JSONL `ts`).

`t1` is echoed back so the initiator does not need state for in-flight probes.

Probe traffic uses the same coordination port — it's bursty but bounded
(N samples × peer_count × variant_count). The receiving runner ALWAYS
responds promptly to a `ProbeRequest`, regardless of its own state, as
long as it has completed discovery.

## Output: clock-sync log file

Each runner writes one JSONL file per benchmark run, named:

```
<runner>-clock-sync-<run>.jsonl
```

Placed in the same directory as the variant log files (so analysis picks it
up by globbing the run directory).

Every line includes the standard common fields plus the clock-sync-specific
fields. `variant` is set to the variant currently being prepared (`""` for
the initial sync that runs before any variant).

**Required columnar fields** (promoted to `analysis/schema.py::SHARD_SCHEMA`):

| Field | Type | Description |
|-------|------|-------------|
| `ts` | string (RFC 3339) | When the measurement was recorded by `self` |
| `runner` | string | This runner's name (the `self` side) |
| `run` | string | Run identifier |
| `variant` | string | Name of the variant about to start, or `""` for initial sync |
| `event` | string | Always `"clock_sync"` |
| `peer` | string | Peer runner name (the `other` side) |
| `offset_ms` | float | `peer.clock − self.clock` in milliseconds (best sample) |
| `rtt_ms` | float | RTT of the selected best sample, in milliseconds |

**Optional diagnostic fields** (kept in the JSONL line for debugging /
network quality inspection but NOT in `SHARD_SCHEMA` — analysis ignores
them):

| Field | Type | Description |
|-------|------|-------------|
| `samples` | integer | Number of samples taken (typically `N`) |
| `min_rtt_ms` | float | Minimum RTT across all samples (= `rtt_ms` for the chosen one) |
| `max_rtt_ms` | float | Maximum RTT across all samples (sanity / network-quality indicator) |
| `outlier_rejected` | bool | `true` if the min-RTT sample was rejected and the median-of-three-lowest-RTT fallback fired (T8.4) |

Single-runner runs (loopback only) emit no clock-sync events — the file may
be absent. Analysis must handle this case gracefully (treat offset as 0).

**Sibling debug file** (T8.4, extended in T8.5): each runner additionally
writes `<runner>-clock-sync-debug-<run>.jsonl` with one line per probe
**attempted** (T8.5: not just successful samples — see "Per-sample
diagnostic log" above for the full schema and the `result` enum). Used
for offline diagnosis only; analysis ignores this file entirely.

## Analysis Application

In `analysis/correlate.py` (or `performance.py`):

1. Load all `*-clock-sync-*.jsonl` files for the run.
2. Build a table `offsets[(run, self_runner, peer_runner)] → list of measurements`.
3. For each delivery record where `writer_runner != receiver_runner`:
   - Look up the most recent measurement (by `ts`) on the **receiver's**
     side with `peer = writer_runner`. That gives
     `offset = writer.clock − receiver.clock` in receiver's frame.
   - Adjusted latency: `(receive_ts − write_ts) + offset_ms`.
4. For same-runner records (`writer == receiver`): no adjustment.
5. If no offset is available for a cross-runner pair: log a warning, fall
   back to no adjustment, and flag the result as uncorrected in the report.

## Limitations

- **Asymmetric paths**: NTP offset estimation assumes the network delay is
  the same in both directions. On a quiet LAN this is generally true to
  within tens of µs. Asymmetric routing, NIC offload variability, or
  congestion can introduce error.
- **OS clock granularity**: `Utc::now()` on Windows has ~100 ns resolution
  but can be quantized to ~1 ms on older systems. This sets a floor on the
  achievable accuracy.
- **Drift between resyncs**: between per-variant resyncs, clocks drift at
  ~10 ppm. Across a 30 s variant operate phase, that's ~300 µs of drift —
  acceptable.
- **Adversarial scenarios** (e.g. one machine's clock is jumping due to
  external NTP corrections during the run) are not handled. Recommend
  disabling automatic OS time sync on benchmark machines.

## Known Deviations

_None yet._
