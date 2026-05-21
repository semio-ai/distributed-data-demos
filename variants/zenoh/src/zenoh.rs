use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinSet;
use zenoh::handlers::{FifoChannel, FifoChannelHandler};
use zenoh::pubsub::{Publisher, Subscriber};
use zenoh::qos::CongestionControl;
use zenoh::sample::Sample;

use variant_base::types::{Qos, ReceivedUpdate, ThreadingMode};
use variant_base::variant_trait::Variant;

/// Internal record of an observed peer EOT marker (T15.8 historical).
///
/// The on-wire EOT exchange was retired in T15.8; this struct is kept
/// so the receive-side machinery that still decodes EOT publications
/// from pre-T15.8 peers compiles without churn. The decoded markers
/// are no longer surfaced to the driver.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct PeerEot {
    writer: String,
    eot_id: u64,
}

/// Helper: emit a `[zenoh-trace]` line on stderr if debug-trace is enabled.
/// The macro is a no-op when `enabled` is false so the hot path stays cheap.
/// Flushes stderr after every line so a hang mid-call still leaves the
/// preceding ENTER on disk for diagnosis.
macro_rules! trace_if {
    ($enabled:expr, $($arg:tt)*) => {
        if $enabled {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "[zenoh-trace] {}", format_args!($($arg)*));
            let _ = stderr.flush();
        }
    };
}

/// Same as `trace_if!` but always emits (used for a trace block where the
/// caller already gated on the debug flag).
macro_rules! trace_now {
    ($($arg:tt)*) => {{
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "[zenoh-trace] {}", format_args!($($arg)*));
        let _ = stderr.flush();
    }};
}

/// Converts a Zenoh ZResult error into an anyhow error.
fn zenoh_err(e: zenoh::Error) -> anyhow::Error {
    anyhow::anyhow!("{}", e)
}

/// Compact binary codec for messages sent over Zenoh.
///
/// Layout (little-endian):
///   - writer_len: u16
///   - writer: [u8; writer_len]
///   - seq: u64
///   - qos: u8
///   - path_len: u16
///   - path: [u8; path_len]
///   - payload: [u8; remaining]
struct MessageCodec;

/// Capacity reserved per encode in the thread-local `BytesMut`. The pool
/// is split-and-freezed per call so a `BytesMut::reserve` only triggers
/// when the rolling window of in-flight `Bytes` views exhausts the
/// underlying allocation. Sized to amortize the syscall to a few times
/// per second under the heaviest 1000 vps × 100 Hz fixture.
const ENCODE_CHUNK_BYTES: usize = 64 * 1024;

thread_local! {
    /// Per-(main-)thread reusable encode buffer. Each `encode` call
    /// reserves enough room for one message, writes the bytes, and
    /// `split_to(...).freeze()`s a refcounted `Bytes` view of those
    /// bytes -- the remaining capacity in the BytesMut is reused on the
    /// next call. Once the rolling capacity is exhausted, a single
    /// `reserve(ENCODE_CHUNK_BYTES)` takes over the next chunk.
    ///
    /// Why a thread-local rather than a per-publisher-task buffer: in
    /// the T10.2b bridge architecture, encoding happens on the variant's
    /// main thread (so the publisher task only spends time on the put,
    /// not on the codec). The thread-local matches that division of
    /// labor and avoids forcing the publisher task to re-encode on a
    /// runtime worker.
    static ENCODE_BUF: RefCell<BytesMut> = RefCell::new(BytesMut::with_capacity(ENCODE_CHUNK_BYTES));
}

impl MessageCodec {
    /// Encode one outbound message into the thread-local `BytesMut` and
    /// return a frozen `Bytes` view. The underlying allocation is shared
    /// with the BytesMut so `bytes::Bytes -> ZBytes` is zero-copy on the
    /// way down to `publisher.put`.
    fn encode(writer: &str, seq: u64, qos: Qos, path: &str, payload: &[u8]) -> Bytes {
        let writer_bytes = writer.as_bytes();
        let path_bytes = path.as_bytes();
        let total = 2 + writer_bytes.len() + 8 + 1 + 2 + path_bytes.len() + payload.len();

        ENCODE_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            // Ensure we can write a contiguous `total` bytes. `BytesMut::reserve`
            // only allocates when the remaining capacity is insufficient.
            if buf.capacity() < total {
                // Grab a fresh ENCODE_CHUNK_BYTES chunk so we amortize
                // allocator traffic across many encodes. `reserve` requests
                // additional capacity beyond `len()`, so this gives us
                // ENCODE_CHUNK_BYTES headroom even if the previous chunk
                // was just frozen out.
                buf.reserve(ENCODE_CHUNK_BYTES.max(total));
            }
            buf.put_u16_le(writer_bytes.len() as u16);
            buf.put_slice(writer_bytes);
            buf.put_u64_le(seq);
            buf.put_u8(qos.as_int());
            buf.put_u16_le(path_bytes.len() as u16);
            buf.put_slice(path_bytes);
            buf.put_slice(payload);
            // Split off exactly the bytes we just wrote and freeze them
            // into a refcounted `Bytes` view. The BytesMut keeps the
            // remaining capacity for the next encode.
            buf.split_to(total).freeze()
        })
    }

    fn decode(data: &[u8]) -> Result<ReceivedUpdate> {
        let mut pos = 0;

        anyhow::ensure!(data.len() >= 2, "message too short for writer_len");
        let writer_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        anyhow::ensure!(
            data.len() >= pos + writer_len,
            "message too short for writer"
        );
        let writer =
            std::str::from_utf8(&data[pos..pos + writer_len]).context("invalid writer UTF-8")?;
        pos += writer_len;

        anyhow::ensure!(data.len() >= pos + 8, "message too short for seq");
        let seq = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        anyhow::ensure!(data.len() > pos, "message too short for qos");
        let qos_val = data[pos];
        let qos = Qos::from_int(qos_val)
            .ok_or_else(|| anyhow::anyhow!("invalid qos value: {}", qos_val))?;
        pos += 1;

        anyhow::ensure!(data.len() >= pos + 2, "message too short for path_len");
        let path_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        anyhow::ensure!(data.len() >= pos + path_len, "message too short for path");
        let path = std::str::from_utf8(&data[pos..pos + path_len]).context("invalid path UTF-8")?;
        pos += path_len;

        let payload = data[pos..].to_vec();

        Ok(ReceivedUpdate {
            writer: writer.to_string(),
            seq,
            path: path.to_string(),
            qos,
            payload,
        })
    }
}

/// Wildcard key expression the subscriber listens on. All published keys
/// derived by [`path_to_key`] must match this expression.
const SUBSCRIBER_WILDCARD: &str = "bench/**";

/// Key prefix for end-of-test (EOT) markers. Each writer publishes its EOT
/// to `bench/__eot__/<writer-runner-name>` once per spawn. See
/// `metak-shared/api-contracts/eot-protocol.md` "Zenoh" section.
///
/// The wildcard [`SUBSCRIBER_WILDCARD`] (`bench/**`) intersects this prefix
/// too, but EOT samples are filtered by a separate dedicated wildcard
/// subscriber so the data subscriber path stays unaffected.
const EOT_KEY_PREFIX: &str = "bench/__eot__/";

/// Wildcard the EOT subscriber listens on. Matches every key of the form
/// `bench/__eot__/<writer>`.
const EOT_WILDCARD: &str = "bench/__eot__/**";

/// Construct the per-writer EOT key from a runner name.
#[allow(dead_code)]
fn eot_key_for(writer: &str) -> String {
    format!("{}{}", EOT_KEY_PREFIX, writer)
}

/// Extract the writer name from an EOT sample key. Returns `None` if the
/// key does not start with [`EOT_KEY_PREFIX`] or has no writer suffix.
fn writer_from_eot_key(key: &str) -> Option<&str> {
    let suffix = key.strip_prefix(EOT_KEY_PREFIX)?;
    if suffix.is_empty() {
        None
    } else {
        Some(suffix)
    }
}

/// Encode an `eot_id` as 8 big-endian bytes per the EOT contract.
#[allow(dead_code)]
fn encode_eot_payload(eot_id: u64) -> [u8; 8] {
    eot_id.to_be_bytes()
}

/// Decode an 8-byte big-endian EOT payload into an `eot_id`. Returns
/// `None` if the payload is the wrong length.
fn decode_eot_payload(data: &[u8]) -> Option<u64> {
    if data.len() != 8 {
        return None;
    }
    Some(u64::from_be_bytes(data.try_into().ok()?))
}

/// Convert a workload path (e.g. `"/bench/0"`) to a Zenoh key expression
/// (e.g. `"bench/0"`).
///
/// Workload paths arrive with a leading `/` from
/// `variant_base::workload::ScalarFlood`, but Zenoh key expressions cannot
/// start with `/`. The `bench/` prefix is already part of the path and must
/// NOT be re-added (the original code double-prefixed to `bench/bench/N` —
/// see DECISIONS.md D7). The result must be matched by [`SUBSCRIBER_WILDCARD`].
fn path_to_key(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

/// T17.8: Key prefix for the **application-level credit/ack side channel**
/// used to peer-coordinate back-pressure at QoS 3 / QoS 4. Each receiver
/// publishes the highest `seq` it has decoded per writer to
/// `bench/__ack__/<receiver>/<writer>` with a single u64 big-endian
/// payload. Writers subscribe to `bench/__ack__/*/<self>` and gate
/// outbound QoS 3/4 publishes on the per-peer ack watermark falling
/// behind by no more than [`QOS_STRICT_WINDOW`].
///
/// Wildcard [`SUBSCRIBER_WILDCARD`] (`bench/**`) intersects this prefix,
/// but ack samples are filtered by a dedicated wildcard subscriber so
/// the data subscriber path is unaffected (same shape as EOT).
const ACK_KEY_PREFIX: &str = "bench/__ack__/";

/// Wildcard the per-writer ack subscriber listens on. Matches every key
/// of the form `bench/__ack__/<receiver>/<self_runner>`. The variant
/// composes this at `connect` time by substituting the own runner name
/// for the trailing wildcard segment.
const ACK_WILDCARD_FOR_SELF_PREFIX: &str = "bench/__ack__/*/";

/// T17.8: Strict-QoS application-level credit window in messages.
///
/// On the QoS 3 / QoS 4 publish path the variant tracks
/// `next_seq - peer_acks[p]` for every known peer `p` and blocks the
/// caller (per DESIGN.md § 6.5) once any peer falls behind by more than
/// this many messages. The smaller this value, the tighter the
/// writer/receiver coupling and the less in-flight tail at end-of-test.
///
/// Sized at one tick-batch of the heaviest reproducer (1000 vpt × 10 Hz
/// = 1000 messages per tick) so the writer always reaches the gate
/// after roughly one tick of un-acked work, while still being large
/// enough to absorb a single 50 ms ack-cycle hiccup at the highest
/// supported workload (100 K msg/s × 0.050 s = 5 000 in-flight). At
/// localhost-typical sub-millisecond ack latency the gate is normally
/// a no-op; under saturation it caps in-flight at this value and lets
/// the receiver catch up before authorising the next batch.
///
/// Earlier iteration: 8192 (matched bridge channel capacity). Lowered
/// to 2048 after the T17.8 local reproducer showed Zenoh's internal
/// CC=Block back-pressure still landed asymmetric drops when the
/// application credit window was wide enough that the data publisher
/// outran the receiver's drain by multiple ticks worth of in-flight.
const QOS_STRICT_WINDOW: u64 = 2048;

/// T19.X: Per-subscriber FIFO channel capacity (overrides Zenoh's
/// default of 256). The default subscriber handler (`FifoChannel`)
/// **blocks the Zenoh routing thread** when full, not drops -- and
/// Zenoh's CC=Block back-pressure path on the writer side parks the
/// publisher's `put().await` once that back-pressure reaches it.
/// At symmetric high-rate QoS 3/4 (1000 vpt × 10 Hz on localhost
/// loopback was the smallest reliable reproducer) the 256-slot FIFO
/// saturates within a few milliseconds, the routing thread parks
/// before `subscriber_task` can drain, and both peers' publishers
/// then wedge on `put().await` while their own subscribers are
/// already parked -- a symmetric deadlock that the T15.11 watchdog
/// terminates as `variant_self_killed_idle`.
///
/// Sized at 131 072 samples so the FIFO never realistically fills
/// (1000 vpt × 100 Hz × 1 s = 100 K samples per second; one full
/// second of unservicing peer is the worst burst the bridge mpsc
/// can plausibly hide). The downstream `recv_tx` bridge (`16384`
/// capacity, `try_send` semantics with drop-on-full) absorbs the
/// burst back-pressure on the variant side; this constant raises
/// the cap on the Zenoh side so the routing thread itself never
/// parks.
///
/// Memory cost: 131 072 × `Sample` (typ. ~64 B + payload) ~= 8 MiB
/// + payload data per subscriber. With three subscribers per
/// variant (data, EOT, ack) the per-spawn overhead is well within
/// the budget recorded by `Sidecar::spawn` and the digest soft
/// ceiling.
const SUBSCRIBER_FIFO_CAPACITY: usize = 131_072;

/// T17.8: How often the ack emitter task publishes per-writer ack
/// watermarks. The trade-off: shorter interval = lower steady-state
/// publish latency (driver thread blocks for at most one interval when
/// the window is exhausted) but more side-channel traffic. 25 ms is a
/// 40 Hz heartbeat which keeps the side-channel cost negligible
/// (2 peers × 40 Hz × ≤2 writers = ≤160 ack-publish/s) while keeping
/// the gate latency under typical Zenoh data-path RTT on localhost.
///
/// Earlier iteration: 50 ms. Halved after the local reproducer showed
/// gate-induced bursting (writer publishes one ack-cycle's worth then
/// stalls until the next ack lands) was the dominant latency
/// contributor at high vpt.
const ACK_EMIT_INTERVAL: Duration = Duration::from_millis(25);

/// T17.8: Per-iteration condvar timeout for the window gate. The driver
/// thread parks here while waiting for ack progress. Bounded so the
/// gate can re-check `peer_acks` even if a notify is somehow missed
/// (defensive), and so the watchdog's coarse 1 Hz progress sampler can
/// observe forward motion within one tick of the actual ack arrival.
const WINDOW_GATE_WAKE_INTERVAL: Duration = Duration::from_millis(100);

/// T17.8: Compose the per-writer ack key the receiver publishes to.
/// Format: `bench/__ack__/<receiver>/<writer>`.
fn ack_key_for(receiver: &str, writer: &str) -> String {
    format!("{}{}/{}", ACK_KEY_PREFIX, receiver, writer)
}

/// T17.8: Compose the wildcard the writer subscribes to so it sees acks
/// directed at itself from every peer. Format:
/// `bench/__ack__/*/<self_runner>`.
fn ack_wildcard_for_self(self_runner: &str) -> String {
    format!("{}{}", ACK_WILDCARD_FOR_SELF_PREFIX, self_runner)
}

/// T17.8: Extract `(receiver, writer)` from an ack key. Returns `None`
/// if the key does not match the `bench/__ack__/<receiver>/<writer>`
/// shape (including missing or empty segments).
fn parse_ack_key(key: &str) -> Option<(&str, &str)> {
    let suffix = key.strip_prefix(ACK_KEY_PREFIX)?;
    let (receiver, writer) = suffix.split_once('/')?;
    if receiver.is_empty() || writer.is_empty() {
        return None;
    }
    // The writer segment must not contain further '/'; the wildcard
    // shape we use is a single `*` segment, not `**`.
    if writer.contains('/') {
        return None;
    }
    Some((receiver, writer))
}

/// T17.8: Encode an ack watermark as 8 big-endian bytes. Mirrors the
/// EOT payload encoding so the codec choice across both side-channels
/// is uniform.
fn encode_ack_payload(max_seq: u64) -> [u8; 8] {
    max_seq.to_be_bytes()
}

/// T17.8: Decode an 8-byte big-endian ack payload. Returns `None` on
/// wrong length so a corrupt or future-format ack is dropped instead
/// of panicking.
fn decode_ack_payload(data: &[u8]) -> Option<u64> {
    if data.len() != 8 {
        return None;
    }
    Some(u64::from_be_bytes(data.try_into().ok()?))
}

/// T17.8: Shared accounting state used by [`subscriber_task`] (writer
/// side: increments per-writer max_seq_seen on QoS 3/4 receives) and
/// [`ack_emitter_task`] (reads the snapshot periodically and publishes
/// one Zenoh sample per known remote writer).
///
/// The map is keyed by remote writer name; the value is the highest
/// `seq` decoded from a QoS 3 / QoS 4 sample where `writer != self`.
/// Self-publishes are NEVER recorded -- the loopback case must not
/// generate self-acks because it would create a spurious "peer" entry
/// in [`WindowState::peer_acks`] and cause solo runs to gate on their
/// own progress.
///
/// Lock granularity: a single `std::sync::Mutex` is sufficient because
/// the subscriber_task writes once per QoS 3/4 receive (small critical
/// section: one HashMap upsert) and the ack_emitter_task reads at most
/// 20 Hz (one snapshot clone every [`ACK_EMIT_INTERVAL`]). Contention
/// is dominated by the receiver's write rate, which is bounded by the
/// receiver-side throughput.
#[derive(Default)]
struct RecvAccounting {
    /// `writer -> highest seq decoded so far at QoS 3 or QoS 4`.
    /// Best-effort QoS 1/2 receives are NOT tracked here; the strict-
    /// delivery window contract only applies to reliable QoS.
    per_writer_max_seq: HashMap<String, u64>,
}

/// T17.8: Writer-side state shared between the driver thread (the
/// strict-QoS publish gate) and the `ack_subscriber_task` (which feeds
/// new per-peer watermarks in). The condvar wakes the gate whenever a
/// watermark advances.
///
/// The map is keyed by the remote *receiver* name (i.e. the peer that
/// sent the ack). Each entry records the highest `seq` that peer has
/// confirmed decoding from us at QoS 3/4. Entries are pre-seeded at
/// `connect` time from the runner-injected `--peers` list with
/// watermark 0, so the gate enforces the credit window from the
/// FIRST publish rather than only after the first real ack arrives.
/// Without the pre-seed, a fresh spawn could burst-publish thousands
/// of messages into the bridge before bob's first ack lands, filling
/// the bridge to overflow when disconnect runs and producing a real
/// end-of-test tail drop.
///
/// **Empty map** means the variant has no known peers (a solo /
/// loopback configuration); the gate is then a no-op so the writer
/// is unthrottled.
#[derive(Default)]
struct WindowState {
    /// `receiver -> highest ack watermark received from that peer`.
    /// Monotonically non-decreasing per peer (the ack subscriber
    /// drops samples that would regress a peer's watermark, which
    /// can happen if Zenoh delivers the ack samples out of order).
    peer_acks: HashMap<String, u64>,
    /// Set to `true` during `disconnect` so any thread blocked in
    /// the gate condvar wakes up and bails out instead of waiting
    /// for a watermark that will never arrive.
    shutting_down: bool,
}

/// T17.8: Container co-locating [`WindowState`] and its condvar. Held
/// behind an `Arc` so the driver thread (sync, calls
/// [`WindowState::wait_for_window`]) and the ack subscriber task
/// (async, calls [`WindowState::apply_ack`]) share a single
/// notification channel.
struct WindowGate {
    state: Mutex<WindowState>,
    cv: Condvar,
}

impl WindowGate {
    /// Test-only constructor: no peers known, gate is a no-op. The
    /// production path always goes through
    /// [`WindowGate::with_expected_peers`] (the runner-injected
    /// peer list seeds the gate at watermark 0 from the first
    /// publish on).
    #[cfg(test)]
    fn new() -> Self {
        Self {
            state: Mutex::new(WindowState::default()),
            cv: Condvar::new(),
        }
    }

    /// T17.8: Construct with the expected peer set pre-seeded at
    /// watermark 0. Called from `connect(Multi)` with the
    /// runner-injected `--peers` list (excluding self). The gate
    /// then enforces the credit window from the first publish,
    /// rather than burst-allowing unbounded writes until the first
    /// real ack from each peer lands.
    fn with_expected_peers(peers: &[String]) -> Self {
        let mut state = WindowState::default();
        for peer in peers {
            state.peer_acks.insert(peer.clone(), 0);
        }
        Self {
            state: Mutex::new(state),
            cv: Condvar::new(),
        }
    }

    /// Update the watermark for a single peer. No-op if the new value
    /// would regress the peer's watermark (Zenoh delivers
    /// CongestionControl::Drop samples in arrival order, but a
    /// belated ack from before a reconnect could still surface
    /// stale). Notifies all waiters on any advance because multiple
    /// publish threads could in principle be waiting on the gate
    /// (today the variant only has one driver thread, but
    /// `notify_all` is robust if that changes).
    fn apply_ack(&self, peer: &str, max_seq: u64) {
        if let Ok(mut s) = self.state.lock() {
            let entry = s.peer_acks.entry(peer.to_string()).or_insert(0);
            if max_seq > *entry {
                *entry = max_seq;
                drop(s);
                self.cv.notify_all();
            }
        }
    }

    /// Signal all waiters to abort. Called from `disconnect` so a
    /// driver thread parked here unblocks cleanly during teardown.
    fn shutdown(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.shutting_down = true;
            drop(s);
            self.cv.notify_all();
        }
    }

    /// Block the calling (sync) thread until `next_seq` falls within
    /// [`QOS_STRICT_WINDOW`] of every known peer's ack watermark, or
    /// the gate has been shut down.
    ///
    /// The first time a peer publishes any ack at all, it joins the
    /// gated set; until then publishes proceed unthrottled (this is
    /// the loopback / solo case). If NO peer has ever acked, the gate
    /// is a no-op.
    ///
    /// Returns `Ok(())` once the window has space, or
    /// `Err` only if the gate is shut down before progress is
    /// observed. The error variant is `anyhow::Error` so the variant's
    /// publish method can propagate it.
    fn wait_for_window(&self, next_seq: u64) -> Result<()> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("window gate mutex poisoned"))?;
        loop {
            if guard.shutting_down {
                anyhow::bail!("window gate: variant disconnecting");
            }
            // Compute minimum ack across known peers. An empty map
            // means "no peer has ever acked" -> no gating; treat as
            // unbounded credit. This is the loopback / solo run
            // behaviour and the warm-up phase before the first ack
            // arrives.
            let min_ack: Option<u64> = guard.peer_acks.values().min().copied();
            match min_ack {
                None => return Ok(()),
                Some(min_ack) => {
                    if next_seq <= min_ack + QOS_STRICT_WINDOW {
                        return Ok(());
                    }
                }
            }
            // Window exhausted: park with a periodic wake so a missed
            // notify is recoverable and so the watchdog's 1 Hz sampler
            // can observe forward motion within one tick of the actual
            // ack arrival.
            let (g, _) = self
                .cv
                .wait_timeout(guard, WINDOW_GATE_WAKE_INTERVAL)
                .map_err(|_| anyhow::anyhow!("window gate cv poisoned"))?;
            guard = g;
        }
    }
}

