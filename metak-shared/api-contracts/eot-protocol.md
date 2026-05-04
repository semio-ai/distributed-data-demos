# API Contract: End-of-Test (EOT) Protocol

Defines the deterministic boundary between active write traffic and the
silent/drain phase. Replaces reliance on `silent_secs` being "long
enough" for transport buffers to drain with an explicit handshake: the
writer broadcasts EOT to peers and waits (bounded) until each peer has
seen it, before the spawn moves on.

Source: STATUS.md T10.6b validation, where `silent_secs = 1` proved
insufficient for TCP back-pressure to drain on localhost at high rates,
and the orchestrator + user agreed an explicit handshake is the right
fix.

## Why

Without EOT, the protocol driver sequences `operate -> silent ->
disconnect`, where `silent` is a wall-clock timer that gives in-flight
data "some time" to land. On a fast LAN this is usually enough. On
localhost at 100K msg/s with `silent_secs = 1`, it isn't: kernel TCP
send buffers can hold tens of thousands of bytes the receiver hasn't
drained when the spawn ends. The lost bytes look like packet loss to
the analysis tool, masking real performance and falsifying any
delivery-percentage assertion.

EOT makes the operate window self-terminating: a writer publishes its
last data message, then publishes EOT, then waits (with a timeout) for
EOT from every peer. Once all peers have echoed back, the operate
window is genuinely over, and `silent_secs` is reduced to a small
post-EOT grace window for any final post-EOT-but-already-in-flight
packet.

## Phase Ordering

Revised, replacing the four-phase driver from BENCHMARK.md S4:

```
connect -> stabilize -> operate -> eot -> silent -> disconnect
```

Per-phase semantics:

| Phase | Duration | Meaning |
|---|---|---|
| `connect` | until `Variant::connect` returns | Establish peer connections. Logged via `connected` event when complete. |
| `stabilize` | `--stabilize-secs` | Idle, no writes. Existing semantics. |
| `operate` | `--operate-secs` | Writes happen at the configured tick rate. **No data writes after this phase ends.** |
| `eot` | bounded by `--eot-timeout-secs` (default `max(operate_secs, 5)`) | The variant calls `signal_end_of_test` once on entry, then `wait_for_peer_eots` until every peer is observed or the timeout elapses. No `write` events. `receive` events from in-flight data are still expected. |
| `silent` | `--silent-secs` (default reduced to 1 in fixtures) | Small grace window for any post-EOT-but-still-arriving packet. |
| `disconnect` | until `Variant::disconnect` returns | Tear down. |

The driver logs `phase` events on every transition (existing behaviour),
so `phase=eot` and `phase=silent` are explicit boundaries the analysis
tool can scope by.

## Variant Trait Additions

Two methods added to the `Variant` trait in `variant-base`. The
poll-style shape matches the existing `poll_receive` idiom (variant
returns observations; driver decides when to stop and what to log).
Both have default implementations so older variants compile and run
unchanged.

```rust
/// Broadcast an "end of test" marker to all peers. Called once by the
/// driver at the start of the EOT phase, after the last data write.
/// Returns the `eot_id` (64-bit random per writer per spawn) so the
/// driver can log it in `eot_sent`.
///
/// Default impl: returns 0 and does nothing (variant opted out of EOT;
/// driver will fall back to silent_secs).
fn signal_end_of_test(&mut self) -> anyhow::Result<u64> {
    Ok(0)
}

/// Return any newly-observed peer EOTs since the last call. Called
/// repeatedly by the driver in a poll loop until every expected peer
/// is observed or the configured timeout elapses.
///
/// The variant MUST dedupe internally: if peer X has already been
/// returned in a previous call, do not return X again. The driver
/// uses dedup-by-writer-name on its side as a defensive backstop, but
/// the variant is the source of truth.
///
/// Default impl: returns an empty vec (variant opted out of EOT).
fn poll_peer_eots(&mut self) -> anyhow::Result<Vec<PeerEot>> {
    Ok(Vec::new())
}

#[derive(Debug, Clone)]
pub struct PeerEot {
    /// Writer's runner name (the peer that sent EOT).
    pub writer: String,
    /// 64-bit id from the writer's `signal_end_of_test`.
    pub eot_id: u64,
}
```

Driver pseudocode in the EOT phase:

