# Zenoh Variant — Custom Instructions

> **T15.8 (E15 cleanup):** The on-wire EOT exchange (the
> `bench/__eot__/<writer>` topic + `Variant::signal_end_of_test` /
> `Variant::poll_peer_eots` trait methods) was removed. End-of-operate
> is now driven by variant-base's idle detection (T15.5) and the
> runner-coordinated termination state machine (T15.4). The `eot_sent`
> JSONL event is still emitted exactly once per spawn (the marker
> analysis T11.5 / T14.17 consume).

## Overview

Thin Rust binary implementing the `Variant` trait from `variant-base` using
Eclipse Zenoh as the transport layer. Represents the "high-level framework"
approach — minimal custom protocol code, relying on Zenoh for discovery,
routing, and delivery.

## Tech Stack

- **Language**: Rust (2021 edition)
- **Crate type**: binary (`variant-zenoh`)
- **Key dependencies**:
  - `variant-base` (path = `../../variant-base`) — Variant trait, types, CLI, driver
  - `zenoh` — the pub/sub transport (use latest stable, currently ~1.9).
    Reached only from the Multi-mode call graph.
  - `tokio` (`rt-multi-thread`, `sync`, `macros`, `time`) — the runtime
    owned by `ZenohVariant` for the Multi-mode publish/receive bridge
    (see Design Guidance below). Pulled in directly even though Zenoh
    already depends on it, so the variant has a stable
    `tokio::runtime::Runtime` handle and `tokio::sync::mpsc` API
    surface. Reached only from the Multi-mode call graph.
  - `ureq` (T14.9b, `default-features = false`) — sync HTTP client
    powering Single mode's `publish`. Reached only from the
    Single-mode call graph; verified tokio-free (see "Tokio-free
    verification" below).
  - `base64` (T14.9b) — standard-alphabet base64 decoder for the
    zenoh-plugin-rest SSE envelope on the receive side.
  - `serde_json` — used by the T14.9a sidecar config generation and
    the T14.9b SSE JSON envelope decode.
  - `anyhow` — error handling
- Follow `metak-shared/coding-standards.md`.

## Build and Test

All commands run from the repo root (Cargo workspace). Do **not** `cd` into
`variants/zenoh/` to build — that produces a stray per-subfolder `target/`
directory which the configs do not point at.

```
cargo build --release -p variant-zenoh            # build variant-zenoh binary
cargo test --release -p variant-zenoh             # unit + integration tests
cargo clippy --release -p variant-zenoh -- -D warnings
cargo fmt -p variant-zenoh -- --check
```

Compiled binary lives at `target/release/variant-zenoh(.exe)`.

## Integration Contracts

- Implements the `Variant` trait from `variant-base`
- CLI args per `metak-shared/api-contracts/variant-cli.md`
- JSONL logs per `metak-shared/api-contracts/jsonl-log-schema.md` (handled by variant-base driver)

## Architecture

```
variants/zenoh/
  src/
    main.rs       -- parse CLI, create ZenohVariant, call run_protocol
    zenoh.rs      -- ZenohVariant struct implementing Variant trait
  Cargo.toml
```

## Design Guidance

### ZenohVariant

- **Construction**: Parse Zenoh-specific CLI args from `extra` (the pass-through
  args from variant-base CLI). Expected args: `--zenoh-mode` (default: `peer`),
  `--zenoh-listen` (optional, e.g. `udp/0.0.0.0:7447`).
- **Lenient parser** (E9): the runner now injects `--peers name=host,...`
  into every variant's extra args (see `variant-cli.md`). Zenoh has its
  own discovery (Zenoh scouting) and does not need peer info. The Zenoh
  arg parser MUST silently ignore `--peers` and any other unknown
  `--<name> <value>` pair instead of erroring. Skip the value token after
  any unknown `--name` so the parser stays in sync. Update
  `ZenohArgs::parse` accordingly and update the test that asserts
  `--unknown` errors — it should now pass through.
- **connect**: Build a dedicated 2-worker multi-thread tokio runtime
  owned by `ZenohVariant`. Open the Zenoh session in peer mode and
  declare a subscriber on `bench/**` from inside the runtime. Spawn two
  long-running tokio tasks: a publisher task that drains a bounded
  `mpsc::channel<OutboundMessage>` and a subscriber task that awaits
  `subscriber.recv_async().await` and forwards decoded
  `ReceivedUpdate`s through a second bounded `mpsc::channel`. Zenoh
  scouting handles peer discovery automatically via multicast.
- **publish**: Encode the message on the main thread, derive the Zenoh
  key from the workload path (`/bench/N` -> `bench/N` — see
  `path_to_key`; do **not** re-add the `bench/` prefix), then `try_send`
  the encoded bytes onto the publish channel. Fall back to
  `blocking_send` only when the channel is full (deliberate
  back-pressure). The publisher task in the runtime maintains a
  `HashMap<String, Publisher<'static>>` cache keyed by Zenoh key
  expression and awaits `publisher.put(...).await` on the cached
  publisher.
- **poll_receive**: Non-blocking `try_recv` on the receive-side mpsc
  channel.
- **disconnect**: Send the oneshot shutdown signal to the subscriber
  task, drop the publish sender (which causes the publisher task to
  drain its publisher cache and close the session), drop the receive
  end, then `runtime.shutdown_timeout(2s)` to wind down anything
  remaining.

### EOT (T12.5)

- **Same session, same runtime.** EOT publishes and EOT subscribes
  ride the existing T10.2b bridge -- a second wildcard subscriber on
  `bench/__eot__/**` is declared on the same Zenoh session as the
  data subscriber inside `connect`'s `block_on`, and a dedicated
  `eot_subscriber_task` is spawned on the same tokio runtime
  alongside `publisher_task` and `subscriber_task`. Do NOT open a
  second session or runtime for EOT -- the deadlock fix from D7
  depends on a single session driven exclusively by one runtime.
- **Outbound EOT.** `signal_end_of_test` generates a `u64` random
  `eot_id`, sends an `OutboundMessage::Eot { key, payload, done }`
  variant on the existing publish channel (the publisher task does
  a one-shot `session.put().await` rather than caching a per-key
  `Publisher`, since EOT is one-shot per spawn), and `block_on`s
  the completion oneshot inside the runtime so the put is committed
  before `signal_end_of_test` returns.
- **Inbound EOT.** The EOT subscriber task forwards `(writer,
  eot_id)` pairs over a bounded mpsc channel into the variant; the
  task filters out self-EOTs (writer == self_runner) so the driver
  never sees its own marker. `poll_peer_eots` drains the channel
  with `try_recv` and applies a `HashSet<(writer, eot_id)>` dedup
  -- the variant is the source of truth per the EOT contract.
- **Shutdown.** A second `oneshot::Sender<()>` (`eot_shutdown_tx`)
  signals the EOT subscriber task to terminate during `disconnect`,
  alongside the existing `shutdown_tx` for the data subscriber. The
  EOT receive channel is dropped before `runtime.shutdown_timeout`
  so the task can exit cleanly.

### Zenoh API style

Zenoh's Rust API is async-first. The variant uses **option 2**: a
dedicated tokio runtime owned by `ZenohVariant` with mpsc-channel
bridges to the synchronous `Variant` trait. Originally the variant
used `zenoh::Wait` blocking wrappers for simplicity, but T10.2b
(see DECISIONS.md D7) found that the synchronous wrappers deadlock
under symmetric high-fanout (1000 distinct keys/tick on both peers
simultaneously) because `session.put().wait()` calls `route_data`
synchronously while the same lock and tokio runtime are needed by
the RX side. The bridge keeps the variant's main thread out of
Zenoh's routing path entirely, lets the runtime fully drive both
TX and RX in parallel, and combines that with a per-key
`Publisher` cache so the route resolution cost is paid once per
distinct key and not once per put.

### Transport queue tuning (T-impl.2)

Zenoh does **not** expose a raw `SO_RCVBUF` / `SO_SNDBUF` knob on its
UDP transport links (the per-link `so_rcvbuf` / `so_sndbuf` options
documented in `DEFAULT_CONFIG.json5` only apply to TCP / TLS / QUIC
links). The closest equivalent — and what the other UDP-using variants
spend their 8 MiB allocation on — is the **transport-level priority
queue depth** that sits immediately above the socket. We raise every
priority queue to its schema maximum:

```
transport/link/tx/queue/size/control            16
transport/link/tx/queue/size/real_time          16
transport/link/tx/queue/size/interactive_high   16
transport/link/tx/queue/size/interactive_low    16
transport/link/tx/queue/size/data_high          16
transport/link/tx/queue/size/data               16
transport/link/tx/queue/size/data_low           16
transport/link/tx/queue/size/background         16
transport/link/rx/buffer_size                   8388608   (8 MiB)
```

The TX queues default to `2` batches per priority and are constrained
to the inclusive range `[1, 16]` by Zenoh itself (a value of 17+ causes
`zenoh::open` to error during config validation). With the default
`batch_size = 65535` bytes, 16 batches = ~1 MiB per priority queue, so
the per-link aggregate across the 8 priorities is ~8 MiB — matching the
8 MiB target the other variants set on `SO_*BUF` directly.

The RX-side `buffer_size` raises the per-link receive buffer from the
default 65 535 bytes to 8 MiB so the RX path absorbs the same bursts
the TX side now buffers.

Both edits live in `build_zenoh_config` (`src/zenoh.rs`); they are
applied to every session the variant opens, regardless of mode
(peer / client / router) or listen-endpoint configuration.

### Message encoding

The variant needs to transmit: `writer` (runner name), `seq`, `path`, `qos`,
and `payload`. Options:
1. Encode all metadata in the Zenoh key expression + attachment
2. Serialize a small header + payload as the Zenoh value

Prefer option 2 — serialize a compact header struct (writer, seq, qos, path
length) followed by the payload bytes. Use `bincode` or manual byte packing.
Keep it simple.

### Testing

- Unit test: construct ZenohVariant, verify connect/disconnect lifecycle.
- Integration test: run the full protocol driver with ZenohVariant in
  single-process mode (the variant publishes and subscribes to itself, similar
  to VariantDummy but over real Zenoh). Short durations (1-2s operate).
- The binary should be runnable via the runner using a config like:
  ```toml
  [[variant]]
  name = "zenoh"
  binary = "./variant-zenoh"
    [variant.common]
    tick_rate_hz = 10
    operate_secs = 2
    ...
    [variant.specific]
    zenoh_mode = "peer"
  ```

### Backpressure semantics (T-impl.7)

`Variant::try_publish` is implemented honestly on the QoS 1/2 best-effort
path and delegates to the default `publish` for the QoS 3/4 reliable path.

**Publisher cache split — congestion control per QoS**

`PublisherState` now carries two pre-declared publisher caches keyed by Zenoh
key expression: `publishers_drop` (`CongestionControl::Drop`) for QoS 1/2
and `publishers_block` (`CongestionControl::Block`) for QoS 3/4. Each cache
is pre-declared concurrently in `connect`'s `block_on` via a `JoinSet`, so
the operate phase pays zero per-message declare cost regardless of which
QoS the workload uses. The publisher task picks the right cache from
`OutboundMessage::Data { qos, .. }` (the `qos` field was added in this
task) and awaits `publisher.put(...).await` against it.

- **QoS 1 / QoS 2 (best-effort / latest-value)**: Publisher uses
  `CongestionControl::Drop`. Zenoh will silently drop messages from its
  internal outgoing queue if a downstream link cannot keep up. The
  variant's `try_publish` surfaces a different backpressure signal:
  the **bridge mpsc channel between the variant's main thread and the
  publisher task**. We `try_send` onto the bounded channel
  (`PUBLISH_CHANNEL_CAPACITY = 1024`); on `TrySendError::Full` we return
  `Ok(false)` and the driver logs `backpressure_skipped`.

  **Limitation (option (b) per the T-impl.7 task brief)**: Zenoh 1.9's
  public Publisher API does not expose a "messages dropped due to
  congestion" counter, nor does it surface a return code from `put` that
  distinguishes "delivered" from "internally dropped". Once a message
  clears our bridge channel, Zenoh's CongestionControl::Drop happens
  transparently inside the publisher and is **not** counted in our
  `backpressure_skipped` metric. The honest interpretation in analysis
  output is therefore: `backpressure_skipped` for Zenoh measures
  **bridge-saturation drops only**; any additional gap between
  per-runner `write` count and the global delivery rate is attributable
  to Zenoh's own internal CC=Drop policy and must be inferred from
  receive-side delivery rate rather than from a discrete counter.

- **QoS 3 / QoS 4 (reliable)**: Publisher uses `CongestionControl::Block`.
  `try_publish` delegates to `publish`, which `try_send`s onto the
  bridge channel and falls back to `blocking_send` if the channel is
  full. The publisher task then awaits `publisher.put(...).await`; with
  `CongestionControl::Block` Zenoh's queue applies back-pressure
  inside the runtime task rather than dropping. `try_publish` therefore
  always returns `Ok(true)` on the reliable path -- no seq gap, no
  `backpressure_skipped` events.

**T16.10 — QoS 3/4 ordering preservation**

T16.5's `publisher_task` originally `tokio::spawn`-ed every put as an
independent task (bounded by a 4096-slot semaphore) to eliminate
head-of-line blocking at 1 000 paths. That fix is correct for QoS 1/2
(BestEffort/LatestValue; ordering is not contractually required) but
broke reliable-ordered delivery for QoS 3/4: with
`CongestionControl::Block`, two concurrent `publisher.put(...).await`
futures for the *same key* can complete in arbitrary order because the
first put's Block-queue wait yields, the second put runs, and the
samples reach the wire reversed. The E16 smoke showed ~17 000
out-of-order receives per direction on 51 000 receives for the
1000x10hz QoS 3 reproducer.

The T16.10 fix: in `publisher_task`, branch on `reliable =
matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp)`:

- **QoS 1/2 (`Drop`)**: keep T16.5's spawn-per-put + semaphore. The
  drain loop acquires an `inflight` permit, `tokio::spawn`s the
  `publisher.put(...).await`, and immediately moves to the next
  message. Cross-key head-of-line blocking stays eliminated.
- **QoS 3/4 (`Block`)**: do **not** spawn. The drain loop awaits
  `publisher.put(encoded).await` inline. Because the publisher_task is
  a single task, every sample for every key is serialised in send
  order — exactly what ordered delivery requires. Per-key parallelism
  across *different* keys is given up on the reliable path, but
  T16.5's own verification (STATUS.md 2026-05-14) showed Zenoh's
  per-publisher Block queue at 1 000 keys on localhost is already the
  rate-limiting factor, so the spawn-per-put bought no meaningful
  additional throughput on this workload — only unordered delivery.

Both paths share the same `publishers_drop` / `publishers_block`
Arc-wrapped caches built in `connect`; the only difference is whether
the `put` is awaited inline or on a sub-task. The `inflight`
semaphore therefore gates QoS 1/2 traffic only (see
`PUBLISH_INFLIGHT_LIMIT` docstring for the updated scope note).

Verification fixtures:
- `variants/zenoh/tests/fixtures/two-runner-zenoh-1000x10hz-qos3-repro.toml`
- `variants/zenoh/tests/fixtures/two-runner-zenoh-1000x10hz-qos4-repro.toml`
  (added in T16.10)

Both reproducers show `Out-of-order 0` across all four (writer ->
receiver) directions; classifications are `runner_idle_terminated`
(driver-side, end-of-test phase) with no `variant_self_killed_idle`
(the internal-stall watchdog from T15.5).

**Exact Zenoh API path used**

Per-publisher congestion control is configured at declare time via
`session.declare_publisher(key).congestion_control(cc).await`, where
`cc` is one of `zenoh::qos::CongestionControl::{Drop, Block}`. Both
caches are populated this way during `connect`; lazy-declare fallback
inside `publisher_task` likewise applies the matching CC for the QoS
on first sight, so a workload using a key outside the pre-declared
`bench/0..N-1` set still gets the right congestion control on its
first publish.

Tests covering this contract live in `src/zenoh.rs` `tests` mod:
- `test_try_publish_qos1_returns_ok_false_when_channel_full` -- saturates
  the bridge channel and asserts `Ok(false)` for both BestEffort and
  LatestValue.
- `test_try_publish_qos3_and_qos4_never_return_ok_false` -- bursts 2x the
  channel capacity through reliable QoS and asserts every call returns
  `Ok(true)` (including across the implicit `blocking_send` fallback).
- `test_try_publish_qos1_default_path_returns_ok_true` -- single-write
  sanity case verifying the message is enqueued with the right `qos`
  tag for downstream cache routing.
- `multi_zenoh_qos3_qos4_preserves_per_key_order` (ignored) -- synthetic
  two-session loopback regression added 2026-05-24. Publishes 200 seqs
  across 4 keys for both `ReliableUdp` and `ReliableTcp` (1 600 messages
  total) from a connected `ZenohVariant` to an in-process auxiliary
  Zenoh session, then asserts each `(qos, key)` receive stream is
  strictly monotonically increasing. Mutation-verified: replacing the
  inline-await branch with the buggy `tokio::spawn` path makes the test
  fail at the first observable reorder.

**T16.10d -- multicast-route deduplication at 1000x100hz**

T16.10b's wired-loopback evidence (binary `aab87a5+dirty`, Multi mode)
showed 6K-22K "out-of-order" receives and 19K-23K "duplicates" per
direction at 1 000 paths x 100 Hz QoS 3/4 -- past T16.10's inline-await
fix had landed. Investigation reproduced the bug on
`logs/t1610d_qos3_run2/` showing 148 % delivery (1.5x receive ratio),
~100K reorders, ~900K duplicates per direction. The
`multi_zenoh_qos3_qos4_preserves_per_key_order` unit test passes; the
publisher_task itself is genuinely serial. The bug is at a different
layer.

**Root cause**: Zenoh's default `autoconnect_strategy = "always"` for
peer-to-peer (per `DEFAULT_CONFIG.json5` lines 154-156: "may result in
redundant connection") makes BOTH peers initiate a TCP connect when
they discover each other via multicast scouting AND gossip
simultaneously. For the window where two transport sessions co-exist
(before Zenoh's "redundant connection close" path completes) the same
sample is delivered through BOTH routes. The interleaved-streams shape
in the per-key receive seqs and the 100 ms-scale ts_ns gaps between
"duplicate" rows match a second-route delivery rather than a
retransmit. Affects every QoS tier equally because the failure is at
the route-establishment layer, above CC.

**Fix**:
1. `build_zenoh_config` now sets
   `scouting/multicast/autoconnect_strategy = "greater-zid"` AND
   `scouting/gossip/autoconnect_strategy = "greater-zid"`. Per the
   1.9 doc this makes the connect side deterministic: only the node
   with the lexicographically greater zid initiates. This reduces but
   does not eliminate the race (both peers may receive each other's
   HELLO at roughly the same instant), so:
2. `subscriber_task` now drops duplicate `(writer, seq)` pairs at the
   receive boundary via a task-local
   `HashMap<String, WriterDedup>`. Each writer's `seq` is a global
   monotonic counter (see `variant_base/src/seq.rs`) so
   `(writer, seq)` is a unique sample identifier. The dedup tracks the
   max seq seen per writer plus a sliding-window
   `HashSet<u64>` bounded by `DEDUP_WINDOW = 4 * QOS_STRICT_WINDOW =
   8192` entries. First sighting forwarded; later sightings dropped.
   No cross-task locking (the dedup map is task-local; the dedup
   decision is made before the recv channel send).

Dedup is applied to all QoS tiers (not just reliable) because the
multicast-route race affects every transport-layer delivery; at QoS
1/2 it is normally a no-op because the only way the receiver sees the
same `(writer, seq)` twice without retransmits is via duplicate-route
delivery.

Unit tests covering the dedup state machine:
- `test_writer_dedup_accepts_novel_seqs`
- `test_writer_dedup_drops_exact_duplicates`
- `test_writer_dedup_drops_too_old_seqs`
- `test_writer_dedup_set_is_bounded`

Multi-mode acceptance fixtures added for T16.10d:
- `variants/zenoh/tests/fixtures/two-runner-zenoh-1000x100hz-qos3-repro.toml`
- `variants/zenoh/tests/fixtures/two-runner-zenoh-1000x100hz-qos4-repro.toml`

Both explicitly pin `threading_modes = ["multi"]` -- T16.10's original
10 Hz fixtures default to Single mode (per
`runner/src/config.rs` `ThreadingModesSpec::Default`), which routes
through the out-of-process zenohd sidecar and trivially preserves
ordering; the publisher_task + subscriber_task in-process path was
never end-to-end exercised by the official fixture set before this
follow-up.

## Peer-coordinated back-pressure (T17.8) — removed in T9.5d

**Status**: REMOVED 2026-05-25 (T9.5d), per direct user request. The
T17.8 application-level credit/ack/dedup wrapper that sat on top of
Zenoh's native QoS no longer exists. The benchmark now measures
Zenoh-native QoS only — the `Reliability` + `CongestionControl`
mapping in `build_publisher_*`, nothing else.

### What was removed

- The `bench/__ack__/<receiver>/<writer>` side-channel
  (`ACK_KEY_PREFIX`, `ack_key_for`, `ack_wildcard_for_self`,
  `parse_ack_key`, `encode_ack_payload`, `decode_ack_payload`).
- The application-level credit window (`QOS_STRICT_WINDOW = 2048`,
  `WindowGate`, `WindowState`, `wait_for_window` on the publish path).
- The ack subscriber + emitter tasks (`ack_subscriber_task`,
  `ack_emitter_task`, `RecvAccounting`, the 25 ms heartbeat, the
  pre-seeded peer ack map).
- The receive-side multicast-route dedup (`WriterDedup`,
  `DEDUP_WINDOW = 8192`, the per-writer task-local dedup state in
  `subscriber_task`).
- The bridge channel up-sizing tied to the credit window
  (`PUBLISH_CHANNEL_CAPACITY` reverted from 16384 to a plain 4096 —
  one tick of headroom on the heaviest 1000-vpt fixture, no
  wrapper-tuned multiplier).
- The publish-side gate call (`gate.wait_for_window(seq)?` in the
  reliable arm of `publish`) and the subscriber-side ack emission
  (writes to `per_writer_max_seq` from `subscriber_task`).

### What stays (explicit per the user request)

- Zenoh-native QoS: the per-publisher `CongestionControl::Drop` (QoS
  1/2) vs `CongestionControl::Block` (QoS 3/4) mapping in
  `build_publisher_*` (the two pre-declared publisher caches in
  `connect`). This is the Zenoh primitive the bench is meant to
  measure.
- The EOT protocol on `bench/__eot__/**` (runner/variant-base
  contract; not application-level QoS).
- Explicit Multi-mode peering (T9.5c).
- `--multicast-interface` pin (T9.5).
- Transport buffer tuning (`transport/link/tx/queue/size/*`,
  `transport/link/rx/buffer_size`).
- `autoconnect_strategy = "greater-zid"` (T16.10d routing
  determinism, not application QoS).
- T16.10's inline-await for QoS 3/4 ordering on `publisher_task`
  (still single-task per-key serialisation; the spawn-per-put split
  on the QoS 1/2 Drop path is unchanged).
- The `SUBSCRIBER_FIFO_CAPACITY = 131_072` E19/T19.X cap (Zenoh
  routing-thread parking fix; independent of the wrapper).

### Failure modes now exposed

The wrapper existed to mechanically eliminate the QoS 3/4 asymmetric
stall T16.12 documented (the `variant_self_killed_idle` watchdog
exit under sustained ≥ ~50 K msg/s reliable load). With the wrapper
gone, that failure mode is back in scope — and that is precisely
what the bench is intended to measure. Operators should expect
Zenoh-native QoS 3/4 to collapse throughput, drop, or self-kill via
the T15.11 watchdog at high loads; report those as raw
Zenoh-behaviour data points, not regressions.

### Verification post-removal

- `cargo test --release -p variant-zenoh`: 79 unit tests + 1
  loopback integration test pass (test count delta from pre-T9.5d:
  -15, exactly the wrapper-coverage tests deleted).
- `cargo clippy --release --workspace --all-targets -- -D warnings`:
  clean.
- `cargo fmt -p variant-zenoh -- --check`: clean.
- Localhost smoke (T9.5d gate, scratch `_t9_5d_smoke.toml`, two
  runners local, 4 vpt × 10 Hz × 4 s operate): all four QoS levels
  reach `phase=done` with 164/164 sent==received on both peers, and
  the JSONL log subfolder contains zero `__ack__` keys.

The single new unit test `multi_zenoh_qos_round_trip_post_wrapper_removal`
(ignored) exercises a Multi-mode QoS 4 loopback round-trip without
any ack infra and asserts the reliable tiers deliver every sample.

## Threading modes (T14.7 / T14.9)

The Zenoh variant declares
`supported_threading_modes() -> &[Single, Multi]` as of T14.9b. The two
modes use radically different code paths:

* **Multi** (the in-process zenoh crate, default and only mode pre-T14.9):
  opens a `zenoh::Session` directly, drives it via a dedicated
  multi-thread tokio runtime owned by `ZenohVariant`, and bridges the
  variant's sync trait methods to the async API through two mpsc
  channels (see "ZenohVariant" / "Zenoh API style" above). The zenoh
  crate runs its own internal multi-threaded engine -- route
  resolution, transport TX/RX, session management, scouting, the
  storage backend all run as tokio tasks on a runtime the crate
  owns -- so Multi mode is genuinely multi-threaded.
* **Single** (T14.9b, see "T14.9b RPC client architecture" below): out-
  of-process `zenohd` sidecar (spawned by T14.9a lifecycle) absorbs all
  the concurrency. The variant's own call graph in Single mode is
  tokio-free: sync HTTP PUT via `ureq` for publishes, one dedicated OS
  thread reading SSE for receives, `std::sync::mpsc` bridge to the
  variant's main thread.

### Why Multi mode is "really" multi-threaded

The zenoh crate's threading is internal to the crate and not under our
control. Even when we open exactly one Session and one Subscriber on a
small fixed key-expression set, zenoh's own tasks are still alive in
the background. This is fundamentally different from QUIC and WebRTC,
where the crate is async but the boundary is sharper (one tokio
runtime we build, one set of tasks we spawn).

### Pre-T14.9b history

T14.9 was filed in `metak-orchestrator/TASKS.md` and **split during
its audit** into two sub-tasks (see STATUS.md "T14.9 -- AUDIT
findings"):

* **T14.9a (delivered)** -- the sidecar **lifecycle** only. Binary
  discovery, spawn at `connect(Single)`, kill at `disconnect`, port
  allocation, and per-platform child-process cleanup. Capability
  stayed `[Multi]` -- the variant errored out of publish/poll_receive
  with a clear T14.9b pointer.
* **T14.9b (delivered, this task)** -- the sync RPC client over the
  REST plugin's HTTP+SSE surface. Capability flipped to
  `[Single, Multi]` and Single-mode publish/poll_receive route
  through the sidecar.

## T14.9b RPC client architecture (Single mode)

After T14.9a brought the `zenohd` sidecar up, T14.9b wires the
variant's `publish` / `try_publish` / `poll_receive` through that
sidecar's REST plugin so the variant's own call graph stays
synchronous and tokio-free. Implementation lives in
`variants/zenoh/src/rest_client.rs`.

### publish / try_publish

A sync `ureq::Agent` issues an HTTP PUT against
`http://127.0.0.1:<rest_port>/<key>` with the encoded message body
and `Content-Type: application/octet-stream`. The plugin stores the
bytes as-is and forwards them to all matching subscribers.

* **Why ureq**: sync, built on `std::net`, no tokio. Verified via
  `cargo tree -e features -p variant-zenoh` (see "Tokio-free
  verification" below). `default-features = false` strips out
  rustls + gzip; the sidecar is bound to 127.0.0.1 so HTTP-only is
  fine.
* **Why `Content-Type: application/octet-stream`**: the plugin's
  `write` path attaches the request's content-type as the sample
  encoding. Without an explicit content-type the plugin defaults to
  zenoh's "empty" encoding, which the REST SSE surface then tries to
  treat as UTF-8/JSON and fails on our binary header. Explicit
  octet-stream makes the plugin take the base64 SSE path -- the
  same path the subscriber side is wired to decode.
* **Keep-alive ENABLED** (ureq defaults: `max_idle_connections = 10`,
  `max_idle_connections_per_host = 3`, `max_idle_age = 15 s`).
  T14.9b initially disabled keep-alive on the suspicion that the
  REST plugin silently dropped kept-alive connections on Windows
  localhost; the stress fixture exposed that decision as wrong.
  T14.9c reverses it. The construction now leaves these knobs at
  their ureq-default values (no explicit override). The previous
  hypothesis was either incorrect or specific to a narrower
  scenario; in practice, the pooled connection is healthy and
  reused for the lifetime of the spawn. `put()` drains the
  Content-Length: 0 response body via
  `response.body_mut().read_to_vec()` so ureq returns the socket
  to the pool rather than leaving it half-consumed (without the
  drain, the response handler's `ended()` is never invoked and
  the connection is not pooled).

  Why keep-alive is **mandatory**: under the T14.9c-era stress
  fixture (1000 vpt × 100 Hz = 100K msg/s × 8 s operate per
  spawn, qos2/3/4 Single), every fresh-per-PUT TCP connection
  consumed one ephemeral port. Windows' default dynamic port
  range is ~16K ports (49152-65535), so the pool exhausted within
  ~1 s and subsequent connects failed as
  `Only one usage of each socket address (...) (os error 10048)`
  — i.e. WSAEADDRINUSE. Linux' default 28K range (`net.ipv4.ip_local_port_range`)
  would hit the same wall at ~30K msg/s. With keep-alive on the
  outbound TCP socket count drops from N-per-publish to ~1, and
  the failure mode is eliminated regardless of platform.

  Pool sizing: ureq's defaults (10 total / 3 per host) are
  deliberately not tuned. The Single-mode call graph only ever
  connects to one host (`127.0.0.1:<rest_port>`), so the per-host
  cap is the relevant constraint; 3 idle connections is more than
  the variant ever needs because publishes are serialised through
  the main thread. The variant uses one pooled connection in
  steady state.
* **Retry-once on transport error**: a single in-method retry on
  any send-side error before propagating to the variant. One retry
  is sufficient: the failure modes seen on Windows localhost are
  transient half-open-connection / ECONNRESET-during-shutdown
  events; a genuinely wedged sidecar fails both attempts and the
  driver propagates the error.
* **`try_publish` delegates to `publish`** and returns `Ok(true)`
  on success. The HTTP PUT path does not surface a backpressure
  signal we can use to short-circuit (it's a blocking request).
  This matches the Multi-mode reliable path's contract (QoS 3/4
  also always returns `Ok(true)`).

### poll_receive

A dedicated OS thread (NOT tokio) opens a long-lived
`GET http://127.0.0.1:<rest_port>/<SUBSCRIBER_WILDCARD>` request with
`Accept: text/event-stream`. The plugin upgrades the connection to
SSE and emits one event per sample matching the wildcard. The thread
parses the chunked-transfer + SSE stream and pushes decoded
`ReceivedUpdate`s onto a bounded `mpsc::sync_channel`. The variant's
main thread drains via `try_recv` on every tick -- same pattern as
the established log-from-reader (T14.10) and progress_coord (T15.3)
threads.

* **Raw `TcpStream`, not ureq**: ureq's response-body API doesn't
  expose a per-read timeout, only the request-level `timeout_global`
  / `timeout_recv_body` knobs. For a long-poll SSE stream the read
  budget must be "no timeout on the stream, bounded timeout per
  read" so the stop-flag check happens every ~500 ms regardless of
  whether traffic is flowing. Issuing the HTTP/1.1 GET directly on
  a `TcpStream` lets us set `set_read_timeout(Some(500ms))` and
  loop on `WouldBlock` / `TimedOut` until either the stop flag
  fires or a real event arrives.
* **SSE event format**: `event:<kind>\ndata:<json>\n\n` per the
  [SSE spec][sse-spec]. The `data:` payload is a JSON envelope:
  ```json
  {"key": "<keyexpr>", "value": "<base64>",
   "encoding": "application/octet-stream",
   "timestamp": "<hlc-or-null>"}
  ```
  `value` is standard (padded) base64 of the sample's bytes when
  the encoding is not text / JSON -- which our binary
  `MessageCodec` output always triggers. The reader unwraps the
  JSON envelope, base64-decodes `value`, and feeds the raw bytes
  to `MessageCodec::decode`. See `extract_payload_from_sse_data`
  in `rest_client.rs` for the codec hook.
* **NB on the audit URL**: the T14.9b task brief suggested
  `GET /<key>?_method=SUB` as the subscription URL. Empirical
  inspection of `zenoh-plugin-rest` 1.9.0 (and the upstream source
  at the same revision) shows the real trigger is the
  `Accept: text/event-stream` header; the `?_method=SUB` query
  parameter is silently ignored. The audit prediction was
  incorrect; the actual URL is plain `GET /<key_expr>` with the
  Accept header.
* **Chunked transfer**: the SSE response uses
  `Transfer-Encoding: chunked`. The reader detects chunk-size
  prefix lines (hex-only lines terminated by `\r\n`) and skips
  them; everything else flows into the SSE parser. SSE blank
  lines (event terminators) are NOT classified as chunk-size
  lines -- the `is_chunk_size_line` helper carefully distinguishes
  the two.
* **Bounded channel + drop-on-full**: `sync_channel(4096)`. Same
  drop semantics as the Multi-mode bridge: a backed-up consumer
  produces JSONL receive gaps in the analysis, not unbounded
  memory growth.

[sse-spec]: https://html.spec.whatwg.org/multipage/server-sent-events.html

### Inter-sidecar Zenoh peering (two-runner topology)

`build_zenohd_config_json` accepts optional `listen_tcp` (host:port)
and `connect_tcp` (list of host:port) parameters that configure
inter-router Zenoh peering. Two per-runner sidecars on the same host
need an explicit peer mesh (multicast scouting alone doesn't reliably
deliver across two same-host routers in the default zenohd config):

* Each sidecar listens on `127.0.0.1:<rest_port + 1000>` for inbound
  Zenoh sessions. The +1000 offset partitions the REST and Zenoh
  TCP port ranges trivially without an extra CLI knob.
* Each sidecar dials out to every other peer's
  `<peer_host>:<peer_rest_port + 1000>` derived from the sorted
  `--peers` map (same convention this variant already uses for the
  REST port). Solo runs without `--peers` leave the connect list
  empty and the sidecar runs in standalone mode.

`connect(Single)` builds this list once at startup and writes it
into the per-spawn zenohd config file (see
`Sidecar::spawn`).

### connect(Single) flow

1. Locate the `zenohd` binary (fail-fast actionable error if
   missing -- contract unchanged from T14.9a).
2. Compute the per-runner REST port + the inter-sidecar peer list.
3. Spawn zenohd with a per-spawn config that enables the REST plugin
   on the derived port AND configures Zenoh peering.
4. Wait up to 5 s for the REST plugin to respond on
   `/@/router/local` before considering the sidecar live.
5. Build the `HttpPublisher` (ureq agent) targeting the REST port.
6. Start the SSE reader thread (raw TcpStream + per-read timeout)
   subscribed to `bench/**`.
7. Record `connected_mode = Single` and return success.

On any failure after the sidecar spawn the sidecar is killed before
the error propagates -- no orphan `zenohd` from a half-failed
connect.

### disconnect flow

1. Stop the SSE reader thread (atomic stop flag + drop the
   receiver). The reader checks the flag between every per-read
   timeout cycle, so worst-case shutdown latency is one
   `SSE_READ_TIMEOUT` (500 ms).
2. Drop the `HttpPublisher` (ureq agent releases its socket).
3. Tear the sidecar down (T14.9a path: SIGTERM/kill +
   per-platform cleanup).

### Tokio-free verification

`cargo tree -e features -p variant-zenoh` -- excerpt showing the
Single-mode call graph reachable from `ureq` and the relevant
direct-dep neighbourhood:

```
variant-zenoh
├── ureq v3.3.0
│   ├── base64 (default-features off via this crate's dep too)
│   ├── log
│   ├── percent-encoding
│   ├── ureq-proto v0.6.0
│   │   ├── httparse
│   │   ├── base64
│   │   ├── log
│   │   └── http v1.4.0
│   │       ├── bytes
│   │       └── itoa
│   └── utf8-zero
├── base64 v0.22.1
├── anyhow
├── bytes
├── clap
├── num_cpus
├── rand
├── serde_json
├── tokio        <-- direct dep, only Multi mode reaches it
├── variant-base
└── zenoh        <-- pulls tokio transitively, only Multi mode reaches it
```

Verification via the inverse-tree perspective (`cargo tree -e
features -p variant-zenoh --invert tokio`) shows tokio is reachable
ONLY through `zenoh`, `variant-zenoh`'s own direct `tokio` dep
(Multi-mode runtime), and a handful of dev-time crates. ureq's
subtree (the entire Single-mode RPC client surface) does NOT
include tokio.

The crates reachable from `connect(Single)` -> `publish` /
`poll_receive` are: `ureq`, `ureq-proto`, `http`, `httparse`,
`bytes`, `log`, `percent-encoding`, `utf8-zero`, `base64`,
`serde_json`, plus `std`. All sync, all `std::net`-backed.

## Single mode scaffolding (T14.9a)

Historical context for the sidecar lifecycle now exercised by
T14.9b. The variant **declares `[Single, Multi]`** as of T14.9b so
the runner spawns Single-mode fixtures through this branch
alongside Multi-mode. Tests + the manual smoke construct the variant
directly with `--threading-mode single`.

### Installing `zenohd`

```
cargo install zenohd --version 1.9.0
```

`cargo install zenohd --version 1.9.0` builds and installs the
router binary into `~/.cargo/bin/zenohd(.exe)`. **However** the
cargo-installed `zenohd` does NOT bundle the REST plugin's dynamic
library — `zenoh_plugin_rest.{dll,so,dylib}` — that the sidecar
needs to expose the HTTP RPC surface T14.9b consumes. There is no
official pre-built distribution for the plugin yet on cargo, so the
operator workaround on a developer host is:

```
# Build the plugin from its cargo-registry source. cdylib output
# lands in `target/release/zenoh_plugin_rest.{dll,so,dylib}`.
cd ~/.cargo/registry/src/index.crates.io-*/zenoh-plugin-rest-1.9.0/
cargo build --release

# Then drop the resulting library next to zenohd so zenohd's
# default plugin search path (`current_exe_parent`) finds it.
cp target/release/zenoh_plugin_rest.dll ~/.cargo/bin/   # Windows
cp target/release/libzenoh_plugin_rest.so ~/.cargo/bin/  # Linux
cp target/release/libzenoh_plugin_rest.dylib ~/.cargo/bin/  # macOS
```

T14.9b can revisit this when the upstream Zenoh project ships a
binary distribution; the variant code itself does NOT depend on
the plugin at compile time.

### Binary discovery

`connect(Single)` resolves the `zenohd` binary in this order, and
returns a clear actionable error if neither finds it:

1. `ZENOHD_PATH` environment variable. Must point at an existing
   file; a bad path is a hard error (no fallthrough to PATH so a
   typo doesn't silently boot a different installation).
2. Walk the `PATH` env var. On Windows we honour `PATHEXT` so
   `zenohd.exe` resolves from a bare `zenohd` lookup.

Failure message (operator-facing contract; do not change without
updating tests + this doc):

```
zenohd binary not found. Install via 'cargo install zenohd --version 1.9.0'
or set ZENOHD_PATH=<path>
```

### Port allocation (`--zenoh-sidecar-base-port`)

The variant accepts a new CLI flag in its `extra` (Zenoh-specific)
args: `--zenoh-sidecar-base-port <u16>`. The REST plugin port for
this runner is derived as:

```
runner_stride = 1
rest_port     = base_port + runner_index * runner_stride
```

`runner_index` is derived from the sorted `--peers` map the runner
injects (same convention as T14.18 / T15.10 control ports across
the TCP / hybrid / QUIC variants). Solo runs without `--peers`
collapse to `runner_index = 0`. The default `base_port` is 20100
so the operator-facing manual smoke ("spawn variant-zenoh
`--threading-mode single`") works without per-spawn TOML wiring;
production fixtures should specify the base port explicitly to
avoid collisions with other infrastructure.

### Sidecar lifecycle

At `connect(ThreadingMode::Single)`:

1. Locate the `zenohd` binary (fail-fast with the actionable error
   above if missing).
2. Generate a per-spawn JSON config enabling the REST plugin on the
   derived port AND (T14.9b) the inter-sidecar Zenoh peer mesh.
   The REST plugin is bound to `127.0.0.1:<port>` so the sidecar's
   RPC surface is not exposed beyond the local host. The Zenoh
   listen / connect endpoints (`tcp/<host>:<rest_port + 1000>`)
   are derived from the runner-injected `--peers` map.
3. `Command::spawn` with per-platform child-cleanup setup (below).
4. Poll the REST admin space (`/@/router/local`) for up to 5 s. If
   the plugin never responds we kill the child and surface a clean
   error rather than letting the sidecar hang the connect path.
5. Build the `HttpPublisher` (`ureq` agent) targeting the REST port
   (T14.9b).
6. Spawn the SSE reader thread subscribed to `bench/**` on the
   REST port (T14.9b).
7. Store all three handles on the variant; record
   `connected_mode = Single`.

`publish` / `try_publish` / `poll_receive` route through the
`HttpPublisher` + SSE reader -- see "T14.9b RPC client architecture"
above for the wire formats.

At `disconnect`:

1. Stop the SSE reader thread (T14.9b).
2. Drop the `HttpPublisher` (T14.9b).
3. Send SIGTERM (Unix) / `Child::kill` (Windows) to the sidecar.
4. Wait up to 500 ms for graceful exit; fall back to `kill()` for
   anything still alive.
5. Remove the per-spawn temp config file.
6. Drop the Job Object handle (Windows) -- belt-and-braces in case
   the explicit kill ever fails.

### Per-platform child-process cleanup

A SIGKILLed variant must not orphan its `zenohd` sidecar. Each
platform uses a different OS-level primitive:

* **Windows** (`windows-sys` crate, JobObjects feature): each
  spawned `zenohd` is assigned to a Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When the variant process
  exits (clean, panic, SIGKILL alike), Windows closes the Job
  Object handle and terminates every process inside the job. This
  is the strongest of the three guarantees and the only one that
  survives `TerminateProcess` of the parent.
* **Linux** (`libc` crate, `Command::pre_exec` hook):
  `prctl(PR_SET_PDEATHSIG, SIGTERM)` tells the kernel to deliver
  SIGTERM to the child as soon as the parent dies. Works for any
  parent-exit cause.
* **macOS / other BSDs** (same pre-exec hook): no
  `PR_SET_PDEATHSIG` equivalent. The hook calls `setpgid(0, 0)`
  so the child becomes its own process-group leader, and the
  variant relies on the explicit kill in `Sidecar::stop` for the
  clean-exit path. A SIGKILLed variant on macOS may leak its
  sidecar until the operator notices — accepted limitation for
  the first cut. T14.9b can revisit if real macOS deployments
  appear.

Implementation lives in `variants/zenoh/src/sidecar.rs`.

#### Why `windows-sys` and not `windows`

`windows-sys` is already a transitive dependency of the workspace
(via `winapi-util` and others) and is significantly lighter than
the higher-level `windows` crate. The required surface (Job Object
+ AssignProcessToJobObject + CloseHandle, all under
`Win32_System_JobObjects` + `Win32_Security` features) is identical
between the two crates.

### Why `connect(Single)` is the production Single-mode path

As of T14.9b the variant declares `[Single, Multi]` and the runner
spawns Single fixtures through `connect(Single)`. The earlier
T14.9a-only state where this branch was only exercised by tests +
the manual smoke is historical.

### Manual smoke

After installing zenohd + dropping the plugin DLL alongside it (see
"Installing zenohd" above):

```
cargo build --release -p variant-zenoh
cargo test --release -p variant-zenoh -- --ignored sidecar_lifecycle_smoke
cargo test --release -p variant-zenoh -- --ignored two_runner_regression_single_mode_t149b
```

Both `#[ignore]` tests skip gracefully if zenohd is not found
(printing a diagnostic) so they work on CI without zenohd
installed. The two-runner test additionally requires the runner +
variant-zenoh release binaries built at the workspace target dir.

For a hand-driven solo smoke that exercises the full Single-mode
path end-to-end:

```
./target/release/variant-zenoh \
  --tick-rate-hz 10 --stabilize-secs 0 --operate-secs 2 --silent-secs 0 \
  --workload scalar-flood --values-per-tick 1 --qos 1 \
  --log-dir /tmp/zenoh-smoke --launch-ts 2026-05-14T00:00:00.000000000Z \
  --variant zenoh --runner smoke --run smoke01 \
  --threading-mode single \
  -- --zenoh-sidecar-base-port 20100
```

Expected output (sample taken from a clean run, T14.9b):

```
[zenoh] build: <sha>+dirty (rustc <version>)
{"eot_received":false,"eot_sent":false,"event":"progress","phase":"operate","received":7,"sent":8,...}
{"eot_received":false,"eot_sent":false,"event":"progress","phase":"operate","received":17,"sent":18,...}
{"eot_received":true,"eot_sent":true,"event":"progress","phase":"done","received":20,"sent":21,...}
```

i.e. the variant brings the sidecar up, the driver publishes 1 vpt
× 10 Hz × 2 s = ~20 messages, the SSE reader delivers them all
back to the variant's main thread, the driver emits `eot_sent` via
T15.5 idle detection, and the process exits cleanly. Inspect
`Get-Process zenohd` (Windows) or `pgrep zenohd` (Unix) afterwards
to confirm no orphan sidecar.

Pre-T14.9b the same command surfaced the explicit
`"Single mode RPC client not yet implemented; pending T14.9b"`
error -- that path is gone now that publish/poll_receive are wired
to the REST plugin.

### `--recv-buffer-kb`

The zenoh crate hides its transport sockets behind the public
`zenoh::Session` API. There is no documented hook to set `SO_RCVBUF`
on the underlying transports, and reaching into the crate's internal
`Transport` types to grab raw sockets is out of contract. The
`--recv-buffer-kb` injected arg is therefore **advisory** for this
variant: the value is recorded in the `connected` JSONL event
(driver-side, per the E14 contract) but the variant cannot honour
per-spawn overrides. Operators tuning kernel buffers for Zenoh need
to use OS-level sysctl knobs outside the benchmark, or wait for the
T14.9 router-sidecar topology which exposes the router's TCP/UDP
listeners directly.

## T16.10c -- synthetic subscriber-jitter env-var hooks

Two env vars in `subscriber_task` simulate cross-WiFi-style receiver
back-pressure on localhost so the publisher-side stall path can be
exercised without a real two-machine WiFi setup. Off by default --
production runs unset both vars and the hot path stays identical to
the T16.10d-verified flow.

- `ZENOH_TEST_SUB_JITTER_MS` (u64, default 0) -- when set to a positive
  value, the subscriber `tokio::time::sleep(jitter_ms).await`s on every
  Nth recv'd sample (see below). 0 disables the jitter completely
  (production).
- `ZENOH_TEST_SUB_JITTER_EVERY` (u64, default 100) -- N: jitter applies
  to 1-in-N samples. Default keeps the cost negligible at moderate
  jitter values; lower values widen the stall fraction.

**Reproducing the cross-WiFi deadlock signature on localhost**:

```powershell
$env:ZENOH_TEST_SUB_JITTER_MS = "500"
$env:ZENOH_TEST_SUB_JITTER_EVERY = "50"
# Then run the standard two-runner reproducer in two terminals; both
# peers' subscriber tasks will sleep 500 ms every 50 samples, parking
# Zenoh's RX routing thread, parking the peer's CC=Block publisher,
# and reproducing the symmetric-stall signature T16.10b catalogued
# on cross-WiFi.
target\release\runner.exe --name alice --config variants\zenoh\tests\fixtures\two-runner-zenoh-1000x100hz-qos3-repro.toml
# (in a second terminal)
target\release\runner.exe --name bob   --config variants\zenoh\tests\fixtures\two-runner-zenoh-1000x100hz-qos3-repro.toml
```

Expected result with current main (T16.10d + this commit, no further
fix): one peer self-kills via T15.5 watchdog OR `variant_self_killed_idle`,
the other completes via runner-idle-termination. With Option A (out-of-
process router sidecar, decoupling the publisher from the peer's
subscriber routing thread) this synthetic test should pass cleanly.

**Why the jitter is in the subscriber task and not in the publisher**:
The cross-WiFi T16.10b evidence pointed at the receive-side routing
thread parking (Zenoh's inbound buffer fills before the subscriber
drains it, then `publisher.put().await` on the remote peer's CC=Block
publisher parks waiting for the receiver to make space). Subscriber-side
jitter directly reproduces that chain. Publisher-side jitter would
simulate a different failure mode (writer-thread stall) that the gate
already absorbs cleanly.

**Limitations**: the synthetic test is intentionally more aggressive
than realistic WiFi (full subscriber-task sleeps stall ALL recv
processing whereas real WiFi loss adds 10-100 ms link retransmits
without halting the routing thread entirely). A passing wired-LAN
re-run of the cross-WiFi matrix on the T16.10d binary is the
authoritative cross-WiFi check; this synthetic test is for designing
and validating fixes BEFORE that real-network re-run.

## T9.5 — `--multicast-interface <ipv4>`

Pins Zenoh's multicast scouting to a specific local IPv4 interface
by setting `scouting/multicast/interface` to the supplied IP string.
Default behaviour (unset) leaves Zenoh at its `"auto"` interface
selection.

### Why this exists (multi-NIC Windows pathology)

On a Windows host with both Ethernet AND WiFi active on the same
subnet (e.g. both NICs holding 192.168.1.x addresses on the same
LAN), Zenoh's `"auto"` pick can land on a different NIC on each
peer. The peers then never observe each other's HELLO traffic and
scouting silently fails — multicast tests at the same group/port
between the same two hosts pass cleanly with PowerShell `socket`,
exonerating the network. Pinning `scouting/multicast/interface` to
the operator-chosen NIC's IPv4 address makes both peers agree on
the multicast NIC unconditionally.

### Wired via the runner's T9.5 `--variant-arg`

The flag is supplied per-machine through the runner's
`--variant-arg` passthrough (see `runner/CUSTOM.md` "Per-variant
CLI overrides (T9.5)"). PowerShell invocation:

```powershell
# alice (Ethernet 192.168.1.68)
target\release\runner.exe --name alice `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg zenoh.multicast_interface=192.168.1.68

# bob (Ethernet 192.168.1.102)
target\release\runner.exe --name bob `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg zenoh.multicast_interface=192.168.1.102
```

The variant name in the `--variant-arg` key (`zenoh` above) must
match each `[[variant]].name` post-template-resolution. For
fixtures whose variant entry is named explicitly (e.g.
`zenoh-1000x100hz-qos3-repro`), use that full name.

### Validation

- Bare IPv4 only. CIDR (`192.168.1.68/24`), hostnames, and IPv6
  literals are rejected at variant startup with a clear error.
- Empty value is treated as missing and the variant falls back to
  `"auto"`.

### Startup confirmation

On every connect the variant emits exactly one stderr line so the
operator can confirm the pin took:

```
[zenoh] multicast interface: 192.168.1.68 (pinned via --multicast-interface)
```

Or, when unset:

```
[zenoh] multicast interface: auto
```

### Out of scope

- Per-NIC interface selection by name (e.g. `eth0`). Zenoh 1.9's
  config accepts only an IPv4 address string for this key; passing a
  device name fails Zenoh's parser. If a future Zenoh release accepts
  names, extend the parser.
- IPv6. Unsupported by the current implementation; revisit when
  there's a concrete dual-stack use case.

### T9.5a (2026-05-25): the `--variant-arg` selector is now a glob

The runner-side T9.5a change widens `--variant-arg <selector>.<key>=<value>`
so the `<selector>` is glob-matched against `[[variant]].name`. The user-
facing impact for the Zenoh variant: `configs/two-runner-zenoh-all.toml`
expands its `zenoh-base` template into 22 variants named
`zenoh-1000x100hz-scalar`, `zenoh-1000x100hz-block`, …, `zenoh-max`. Use
the glob `'zenoh-*'` to apply one multicast-interface pin across the
whole family rather than typing 22 literal selectors:

```powershell
# alice (Ethernet 192.168.1.68)
target\release\runner.exe --name alice `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg 'zenoh-*.multicast_interface=192.168.1.68'

# bob (Ethernet 192.168.1.102)
target\release\runner.exe --name bob `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg 'zenoh-*.multicast_interface=192.168.1.102'
```

**Single-quote the selector** so PowerShell does not expand `*` against
the local filesystem before passing the arg through. The earlier T9.5
PowerShell incantation in this section (`zenoh.multicast_interface=...`)
would silently apply to nothing on this config because no variant is
literally named `zenoh` post-template expansion. Keep the unquoted-literal
form only for configs whose `[[variant]].name` is exactly `zenoh`.

## T9.5c — explicit Multi-mode peering via `--zenoh-peer-tcp-base-port`

Multi mode now derives explicit Zenoh `listen/endpoints` +
`connect/endpoints` from the runner-injected `--peers` map, so the
data plane no longer depends on multicast scouting being functional.
T16.20's diagnostic smoke proved empirically that on the user's
two-Windows-host WiFi setup, Zenoh's multicast scouting **silently
fails** (peers' `session.info().peers_zid()` stays empty for the full
session) — even with `scouting.multicast.interface` pinned via the
T9.5 flag, even though the raw UDP multicast frames at
`224.0.0.224:7446` traverse the link cleanly per the PowerShell
multicast sanity test. The probable root cause is Windows multi-NIC
multicast group-join asymmetry (the kernel only delivers inbound
multicast on the exact interface the socket joined the group from,
which on a multi-NIC Windows host is often different from the
outbound interface the auto-pick chooses). Pinning the multicast
interface helped T9.5 in some setups but did not close the case for
this user's hardware, so T9.5c bypasses scouting for the data plane
entirely.

### The flag

`--zenoh-peer-tcp-base-port <u16>` on `ZenohArgs`. Default 7447
(Zenoh's standard TCP port). Operator override via the runner's T9.5a
glob channel:

```powershell
target\release\runner.exe --name alice `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg 'zenoh-*.zenoh_peer_tcp_base_port=9000'
```

Zero is rejected at parse time. The non-zero range is `1..=65535`;
`base_port + index > u16::MAX` errors out with a clear
"pick a lower base port" message rather than silently clamping
(clamping would produce same-port collisions across peers).

### Derivation rule

Identical convention to the existing Single-mode sidecar peering
(`zenoh.rs::Variant::connect` lines 1124-1160), but applied to the
in-process Multi-mode session:

- `self_index` = position of `runner_name` in the alphabetically
  sorted `--peers` name list (matches `derive_runner_index`).
- `self_port = base_port + self_index`.
- For each peer `(name, host)` where `name != runner_name`:
  `peer_port = base_port + peer_index` (peer's position in the same
  sorted list).
- `listen/endpoints = ["tcp/0.0.0.0:<self_port>"]` — applied when the
  operator did NOT pass `--zenoh-listen` (operator override wins).
- `connect/endpoints = ["tcp/<peer_host>:<peer_port>", ...]` —
  applied unconditionally (independent of the listen override).

Both peers compute the same map from the same sorted `--peers` list,
so the `(listen, connect)` ports match symmetrically without any
extra coordination.

### Solo runs

`peer_pairs` empty OR the only entry is self → no derived listen,
no connect endpoints. The unit test `loopback_full_protocol` and the
`zenoh_bridge_stress_10000_messages` ignored test both exercise this
path; multicast scouting handles same-process loopback unchanged.

### Multicast scouting stays enabled

T9.5c does **not** disable `scouting/multicast/enabled` or
`scouting/gossip/enabled`. The whole point is ADDITIVE explicit
peering: when scouting works it's harmless redundancy; when it
doesn't (the user's WiFi case) the explicit endpoints carry data.
The T16.10d `autoconnect_strategy = "greater-zid"` setting is
preserved verbatim and explicitly regression-tested.

### Startup confirmation

`build_zenoh_config` emits exactly one stderr line per session open,
analogous to T9.5's `[zenoh] multicast interface: ...`:

```
[zenoh] explicit peering: listen=tcp/0.0.0.0:7447 connect=[tcp/127.0.0.1:7448]
```

…or when solo:

```
[zenoh] explicit peering: solo (no --peers entries beyond self)
```

Operators grep for this in captured variant stderr to confirm the
feature took.

### What the user runs on the cross-machine WiFi setup

No `--variant-arg` needed at all — the default `base_port = 7447`
is the path of least surprise on a fresh setup:

```powershell
# alice (192.168.1.68)
target\release\runner.exe --name alice `
  --config configs\two-runner-zenoh-all.toml

# bob (192.168.1.102)
target\release\runner.exe --name bob `
  --config configs\two-runner-zenoh-all.toml
```

If TCP/7447-7448 are reserved by other infrastructure, override per
the T9.5a glob:

```powershell
target\release\runner.exe --name alice `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg 'zenoh-*.zenoh_peer_tcp_base_port=20447'
```

Closes T16.10c (the cross-WiFi data-plane failure mode) by sidestepping
the multicast-scouting layer entirely. Single-mode peering (T14.9b)
is unchanged; it already had its own explicit peer mesh.

## T9.5e — `--zenoh-express <true|false>` (publisher express / immediate send)

A configurable boolean arg on `ZenohArgs`, **default `false`**. When
`true`, every publisher the variant declares is built with Zenoh's
*express* policy — `PublisherBuilder::express(true)` — alongside the
existing `.congestion_control(cc)`. When `false` (the default) the
builder is left at Zenoh's default batching behaviour, so existing
configs and benchmark numbers are byte-for-byte reproducible.

### What express does

Zenoh batches small outgoing samples by default and flushes the batch
on a timer / size threshold. At **low publish rates** with small
messages (e.g. the 10 Hz scalar/block fixtures), a partial batch sits
waiting for the next sample, adding tens of milliseconds of latency
before the bytes leave the host. Setting `express(true)` tells Zenoh to
send each sample **immediately** (no batching), which collapses that
low-rate latency. The benchmark observed Zenoh latency rising to
~50–90 ms (scalar/block) at 10 Hz with batching on, vs ~4.4 ms for the
Custom-UDP variant; express is the targeted fix for that gap.

### Tradeoff (why it is opt-in, not forced on)

Express is a latency/throughput tradeoff. Disabling batching can **cost
aggregate throughput at very high publish rates** (more, smaller
network writes instead of fewer, fuller batches). It therefore defaults
off; operators enable it only for the low-rate scenarios where the
batch-flush latency dominates.

### Where it is applied

Both publisher-declare sites in `src/zenoh.rs` apply the flag:

1. The `connect` **pre-declare** path (the `JoinSet` that declares
   `bench/0..values_per_tick-1` for both `Drop` and `Block`
   congestion-control caches) — `.congestion_control(cc).express(express)`.
2. The `publisher_task` **lazy-declare fallback** (first-sight declare
   for keys outside the pre-declared set) — same builder chain. The
   flag is carried on `PublisherState.express` so the lazy path matches
   the pre-declared publishers.

Exact API (zenoh 1.9.0): `PublisherBuilder::express(is_express: bool)`
from the `QoSBuilderTrait` (the same trait that provides
`congestion_control`), returning the builder for chaining. It is a
publisher-builder method, not a per-`put()` option, so the policy is
fixed once at declare time for every sample on that publisher.

### How the operator enables it

Through the runner's T9.5a `--variant-arg` glob passthrough (the runner
forwards `zenoh_express=true` as the two tokens `--zenoh-express true`):

```powershell
# both peers — enable express across the whole zenoh-* family
target\release\runner.exe --name alice `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg 'zenoh-*.zenoh_express=true'

target\release\runner.exe --name bob `
  --config configs\two-runner-zenoh-all.toml `
  --variant-arg 'zenoh-*.zenoh_express=true'
```

Single-quote the `zenoh-*` selector so PowerShell does not expand `*`
against the filesystem. Omit the `--variant-arg` entirely to keep the
default (batching on, today's behaviour).