/// Default capacity for the publish-side bounded channel.
///
/// T17.8: bumped from 1024 to 16384 so the application-level credit
/// window protocol (see [`WindowGate`]) is the **binding** back-
/// pressure surface at QoS 3 / QoS 4, not the bridge channel. With
/// the bridge at 1024 (the pre-T17.8 size), the writer would fill
/// the bridge after one tick at the standard 1000-vpt workload and
/// park on `blocking_send` long before the credit window engaged,
/// re-creating the exact failure mode the T17.8 protocol was meant
/// to eliminate. 16384 is comfortably larger than
/// [`QOS_STRICT_WINDOW`] so the credit gate parks the driver thread
/// on the condvar (the controlled back-pressure path) before the
/// bridge fills.
///
/// At QoS 1 / QoS 2 the bridge is still the back-pressure surface
/// (writer's `try_send` returns `Full` -> `Ok(false)` ->
/// `backpressure_skipped`). The deeper queue marginally delays the
/// first `Ok(false)` under a momentary burst but does not break
/// the contract -- the QoS 1/2 path produces `backpressure_skipped`
/// as soon as the channel fills, regardless of capacity.
const PUBLISH_CHANNEL_CAPACITY: usize = 16384;

/// Recover `--values-per-tick` from the variant process's CLI args.
///
/// The `Variant` trait only hands `runner` and the trailing `extra` args
/// to `Variant::new`; `--values-per-tick` is a top-level arg on
/// `variant_base::cli::CliArgs` that is *not* propagated into `extra`.
/// To pre-declare publishers from the workload's path set during
/// `connect` (T-zenoh.1), we re-parse the same arg from
/// `std::env::args` -- the same buffer clap reads. The variant is
/// therefore reading exactly what the runner spawned it with.
///
/// Returns `None` if the arg is absent or unparseable; the caller
/// falls back to lazy declare in that case (e.g. unit tests that
/// construct `ZenohVariant::new` without a real runner spawn, or
/// future workloads with non-`scalar-flood` path schemes).
fn values_per_tick_from_env() -> Option<u32> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--values-per-tick" {
            return args.next().and_then(|v| v.parse::<u32>().ok());
        }
        if let Some(stripped) = arg.strip_prefix("--values-per-tick=") {
            return stripped.parse::<u32>().ok();
        }
    }
    None
}

/// Default capacity for the receive-side bounded channel. Sized for the
/// same heavy-fanout workload; samples that don't fit (channel full) are
/// dropped from the bridge layer with a periodic stderr warning when
/// `--debug-trace` is on. Dropping is acceptable for benchmark purposes —
/// the JSONL receive log will simply show fewer matches than writes, which
/// is exactly what the analysis tool measures.
const RECEIVE_CHANNEL_CAPACITY: usize = 16384;

/// T16.5: maximum number of in-flight `publisher.put(...).await` futures
/// the publisher task allows to run concurrently. Each pending put holds
/// one permit from a `Semaphore`; the task acquires a permit before
/// spawning the put as an independent tokio task and releases it on
/// completion.
///
/// **Scope (T16.10)**: this limit applies to the **QoS 1/2 (Drop) path
/// only**. T16.5 originally spawned every put unconditionally, but that
/// broke reliable-ordered delivery for QoS 3/4: concurrent puts on the
/// same key could complete out of order (the receiver's
/// `MessageCodec::decode` then surfaced ~17 000 out-of-order arrivals
/// per direction on the 1000x10hz QoS 3 reproducer). For QoS 3/4 the
/// drain loop now awaits each put inline so the single-task pipeline
/// preserves per-key ordering naturally. The semaphore therefore only
/// gates QoS 1/2 traffic, which has no ordering contract anyway.
///
/// **Why this exists**: prior to T16.5 `publisher_task` drained the
/// outbound mpsc on a single async task and `await`-ed each put inline
/// for *every* QoS. At 1000 distinct keys x 100 Hz that's 100 K msg/s
/// squeezed through one sequential await chain; if even one publisher's
/// `put().await` blocked (CC=Block) or merely paid a route-resolution
/// cost, every subsequent message — *for unrelated keys* — waited
/// behind it. The observed failure pattern was extreme asymmetry: one
/// peer would drain its bridge channel at line rate while the other
/// peer's publisher task stalled, filled its bridge channel within
/// ~20 ms (QoS 1 Drop) or blocked at ~1500 writes (QoS 3 Block), and
/// ultimately tripped the 30 s internal-stall watchdog
/// (`variant_self_killed_idle`). The spawn-per-put fix solved this for
/// QoS 1/2; QoS 3/4 falls back to the original serial drain in T16.10.
///
/// **Why bounded**: unbounded `tokio::spawn` would let memory grow without
/// limit if puts genuinely back up. 4096 is comfortably above the bridge
/// channel capacity (1024), so the steady-state hot path never blocks on
/// the semaphore — the bridge channel is the real backpressure surface
/// for QoS 1/2. The semaphore is just a safety net against pathological
/// queue growth.
const PUBLISH_INFLIGHT_LIMIT: usize = 4096;

/// T16.5: how long `connect()` sleeps after declaring subscribers,
/// pre-declaring publishers, and spawning the bridge tasks but before
/// returning to the driver. This gives Zenoh's peer discovery + key
/// expression interest propagation time to settle for the full
/// `bench/0..N-1` key set before the driver enters `stabilize`/`operate`.
///
/// **Why this exists**: with 1000 distinct keys per tick, the interest
/// declaration from each peer's subscriber must reach the other peer's
/// router/publisher state *before* the first publish, or the publisher
/// has no route for that key and the message is silently dropped at the
/// transport layer. The 1000-path full-matrix run on
/// `logs/same-machine-all-variants-01-20260514_084636/` showed 0.00 %
/// delivery on alice->bob (and reversed on qos4) consistent with the
/// subscriber not having declared interest by the time alice's first
/// tick fired. The stabilize phase exists for exactly this kind of
/// propagation but it runs *after* `connect` returns, and the
/// `stabilize_secs` budget is workload-controlled (often 1-3 s) which
/// proved insufficient for 1000 keys on the failing rig. Adding this
/// fixed in-connect delay guarantees a minimum settle window
/// regardless of fixture configuration.
///
/// Sized to be long enough for 1000 keys to propagate on localhost
/// (empirically a few hundred ms suffices) but short enough not to
/// dominate any production fixture's `connect` budget. The runner's
/// existing `default_timeout_secs` (typically 30-60 s) easily absorbs
/// this.
const CONNECT_PROPAGATION_SETTLE_MS: u64 = 500;

/// Zenoh-specific CLI arguments parsed from the `extra` pass-through args.
pub struct ZenohArgs {
    pub mode: String,
    pub listen: Option<String>,
    /// When true, emit `[zenoh-trace]` lines on stderr for connect/publish
    /// hot path / poll_receive / disconnect. Off by default so production
    /// runs are quiet; enable by passing `--debug-trace` (no value).
    pub debug_trace: bool,
    /// T14.9a: base port for the `zenohd` sidecar's REST plugin in Single
    /// mode. The per-runner port is derived as
    /// `base + runner_index * runner_stride` (runner_stride = 1, matching
    /// T14.18 / T15.10 control ports). Optional: Multi mode does not
    /// spawn a sidecar and silently ignores this arg. Single mode
    /// requires it (T14.9b will surface the derived port to the RPC
    /// client); if absent at `connect(Single)` we fall back to a
    /// reasonable default of 20100 so the operator-facing manual smoke
    /// "spawn variant-zenoh --threading-mode single" path works
    /// without per-spawn TOML wiring.
    pub sidecar_base_port: Option<u16>,
}

impl ZenohArgs {
    /// Parse Zenoh-specific arguments from the extra CLI args.
    pub fn parse(extra: &[String]) -> Result<Self> {
        let mut mode = String::from("peer");
        let mut listen = None;
        let mut debug_trace = false;
        let mut sidecar_base_port: Option<u16> = None;

        let mut i = 0;
        while i < extra.len() {
            match extra[i].as_str() {
                "--zenoh-mode" => {
                    i += 1;
                    anyhow::ensure!(i < extra.len(), "--zenoh-mode requires a value");
                    mode = extra[i].clone();
                }
                "--zenoh-listen" => {
                    i += 1;
                    anyhow::ensure!(i < extra.len(), "--zenoh-listen requires a value");
                    listen = Some(extra[i].clone());
                }
                "--zenoh-sidecar-base-port" => {
                    // T14.9a: base port for the zenohd sidecar's REST
                    // plugin. Parsed as u16; the per-runner port is
                    // derived in connect(Single) via
                    // `sidecar::derive_sidecar_port`.
                    i += 1;
                    anyhow::ensure!(
                        i < extra.len(),
                        "--zenoh-sidecar-base-port requires a value"
                    );
                    let raw = &extra[i];
                    let port: u16 = raw.parse().with_context(|| {
                        format!("invalid --zenoh-sidecar-base-port value '{raw}': expected u16")
                    })?;
                    sidecar_base_port = Some(port);
                }
                "--debug-trace" => {
                    // Boolean flag: no value follows.
                    debug_trace = true;
                }
                other => {
                    // Lenient skip: the runner injects extra args (e.g. --peers)
                    // that Zenoh does not need. Treat any unknown `--<name>` as
                    // a `--name value` pair and skip both tokens; otherwise
                    // skip just the token.
                    if other.starts_with("--") {
                        i += 1;
                    }
                }
            }
            i += 1;
        }

        Ok(Self {
            mode,
            listen,
            debug_trace,
            sidecar_base_port,
        })
    }
}