```rust
let my_eot_id = variant.signal_end_of_test()?;
logger.log("eot_sent", json!({ "eot_id": my_eot_id }));

let deadline = Instant::now() + eot_timeout;
let mut seen: HashSet<String> = HashSet::new();
let expected: HashSet<String> = peers_excluding_self.iter().cloned().collect();

while seen != expected && Instant::now() < deadline {
    for eot in variant.poll_peer_eots()? {
        if seen.insert(eot.writer.clone()) {
            logger.log("eot_received",
                json!({ "writer": eot.writer, "eot_id": eot.eot_id }));
        }
    }
    thread::sleep(Duration::from_millis(10));
}

if seen != expected {
    let missing: Vec<_> = expected.difference(&seen).cloned().collect();
    logger.log("eot_timeout",
        json!({ "missing": missing, "wait_ms": elapsed_ms }));
}
```

A variant that does NOT override these methods sees:
- `signal_end_of_test` returns `Ok(0)`. Driver logs `eot_sent` with
  `eot_id: 0`.
- `poll_peer_eots` returns `Ok(vec![])` forever. Driver loops until the
  timeout elapses and logs `eot_timeout` with the full peer set as
  `missing`.

That degenerate behaviour is by design: it's diagnostic (the analysis
tool can detect "variant has no EOT support") without aborting the
spawn.

## Per-Variant Mechanics

EOT is delivered through whatever transport the variant uses. The
contract is:

- Every peer must eventually observe a single EOT per writer
  (idempotent — receivers MUST dedupe by `(writer, eot_id)`).
- EOT MUST be delivered through the same transport channel(s) that
  carry data; do NOT introduce a sideband channel just for EOT.
- EOT MUST NOT block on the data channel being fully drained. Send the
  EOT marker and let it ride the existing transport's ordering /
  reliability semantics.
- Receivers MUST continue to drain in-flight data while waiting for
  EOT — EOT is an additional signal, not a barrier that suppresses
  receives.

### Hybrid

- **TCP path (qos 3-4)**: send a tagged control frame on the same
  per-peer TCP stream after the last data frame. Receiver sees ordered
  delivery; ack is implicit (TCP delivery semantics).
- **UDP path (qos 1-2)**: send a typed multicast packet, repeated 5
  times with 5 ms spacing for redundancy under loss. Receivers dedupe
  by `(writer, eot_id)`.

### Custom UDP

- **TCP path (qos 4)**: same as Hybrid TCP.
- **UDP path (qos 1-3)**: typed multicast packet, 5 retries with 5 ms
  spacing. Receivers dedupe by `(writer, eot_id)`.

### QUIC

- **Reliable streams (qos 3-4)**: close the data stream cleanly after
  the last write. Receivers treat stream-end as EOT.
- **Datagrams (qos 1-2)**: typed datagram packet, 5 retries with 5 ms
  spacing. Receivers dedupe.

### Zenoh

- Publish to a sibling key per writer:
  `bench/__eot__/<writer-runner-name>`. Subscribers pre-declare interest
  in `bench/__eot__/**` during `connect`. Each EOT message includes the
  writer name and an `eot_id` (random 64-bit) for dedup.

## Wire Format (variant-internal)

The control marker layout is variant-specific (since transports vary)
but each MUST encode at minimum:
- A tag distinguishing EOT from data
- The writer's runner name
- An `eot_id` (random 64-bit) for dedup

For variants with typed message frames (Hybrid TCP, Custom UDP TCP,
Zenoh), prefer extending the existing wire format with a new `EOT`
variant rather than reusing a sentinel value in the data range.

## JSONL Events

Three new event types in `jsonl-log-schema.md`. The existing `phase`
event gains a new valid value `"eot"`.

### `eot_sent`

Logged once by the writer immediately after `signal_end_of_test`
returns.

| Field | Type | Description |
|---|---|---|
| `eot_id` | integer | The 64-bit id used for this writer's EOT. Lets a receiver's `eot_received.eot_id` join with the writer's `eot_sent.eot_id`. |

### `eot_received`

Logged once per peer per writer, as the receiver observes EOT (after
dedup).

| Field | Type | Description |
|---|---|---|
| `writer` | string | Runner name of the writer whose EOT was just observed |
| `eot_id` | integer | The id from the writer's `eot_sent` |

### `eot_timeout`

Logged once at the end of the EOT phase IF `wait_for_peer_eots` returned
`TimedOut`. Diagnostic; presence does NOT abort the spawn.

| Field | Type | Description |
|---|---|---|
| `missing` | array of strings | Peer runner names that never signalled EOT |
| `wait_ms` | integer | Wall-clock duration of the wait |

