# Zenoh Variant — Custom Instructions

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
  - `zenoh` — the pub/sub transport (use latest stable, currently ~1.9)
  - `tokio` (`rt-multi-thread`, `sync`, `macros`, `time`) — the runtime
    owned by `ZenohVariant` for the publish/receive bridge (see Design
    Guidance below). Pulled in directly even though Zenoh already
    depends on it, so the variant has a stable `tokio::runtime::Runtime`
    handle and `tokio::sync::mpsc` API surface.
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
