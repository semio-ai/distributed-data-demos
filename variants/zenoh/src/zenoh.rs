use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::Instant;

use anyhow::{Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::{Publisher, Subscriber};
use zenoh::sample::Sample;

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::variant_trait::{PeerEot, Variant};

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

/// Default capacity for the publish-side bounded channel.
///
/// Sized small (1024) on purpose so that genuine producer-faster-than-
/// consumer pressure shows up at the writer's `blocking_send` instead of
/// being absorbed into a deep queue (which inflates p95 latency and masks
/// the real stall). The earlier 8192 cap was tuned around the lazy
/// first-tick `declare_publisher` storm that T-zenoh.1 fixed by
/// pre-declaring publishers in `connect`; with declares out of the
/// operate hot path, the publisher task drains at line rate and a small
/// channel is sufficient.
const PUBLISH_CHANNEL_CAPACITY: usize = 1024;

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

/// Zenoh-specific CLI arguments parsed from the `extra` pass-through args.
pub struct ZenohArgs {
    pub mode: String,
    pub listen: Option<String>,
    /// When true, emit `[zenoh-trace]` lines on stderr for connect/publish
    /// hot path / poll_receive / disconnect. Off by default so production
    /// runs are quiet; enable by passing `--debug-trace` (no value).
    pub debug_trace: bool,
}

impl ZenohArgs {
    /// Parse Zenoh-specific arguments from the extra CLI args.
    pub fn parse(extra: &[String]) -> Result<Self> {
        let mut mode = String::from("peer");
        let mut listen = None;
        let mut debug_trace = false;

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
        })
    }
}

/// Outbound publish request shuttled from the variant's main thread to the
/// publisher task running on the dedicated tokio runtime.
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
struct PublisherState {
    session: zenoh::Session,
    publishers: HashMap<String, Publisher<'static>>,
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
    // Diagnostic counters used only when `zenoh_args.debug_trace` is true.
    publish_count: u64,
    publish_total_us: u128,
    publish_max_us: u128,
    poll_count: u64,
    poll_recv_count: u64,
}