## CLI Argument

A new variant-base CLI flag, injected by the runner:

```
--eot-timeout-secs <integer>
```

Default value: `max(operate_secs, 5)`. Configurable via TOML
`[variant.common].eot_timeout_secs`.

## Analysis Tool Implications

The analysis tool uses **asymmetric write/receive windows** to handle
the in-flight tail correctly. Previously the operate window was a
single symmetric `[operate_start, silent_start]` interval applied to
both writes and receives. After E12 the windows are split:

```
write_window  = [W.operate_start, W.eot_sent.ts]
receive_window = [W.operate_start, R.silent_start]   (per receiver R)
```

Rationale:
- **`write_window`** ends at the writer's `eot_sent.ts`. The writer
  declared "I am done writing" at that timestamp, so any later
  `write` event would be a contract violation. Writes outside this
  window are not legitimate operate-phase writes and don't count
  toward delivery denominators.
- **`receive_window`** extends to the receiver's `silent_start`. A
  message the writer wrote at `T_write < W.eot_sent.ts` may still be
  in flight when the writer's EOT arrives (network/processing
  latency, transport buffer), and the receiver legitimately logs it
  with `ts > W.eot_sent.ts`. We MUST count those receives -- they
  correspond to writes that happened in-window. The receiver's
  `silent_start` is the hard cutoff because by then the receiver is
  winding down and the cross-peer test is effectively over.

Loss percentage for delivery-completeness checks:

```
denom = count(write events from W with ts in W.write_window)
numer = count(receive events on R with writer=W and ts in R.receive_window)
loss% = 1 - (numer / denom)
```

Note: this asymmetry can in extreme cases produce
`numer > denom` (a fast in-flight tail combined with a tight
`eot_sent` race). When that happens, clamp `loss%` to a minimum of
0% (delivery% capped at 100%) and emit a warning to stderr that the
EOT race was tight enough to skew the window scoping; consider
increasing `silent_secs` or reducing operate-phase tail bursts.

### `late_receives` diagnostic (kept)

The analysis tool still surfaces a `late_receives` count: receives
whose `ts > W.eot_sent.ts` (i.e. inside the in-flight tail bracket).
This is **diagnostic, not a regression flag** -- a high
`late_receives` count means the EOT raced significantly ahead of the
data, which is informative for tuning but not itself an error. The
late_receives are STILL counted in the `numer` above; the metric is
purely observational.

### Test-level scoping (T10.6 / T12.7 regression tests)

The two-runner regression tests under each variant's
`tests/two_runner_regression.rs` use the same asymmetric-window
scoping when computing per-(writer, receiver) delivery percentages.
This is what allows tight thresholds (>=99% TCP, ==100% Zenoh
1000paths) to hold deterministically in the presence of normal
in-flight tails.

## Backward Compatibility

- Variants that don't override `signal_end_of_test` / `wait_for_peer_eots`
  fall through to no-op default impls. The driver still logs `phase=eot`
  and `phase=silent` and a synthesised `eot_sent`+`eot_received` for the
  variant's own runner (so the analysis pipeline sees a uniform shape).
  Cross-runner delivery falls back to silent_secs-driven drain — i.e.
  pre-E12 behaviour.
- Older log files (no `eot_sent` event) are handled by the analysis
  tool's existing fallback: when `eot_sent.ts` is missing for a
  (variant, run, writer), use `phase==silent.ts` as the operate-window
  end.

## Validation

- Per-variant unit tests for `signal_end_of_test` / `wait_for_peer_eots`
  including the timeout path.
- Per-variant localhost two-runner integration test that asserts:
  - Both runners log `eot_sent` exactly once
  - Each runner's JSONL contains `eot_received{writer=peer}` for every
    other peer
  - No `eot_timeout` event under normal conditions
- Cross-variant: T10.6 regression suite retightened to:
  - Hybrid TCP qos 3-4: `>=99%` cross-peer delivery in operate window
  - Hybrid UDP qos 1-2: `>=99%` (correctness) / `>=95%` (high-rate)
  - Custom-UDP qos 4 TCP: `>=99%`
  - Custom-UDP qos 1-3 UDP: `>=99%` (correctness scenarios)
  - QUIC qos 1-4: `>=99%` (datagram qos 1-2 may dip; per-fixture spec)
  - Zenoh `1000paths`: `==100%` (already locked in)
  - Zenoh `max-throughput`: `>=80%` (documented mpsc-receive drop)
