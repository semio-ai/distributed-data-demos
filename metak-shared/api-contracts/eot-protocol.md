# API Contract: End-of-Test (EOT) Protocol -- HISTORICAL (E12-E14)

> **Historical (E12-E14).** This protocol was used until E15 introduced
> runner-coordinated termination based on the variant's stdout
> `progress` events (T15.1-T15.4) combined with variant-side idle
> detection (T15.5). The on-wire EOT exchange described below -- and
> the dedicated TCP control side-channel added in T14.18 for
> `custom-udp` / `hybrid` -- was removed in **T15.8**. The
> `Variant::signal_end_of_test` / `Variant::poll_peer_eots` trait
> methods, the `PeerEot` type, the `--eot-timeout-secs` CLI arg, and
> the `--control-base-port` CLI arg are all gone.
>
> The `eot_sent` JSONL event still appears in every variant log -- the
> driver emits it once between operate and silent on every spawn so
> the analysis pipeline (T11.5, T14.17) keeps working unchanged. No
> `eot_received` or `eot_timeout` events are emitted by post-T15.8
> variants.
>
> The document is retained for archaeology and for interpreting
> pre-E15 datasets where the on-wire exchange did run.

---

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
  **EXCEPTION**: see "Control side-channel (T14.18)" below — variants
  MAY use a dedicated per-peer-pair TCP control connection for EOT
  when the data path cannot guarantee delivery under saturation.
- EOT MUST NOT block on the data channel being fully drained. Send the
  EOT marker and let it ride the existing transport's ordering /
  reliability semantics.
- Receivers MUST continue to drain in-flight data while waiting for
  EOT — EOT is an additional signal, not a barrier that suppresses
  receives.

## Control side-channel (T14.18)

Variants MAY use a dedicated per-peer-pair TCP control connection for
EOT exchange, separate from the data path. This was introduced to fix
an architectural failure mode where high-rate symmetric workloads
(observed at 100K msg/s on `custom-udp` qos1-3 and `hybrid` qos1-2,
Single mode) saturate the kernel UDP recv buffer faster than userspace
can drain it, dropping the EOT marker datagram before it reaches the
variant. In Single mode the data path is constrained to a single
thread (WASM-compatibility goal), so a separate control plane is the
only way to keep EOT delivery deterministic at saturating throughput.

Variants in scope as of T14.18: `custom-udp`, `hybrid`. Variants whose
data transport already provides reliable delivery (`websocket`, post-
T14.13 `quic`) are NOT in scope and should keep using the existing
data-path EOT mechanism. `zenoh` is out of scope (its own publisher
mechanics handle this; T14.9 covers the sidecar topology).

### Wire shape

The control connection carries length-prefixed binary frames:

```
[u32 BE length] [tag: u8] [tag-specific payload]
```

Tags:
- `0x01` — EOT marker. The tag-specific payload is the existing
  variant-internal EOT encoding (same `(writer, eot_id)` shape used
  on the data path), unmodified.
- `0x02` — `bye` marker. Sent by both sides during `disconnect()` to
  signal "I am done sending on this control channel; you may close."
  No payload follows the tag.

Frames are bounded at 4 KiB (`MAX_CONTROL_FRAME_BYTES`) on the wire.
Receivers that see a frame larger than this MUST drop the peer.

### Connection lifecycle

- **Pairing**: lower-sorted-name peer is the **server** (binds +
  listens on its `control_base_port + runner_index`). Higher-sorted
  peer is the **client** (connects). Same convention as Hybrid TCP /
  QUIC / WebSocket data ports.
- **Port derivation**: each variant exposes a variant-specific
  `--control-base-port <u16>` CLI arg. Per-runner stride = 1; **no
  QoS stride** — one control port per (runner, variant binary),
  shared across all QoS levels of that variant binary.
- **At `connect()`**: server binds + accepts; client dials with
  bounded retry on `ConnectionRefused` (the two runners race past
  the ready barrier). `TCP_NODELAY` is set immediately. One
  bidirectional connection per peer pair.
- **At `disconnect()`**: send a `bye` frame, half-close the write
  side, drain the read side until the peer closes or
  `--eot-timeout-secs` elapses, then close. Frames received during
  the drain (typically a last EOT that raced the local `bye`) are
  still applied.

### Threading

- **Multi mode**: one dedicated OS thread per control connection
  reads length-prefixed frames in a blocking loop with short
  `SO_RCVTIMEO` and pushes decoded EOT markers onto the variant's
  existing T14.16 lifecycle channel. The data path is unchanged.
- **Single mode**: control socket is blocking with a short
  `SO_RCVTIMEO` (~1 ms). The variant's `poll_receive` polls each
  control peer inline via `try_recv_frame`. **No additional threads
  are introduced in Single mode** — the data path remains
  single-threaded as required for the WASM-compatibility goal.

### On-wire semantics and JSONL events: unchanged

The control side-channel is a routing change only. The on-wire EOT
payload (`writer`, `eot_id`) is identical to the data-path encoding,
and the driver still emits `eot_sent` / `eot_received` / `eot_timeout`
events with the same fields documented elsewhere in this contract.
Analysis tools require no changes.

### Per-variant adjustments

When a variant adopts the control side-channel, it MUST remove
EOT-over-data-transport for every QoS level it covers:

- `custom-udp`: EOT-over-multicast (qos1-3) and EOT-on-TCP-stream
  (qos4) are removed. Control connection is always present regardless
  of QoS.
- `hybrid`: EOT-over-multicast (qos1-2) and EOT-on-TCP-stream
  (qos3-4) are removed. Control connection is always present
  regardless of QoS.

The data path remains unchanged in both variants; only the EOT
routing moves.

### Hybrid

- **All QoS levels (T14.18)**: EOT travels exclusively over the
  per-peer-pair TCP control connection (see "Control side-channel
  (T14.18)" above). The data-path EOT mechanisms documented below
  are removed as of T14.18 and retained only for historical context.
- ~~**TCP path (qos 3-4)**: send a tagged control frame on the same
  per-peer TCP stream after the last data frame.~~ (Removed T14.18.)
- ~~**UDP path (qos 1-2)**: send a typed multicast packet, repeated 5
  times with 5 ms spacing for redundancy under loss.~~ (Removed
  T14.18.)

### Custom UDP

- **All QoS levels (T14.18)**: EOT travels exclusively over the
  per-peer-pair TCP control connection. The data-path mechanisms
  below are removed.
- ~~**TCP path (qos 4)**: same as Hybrid TCP.~~ (Removed T14.18.)
- ~~**UDP path (qos 1-3)**: typed multicast packet, 5 retries with 5 ms
  spacing.~~ (Removed T14.18.)

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
