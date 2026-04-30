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
estimate. (Min-RTT selection is the standard NTP heuristic — the sample with
the least asymmetric queueing delay produces the least biased offset.)

Default `N = 32`. Inter-sample delay: 5 ms (so 32 samples take ~160 ms +
RTTs).

## When to Run

Twice in the runner lifecycle:

1. **Initial sync**: after discovery completes and config hashes match,
   before the first `ready` barrier. This gives a baseline offset for each
   peer.

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
| `samples` | integer | Number of samples taken (typically `N`) |
| `min_rtt_ms` | float | Minimum RTT across all samples (= `rtt_ms` for the chosen one) |
| `max_rtt_ms` | float | Maximum RTT across all samples (sanity / network-quality indicator) |

Single-runner runs (loopback only) emit no clock-sync events — the file may
be absent. Analysis must handle this case gracefully (treat offset as 0).

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