/// Outbound publish request shuttled from the variant's main thread to the
/// publisher task running on the dedicated tokio runtime.
#[allow(dead_code)]
enum OutboundMessage {
    /// A regular data publish to a workload key.
    Data {
        /// Already-derived Zenoh key (no leading slash, no double prefix —
        /// see [`path_to_key`]).
        key: String,
        /// Already-encoded message body, frozen out of the thread-local
        /// `ENCODE_BUF` (see `MessageCodec::encode`). `bytes::Bytes` is
        /// refcounted so cloning across the channel is cheap, and zenoh's
        /// `From<bytes::Bytes> for ZBytes` impl is zero-copy -- the
        /// publisher task hands the same allocation to `publisher.put`
        /// without re-allocating.
        encoded: Bytes,
        /// Sequence number for diagnostic tracing.
        seq: u64,
        /// QoS level the message was published with. Drives which
        /// publisher cache (Drop vs Block congestion control) the
        /// publisher task selects per T-impl.7.
        qos: Qos,
    },
    /// A one-shot EOT publish to `bench/__eot__/<self_runner>`. The variant
    /// blocks on `done` to confirm the publish has been committed inside
    /// the runtime before `signal_end_of_test` returns.
    Eot {
        /// `bench/__eot__/<self_runner>` key.
        key: String,
        /// 8-byte big-endian `eot_id` payload.
        payload: [u8; 8],
        /// Notification that the put has completed (Ok or error). The
        /// variant's main thread waits on this so it returns the
        /// `eot_id` only after the marker is on the wire.
        done: oneshot::Sender<Result<()>>,
    },
}

/// Shared state held inside the dedicated tokio runtime. Owned by the
/// publisher task; the subscriber task only ever reads from the
/// `Subscriber` it was given at spawn time.
///
/// T-impl.7: two separate publisher caches, one per congestion-control
/// policy, so each QoS path gets its appropriate behaviour:
///
/// - `publishers_drop` (`CongestionControl::Drop`) is used for QoS 1/2
///   (BestEffort, LatestValue). Zenoh drops messages internally if its
///   outgoing queue is full -- which means our `try_publish` returns
///   `Ok(true)` even when Zenoh later drops, and we cannot count those
///   internal drops in `backpressure_skipped`. The honest backpressure
///   signal we *do* surface is the **publish channel full** condition
///   on our own bridge mpsc -- if `try_send` returns `Full`,
///   `try_publish` returns `Ok(false)`. See CUSTOM.md
///   "Backpressure semantics (T-impl.7)" for the full rationale and
///   limitation note.
/// - `publishers_block` (`CongestionControl::Block`) is used for QoS
///   3/4 (ReliableUdp, ReliableTcp). `publisher.put(...).await` blocks
///   inside Zenoh until queue space is available, so the reliable path
///   never produces a seq gap. `try_publish` returns `Ok(true)`.
///
/// T16.5: `Publisher<'static>` is wrapped in `Arc` so the publisher task
/// can clone the handle, hand the clone to a spawned `put` task, and
/// keep the cached entry alive for the next message. The original
/// design held one `Publisher` per cache entry and awaited each put
/// inline on the drain loop, which serialised the entire outbound path.
/// Arc-wrapping is the minimal change to let independent puts proceed
/// in parallel without touching the per-key cache semantics.
struct PublisherState {
    session: zenoh::Session,
    publishers_drop: HashMap<String, Arc<Publisher<'static>>>,
    publishers_block: HashMap<String, Arc<Publisher<'static>>>,
}

/// Zenoh variant implementing the `Variant` trait.
///
/// Architecture (T10.2b Option B — see DECISIONS.md D7): all Zenoh API
/// calls execute on a dedicated multi-thread tokio runtime owned by this
/// struct. The variant's main thread bridges to that runtime via two
/// bounded mpsc channels:
///
/// - **Publish path**: `publish` encodes the message on the main thread,
///   then sends the encoded bytes + key over `send_tx`. A tokio task
///   drains `send_rx`, looks up or declares a per-key cached `Publisher`,
///   and awaits `publisher.put(...).await`. The publisher cache eliminates
///   the per-call `PublisherBuilder` construction cost; running the put
///   inside the runtime ensures `route_data` runs on a tokio worker that
///   can fully drive the routing path (incl. socket I/O) without
///   competing with the main thread for Zenoh's internal locks.
/// - **Receive path**: a tokio task awaits `subscriber.recv_async().await`
///   and forwards decoded `ReceivedUpdate`s over `recv_tx`. `poll_receive`
///   on the main thread drains `recv_rx` non-blockingly via `try_recv`.
///
/// `disconnect` signals shutdown via a oneshot, which both tasks select
/// against; the runtime is then shut down with a 2-second timeout.
pub struct ZenohVariant {
    runner: String,
    zenoh_args: ZenohArgs,
    /// Extra args buffer kept around so connect() can re-parse the
    /// runner-injected `--peers` map for sidecar port derivation
    /// (T14.9a). Owned (not borrowed) so the variant doesn't carry a
    /// lifetime parameter.
    extra: Vec<String>,
    /// T14.9a: zenohd sidecar lifetime. Populated when
    /// `connect(Single)` succeeds; dropped (which kills zenohd) on
    /// `disconnect`. Multi mode never sets this.
    sidecar: Option<crate::sidecar::Sidecar>,
    /// T14.9b: Single-mode sync RPC client targeting the sidecar's
    /// REST plugin. `publish`/`try_publish` route through this when
    /// `connected_mode == Single`. Multi mode never sets this.
    rest_publisher: Option<crate::rest_client::HttpPublisher>,
    /// T14.9b: Single-mode SSE reader thread + its mpsc receiver.
    /// `poll_receive` drains the receiver. Populated in
    /// `connect(Single)`; dropped (which signals the thread to stop)
    /// during `disconnect`. Multi mode never sets this.
    sse_reader: Option<crate::rest_client::SseReader>,
    /// T14.9a: which threading mode the current connection was opened
    /// with. Used by `publish`/`poll_receive` to branch between the
    /// Multi-mode tokio bridge and the Single-mode REST+SSE path.
    connected_mode: Option<ThreadingMode>,
    runtime: Option<Runtime>,
    send_tx: Option<mpsc::Sender<OutboundMessage>>,
    recv_rx: Option<mpsc::Receiver<ReceivedUpdate>>,
    /// Oneshot sender used to signal both data background tasks to wind
    /// down during `disconnect`. Wrapped in `Option` so it can be taken
    /// on shutdown.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Oneshot sender that signals the EOT subscriber task to stop. Held
    /// alongside `shutdown_tx` so the variant can wind both subscribers
    /// down independently (each task owns its own oneshot::Receiver).
    eot_shutdown_tx: Option<oneshot::Sender<()>>,
    /// Receive end of the EOT observations channel. The EOT subscriber
    /// task pushes `(writer, eot_id)` pairs here; `poll_peer_eots`
    /// drains it on the variant's main thread, applies a `(writer,
    /// eot_id)` HashSet dedup, and returns only newly-seen pairs to the
    /// driver.
    eot_rx: Option<mpsc::Receiver<(String, u64)>>,
    /// Dedup set: every `(writer, eot_id)` tuple already returned by
    /// `poll_peer_eots`. The variant is the source of truth for dedup
    /// per the EOT contract; the driver applies a defensive
    /// dedup-by-writer backstop on its side, but the variant must not
    /// rely on it.
    eot_seen: HashSet<(String, u64)>,
    /// T17.8: Application-level credit/window gate for QoS 3 / QoS 4.
    /// Held behind `Arc` so the driver thread (sync, calls
    /// [`WindowGate::wait_for_window`] from `publish`) and the
    /// ack subscriber task (async, calls [`WindowGate::apply_ack`])
    /// share a single notification surface. Constructed in
    /// `connect(Multi)`; left `None` in Single mode (the REST plugin
    /// surface has its own back-pressure semantics).
    window_gate: Option<Arc<WindowGate>>,
    /// T17.8: Oneshot signalling the ack emitter task to stop. Held
    /// separately from `shutdown_tx` / `eot_shutdown_tx` so the
    /// variant's three Zenoh-runtime tasks (data subscriber, EOT
    /// subscriber, ack subscriber + emitter) wind down independently.
    ack_shutdown_tx: Option<oneshot::Sender<()>>,
    /// T17.8: Oneshot signalling the ack subscriber task to stop.
    /// Each task owns one `oneshot::Receiver` so a single sender per
    /// task is the minimum-coupling shutdown channel.
    ack_sub_shutdown_tx: Option<oneshot::Sender<()>>,
    // Diagnostic counters used only when `zenoh_args.debug_trace` is true.
    publish_count: u64,
    publish_total_us: u128,
    publish_max_us: u128,
    poll_count: u64,
    poll_recv_count: u64,
}

impl ZenohVariant {
    /// Establish a Single-mode connection: spawn the zenohd sidecar
    /// (T14.9a lifecycle) and wire the sync RPC client (T14.9b) on
    /// top.
    ///
    /// Single-mode call graph is tokio-free: `ureq` (HTTP PUT) +
    /// dedicated OS thread reading the SSE stream + `std::sync::mpsc`
    /// to the variant's main thread. No `tokio::runtime`, no `async`,
    /// no Zenoh `Session` is opened from inside this process.
    ///
    /// On any failure after the sidecar spawn, the sidecar is killed
    /// before returning the error so we don't leak a `zenohd` child.
    fn connect_single(&mut self) -> Result<()> {
        let trace = self.zenoh_args.debug_trace;
        // Locate the binary first so a missing zenohd surfaces as
        // the actionable error documented in CUSTOM.md, BEFORE we
        // do any other work.
        let binary = crate::sidecar::locate_zenohd()
            .context("zenoh Single mode requires the zenohd sidecar")?;
        trace_if!(
            trace,
            "connect(Single): zenohd located at {} (source: {:?})",
            binary.path.display(),
            binary.source,
        );

        // Default base port chosen to be well above the
        // T14.18 / T15.10 control-port range and outside the typical
        // ephemeral pool to avoid clashing with other infrastructure.
        // 20100 is the canonical default; operators override via
        // --zenoh-sidecar-base-port.
        const DEFAULT_SIDECAR_BASE_PORT: u16 = 20100;
        let base_port = self
            .zenoh_args
            .sidecar_base_port
            .unwrap_or(DEFAULT_SIDECAR_BASE_PORT);

        // Derive the per-runner port from the runner-injected peer
        // map. Solo invocations (no `--peers`) collapse to index 0.
        let runner_index = self.derive_runner_index();
        let rest_port = crate::sidecar::derive_sidecar_port(base_port, runner_index)
            .context("zenoh sidecar port derivation overflowed")?;
        trace_if!(
            trace,
            "connect(Single): sidecar base_port={} runner_index={} rest_port={}",
            base_port,
            runner_index,
            rest_port,
        );

        // T14.9b: derive the inter-router Zenoh TCP listen port
        // (this runner's sidecar) and the connect endpoints (peer
        // runners' sidecar TCP ports). We offset the REST port by
        // a fixed +1000 to get the Zenoh TCP port -- this keeps the
        // two port ranges trivially partitioned without requiring
        // an extra CLI knob. Solo runs (no `--peers`) leave both
        // lists empty and zenohd's default multicast scouting
        // handles same-host discovery -- not strictly necessary
        // but consistent with the operator-facing manual smoke.
        const ZENOH_TCP_PORT_OFFSET: u16 = 1000;
        let zenoh_tcp_port = rest_port + ZENOH_TCP_PORT_OFFSET;
        let listen_tcp = Some(format!("127.0.0.1:{}", zenoh_tcp_port));
        let pairs = self.peer_name_host_pairs();
        let mut connect_tcp: Vec<String> = Vec::new();
        for (idx, (name, host)) in pairs.iter().enumerate() {
            if name == &self.runner {
                continue;
            }
            // Derive the peer's REST port from its index in the
            // sorted peer list (same convention this variant uses
            // for its own port). Add the +1000 Zenoh-TCP offset.
            let peer_rest_port =
                crate::sidecar::derive_sidecar_port(base_port, idx).with_context(|| {
                    format!("derive peer sidecar port for {} (index {})", name, idx)
                })?;
            let peer_zenoh_tcp = peer_rest_port + ZENOH_TCP_PORT_OFFSET;
            // Peer host: the runner-injected map carries the host
            // verbatim from the bench config; on the localhost test
            // fixture it is "127.0.0.1". We use it as-is.
            connect_tcp.push(format!("{}:{}", host, peer_zenoh_tcp));
        }
        trace_if!(
            trace,
            "connect(Single): sidecar peering listen={:?} connect={:?}",
            listen_tcp,
            connect_tcp,
        );

        let sidecar =
            crate::sidecar::Sidecar::spawn(&binary.path, rest_port, listen_tcp, &connect_tcp)
                .with_context(|| format!("spawn zenohd sidecar on REST port {rest_port}"))?;
        trace_if!(
            trace,
            "connect(Single): zenohd sidecar live on REST port {}",
            rest_port,
        );
        self.sidecar = Some(sidecar);

        // T14.9b: wire the sync RPC client. ureq agent for publish,
        // dedicated OS thread for SSE poll_receive. Both target the
        // same `127.0.0.1:<rest_port>` the sidecar is bound to.
        let publisher = crate::rest_client::HttpPublisher::new(rest_port);
        self.rest_publisher = Some(publisher);
        trace_if!(
            trace,
            "connect(Single): REST publisher ready (port {})",
            rest_port,
        );

        let sse_reader = crate::rest_client::SseReader::start(rest_port, MessageCodec::decode);
        self.sse_reader = Some(sse_reader);
        trace_if!(
            trace,
            "connect(Single): SSE reader thread started (port {})",
            rest_port,
        );

        self.connected_mode = Some(ThreadingMode::Single);
        Ok(())
    }

    /// Determine this runner's 0-based index in the runner-injected
    /// `--peers` map. Sorted alphabetically (matches the other
    /// variants' `parse_peers` convention) so the derivation is
    /// stable across all runners. Returns 0 when `--peers` is absent
    /// or this runner is not in the map (e.g. unit tests).
    fn derive_runner_index(&self) -> usize {
        let mut names = variant_base::cli::parse_peer_names_from_extra(&self.extra);
        names.sort();
        names.iter().position(|n| n == &self.runner).unwrap_or(0)
    }

    /// Parse the runner-injected `--peers` extra arg into a sorted
    /// list of `(name, host)` pairs. Returns the pairs in the same
    /// alphabetical order [`derive_runner_index`] uses, so caller
    /// can derive remote ports from the index symmetrically across
    /// all runners. Empty / absent `--peers` -> empty vec.
    fn peer_name_host_pairs(&self) -> Vec<(String, String)> {
        let raw = extract_extra_arg(&self.extra, "peers").unwrap_or_default();
        let mut pairs: Vec<(String, String)> = Vec::new();
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((name, host)) = part.split_once('=') {
                pairs.push((name.trim().to_string(), host.trim().to_string()));
            }
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }
}

/// Tiny `--<name> <value>` extractor for the `extra` args buffer. The
/// variant's lenient `ZenohArgs::parse` already skips `--peers`, so
/// we re-walk `extra` here when we need the peer map for sidecar
/// peering (T14.9b). Returns the first match; returns `None` if the
/// flag is absent or has no value.
fn extract_extra_arg(extra: &[String], name: &str) -> Option<String> {
    let target = format!("--{}", name);
    let mut i = 0;
    while i < extra.len() {
        if extra[i] == target {
            if i + 1 < extra.len() {
                return Some(extra[i + 1].clone());
            }
            return None;
        }
        if let Some(stripped) = extra[i].strip_prefix(&format!("--{}=", name)) {
            return Some(stripped.to_string());
        }
        i += 1;
    }
    None
}

impl ZenohVariant {
    pub fn new(runner: &str, extra: &[String]) -> Result<Self> {
        let zenoh_args = ZenohArgs::parse(extra)?;
        Ok(Self {
            runner: runner.to_string(),
            zenoh_args,
            extra: extra.to_vec(),
            sidecar: None,
            rest_publisher: None,
            sse_reader: None,
            connected_mode: None,
            runtime: None,
            send_tx: None,
            recv_rx: None,
            shutdown_tx: None,
            eot_shutdown_tx: None,
            eot_rx: None,
            eot_seen: HashSet::new(),
            window_gate: None,
            ack_shutdown_tx: None,
            ack_sub_shutdown_tx: None,
            publish_count: 0,
            publish_total_us: 0,
            publish_max_us: 0,
            poll_count: 0,
            poll_recv_count: 0,
        })
    }
}