impl ZenohVariant {
    /// Create a new Zenoh variant.
    ///
    /// `runner` is the runner name used as the writer field in messages.
    /// `extra` contains the pass-through CLI args for Zenoh-specific config.
    pub fn new(runner: &str, extra: &[String]) -> Result<Self> {
        let zenoh_args = ZenohArgs::parse(extra)?;
        Ok(Self {
            runner: runner.to_string(),
            zenoh_args,
            runtime: None,
            send_tx: None,
            recv_rx: None,
            shutdown_tx: None,
            eot_shutdown_tx: None,
            eot_rx: None,
            eot_seen: HashSet::new(),
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
    while let Some(msg) = send_rx.recv().await {
        match msg {
            OutboundMessage::Data { key, encoded, seq } => {
                // Standard hot path: publisher was pre-declared in
                // `connect` from the workload's known path set, so this
                // lookup is a HashMap hit and the put runs on a routine
                // tokio worker. The lazy-declare fallback below covers
                // workloads outside the standard `bench/0..N-1` scheme;
                // a missing entry on the standard fixture is unexpected
                // and surfaces as a trace warning so we notice if the
                // pre-declare contract drifts.
                if let Some(publisher) = state.publishers.get(&key) {
                    if let Err(e) = publisher.put(encoded).await {
                        if trace {
                            trace_now!(
                                "publisher_task: put failed seq={} key={} err={}",
                                seq,
                                key,
                                e
                            );
                        }
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
                            "publisher_task: lazy declare key={} (pre-declare missed)",
                            key
                        );
                    }
                    match state.session.declare_publisher(key.clone()).await {
                        Ok(publisher) => {
                            if let Err(e) = publisher.put(encoded).await {
                                if trace {
                                    trace_now!(
                                        "publisher_task: put failed seq={} key={} err={}",
                                        seq,
                                        key,
                                        e
                                    );
                                }
                            }
                            state.publishers.insert(key, publisher);
                        }
                        Err(e) => {
                            if trace {
                                trace_now!(
                                    "publisher_task: declare_publisher({}) failed: {}",
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

    // Channel closed: drain the publisher cache. Undeclaring explicitly
    // gives consistent teardown timing and surfaces errors via the trace
    // log; without this the publishers would Drop-undeclare on session
    // close, which is fine but less observable.
    let pub_count = state.publishers.len();
    let t = Instant::now();
    for (_, publisher) in state.publishers.drain() {
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
async fn subscriber_task(
    subscriber: Subscriber<FifoChannelHandler<Sample>>,
    recv_tx: mpsc::Sender<ReceivedUpdate>,
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

    fn connect(&mut self) -> Result<()> {
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
        let t_open = Instant::now();
        let (session, subscriber, eot_subscriber, predeclared_publishers) = runtime
            .block_on(async {
                let session = zenoh::open(config).await.map_err(zenoh_err)?;
                let subscriber = session
                    .declare_subscriber(SUBSCRIBER_WILDCARD)
                    .await
                    .map_err(zenoh_err)?;
                let eot_subscriber = session
                    .declare_subscriber(EOT_WILDCARD)
                    .await
                    .map_err(zenoh_err)?;

                // Pre-declare publishers for the workload's known path
                // set. Concurrent via JoinSet; results collected into
                // the publisher cache before the publisher task starts
                // draining the publish channel, so the operate phase
                // sees zero declares for the standard fixture.
                let mut publishers: HashMap<String, Publisher<'static>> =
                    HashMap::with_capacity(pre_declare_count as usize);
                if pre_declare_count > 0 {
                    let mut set: JoinSet<(String, zenoh::Result<Publisher<'static>>)> =
                        JoinSet::new();
                    for i in 0..pre_declare_count {
                        let key = format!("bench/{}", i);
                        let session_clone = session.clone();
                        let key_for_task = key.clone();
                        set.spawn(async move {
                            let res = session_clone.declare_publisher(key_for_task.clone()).await;
                            (key_for_task, res)
                        });
                    }
                    while let Some(joined) = set.join_next().await {
                        let (key, res) = joined.context("declare_publisher task panicked")?;
                        match res {
                            Ok(publisher) => {
                                publishers.insert(key, publisher);
                            }
                            Err(e) => {
                                // Don't fail connect on a single
                                // declare error -- fall back to the
                                // lazy path on first publish for that
                                // key. Zenoh's declare can fail
                                // transiently during scout warm-up.
                                eprintln!(
                                    "[zenoh] warning: pre-declare publisher for {} failed: {}",
                                    key, e
                                );
                            }
                        }
                    }
                }
                Ok::<_, anyhow::Error>((session, subscriber, eot_subscriber, publishers))
            })
            .context(
                "failed to open zenoh session / declare subscribers / pre-declare publishers",
            )?;
        trace_if!(
            trace,
            "connect: session opened + subscribers declared + {} publishers pre-declared in {} ms",
            predeclared_publishers.len(),
            t_open.elapsed().as_millis()
        );

        let (send_tx, send_rx) = mpsc::channel::<OutboundMessage>(PUBLISH_CHANNEL_CAPACITY);
        let (recv_tx, recv_rx) = mpsc::channel::<ReceivedUpdate>(RECEIVE_CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        // Separate shutdown oneshot for the EOT subscriber so each task
        // owns exactly one Receiver (oneshot::Receiver is single-consumer).
        let (eot_shutdown_tx, eot_shutdown_rx) = oneshot::channel::<()>();
        // EOT observations channel: small bound is sufficient because the
        // contract is one EOT per peer per spawn. 256 leaves ample
        // headroom for retries / late duplicates without unbounded
        // memory.
        let (eot_tx, eot_rx) = mpsc::channel::<(String, u64)>(256);

        let pub_state = PublisherState {
            session,
            publishers: predeclared_publishers,
        };

        runtime.spawn(publisher_task(pub_state, send_rx, trace));
        runtime.spawn(subscriber_task(subscriber, recv_tx, shutdown_rx, trace));
        runtime.spawn(eot_subscriber_task(
            eot_subscriber,
            eot_tx,
            self.runner.clone(),
            eot_shutdown_rx,
            trace,
        ));

        self.runtime = Some(runtime);
        self.send_tx = Some(send_tx);
        self.recv_rx = Some(recv_rx);
        self.shutdown_tx = Some(shutdown_tx);
        self.eot_shutdown_tx = Some(eot_shutdown_tx);
        self.eot_rx = Some(eot_rx);
        self.eot_seen.clear();

        trace_if!(trace, "connect: total {} ms", t0.elapsed().as_millis());
        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        let trace = self.zenoh_args.debug_trace;
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

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
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

    fn signal_end_of_test(&mut self) -> Result<u64> {
        let trace = self.zenoh_args.debug_trace;
        let send_tx = self
            .send_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;

        let eot_id: u64 = rand::random::<u64>();
        let key = eot_key_for(&self.runner);
        let payload = encode_eot_payload(eot_id);

        if trace {
            trace_now!(
                "signal_end_of_test: publishing EOT key={} id={}",
                key,
                eot_id
            );
        }

        let (done_tx, done_rx) = oneshot::channel::<Result<()>>();
        let outbound = OutboundMessage::Eot {
            key: key.clone(),
            payload,
            done: done_tx,
        };
        send_tx
            .blocking_send(outbound)
            .map_err(|_| anyhow::anyhow!("publish channel closed during signal_end_of_test"))?;

        // Block on the publisher_task's confirmation that the put committed.
        // The recv runs inside the existing runtime via `block_on` so the
        // EOT publish lands on the same runtime as every other Zenoh call
        // (per T10.2b's bridge architecture).
        let put_result = runtime
            .block_on(done_rx)
            .map_err(|_| anyhow::anyhow!("EOT publisher dropped completion oneshot"))?;
        put_result?;

        if trace {
            trace_now!(
                "signal_end_of_test: EOT committed key={} id={}",
                key,
                eot_id
            );
        }
        Ok(eot_id)
    }

    fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
        let eot_rx = match self.eot_rx.as_mut() {
            Some(rx) => rx,
            None => return Ok(Vec::new()),
        };

        let mut out: Vec<PeerEot> = Vec::new();
        loop {
            match eot_rx.try_recv() {
                Ok((writer, eot_id)) => {
                    // Variant-side dedup is the source of truth per the
                    // EOT contract -- a (writer, eot_id) pair is returned
                    // to the driver at most once per spawn.
                    if self.eot_seen.insert((writer.clone(), eot_id)) {
                        out.push(PeerEot { writer, eot_id });
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Subscriber task ended; nothing more is coming.
                    break;
                }
            }
        }
        Ok(out)
    }

    fn disconnect(&mut self) -> Result<()> {
        let trace = self.zenoh_args.debug_trace;
        let t0 = Instant::now();
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

    #[test]
    fn test_poll_peer_eots_dedups_repeated_pairs() {
        // Inject two arrivals with the same (writer, eot_id) into the
        // EOT observations channel. `poll_peer_eots` MUST return only
        // the first as a `PeerEot` -- the variant is the source of
        // truth for dedup per the EOT contract.
        let mut variant =
            ZenohVariant::new("self-runner", &[]).expect("construct variant for dedup test");

        let (tx, rx) = mpsc::channel::<(String, u64)>(8);
        variant.eot_rx = Some(rx);

        // Two identical arrivals plus a distinct (writer, eot_id) plus a
        // same-writer-different-id arrival to confirm dedup is on the
        // FULL pair, not just the writer.
        tx.try_send(("peer-a".to_string(), 0xAAAA)).unwrap();
        tx.try_send(("peer-a".to_string(), 0xAAAA)).unwrap();
        tx.try_send(("peer-b".to_string(), 0xBBBB)).unwrap();
        tx.try_send(("peer-a".to_string(), 0xCCCC)).unwrap();
        drop(tx);

        let observed = variant.poll_peer_eots().expect("poll_peer_eots");
        assert_eq!(
            observed.len(),
            3,
            "expected 3 unique (writer, eot_id) pairs after dedup, got {observed:?}"
        );
        assert!(observed.contains(&PeerEot {
            writer: "peer-a".to_string(),
            eot_id: 0xAAAA,
        }));
        assert!(observed.contains(&PeerEot {
            writer: "peer-b".to_string(),
            eot_id: 0xBBBB,
        }));
        assert!(observed.contains(&PeerEot {
            writer: "peer-a".to_string(),
            eot_id: 0xCCCC,
        }));

        // A second poll must return nothing (channel closed; all dedup
        // entries have already been emitted).
        let again = variant
            .poll_peer_eots()
            .expect("poll_peer_eots second call");
        assert!(
            again.is_empty(),
            "second poll must be empty after dedup, got {again:?}"
        );
    }

    #[test]
    fn test_poll_peer_eots_returns_empty_when_disconnected() {
        // Before connect, eot_rx is None -- the trait default behaviour
        // is to return an empty vec, and our impl matches that.
        let mut variant = ZenohVariant::new("solo", &[]).expect("construct variant");
        let observed = variant
            .poll_peer_eots()
            .expect("poll_peer_eots without connect");
        assert!(observed.is_empty());
    }

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
        variant.connect().expect("connect");

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
}