/// Build a Zenoh `Config` from the parsed args. Pure helper so the runtime
/// initialisation in `connect` stays linear.
///
/// **T-impl.2**: Zenoh doesn't expose a raw `SO_RCVBUF` / `SO_SNDBUF`
/// knob on its UDP transport links, so we tune the **transport-level**
/// queues that sit immediately above the socket instead. The plain-UDP
/// variants bump `SO_*BUF` to 8 MiB so that at 100 K pkt/s sustained
/// the kernel queue does not overflow within milliseconds. The Zenoh
/// equivalent is to raise each transport-link priority queue's batch
/// count to its maximum (16) — with the default `batch_size = 65535`
/// bytes that gives ~1 MiB of TX-side queue depth per QoS priority,
/// and with 8 priority queues the per-link aggregate is ~8 MiB which
/// matches the 8 MiB target the other variants use. We also raise the
/// receive-side per-link buffer (`transport/link/rx/buffer_size`) from
/// its default 65 535 bytes to 8 MiB so the RX path can absorb the
/// same bursts. Field paths chosen against Zenoh 1.9's
/// `DEFAULT_CONFIG.json5` schema; the upper limit of 16 on
/// `transport/link/tx/queue/size/*` is enforced by Zenoh itself and
/// values outside `[1, 16]` cause `zenoh::open` to error. See
/// `variants/zenoh/CUSTOM.md` for the full rationale.
fn build_zenoh_config(args: &ZenohArgs) -> Result<zenoh::Config> {
    let mut config = zenoh::Config::default();

    match args.mode.as_str() {
        "peer" | "client" | "router" => {}
        other => anyhow::bail!("unsupported zenoh mode: {}", other),
    };
    config
        .insert_json5("mode", &format!("\"{}\"", args.mode))
        .map_err(zenoh_err)?;

    if let Some(ref listen) = args.listen {
        config
            .insert_json5("listen/endpoints", &format!("[\"{}\"]", listen))
            .map_err(zenoh_err)?;
    }

    // Raise each priority queue's batch count to the schema maximum (16),
    // giving ~16 * 65535 = ~1 MiB of TX buffering per priority, and
    // ~8 MiB across the 8 priorities — i.e. the 8 MiB target T-impl.2
    // mandates for every UDP-using variant. `insert_json5` parses the
    // value as JSON5; an integer literal works directly.
    config
        .insert_json5("transport/link/tx/queue/size/control", "16")
        .map_err(zenoh_err)?;
    config
        .insert_json5("transport/link/tx/queue/size/real_time", "16")
        .map_err(zenoh_err)?;
    config
        .insert_json5("transport/link/tx/queue/size/interactive_high", "16")
        .map_err(zenoh_err)?;
    config
        .insert_json5("transport/link/tx/queue/size/interactive_low", "16")
        .map_err(zenoh_err)?;
    config
        .insert_json5("transport/link/tx/queue/size/data_high", "16")
        .map_err(zenoh_err)?;
    config
        .insert_json5("transport/link/tx/queue/size/data", "16")
        .map_err(zenoh_err)?;
    config
        .insert_json5("transport/link/tx/queue/size/data_low", "16")
        .map_err(zenoh_err)?;
    config
        .insert_json5("transport/link/tx/queue/size/background", "16")
        .map_err(zenoh_err)?;

    // RX-side per-link buffer: bump from the default 65 535 bytes to
    // 8 MiB so a burst of receives doesn't get clipped before zenoh's
    // own routing layer can pull it off the link.
    config
        .insert_json5("transport/link/rx/buffer_size", "8388608")
        .map_err(zenoh_err)?;

    Ok(config)
}

/// Publisher-side background task. Drains outbound messages, manages the
/// per-key `Publisher` cache, and awaits each put on the runtime so that
/// `route_data` and the underlying transport TX path get full async
/// scheduling.
async fn publisher_task(
    mut state: PublisherState,
    mut send_rx: mpsc::Receiver<OutboundMessage>,
    trace: bool,
) {
    // T16.5: bounded in-flight permit pool. Each parallel `put().await`
    // task holds one permit for its lifetime, which both caps memory
    // growth under pathological backpressure (Zenoh's CC=Block can
    // wedge a put indefinitely if the peer is gone) and gives us a
    // clean "drain at shutdown" semantic: closing the channel + waiting
    // for all permits to be returned reproduces the old serial-task
    // teardown.
    let inflight = Arc::new(Semaphore::new(PUBLISH_INFLIGHT_LIMIT));
    while let Some(msg) = send_rx.recv().await {
        match msg {
            OutboundMessage::Data {
                key,
                encoded,
                seq,
                qos,
            } => {
                // T-impl.7: pick the publisher cache that matches the
                // QoS's congestion-control policy. QoS 1/2 -> Drop
                // (Zenoh silently drops if its queue is full; bridge
                // already short-circuited at try_send if our channel
                // was full). QoS 3/4 -> Block (publisher.put().await
                // back-pressures inside Zenoh's queue, so the reliable
                // path never produces a seq gap).
                let reliable = matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp);
                let (cache, cc_label) = if reliable {
                    (&mut state.publishers_block, "block")
                } else {
                    (&mut state.publishers_drop, "drop")
                };
                let cc = if reliable {
                    CongestionControl::Block
                } else {
                    CongestionControl::Drop
                };
                // Standard hot path: publisher was pre-declared in
                // `connect` from the workload's known path set, so this
                // lookup is a HashMap hit.
                //
                // T16.5: for QoS 1/2 we clone the Arc<Publisher> and
                // spawn the put on its own task so the drain loop can
                // immediately receive the next message instead of
                // waiting on this put. This was the T16.5 fix for the
                // 1000-path asymmetric collapse: without it, one slow
                // `put().await` head-of-line blocked every other key.
                //
                // T16.10: that pattern is **only valid for QoS 1/2**
                // (CongestionControl::Drop, where ordering is *not*
                // contractually required -- BestEffort/LatestValue
                // both permit drops + reorders). For QoS 3/4 (Block,
                // reliable-ordered) spawning lets multiple
                // `put().await` futures for the same key race: the
                // first put's Block-queue wait can let a later put
                // for the same key complete first, and the receiver
                // sees out-of-order samples (~17 000 OOO / 51 000 on
                // the 1000x10hz QoS 3 reproducer, per the T16.10
                // task brief).
                //
                // The fix: for reliable QoS, do *not* spawn -- await
                // the put inline on the drain loop. The single-task
                // pipeline naturally serialises every key, which is
                // exactly what ordered delivery requires. Cross-key
                // parallelism is given up on the reliable path, but
                // T16.5's own verification (STATUS.md 2026-05-14)
                // showed Zenoh's per-publisher Block queue at 1000
                // keys on localhost is already the rate-limiting
                // factor (1000 writes/10s observed for the slower
                // peer) -- spawning never bought meaningful
                // additional throughput on this workload, only
                // unordered delivery.
                let publisher = cache.get(&key).cloned();
                if let Some(publisher) = publisher {
                    if reliable {
                        // T16.10: inline await preserves per-key
                        // ordering for QoS 3/4. No semaphore is needed
                        // because there's at most one outstanding put
                        // from this task at a time (the drain loop is
                        // single-task).
                        if let Err(e) = publisher.put(encoded).await {
                            if trace {
                                trace_now!(
                                    "publisher_task: put failed cc={} seq={} key={} err={}",
                                    cc_label,
                                    seq,
                                    key,
                                    e
                                );
                            }
                        }
                    } else {
                        // QoS 1/2 (Drop): preserve T16.5 spawn-per-put so
                        // a slow downstream link cannot head-of-line
                        // block unrelated keys. Acquire the in-flight
                        // permit *before* spawning so a truly stuck
                        // publisher actually backs the drain loop up at
                        // the semaphore (rather than spawning infinite
                        // tasks that pile up on the runtime).
                        let permit = match inflight.clone().acquire_owned().await {
                            Ok(p) => p,
                            Err(_) => {
                                // Semaphore closed -- task is shutting down.
                                break;
                            }
                        };
                        let trace_for_spawn = trace;
                        let key_for_spawn = key.clone();
                        let cc_label_for_spawn = cc_label;
                        tokio::spawn(async move {
                            if let Err(e) = publisher.put(encoded).await {
                                if trace_for_spawn {
                                    trace_now!(
                                        "publisher_task: put failed cc={} seq={} key={} err={}",
                                        cc_label_for_spawn,
                                        seq,
                                        key_for_spawn,
                                        e
                                    );
                                }
                            }
                            drop(permit);
                        });
                    }
                } else {
                    // Lazy fallback: declare on first sight. This used to
                    // be the universal path and was the root cause of
                    // T-zenoh.1's 8k-message hang -- 1000 declares
                    // serialised on a 2-worker runtime stalled the
                    // publisher task while the channel filled. Keep it
                    // for non-standard workloads, but emit a trace so
                    // we notice if a fixture starts hitting it.
                    if trace {
                        trace_now!(
                            "publisher_task: lazy declare cc={} key={} (pre-declare missed)",
                            cc_label,
                            key
                        );
                    }
                    match state
                        .session
                        .declare_publisher(key.clone())
                        .congestion_control(cc)
                        .await
                    {
                        Ok(publisher) => {
                            let publisher = Arc::new(publisher);
                            // T16.5 / T16.10: mirror the hot-path
                            // branching. The declare is awaited inline
                            // (lazy path, infrequent); the put is then
                            // either spawned (QoS 1/2, Drop) or awaited
                            // inline (QoS 3/4, Block — preserves
                            // per-key ordering).
                            if reliable {
                                if let Err(e) = publisher.put(encoded).await {
                                    if trace {
                                        trace_now!(
                                            "publisher_task: put failed cc={} seq={} key={} err={}",
                                            cc_label,
                                            seq,
                                            key,
                                            e
                                        );
                                    }
                                }
                            } else {
                                let permit = match inflight.clone().acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => break,
                                };
                                let publisher_for_spawn = Arc::clone(&publisher);
                                let trace_for_spawn = trace;
                                let key_for_spawn = key.clone();
                                let cc_label_for_spawn = cc_label;
                                tokio::spawn(async move {
                                    if let Err(e) = publisher_for_spawn.put(encoded).await {
                                        if trace_for_spawn {
                                            trace_now!(
                                                "publisher_task: put failed cc={} seq={} key={} err={}",
                                                cc_label_for_spawn,
                                                seq,
                                                key_for_spawn,
                                                e
                                            );
                                        }
                                    }
                                    drop(permit);
                                });
                            }
                            cache.insert(key, publisher);
                        }
                        Err(e) => {
                            if trace {
                                trace_now!(
                                    "publisher_task: declare_publisher cc={} ({}) failed: {}",
                                    cc_label,
                                    key,
                                    e
                                );
                            }
                            continue;
                        }
                    }
                }
            }
            OutboundMessage::Eot { key, payload, done } => {
                // EOT is a one-shot per spawn -- do NOT cache the publisher.
                // Use the session's `put` directly. The variant's main
                // thread is blocking on `done.recv` so it returns from
                // `signal_end_of_test` only after the put has committed.
                let put_result = state
                    .session
                    .put(&key, payload.to_vec())
                    .await
                    .map_err(zenoh_err)
                    .with_context(|| format!("zenoh put for EOT key {} failed", key));
                if trace {
                    match &put_result {
                        Ok(()) => trace_now!("publisher_task: EOT put ok key={}", key),
                        Err(e) => {
                            trace_now!("publisher_task: EOT put failed key={} err={}", key, e)
                        }
                    }
                }
                let _ = done.send(put_result);
            }
        }
    }

    // T16.5: wait for all in-flight `put` tasks to release their
    // permits before tearing down publishers / closing the session.
    // Acquiring all PUBLISH_INFLIGHT_LIMIT permits guarantees every
    // spawned put has finished (or errored) and dropped its
    // Arc<Publisher>, so the `Arc::try_unwrap` below can succeed for
    // the typical case where the variant's main thread shut down
    // cleanly. We close the semaphore first so any future
    // `acquire_owned()` returns Err and the drain loop bails fast.
    inflight.close();
    let acquire_all = inflight.acquire_many(PUBLISH_INFLIGHT_LIMIT as u32).await;
    // `acquire_many` on a closed semaphore returns Err once no permits
    // remain free; we don't care about the result, only that we've
    // waited for outstanding holders to release. Drop the guard so the
    // permits are released back; Zenoh's session close will undeclare
    // anyway as a fallback.
    drop(acquire_all);

    // Channel closed: drain both publisher caches. Undeclaring
    // explicitly gives consistent teardown timing and surfaces errors
    // via the trace log; without this the publishers would
    // Drop-undeclare on session close, which is fine but less
    // observable.
    let pub_count = state.publishers_drop.len() + state.publishers_block.len();
    let t = Instant::now();
    for (_, publisher) in state
        .publishers_drop
        .drain()
        .chain(state.publishers_block.drain())
    {
        // T16.5: try to unwrap the Arc; if a put task is somehow
        // still holding the Publisher (e.g. an `await` wedged in
        // Zenoh's queue past the semaphore-drain window above) fall
        // back to letting the session-close handle the undeclare.
        let publisher = match Arc::try_unwrap(publisher) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Err(e) = publisher.undeclare().await {
            if trace {
                trace_now!("publisher_task: undeclare failed: {}", e);
            }
        }
    }
    if trace {
        trace_now!(
            "publisher_task: undeclared {} publishers in {} ms",
            pub_count,
            t.elapsed().as_millis()
        );
    }

    // Best-effort session close. We deliberately ignore errors here — the
    // runtime is about to be shut down and any close failure is at most
    // a logged curiosity.
    if let Err(e) = state.session.close().await {
        if trace {
            trace_now!("publisher_task: session close failed: {}", e);
        }
    }
}

/// Subscriber-side background task. Awaits incoming samples and forwards
/// decoded updates over the receive channel. If the channel is full
/// (consumer can't drain fast enough), the sample is dropped — the
/// benchmark measures end-to-end delivery, so a drop here looks
/// equivalent to a wire-level loss in the resulting analysis.
///
/// T17.8: this task ALSO updates the shared [`RecvAccounting`] map on
/// every successfully-decoded **QoS 3 / QoS 4** sample from a *non-self*
/// writer. The ack_emitter_task reads that map on its 50 ms cadence and
/// publishes the per-writer max_seq back to the writer over the
/// `bench/__ack__/<self>/<writer>` side-channel. The combination
/// realises the application-level credit/window protocol that replaces
/// Zenoh's native `CongestionControl::Block` as the dominant
/// back-pressure surface (CC=Block is still set on the data publisher
/// as a last-resort safety net — see CUSTOM.md
/// "Peer-coordinated back-pressure (T17.8)").
async fn subscriber_task(
    subscriber: Subscriber<FifoChannelHandler<Sample>>,
    recv_tx: mpsc::Sender<ReceivedUpdate>,
    recv_accounting: Arc<Mutex<RecvAccounting>>,
    self_runner: String,
    mut shutdown_rx: oneshot::Receiver<()>,
    trace: bool,
) {
    let mut dropped = 0u64;
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                if trace {
                    trace_now!("subscriber_task: shutdown signal received; dropped_total={}", dropped);
                }
                break;
            }
            sample_result = subscriber.recv_async() => {
                match sample_result {
                    Ok(sample) => {
                        let data: Vec<u8> = sample.payload().to_bytes().to_vec();
                        match MessageCodec::decode(&data) {
                            Ok(update) => {
                                // T17.8: update the ack-emitter's view of
                                // "highest reliable seq from this writer"
                                // BEFORE forwarding to the variant. We
                                // gate on `qos in (ReliableUdp,
                                // ReliableTcp)` AND `writer != self` so
                                // (a) QoS 1/2 traffic does not produce
                                // ack noise (best-effort QoS doesn't
                                // need the window) and (b) loopback
                                // (writer == self) does not register
                                // self-as-peer, which would cause solo
                                // runs to gate on their own progress.
                                let is_strict = matches!(
                                    update.qos,
                                    Qos::ReliableUdp | Qos::ReliableTcp,
                                );
                                if is_strict && update.writer != self_runner {
                                    if let Ok(mut acc) = recv_accounting.lock() {
                                        let entry = acc
                                            .per_writer_max_seq
                                            .entry(update.writer.clone())
                                            .or_insert(0);
                                        if update.seq > *entry {
                                            *entry = update.seq;
                                        }
                                    }
                                }
                                // try_send so a slow consumer (or a backed-up
                                // channel) doesn't block the subscriber task —
                                // blocking here would let Zenoh's internal FIFO
                                // back up and reintroduce the very head-of-line
                                // pressure Option B is meant to relieve.
                                if let Err(e) = recv_tx.try_send(update) {
                                    dropped += 1;
                                    if trace && dropped.is_multiple_of(1000) {
                                        trace_now!(
                                            "subscriber_task: recv channel full; dropped={} (last: {})",
                                            dropped,
                                            e,
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                if trace {
                                    trace_now!("subscriber_task: decode failed: {}", e);
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Subscriber channel disconnected -- session
                        // probably closing. Bail out.
                        if trace {
                            trace_now!("subscriber_task: recv_async returned Err; ending");
                        }
                        break;
                    }
                }
            }
        }
    }

    // Best-effort undeclare. Errors are non-fatal at shutdown time.
    if let Err(e) = subscriber.undeclare().await {
        if trace {
            trace_now!("subscriber_task: undeclare failed: {}", e);
        }
    }
}

/// T17.8: Ack emitter task. Periodically snapshots [`RecvAccounting`]
/// and publishes one `bench/__ack__/<self>/<writer>` sample per known
/// writer with the highest decoded `seq` payload (8 bytes big-endian).
/// Runs on the same Zenoh session as the data path so peer discovery /
/// route resolution is shared (no extra session, no extra runtime).
///
/// **Publisher cache**: ack publishers are declared lazily on first
/// sight of a new writer. Use `CongestionControl::Drop` so an
/// ack-key route hiccup never blocks this task -- acks are
/// idempotent (latest watermark replaces previous), so a dropped ack
/// is recovered on the next 50 ms tick. **Crucially**, this means the
/// ack channel itself cannot stall, which is what makes the
/// application-level credit/window protocol robust against the
/// asymmetric stall the T16.12 data path exhibited under CC=Block.
///
/// **Shutdown**: when the shutdown oneshot fires the task drops its
/// publisher cache (which undeclares via the session close path) and
/// exits. Failing to undeclare here is non-fatal -- session close
/// undeclares for us.
async fn ack_emitter_task(
    session: zenoh::Session,
    self_runner: String,
    recv_accounting: Arc<Mutex<RecvAccounting>>,
    mut shutdown_rx: oneshot::Receiver<()>,
    trace: bool,
) {
    let mut publishers: HashMap<String, Publisher<'static>> = HashMap::new();
    let mut interval = tokio::time::interval(ACK_EMIT_INTERVAL);
    // Skip the first immediate-fire tick so a fresh `connect` doesn't
    // publish an empty snapshot before the receiver has decoded anything.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                if trace {
                    trace_now!("ack_emitter_task: shutdown signal received");
                }
                break;
            }
            _ = interval.tick() => {
                // Snapshot under the mutex. Cloning a small HashMap is
                // cheap and keeps the lock window minimal -- the
                // subscriber_task can keep updating while we publish.
                let snapshot: Vec<(String, u64)> = match recv_accounting.lock() {
                    Ok(acc) => acc
                        .per_writer_max_seq
                        .iter()
                        .map(|(k, v)| (k.clone(), *v))
                        .collect(),
                    Err(_) => continue,
                };
                for (writer, max_seq) in snapshot {
                    let key = ack_key_for(&self_runner, &writer);
                    let publisher = match publishers.get(&key) {
                        Some(p) => p,
                        None => {
                            // Lazy declare; ack keys are bounded by the
                            // number of distinct *non-self* writers we
                            // have ever received from (typically 1-2 in
                            // a two-runner topology, never more than the
                            // peer count).
                            match session
                                .declare_publisher(key.clone())
                                .congestion_control(CongestionControl::Drop)
                                .await
                            {
                                Ok(p) => {
                                    publishers.insert(key.clone(), p);
                                    publishers.get(&key).expect("just inserted")
                                }
                                Err(e) => {
                                    if trace {
                                        trace_now!(
                                            "ack_emitter_task: declare_publisher failed key={} err={}",
                                            key,
                                            e,
                                        );
                                    }
                                    continue;
                                }
                            }
                        }
                    };
                    let payload = encode_ack_payload(max_seq);
                    if let Err(e) = publisher.put(payload.to_vec()).await {
                        if trace {
                            trace_now!(
                                "ack_emitter_task: put failed key={} max_seq={} err={}",
                                key,
                                max_seq,
                                e,
                            );
                        }
                    }
                }
            }
        }
    }

    // Best-effort undeclare loop on shutdown.
    for (_, publisher) in publishers.drain() {
        if let Err(e) = publisher.undeclare().await {
            if trace {
                trace_now!("ack_emitter_task: undeclare failed: {}", e);
            }
        }
    }
}

/// T17.8: Ack subscriber task. Listens on the
/// `bench/__ack__/*/<self_runner>` wildcard and feeds every observed
/// `(peer, max_seq)` watermark into the [`WindowGate`] so the driver
/// thread's strict-QoS publish gate can open as soon as the peer
/// confirms progress.
///
/// **Self-ack filter**: in the (theoretical) loopback case where a
/// receiver and writer share a runner name, the subscriber task
/// already skips self-publishes when updating [`RecvAccounting`], so
/// no self-ack ever leaves the local session. As belt-and-braces the
/// ack subscriber also skips samples whose receiver segment equals
/// `self_runner`.
async fn ack_subscriber_task(
    subscriber: Subscriber<FifoChannelHandler<Sample>>,
    window_gate: Arc<WindowGate>,
    self_runner: String,
    mut shutdown_rx: oneshot::Receiver<()>,
    trace: bool,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                if trace {
                    trace_now!("ack_subscriber_task: shutdown signal received");
                }
                break;
            }
            sample_result = subscriber.recv_async() => {
                match sample_result {
                    Ok(sample) => {
                        let key_str = sample.key_expr().as_str().to_string();
                        let (receiver, writer) = match parse_ack_key(&key_str) {
                            Some(t) => t,
                            None => {
                                if trace {
                                    trace_now!(
                                        "ack_subscriber_task: malformed ack key {}",
                                        key_str,
                                    );
                                }
                                continue;
                            }
                        };
                        // The wildcard `bench/__ack__/*/<self>` should
                        // already constrain `writer == self_runner` and
                        // `receiver != self_runner` for non-loopback
                        // traffic; the explicit checks here defend
                        // against a misuse / unexpected key shape.
                        if writer != self_runner {
                            continue;
                        }
                        if receiver == self_runner {
                            continue;
                        }
                        let data: Vec<u8> = sample.payload().to_bytes().to_vec();
                        let max_seq = match decode_ack_payload(&data) {
                            Some(s) => s,
                            None => {
                                if trace {
                                    trace_now!(
                                        "ack_subscriber_task: bad ack payload len={} peer={}",
                                        data.len(),
                                        receiver,
                                    );
                                }
                                continue;
                            }
                        };
                        window_gate.apply_ack(receiver, max_seq);
                    }
                    Err(_) => {
                        if trace {
                            trace_now!("ack_subscriber_task: recv_async returned Err; ending");
                        }
                        break;
                    }
                }
            }
        }
    }

    if let Err(e) = subscriber.undeclare().await {
        if trace {
            trace_now!("ack_subscriber_task: undeclare failed: {}", e);
        }
    }
}

/// EOT-side subscriber task. Awaits incoming EOT samples on the
/// `bench/__eot__/**` wildcard and forwards `(writer, eot_id)` pairs over
/// the EOT channel. Self-EOTs (writer == self_runner) are filtered out so
/// the variant's poll never returns its own EOT to the driver.
///
/// Decode failures and malformed keys are logged under `--debug-trace` and
/// otherwise silently ignored -- a corrupt EOT is no worse than a missed
/// one, and the driver will fall back to `eot_timeout` if no peer ever
/// signals.
async fn eot_subscriber_task(
    subscriber: Subscriber<FifoChannelHandler<Sample>>,
    eot_tx: mpsc::Sender<(String, u64)>,
    self_runner: String,
    mut shutdown_rx: oneshot::Receiver<()>,
    trace: bool,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                if trace {
                    trace_now!("eot_subscriber_task: shutdown signal received");
                }
                break;
            }
            sample_result = subscriber.recv_async() => {
                match sample_result {
                    Ok(sample) => {
                        let key_str = sample.key_expr().as_str().to_string();
                        let writer = match writer_from_eot_key(&key_str) {
                            Some(w) => w.to_string(),
                            None => {
                                if trace {
                                    trace_now!(
                                        "eot_subscriber_task: malformed EOT key {}",
                                        key_str
                                    );
                                }
                                continue;
                            }
                        };
                        if writer == self_runner {
                            // Filter out our own EOT so the driver never
                            // sees self in poll_peer_eots. Zenoh subscriber
                            // wildcards do match our own publishes.
                            continue;
                        }
                        let data: Vec<u8> = sample.payload().to_bytes().to_vec();
                        let eot_id = match decode_eot_payload(&data) {
                            Some(id) => id,
                            None => {
                                if trace {
                                    trace_now!(
                                        "eot_subscriber_task: bad EOT payload len={} writer={}",
                                        data.len(),
                                        writer
                                    );
                                }
                                continue;
                            }
                        };
                        if let Err(e) = eot_tx.try_send((writer, eot_id)) {
                            if trace {
                                trace_now!(
                                    "eot_subscriber_task: enqueue failed: {}",
                                    e
                                );
                            }
                        }
                    }
                    Err(_) => {
                        if trace {
                            trace_now!("eot_subscriber_task: recv_async returned Err; ending");
                        }
                        break;
                    }
                }
            }
        }
    }

    if let Err(e) = subscriber.undeclare().await {
        if trace {
            trace_now!("eot_subscriber_task: undeclare failed: {}", e);
        }
    }
}

impl Variant for ZenohVariant {
    fn name(&self) -> &str {
        "zenoh"
    }

    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        // T14.9b: Single mode is now genuinely single-threaded. It
        // talks to an out-of-process `zenohd` sidecar (T14.9a
        // lifecycle) through the REST plugin -- HTTP PUT for
        // publish, dedicated OS thread reading SSE for receive --
        // and the in-process call graph is tokio-free.
        //
        // Multi mode continues to use the in-process zenoh crate
        // with its internal tokio runtime; that runtime is the
        // exact reason Single used to be unsupported before T14.9.
        //
        // See `variants/zenoh/CUSTOM.md` "Threading modes" and
        // "T14.9b RPC client architecture".
        &[ThreadingMode::Single, ThreadingMode::Multi]
    }

    fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()> {
        // T14.9b: Single mode delegates to `connect_single`, which
        // composes T14.9a's sidecar spawn with this task's sync
        // RPC client (HTTP PUT publisher + SSE reader thread). The
        // variant now declares `[Single, Multi]` capability so the
        // runner spawns Single-mode fixtures through this branch
        // alongside the in-process Multi-mode path.
        if threading_mode == ThreadingMode::Single {
            return self.connect_single();
        }
        let trace = self.zenoh_args.debug_trace;
        let t0 = Instant::now();
        trace_if!(
            trace,
            "connect: start mode={} listen={:?}",
            self.zenoh_args.mode,
            self.zenoh_args.listen,
        );

        let config = build_zenoh_config(&self.zenoh_args)?;

        // Multi-thread runtime sized to the host so the publisher task,
        // both subscriber tasks, and zenoh's internal driver tasks
        // (route-resolution, transport TX/RX) all get real worker
        // threads. The previous 2-worker cap (chosen to keep the bridge
        // small) was the proximate cause of T-zenoh.1's first-tick
        // hang: 1000 lazy `declare_publisher().await` calls plus 3
        // bridge tasks plus zenoh's own background work serialised
        // onto 2 workers, the publisher task starved, the publish
        // channel filled, and `blocking_send` deadlocked the writer.
        // `num_cpus::get().max(4)` gives at least 4 workers even on
        // small VMs and scales with the host on bigger boxes.
        let worker_threads = num_cpus::get().max(4);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .enable_all()
            .thread_name("zenoh-bridge")
            .build()
            .context("failed to build tokio runtime")?;
        trace_if!(
            trace,
            "connect: tokio runtime worker_threads={}",
            worker_threads,
        );

        // Open the session and declare BOTH subscribers (data + EOT)
        // inside the runtime so any task spawning Zenoh does at
        // construction time happens on the right runtime. Both
        // subscribers share the single session per the T10.2b bridge
        // architecture; do NOT open a second session for EOT.
        //
        // Publishers are also pre-declared inside this same `block_on`
        // when the workload's path set is known (T-zenoh.1, scope
        // item 1). The standard `scalar-flood` workload publishes to
        // `bench/0..values_per_tick-1`, which we recover from
        // `std::env::args` because the `Variant` trait does not pass
        // `values_per_tick` through. Concurrent declares via a
        // `JoinSet` so 1000 keys finish in roughly the cost of a few
        // dozen sequential declares (the runtime now has enough
        // workers to actually parallelise them).
        let pre_declare_count = values_per_tick_from_env().unwrap_or(0);
        let ack_self_wildcard = ack_wildcard_for_self(&self.runner);
        let t_open = Instant::now();
        let (
            session,
            subscriber,
            eot_subscriber,
            ack_subscriber,
            predeclared_publishers_drop,
            predeclared_publishers_block,
        ) = runtime
            .block_on(async {
                let session = zenoh::open(config).await.map_err(zenoh_err)?;
                // T19.X: Override the default FIFO subscriber capacity
                // (256) with [`SUBSCRIBER_FIFO_CAPACITY`] so the
                // Zenoh routing thread never parks on a full
                // subscriber channel. See the constant's docstring
                // for the deadlock-symmetry rationale.
                let subscriber = session
                    .declare_subscriber(SUBSCRIBER_WILDCARD)
                    .with(FifoChannel::new(SUBSCRIBER_FIFO_CAPACITY))
                    .await
                    .map_err(zenoh_err)?;
                let eot_subscriber = session
                    .declare_subscriber(EOT_WILDCARD)
                    .with(FifoChannel::new(SUBSCRIBER_FIFO_CAPACITY))
                    .await
                    .map_err(zenoh_err)?;
                // T17.8: ack subscriber listens on
                // `bench/__ack__/*/<self_runner>` -- the wildcard
                // segment matches the *receiver* runner that
                // published the ack. The data subscriber's
                // SUBSCRIBER_WILDCARD (`bench/**`) intersects this
                // prefix too, but ack samples carry a u64 payload
                // (not a MessageCodec-encoded message), so they
                // would fail to decode and be dropped silently in
                // the data path. The dedicated ack subscriber gets
                // the samples first and parses them with the right
                // codec.
                let ack_subscriber = session
                    .declare_subscriber(ack_self_wildcard.clone())
                    .with(FifoChannel::new(SUBSCRIBER_FIFO_CAPACITY))
                    .await
                    .map_err(zenoh_err)?;

                // Pre-declare publishers for the workload's known path
                // set. T-impl.7: we declare *two* publishers per key, one
                // per congestion-control policy, so the publisher task
                // can route messages to the appropriate cache by QoS
                // without paying a declare cost on the hot path.
                // Concurrent via JoinSet; results collected into the
                // publisher caches before the publisher task starts
                // draining the publish channel, so the operate phase
                // sees zero declares for the standard fixture.
                // T16.5: caches now hold `Arc<Publisher<'static>>` so the
                // publisher task can clone the handle per outbound message
                // and spawn the `put().await` as a parallel sub-task. The
                // pre-declare path constructs the publishers and wraps
                // each one in an Arc before insertion.
                let mut publishers_drop: HashMap<String, Arc<Publisher<'static>>> =
                    HashMap::with_capacity(pre_declare_count as usize);
                let mut publishers_block: HashMap<String, Arc<Publisher<'static>>> =
                    HashMap::with_capacity(pre_declare_count as usize);
                if pre_declare_count > 0 {
                    let mut set: JoinSet<(
                        String,
                        CongestionControl,
                        zenoh::Result<Publisher<'static>>,
                    )> = JoinSet::new();
                    for i in 0..pre_declare_count {
                        let key = format!("bench/{}", i);
                        for cc in [CongestionControl::Drop, CongestionControl::Block] {
                            let session_clone = session.clone();
                            let key_for_task = key.clone();
                            set.spawn(async move {
                                let res = session_clone
                                    .declare_publisher(key_for_task.clone())
                                    .congestion_control(cc)
                                    .await;
                                (key_for_task, cc, res)
                            });
                        }
                    }
                    while let Some(joined) = set.join_next().await {
                        let (key, cc, res) =
                            joined.context("declare_publisher task panicked")?;
                        match res {
                            Ok(publisher) => match cc {
                                CongestionControl::Drop => {
                                    publishers_drop.insert(key, Arc::new(publisher));
                                }
                                CongestionControl::Block => {
                                    publishers_block.insert(key, Arc::new(publisher));
                                }
                            },
                            Err(e) => {
                                // Don't fail connect on a single
                                // declare error -- fall back to the
                                // lazy path on first publish for that
                                // key. Zenoh's declare can fail
                                // transiently during scout warm-up.
                                eprintln!(
                                    "[zenoh] warning: pre-declare publisher for {} cc={:?} failed: {}",
                                    key, cc, e
                                );
                            }
                        }
                    }
                }
                Ok::<_, anyhow::Error>((
                    session,
                    subscriber,
                    eot_subscriber,
                    ack_subscriber,
                    publishers_drop,
                    publishers_block,
                ))
            })
            .context(
                "failed to open zenoh session / declare subscribers / pre-declare publishers",
            )?;
        trace_if!(
            trace,
            "connect: session opened + subscribers declared + {}/{} publishers pre-declared (drop/block) in {} ms",
            predeclared_publishers_drop.len(),
            predeclared_publishers_block.len(),
            t_open.elapsed().as_millis()
        );

        let (send_tx, send_rx) = mpsc::channel::<OutboundMessage>(PUBLISH_CHANNEL_CAPACITY);
        let (recv_tx, recv_rx) = mpsc::channel::<ReceivedUpdate>(RECEIVE_CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        // Separate shutdown oneshot for the EOT subscriber so each task
        // owns exactly one Receiver (oneshot::Receiver is single-consumer).
        let (eot_shutdown_tx, eot_shutdown_rx) = oneshot::channel::<()>();
        // T17.8: two more oneshots for the ack subscriber + ack emitter.
        let (ack_shutdown_tx, ack_shutdown_rx) = oneshot::channel::<()>();
        let (ack_sub_shutdown_tx, ack_sub_shutdown_rx) = oneshot::channel::<()>();
        // EOT observations channel: small bound is sufficient because the
        // contract is one EOT per peer per spawn. 256 leaves ample
        // headroom for retries / late duplicates without unbounded
        // memory.
        let (eot_tx, eot_rx) = mpsc::channel::<(String, u64)>(256);

        // T17.8: shared state for the application-level credit/window
        // protocol. `RecvAccounting` is written by `subscriber_task`
        // (one entry advance per QoS 3/4 receive) and read by
        // `ack_emitter_task` (one snapshot every 25 ms).
        // `WindowGate` is written by `ack_subscriber_task` (one
        // `apply_ack` per ack sample) and read by the variant's
        // `publish` on the driver thread.
        //
        // The gate is **pre-seeded** with the runner-injected peer
        // set (minus self) at watermark 0. Without the pre-seed, a
        // fresh spawn would burst-publish thousands of messages
        // before the peer's first ack landed; the bridge would
        // overflow with un-drainable in-flight work which then
        // shows up as an end-of-test tail drop. Solo / loopback
        // configurations have an empty peer set and the gate is a
        // no-op (see `WindowGate::wait_for_window`).
        let expected_peers: Vec<String> = self
            .peer_name_host_pairs()
            .into_iter()
            .map(|(name, _host)| name)
            .filter(|n| n != &self.runner)
            .collect();
        let recv_accounting: Arc<Mutex<RecvAccounting>> =
            Arc::new(Mutex::new(RecvAccounting::default()));
        let window_gate = Arc::new(WindowGate::with_expected_peers(&expected_peers));

        // Clone the session for the ack emitter task; `zenoh::Session`
        // is cheap to clone (it's an Arc-wrapped handle internally) and
        // sharing the session is the established pattern in this file
        // (the JoinSet above also clones it for parallel declares).
        let session_for_ack_emitter = session.clone();

        let pub_state = PublisherState {
            session,
            publishers_drop: predeclared_publishers_drop,
            publishers_block: predeclared_publishers_block,
        };

        runtime.spawn(publisher_task(pub_state, send_rx, trace));
        runtime.spawn(subscriber_task(
            subscriber,
            recv_tx,
            Arc::clone(&recv_accounting),
            self.runner.clone(),
            shutdown_rx,
            trace,
        ));
        runtime.spawn(eot_subscriber_task(
            eot_subscriber,
            eot_tx,
            self.runner.clone(),
            eot_shutdown_rx,
            trace,
        ));
        // T17.8: ack publisher (50 ms heartbeat) and ack subscriber
        // (consumes peer watermarks into the WindowGate). Both run on
        // the same tokio runtime as the data path; the emitter shares
        // the data session via clone so peer discovery / route
        // resolution is paid once.
        runtime.spawn(ack_emitter_task(
            session_for_ack_emitter,
            self.runner.clone(),
            Arc::clone(&recv_accounting),
            ack_shutdown_rx,
            trace,
        ));
        runtime.spawn(ack_subscriber_task(
            ack_subscriber,
            Arc::clone(&window_gate),
            self.runner.clone(),
            ack_sub_shutdown_rx,
            trace,
        ));

        // T16.5: hold the connect path open for a fixed window so the
        // 1000-key subscriber/publisher declarations have a chance to
        // propagate to the peer's session state before the driver enters
        // stabilize/operate. Without this, the 1000-path full-matrix
        // run reproducibly showed 0.00 % one-direction delivery for both
        // QoS 1 (Drop) and QoS 4 (Block) -- the publishers had no route
        // for the freshly-declared keys at the moment the first tick
        // fired. See CONNECT_PROPAGATION_SETTLE_MS docstring for full
        // rationale.
        let settle_ms = CONNECT_PROPAGATION_SETTLE_MS;
        if settle_ms > 0 {
            runtime.block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
            });
            trace_if!(
                trace,
                "connect: settled {} ms for declaration propagation",
                settle_ms
            );
        }

        self.runtime = Some(runtime);
        self.send_tx = Some(send_tx);
        self.recv_rx = Some(recv_rx);
        self.shutdown_tx = Some(shutdown_tx);
        self.eot_shutdown_tx = Some(eot_shutdown_tx);
        self.eot_rx = Some(eot_rx);
        self.eot_seen.clear();
        self.window_gate = Some(window_gate);
        self.ack_shutdown_tx = Some(ack_shutdown_tx);
        self.ack_sub_shutdown_tx = Some(ack_sub_shutdown_tx);
        self.connected_mode = Some(ThreadingMode::Multi);

        trace_if!(trace, "connect: total {} ms", t0.elapsed().as_millis());
        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        // T14.9b: Single mode routes through the sidecar's REST
        // plugin. HTTP PUT is synchronous + blocking; the call
        // graph from here is `ureq` -> `std::net` -> `WinSock` /
        // BSD sockets -- no tokio, no async.
        if self.connected_mode == Some(ThreadingMode::Single) {
            let key = path_to_key(path).to_string();
            let encoded = MessageCodec::encode(&self.runner, seq, qos, path, payload);
            let publisher = self
                .rest_publisher
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Single mode publisher not initialised"))?;
            return publisher.put(&key, encoded.to_vec());
        }
        let trace = self.zenoh_args.debug_trace;

        // T17.8: application-level peer-coordinated back-pressure gate.
        // For QoS 3 / QoS 4 (the strict-no-skip tiers per DESIGN.md
        // § 6.5) we block here -- on the driver thread, by design
        // (see CUSTOM.md "Peer-coordinated back-pressure (T17.8)") --
        // until every known peer has acknowledged decoding up to
        // `seq - QOS_STRICT_WINDOW`. The gate is a no-op until the
        // first ack from at least one peer arrives, so solo / loopback
        // configurations and the warm-up window of a fresh spawn run
        // unthrottled. QoS 1/2 (best-effort) traffic bypasses the gate
        // entirely; the previous bridge-mpsc `try_send` short-circuit
        // remains the back-pressure surface there.
        if matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp) {
            if let Some(gate) = self.window_gate.as_ref() {
                gate.wait_for_window(seq)?;
            }
        }

        let send_tx = self
            .send_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        let key = path_to_key(path).to_string();
        let encoded = MessageCodec::encode(&self.runner, seq, qos, path, payload);

        // When trace is on, log ENTER/EXIT for every publish past the warm-up
        // (publish_count >= 150) so we can pinpoint a hanging send. Below
        // the threshold we only emit a periodic summary every 50.
        let log_each = trace && self.publish_count >= 150;
        if log_each {
            trace_now!(
                "publish: ENTER seq={} key={} count={}",
                seq,
                key,
                self.publish_count
            );
        }
        let t = Instant::now();

        // try_send first to keep the hot path lock-free; only fall back to
        // blocking_send when the channel is full (deliberate back-pressure
        // — this means the publisher task hasn't drained yet, and the only
        // way to make progress without unbounded memory growth is to wait).
        let outbound = OutboundMessage::Data {
            key: key.clone(),
            encoded,
            seq,
            qos,
        };
        let send_result = match send_tx.try_send(outbound) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(msg)) => {
                if log_each {
                    trace_now!("publish: channel full seq={} -- back-pressuring", seq);
                }
                send_tx
                    .blocking_send(msg)
                    .map_err(|_| anyhow::anyhow!("publish channel closed"))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(anyhow::anyhow!("publish channel closed"))
            }
        };
        send_result?;

        let elapsed_us = t.elapsed().as_micros();
        if log_each {
            trace_now!("publish: EXIT  seq={} took {} us", seq, elapsed_us);
        }

        if trace {
            self.publish_count += 1;
            self.publish_total_us += elapsed_us;
            if elapsed_us > self.publish_max_us {
                self.publish_max_us = elapsed_us;
            }
            if elapsed_us > 1_000 {
                trace_now!(
                    "publish: SLOW seq={} key={} took {} us",
                    seq,
                    key,
                    elapsed_us
                );
            }
            if self.publish_count.is_multiple_of(50) {
                let avg = self.publish_total_us / u128::from(self.publish_count);
                trace_now!(
                    "publish: count={} avg={} us max={} us last_seq={}",
                    self.publish_count,
                    avg,
                    self.publish_max_us,
                    seq
                );
            }
        }

        Ok(())
    }

    /// Backpressure-aware publish for Zenoh (T-impl.7).
    ///
    /// - **QoS 1 / QoS 2 (best-effort / latest-value)**: encode the
    ///   message on the variant's main thread and `try_send` it onto
    ///   the bounded bridge channel. If the channel is full we report
    ///   `Ok(false)` and the driver logs `backpressure_skipped`
    ///   instead of letting `publish`'s `blocking_send` stall the
    ///   write loop. The downstream publisher uses
    ///   `CongestionControl::Drop` so Zenoh itself may silently drop
    ///   messages once they're accepted by our bridge -- those internal
    ///   drops are NOT counted in `backpressure_skipped` and have to
    ///   be inferred from receive-side delivery rate (Zenoh 1.9 does
    ///   not expose a public dropped-message counter on the
    ///   Publisher). See CUSTOM.md "Backpressure semantics (T-impl.7)"
    ///   for the trade-off rationale.
    /// - **QoS 3 / QoS 4 (reliable)**: delegate to `publish`. The
    ///   downstream publisher uses `CongestionControl::Block` so
    ///   `publisher.put(...).await` back-pressures inside Zenoh's
    ///   queue; the bridge channel may also back-pressure via
    ///   `blocking_send` upstream of that. Either way the driver sees
    ///   `Ok(true)` and no seq gap, which is the reliable-QoS
    ///   contract.
    fn try_publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<bool> {
        // T14.9b: Single mode delegates to `publish` for simplicity.
        // The REST plugin's PUT path does not surface a backpressure
        // signal we can use to short-circuit (it's a blocking
        // HTTP request); the closest analogue is "PUT took longer
        // than expected", which we already cap via the ureq agent's
        // global timeout. Returning `Ok(true)` whenever the PUT
        // succeeded keeps the contract symmetric with the
        // Multi-mode reliable path (QoS 3/4 there is also always
        // `Ok(true)` once `publisher.put().await` resolves).
        if self.connected_mode == Some(ThreadingMode::Single) {
            self.publish(path, payload, qos, seq)?;
            return Ok(true);
        }
        // Reliable path: full delegation to publish() which uses
        // try_send + blocking_send fallback. Publish-side back-pressure
        // is absorbed inside Zenoh's per-publisher Block queue.
        if matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp) {
            self.publish(path, payload, qos, seq)?;
            return Ok(true);
        }

        let trace = self.zenoh_args.debug_trace;
        let send_tx = self
            .send_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        let key = path_to_key(path).to_string();
        let encoded = MessageCodec::encode(&self.runner, seq, qos, path, payload);
        let outbound = OutboundMessage::Data {
            key: key.clone(),
            encoded,
            seq,
            qos,
        };

        match send_tx.try_send(outbound) {
            Ok(()) => {
                if trace {
                    self.publish_count += 1;
                }
                Ok(true)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Bridge channel is full -- the publisher task hasn't
                // drained yet. This is the honest backpressure signal
                // we surface to the driver for QoS 1/2: refuse the
                // write rather than blocking, so the driver logs a
                // `backpressure_skipped` event and the seq gap is
                // recorded explicitly instead of being hidden behind
                // a tick-stretching `blocking_send`.
                if trace {
                    trace_now!(
                        "try_publish: bridge channel full seq={} qos={:?} -- Ok(false)",
                        seq,
                        qos
                    );
                }
                Ok(false)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(anyhow::anyhow!("publish channel closed"))
            }
        }
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        // T14.9b: Single mode drains the SSE reader's mpsc. The
        // dedicated reader thread (started in connect(Single)) parses
        // the JSON-wrapped + base64-encoded payload off the SSE
        // stream and pushes decoded `ReceivedUpdate`s here. Same
        // try_recv shape as the established log-from-reader (T14.10)
        // and progress_coord (T15.3) patterns.
        if self.connected_mode == Some(ThreadingMode::Single) {
            let reader = self
                .sse_reader
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Single mode SSE reader not initialised"))?;
            return reader.try_recv();
        }
        let trace = self.zenoh_args.debug_trace;
        let recv_rx = self
            .recv_rx
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        match recv_rx.try_recv() {
            Ok(update) => {
                if trace {
                    self.poll_recv_count += 1;
                    if self.poll_recv_count.is_multiple_of(1000) {
                        trace_now!(
                            "poll_receive: recv_count={} poll_count={}",
                            self.poll_recv_count,
                            self.poll_count
                        );
                    }
                }
                Ok(Some(update))
            }
            Err(mpsc::error::TryRecvError::Empty) => {
                if trace {
                    self.poll_count += 1;
                }
                Ok(None)
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                // Subscriber task ended. Treat as no-data so the driver
                // can finish its tick gracefully; subsequent calls will
                // see the same.
                Ok(None)
            }
        }
    }

    // T15.8: signal_end_of_test / poll_peer_eots removed from the Variant
    // trait. The on-wire EOT exchange is no longer used.

    fn disconnect(&mut self) -> Result<()> {
        let trace = self.zenoh_args.debug_trace;
        let t0 = Instant::now();

        // T14.9b: stop the SSE reader thread first (it blocks on the
        // sidecar's HTTP/SSE socket; closing the socket below would
        // surface as a connect-error retry loop without this stop).
        // Drop the publisher so the underlying ureq agent / TCP
        // socket releases cleanly. Both must precede the sidecar
        // kill so the threads don't observe the half-broken sidecar
        // state.
        if let Some(mut reader) = self.sse_reader.take() {
            reader.stop();
            trace_if!(trace, "disconnect: SSE reader stopped");
        }
        if self.rest_publisher.take().is_some() {
            trace_if!(trace, "disconnect: REST publisher dropped");
        }

        // T14.9a: tear down the zenohd sidecar if connect spawned
        // one. Done up-front so a panic in the runtime shutdown
        // path below still leaves no orphan zenohd. The Job Object
        // (Windows) / pre-exec hook (Linux) is belt-and-braces for
        // crash paths.
        if let Some(mut sidecar) = self.sidecar.take() {
            if let Err(e) = sidecar.stop() {
                trace_if!(trace, "disconnect: sidecar stop failed: {}", e);
            } else {
                trace_if!(trace, "disconnect: sidecar stopped");
            }
        }
        self.connected_mode = None;

        if trace {
            trace_now!(
                "disconnect: start; publishes={} avg_pub={} us max_pub={} us recv={} polls={}",
                self.publish_count,
                if self.publish_count > 0 {
                    self.publish_total_us / u128::from(self.publish_count)
                } else {
                    0
                },
                self.publish_max_us,
                self.poll_recv_count,
                self.poll_count,
            );
        }

        // Signal the data subscriber task to exit its select loop.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Signal the EOT subscriber task to exit independently.
        if let Some(tx) = self.eot_shutdown_tx.take() {
            let _ = tx.send(());
        }
        // T17.8: signal the ack subscriber and ack emitter tasks to
        // stop. Doing this BEFORE waking any thread parked in the
        // window gate (next call) so a fresh ack cannot land between
        // wake and gate-shutdown.
        if let Some(tx) = self.ack_sub_shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.ack_shutdown_tx.take() {
            let _ = tx.send(());
        }
        // T17.8: wake any driver thread still parked on the strict-QoS
        // window gate so disconnect doesn't deadlock against a
        // mid-publish wait. The driver should never call publish
        // again after disconnect, but `wait_for_window` is robust
        // against a stale call: it returns an error once
        // `shutting_down` is set.
        if let Some(ref gate) = self.window_gate {
            gate.shutdown();
        }
        self.window_gate = None;

        // Drop the publish sender — when the publisher task sees the
        // channel closed it will drain its publisher cache and close the
        // session before exiting.
        self.send_tx.take();

        // Drop the receive end before runtime shutdown so the subscriber
        // task isn't blocked on a try_send.
        self.recv_rx.take();
        // Drop the EOT receive end too.
        self.eot_rx.take();

        // Shut down the runtime with a bounded grace period. Anything
        // still pending after 2s gets cancelled — that matches the QUIC
        // variant's behaviour and keeps the disconnect bounded even if a
        // background put is wedged for some reason.
        if let Some(rt) = self.runtime.take() {
            let t = Instant::now();
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
            trace_if!(
                trace,
                "disconnect: runtime shut down in {} ms",
                t.elapsed().as_millis()
            );
        }

        trace_if!(trace, "disconnect: total {} ms", t0.elapsed().as_millis());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_codec_roundtrip() {
        let writer = "runner-a";
        let seq = 42;
        let qos = Qos::BestEffort;
        let path = "/bench/0";
        let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];

        let encoded = MessageCodec::encode(writer, seq, qos, path, &payload);
        let decoded = MessageCodec::decode(&encoded).unwrap();

        assert_eq!(decoded.writer, writer);
        assert_eq!(decoded.seq, seq);
        assert_eq!(decoded.qos, qos);
        assert_eq!(decoded.path, path);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_message_codec_empty_payload() {
        let encoded = MessageCodec::encode("w", 0, Qos::ReliableTcp, "/p", &[]);
        let decoded = MessageCodec::decode(&encoded).unwrap();

        assert_eq!(decoded.writer, "w");
        assert_eq!(decoded.seq, 0);
        assert_eq!(decoded.qos, Qos::ReliableTcp);
        assert_eq!(decoded.path, "/p");
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn test_message_codec_large_seq() {
        let encoded = MessageCodec::encode("x", u64::MAX, Qos::LatestValue, "/a/b/c", &[0xFF]);
        let decoded = MessageCodec::decode(&encoded).unwrap();

        assert_eq!(decoded.seq, u64::MAX);
    }

    #[test]
    fn test_message_codec_decode_too_short() {
        assert!(MessageCodec::decode(&[]).is_err());
        assert!(MessageCodec::decode(&[0]).is_err());
    }

    #[test]
    fn test_zenoh_args_defaults() {
        let args = ZenohArgs::parse(&[]).unwrap();
        assert_eq!(args.mode, "peer");
        assert!(args.listen.is_none());
        assert!(!args.debug_trace);
    }

    #[test]
    fn test_zenoh_args_debug_trace_flag() {
        let extra = vec!["--debug-trace".to_string()];
        let args = ZenohArgs::parse(&extra).unwrap();
        assert!(args.debug_trace);
        assert_eq!(args.mode, "peer");
    }

    #[test]
    fn test_zenoh_args_mode_and_listen() {
        let extra = vec![
            "--zenoh-mode".to_string(),
            "client".to_string(),
            "--zenoh-listen".to_string(),
            "tcp/0.0.0.0:7447".to_string(),
        ];
        let args = ZenohArgs::parse(&extra).unwrap();
        assert_eq!(args.mode, "client");
        assert_eq!(args.listen.as_deref(), Some("tcp/0.0.0.0:7447"));
    }

    #[test]
    fn test_zenoh_args_unknown_arg_is_lenient() {
        // Unknown `--<name>` tokens are silently skipped (treated as a
        // `--name value` pair if a following token exists) so the runner
        // can inject extra args like `--peers` without breaking Zenoh.
        let extra = vec!["--unknown".to_string()];
        let args = ZenohArgs::parse(&extra).unwrap();
        assert_eq!(args.mode, "peer");
        assert!(args.listen.is_none());
    }

    #[test]
    fn test_zenoh_args_peers_injection_ignored() {
        let extra = vec![
            "--peers".to_string(),
            "alice=127.0.0.1,bob=192.168.1.10".to_string(),
        ];
        let args = ZenohArgs::parse(&extra).unwrap();
        assert_eq!(args.mode, "peer");
        assert!(args.listen.is_none());
    }

    #[test]
    fn test_zenoh_variant_name() {
        let v = ZenohVariant::new("a", &[]).unwrap();
        assert_eq!(v.name(), "zenoh");
    }

    /// T14.9b: Zenoh now supports BOTH threading modes. Multi keeps
    /// the in-process zenoh crate with its internal tokio runtime;
    /// Single talks to an out-of-process zenohd sidecar via the
    /// REST plugin (HTTP PUT + SSE) and is genuinely tokio-free in
    /// the variant's call graph.
    #[test]
    fn test_supported_threading_modes_is_single_and_multi() {
        let v = ZenohVariant::new("a", &[]).unwrap();
        let modes = v.supported_threading_modes();
        assert_eq!(modes, &[ThreadingMode::Single, ThreadingMode::Multi]);
    }

    /// T14.9a: `connect(Single)` no longer aborts pre-I/O -- it now
    /// spawns the zenohd sidecar (lifecycle only; the RPC client is
    /// T14.9b). Two outcomes are valid depending on the test host:
    ///
    /// 1. **`zenohd` not installed**: discovery errors with a clear
    ///    message naming `ZENOHD_PATH` and the install command.
    ///    No tokio runtime / session is set up.
    /// 2. **`zenohd` installed**: the sidecar spawns, the variant
    ///    records `connected_mode = Single`, and publish /
    ///    poll_receive return the "not yet implemented (T14.9b)"
    ///    error. No tokio runtime / session is set up (those are
    ///    Multi mode infrastructure).
    ///
    /// Either way the bridge handles must remain `None` because
    /// Single mode does NOT exercise the Multi-mode bridge.
    #[test]
    fn test_connect_single_mode_spawns_sidecar_or_errors_cleanly() {
        // Force the "no binary" path so this test is hermetic on any
        // CI without zenohd installed. We point ZENOHD_PATH at a
        // non-existent file; the variant should surface the
        // actionable error rather than falling through to PATH.
        let nonexistent = std::env::temp_dir().join("variant-zenoh-test-no-such-zenohd");
        let _ = std::fs::remove_file(&nonexistent);
        let prev = std::env::var_os("ZENOHD_PATH");
        // SAFETY (test-only): mutating env is fine in a single test
        // because the harness runs unit tests on a single thread by
        // default; we restore the previous value at the end.
        unsafe {
            std::env::set_var("ZENOHD_PATH", &nonexistent);
        }

        let mut v = ZenohVariant::new("a", &[]).expect("construct ZenohVariant");
        let err = v
            .connect(variant_base::ThreadingMode::Single)
            .expect_err("connect(Single) must error when zenohd is not findable");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ZENOHD_PATH"),
            "error should mention ZENOHD_PATH, got: {msg}",
        );
        assert!(
            msg.contains("cargo install zenohd"),
            "error should suggest install command, got: {msg}",
        );

        // No Multi-mode infrastructure should have been touched.
        assert!(v.runtime.is_none(), "no tokio runtime in Single mode");
        assert!(v.send_tx.is_none(), "no publish channel in Single mode");
        assert!(v.recv_rx.is_none(), "no receive channel in Single mode");
        assert!(v.shutdown_tx.is_none());
        assert!(v.eot_shutdown_tx.is_none());
        assert!(v.eot_rx.is_none());
        // The connect call errored before the sidecar handle could
        // be installed, so it remains None too.
        assert!(
            v.sidecar.is_none(),
            "sidecar must be None when discovery failed"
        );
        assert!(v.connected_mode.is_none(), "connected_mode unset on error");

        // Restore env. Tests share the process; do not leak.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ZENOHD_PATH", v),
                None => std::env::remove_var("ZENOHD_PATH"),
            }
        }
    }

    #[test]
    fn test_path_to_key_strips_leading_slash() {
        // Workload paths arrive as `/bench/N`; the derived key must be
        // `bench/N` (no leading slash, no double `bench/` prefix).
        // Regression-protect for the bug fixed in T10.2b (DECISIONS.md D7).
        assert_eq!(path_to_key("/bench/0"), "bench/0");
        assert_eq!(path_to_key("/bench/999"), "bench/999");
        assert_eq!(path_to_key("bench/42"), "bench/42");
    }

    #[test]
    fn test_publisher_key_matches_subscriber_wildcard() {
        // The key derived from a workload path MUST be matched by the
        // wildcard the subscriber is declared with — otherwise we'd
        // publish into a void. This guards against accidental drift if
        // either `path_to_key` or `SUBSCRIBER_WILDCARD` is changed in
        // isolation in a future edit.
        use zenoh::key_expr::KeyExpr;

        let wildcard = KeyExpr::try_from(SUBSCRIBER_WILDCARD)
            .expect("SUBSCRIBER_WILDCARD is a valid Zenoh key expression");

        for path in [
            "/bench/0",
            "/bench/1",
            "/bench/999",
            "/bench/12345",
            "bench/0",
        ] {
            let key = path_to_key(path);
            let key_expr = KeyExpr::try_from(key)
                .unwrap_or_else(|e| panic!("derived key {key:?} is not a valid keyexpr: {e}"));
            assert!(
                wildcard.intersects(&key_expr),
                "wildcard {SUBSCRIBER_WILDCARD:?} does not match key {key:?} (from path {path:?})",
            );
        }
    }

    #[test]
    fn test_eot_key_for_round_trips_through_writer_extraction() {
        // Construction + extraction must be inverses for any non-empty
        // runner name. This guards the contract:
        //   key == EOT_KEY_PREFIX + writer  =>  writer_from_eot_key(&key) == Some(writer)
        for writer in ["alice", "bob", "runner-a", "node_42", "x"] {
            let key = eot_key_for(writer);
            assert!(
                key.starts_with(EOT_KEY_PREFIX),
                "constructed key {key:?} must begin with the EOT prefix"
            );
            assert_eq!(writer_from_eot_key(&key), Some(writer));
        }
    }

    #[test]
    fn test_eot_key_matches_eot_wildcard() {
        // Every key produced by `eot_key_for` MUST be matched by
        // `EOT_WILDCARD` -- otherwise the EOT subscriber would never see
        // outbound EOTs.
        use zenoh::key_expr::KeyExpr;

        let wildcard =
            KeyExpr::try_from(EOT_WILDCARD).expect("EOT_WILDCARD is a valid Zenoh key expression");

        for writer in ["alice", "bob", "runner-a", "node42"] {
            let key = eot_key_for(writer);
            let key_expr = KeyExpr::try_from(key.as_str())
                .unwrap_or_else(|e| panic!("EOT key {key:?} is not a valid keyexpr: {e}"));
            assert!(
                wildcard.intersects(&key_expr),
                "wildcard {EOT_WILDCARD:?} does not match key {key:?} (writer={writer:?})",
            );
        }
    }

    #[test]
    fn test_writer_from_eot_key_rejects_bad_keys() {
        // Keys without the prefix or with no writer suffix must yield None
        // so the EOT subscriber task can drop them without panicking.
        assert_eq!(writer_from_eot_key("bench/0"), None);
        assert_eq!(writer_from_eot_key(""), None);
        assert_eq!(writer_from_eot_key("bench/__eot__/"), None);
        assert_eq!(writer_from_eot_key("bench/__eot/x"), None);
    }

    #[test]
    fn test_eot_payload_encode_decode_roundtrip() {
        // 8-byte big-endian per the contract.
        for id in [0u64, 1, 42, u64::MAX, 0xDEADBEEF_CAFEBABE] {
            let bytes = encode_eot_payload(id);
            assert_eq!(bytes.len(), 8);
            // Big-endian: the high byte is at index 0.
            assert_eq!(bytes[0], (id >> 56) as u8);
            assert_eq!(bytes[7], id as u8);
            assert_eq!(decode_eot_payload(&bytes), Some(id));
        }
    }

    #[test]
    fn test_eot_payload_decode_rejects_wrong_length() {
        // Anything other than exactly 8 bytes is invalid.
        assert_eq!(decode_eot_payload(&[]), None);
        assert_eq!(decode_eot_payload(&[1, 2, 3]), None);
        assert_eq!(decode_eot_payload(&[0; 7]), None);
        assert_eq!(decode_eot_payload(&[0; 9]), None);
        assert_eq!(decode_eot_payload(&[0; 16]), None);
    }

    // T15.8: removed tests `test_poll_peer_eots_dedups_repeated_pairs`
    // and `test_poll_peer_eots_returns_empty_when_disconnected`. They
    // exercised the poll_peer_eots trait method that no longer exists.

    /// Stress test for the bridge: publish 10000 messages back-to-back
    /// through a connected ZenohVariant in single-process loopback and
    /// verify they all land in the receive channel. Gated `#[ignore]`
    /// because it's slower than the rest of the unit suite (spins up a
    /// real Zenoh peer and a tokio runtime); run with
    /// `cargo test --release -- --ignored zenoh_bridge_stress`.
    #[test]
    #[ignore]
    fn zenoh_bridge_stress_10000_messages() {
        const N: u64 = 10_000;

        let mut variant = ZenohVariant::new("stress-runner", &[]).expect("construct variant");
        variant
            .connect(variant_base::ThreadingMode::Multi)
            .expect("connect");

        // Give Zenoh a moment to warm up its loopback discovery before we
        // start measuring delivery — without a brief settle the first
        // dozens of puts can race the subscriber declaration.
        std::thread::sleep(std::time::Duration::from_millis(500));

        for seq in 0..N {
            let path = format!("/bench/{}", seq % 1000);
            variant
                .publish(&path, &[0u8, 1, 2, 3, 4, 5, 6, 7], Qos::BestEffort, seq)
                .expect("publish");
        }

        // Drain receives with a deadline. We tolerate some loss here —
        // the bridge documents that try_send drops when the receive
        // channel is full — but require a strong majority to confirm
        // the bridge isn't deadlocking under sustained pressure.
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        let mut received = 0u64;
        while Instant::now() < deadline && received < N {
            match variant.poll_receive().expect("poll_receive") {
                Some(_) => received += 1,
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }

        variant.disconnect().expect("disconnect");

        // The bridge must not deadlock and must deliver the bulk of the
        // workload. We assert >=80% rather than 100% because Zenoh's
        // CongestionControl::Drop is in effect and the receive channel
        // can drop under pressure -- both of those are acceptable, but
        // a deadlock or a >50% loss would indicate a real regression.
        assert!(
            received as f64 / N as f64 >= 0.8,
            "bridge stress test received {received}/{N} -- bridge may be deadlocking or dropping excessively",
        );
    }

    /// T-impl.7: an un-connected `try_publish` returns an error rather
    /// than `Ok(false)`. Mirrors the QUIC variant's contract: the
    /// no-connection state is a user error, not a backpressure signal.
    #[test]
    fn test_try_publish_without_connect_errors() {
        let mut variant = ZenohVariant::new("solo", &[]).expect("construct variant");
        let r = variant.try_publish("/bench/0", &[0u8; 8], Qos::BestEffort, 1);
        assert!(r.is_err(), "try_publish before connect must error");
    }

    /// T-impl.7: when the bridge mpsc channel is full,
    /// `try_publish` for QoS 1/2 MUST return `Ok(false)`. We exercise
    /// the path WITHOUT a real Zenoh session by swapping in our own
    /// `(tx, rx)` pair sized identically to the production bridge and
    /// then dropping the receiver -- `try_send` will then fail with
    /// `Full` once the channel saturates. The test does not need a
    /// running runtime; it isolates the synchronous `try_publish` logic.
    #[test]
    fn test_try_publish_qos1_returns_ok_false_when_channel_full() {
        let mut variant = ZenohVariant::new("solo", &[]).expect("construct variant");

        // Wire a tiny bridge channel directly into the variant. Capacity
        // 2 ensures we hit Full quickly. We deliberately keep the
        // receiver alive (in a held variable) so try_send returns
        // `Full` rather than `Closed`.
        let (tx, _rx_held) = mpsc::channel::<OutboundMessage>(2);
        variant.send_tx = Some(tx);

        // First two sends should fit (Ok(true)). The third must
        // observe a full channel and return Ok(false).
        for seq in 0..2u64 {
            let r = variant
                .try_publish("/bench/0", &[0u8; 8], Qos::BestEffort, seq)
                .expect("try_publish should not error while there is room");
            assert!(r, "fill-up send {} should have returned Ok(true)", seq);
        }
        let r = variant
            .try_publish("/bench/0", &[0u8; 8], Qos::BestEffort, 99)
            .expect("try_publish should not error when channel is full");
        assert!(
            !r,
            "try_publish must return Ok(false) when the bridge channel is full"
        );

        // QoS 2 (LatestValue) takes the same path -- assert consistent
        // behaviour. Capacity is already saturated so we expect
        // Ok(false) without ever filling further.
        let r = variant
            .try_publish("/bench/0", &[0u8; 8], Qos::LatestValue, 100)
            .expect("try_publish should not error when channel is full");
        assert!(
            !r,
            "try_publish QoS 2 must return Ok(false) when the bridge channel is full"
        );
    }

    /// T-impl.7: QoS 3/4 (reliable) MUST never produce `Ok(false)`.
    /// The reliable path delegates to `publish`, which uses
    /// `try_send` then `blocking_send`. We model "channel under
    /// pressure but a consumer eventually drains" by spawning a
    /// background drain thread; the variant's main thread keeps
    /// pushing reliable writes and never sees Ok(false).
    #[test]
    fn test_try_publish_qos3_and_qos4_never_return_ok_false() {
        let mut variant = ZenohVariant::new("solo", &[]).expect("construct variant");

        // Match production channel capacity so the test mirrors the
        // real bridge timing pattern (occasional Full -> blocking_send
        // -> consumer drains -> writer continues).
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(PUBLISH_CHANNEL_CAPACITY);
        variant.send_tx = Some(tx);

        // Drain in a worker thread: receive each message and discard
        // it. No sleeps — the goal is to keep the channel from blocking
        // the writer indefinitely while still verifying the reliable
        // path's Ok(true) contract under a brief burst.
        let drain_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("drain rt");
            rt.block_on(async move { while rx.recv().await.is_some() {} });
        });

        // Burst reliable writes spanning 2x the channel capacity to
        // exercise the try_send -> blocking_send fallback. We do not
        // care whether some end up calling blocking_send -- only that
        // the return value is always Ok(true).
        let burst = (PUBLISH_CHANNEL_CAPACITY as u64) * 2;
        for seq in 0..burst {
            for qos in [Qos::ReliableUdp, Qos::ReliableTcp] {
                let r = variant
                    .try_publish("/bench/0", &[0u8; 8], qos, seq)
                    .expect("try_publish reliable should not error");
                assert!(
                    r,
                    "try_publish for qos {:?} seq {} must return Ok(true)",
                    qos, seq
                );
            }
        }

        // Drop the sender to let the drain task finish.
        variant.send_tx.take();
        drain_handle.join().expect("drain thread join");
    }

    /// T-impl.7 default-path sanity: `try_publish` with an empty
    /// channel returns `Ok(true)` and pushes onto the bridge. We
    /// confirm by pulling the message off the receive side.
    #[test]
    fn test_try_publish_qos1_default_path_returns_ok_true() {
        let mut variant = ZenohVariant::new("solo", &[]).expect("construct variant");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(PUBLISH_CHANNEL_CAPACITY);
        variant.send_tx = Some(tx);

        let r = variant
            .try_publish("/bench/0", &[1u8, 2, 3, 4], Qos::BestEffort, 42)
            .expect("try_publish should succeed");
        assert!(r, "empty channel must accept the write");

        // The message must have been enqueued with the right qos tag
        // so the publisher_task can route it to the Drop cache.
        let msg = rx.try_recv().expect("message should be enqueued");
        match msg {
            OutboundMessage::Data { qos, seq, key, .. } => {
                assert_eq!(qos, Qos::BestEffort);
                assert_eq!(seq, 42);
                assert_eq!(key, "bench/0");
            }
            OutboundMessage::Eot { .. } => panic!("unexpected EOT message"),
        }
    }

    // ---- T17.8 tests: peer-coordinated back-pressure ----

    #[test]
    fn test_ack_key_round_trip_and_wildcard_intersection() {
        use zenoh::key_expr::KeyExpr;

        // Composed key must parse back to the same `(receiver, writer)`
        // pair, and the writer's wildcard subscription
        // `bench/__ack__/*/<self>` must intersect every key targeted
        // at `self`. Together these are the on-wire contract the ack
        // emitter (writes) and ack subscriber (reads) implement.
        for (receiver, writer) in [("bob", "alice"), ("runner-2", "runner-1"), ("c", "d-7")] {
            let key = ack_key_for(receiver, writer);
            let (r, w) = parse_ack_key(&key).expect("ack key must round-trip");
            assert_eq!(r, receiver, "receiver parsed from {key:?}");
            assert_eq!(w, writer, "writer parsed from {key:?}");

            let wildcard = ack_wildcard_for_self(writer);
            let wildcard_expr =
                KeyExpr::try_from(wildcard.as_str()).expect("ack wildcard is a valid keyexpr");
            let key_expr = KeyExpr::try_from(key.as_str()).expect("ack key is a valid keyexpr");
            assert!(
                wildcard_expr.intersects(&key_expr),
                "ack wildcard {wildcard:?} must match key {key:?}"
            );
        }
    }

    #[test]
    fn test_parse_ack_key_rejects_bad_shapes() {
        // Wrong prefix, missing segments, empty segments, and the
        // `bench/__eot__/...` family must all yield None so the
        // ack subscriber can drop them without panicking. (The
        // dedicated EOT subscriber owns those keys.)
        assert_eq!(parse_ack_key("bench/0"), None);
        assert_eq!(parse_ack_key(""), None);
        assert_eq!(parse_ack_key(ACK_KEY_PREFIX), None);
        assert_eq!(parse_ack_key("bench/__ack__/alice"), None);
        assert_eq!(parse_ack_key("bench/__ack__//bob"), None);
        assert_eq!(parse_ack_key("bench/__ack__/alice/"), None);
        // Extra path components (writer segment containing '/') are
        // not part of the contract; reject so a future schema
        // change doesn't silently produce wrong (peer, writer) pairs.
        assert_eq!(parse_ack_key("bench/__ack__/alice/bob/extra"), None);
        assert_eq!(parse_ack_key("bench/__eot__/alice"), None);
    }

    #[test]
    fn test_ack_payload_encode_decode_roundtrip() {
        // 8-byte big-endian per the contract (same encoding as the
        // EOT payload — the two side-channels share a payload
        // codec so a future audit only needs to verify one shape).
        for v in [0u64, 1, 42, u64::MAX, 0x1234_5678_9abc_def0_u64] {
            let bytes = encode_ack_payload(v);
            assert_eq!(bytes.len(), 8);
            assert_eq!(bytes[0], (v >> 56) as u8);
            assert_eq!(bytes[7], v as u8);
            assert_eq!(decode_ack_payload(&bytes), Some(v));
        }
    }

    #[test]
    fn test_ack_payload_decode_rejects_wrong_length() {
        assert_eq!(decode_ack_payload(&[]), None);
        assert_eq!(decode_ack_payload(&[1, 2, 3]), None);
        assert_eq!(decode_ack_payload(&[0; 7]), None);
        assert_eq!(decode_ack_payload(&[0; 9]), None);
        assert_eq!(decode_ack_payload(&[0; 16]), None);
    }

    #[test]
    fn test_window_gate_no_peers_never_blocks() {
        // T17.8: with no peer acks observed, the window gate is a
        // no-op. This is the solo / loopback / pre-first-ack
        // warm-up case. The gate must NOT block here -- if it did,
        // solo runs would deadlock at the first publish.
        let gate = WindowGate::new();
        // Far above QOS_STRICT_WINDOW; would block immediately if
        // any peer were known with watermark 0.
        for seq in [0u64, 1, 1_000, QOS_STRICT_WINDOW, QOS_STRICT_WINDOW * 100] {
            gate.wait_for_window(seq).expect("no peers -> no block");
        }
    }

    #[test]
    fn test_window_gate_within_window_passes() {
        // T17.8: once a peer has acked, the gate allows
        // `next_seq <= ack + WINDOW`. Exercise the boundary
        // exactly (== ack + WINDOW must pass; > ack + WINDOW
        // must block, which we test separately).
        let gate = WindowGate::new();
        gate.apply_ack("alice", 100);
        // 100 (the ack) up to 100 + WINDOW (inclusive) must pass.
        gate.wait_for_window(100).expect("at-ack must pass");
        gate.wait_for_window(100 + QOS_STRICT_WINDOW)
            .expect("ack + WINDOW boundary must pass");
        // A lower seq always passes (the writer can publish any
        // already-acked-or-earlier seq trivially).
        gate.wait_for_window(50).expect("below ack must pass");
    }

    #[test]
    fn test_window_gate_blocks_until_ack_arrives() {
        // T17.8: when the writer outruns the peer by more than
        // WINDOW, the gate parks. An ack arriving on another
        // thread must wake the gate. This test asserts the
        // wait-then-wake handshake without relying on tokio.
        let gate = Arc::new(WindowGate::new());
        gate.apply_ack("alice", 0);
        // next_seq = WINDOW + 1 puts us 1 over the budget.
        let target_seq = QOS_STRICT_WINDOW + 1;

        let gate_for_waiter = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || {
            gate_for_waiter
                .wait_for_window(target_seq)
                .expect("wait_for_window should succeed once ack arrives")
        });
        // Give the waiter a moment to park in the condvar. If we
        // ack instantly the waiter might not yet have grabbed the
        // mutex; the gate would still wake on the next condvar
        // check, but the test would be timing-fragile. A short
        // sleep keeps the order deterministic.
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Advance the peer's watermark by 1 -- now next_seq is
        // exactly at the boundary `ack + WINDOW` (1 + WINDOW), so
        // the gate releases.
        gate.apply_ack("alice", 1);
        // The waiter must complete within a wallclock budget
        // proportional to the condvar wake-interval (100 ms). 2 s
        // is generous on CI without permitting an actual deadlock
        // to pass silently.
        let start = std::time::Instant::now();
        loop {
            if waiter.is_finished() {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(2) {
                panic!("window gate did not release within 2 s after ack");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        waiter.join().expect("waiter thread joined");
    }

    #[test]
    fn test_window_gate_shutdown_unblocks_waiters() {
        // T17.8: a parked waiter must exit cleanly on disconnect.
        // The variant calls `gate.shutdown()` from `disconnect()`;
        // any thread parked in `wait_for_window` must observe the
        // shutdown flag and return Err rather than hang forever.
        let gate = Arc::new(WindowGate::new());
        gate.apply_ack("alice", 0);
        let target_seq = QOS_STRICT_WINDOW + 1;
        let gate_for_waiter = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || gate_for_waiter.wait_for_window(target_seq));
        std::thread::sleep(std::time::Duration::from_millis(20));
        gate.shutdown();
        let start = std::time::Instant::now();
        loop {
            if waiter.is_finished() {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(2) {
                panic!("window gate did not abort on shutdown within 2 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let result = waiter.join().expect("waiter thread joined");
        assert!(
            result.is_err(),
            "shutdown path must propagate Err from wait_for_window"
        );
    }

    #[test]
    fn test_window_gate_ack_does_not_regress() {
        // T17.8: a stale ack (lower than current watermark) must
        // not roll back the peer's progress. Zenoh delivers Drop
        // samples in arrival order, but a slow-to-route stale ack
        // could in principle still surface; if it did and we
        // honoured it, the gate would slam shut and the publisher
        // would stall.
        let gate = WindowGate::new();
        gate.apply_ack("alice", 1000);
        gate.apply_ack("alice", 500); // stale; should be ignored
                                      // 1000 + WINDOW must still pass.
        gate.wait_for_window(1000 + QOS_STRICT_WINDOW)
            .expect("stale ack must not regress the watermark");
    }

    #[test]
    fn test_window_gate_min_across_peers() {
        // T17.8: the gate uses the *slowest* peer's watermark.
        // Otherwise a fast peer could let the writer outrun a
        // slow peer, which is exactly what the protocol must
        // prevent.
        let gate = WindowGate::new();
        gate.apply_ack("alice", 10_000);
        gate.apply_ack("bob", 100);
        // Bob is the binding constraint: window allows up to
        // 100 + WINDOW; 10_000 + WINDOW would be wrong.
        gate.wait_for_window(100 + QOS_STRICT_WINDOW)
            .expect("at bob's boundary must pass");
        // Construct a parked waiter at 100 + WINDOW + 1; it must
        // remain blocked while only alice advances.
        let gate_arc = Arc::new(gate);
        let waiter_gate = Arc::clone(&gate_arc);
        let target_seq = 100 + QOS_STRICT_WINDOW + 1;
        let waiter = std::thread::spawn(move || waiter_gate.wait_for_window(target_seq));
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Alice racing ahead must NOT release bob's gate.
        gate_arc.apply_ack("alice", 1_000_000);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            !waiter.is_finished(),
            "min-across-peers gate must not release on a fast peer's progress alone"
        );
        // Bob catching up must release the gate.
        gate_arc.apply_ack("bob", 101);
        let start = std::time::Instant::now();
        loop {
            if waiter.is_finished() {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(2) {
                panic!("gate did not release within 2 s after slow peer advanced");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        waiter
            .join()
            .expect("waiter thread joined")
            .expect("wait_for_window succeeds on bob advance");
    }

    /// T17.8 contract: at QoS 3/4 `publish` must NEVER return
    /// `Ok(false)` (it returns `Ok(())`, but the failure mode the
    /// contract forbids is "skip without delivery"). With no peers
    /// known, the gate is a no-op so the write reaches the bridge
    /// channel.
    #[test]
    fn test_publish_qos3_no_peers_known_does_not_block() {
        let mut variant = ZenohVariant::new("solo", &[]).expect("construct variant");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(PUBLISH_CHANNEL_CAPACITY);
        variant.send_tx = Some(tx);
        variant.window_gate = Some(Arc::new(WindowGate::new()));

        for seq in 0..16u64 {
            variant
                .publish("/bench/0", &[0u8; 8], Qos::ReliableUdp, seq)
                .expect("publish should succeed");
        }
        // Confirm 16 messages landed in the bridge.
        for _ in 0..16 {
            let msg = rx.try_recv().expect("message should be enqueued");
            match msg {
                OutboundMessage::Data { qos, .. } => {
                    assert_eq!(qos, Qos::ReliableUdp);
                }
                OutboundMessage::Eot { .. } => panic!("unexpected EOT message"),
            }
        }
    }
}
