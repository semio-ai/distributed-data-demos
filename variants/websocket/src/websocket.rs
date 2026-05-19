//! WebSocket variant: reliable QoS only (3-4) using sync `tungstenite`
//! over `std::net::TcpStream`.
//!
//! Symmetric peer pairing: lower-sorted-name runner is the WS client
//! (`tungstenite::client::connect`), higher-sorted-name runner is the WS
//! server (`tungstenite::server::accept`). One full-duplex WS connection
//! per peer pair.
//!
//! ## Truly-blocking writes, polled reads via `SO_RCVTIMEO`
//!
//! Per CUSTOM.md and `variants/hybrid/CUSTOM.md`, the underlying TCP
//! socket stays in **blocking mode** so writes through tungstenite's
//! `send` truly block under kernel back-pressure -- the back-pressure
//! signal we want to measure for this benchmark.
//!
//! To make reads pollable without flipping the socket-wide non-blocking
//! flag (which on Windows would silently un-block writes too), we install
//! a short `SO_RCVTIMEO` via `TcpStream::set_read_timeout` on the same
//! socket. `SO_RCVTIMEO` only affects `recv` syscalls -- writes are
//! unaffected and remain blocking. Reads from tungstenite then surface
//! `Error::Io(WouldBlock)` (Unix) or `Error::Io(TimedOut)` (Windows) when
//! no data has arrived, allowing the protocol loop to interleave peers
//! without stalling.
//!
//! ## Per-peer fault tolerance
//!
//! If a single peer's connection closes (`ConnectionClosed`,
//! `AlreadyClosed`, fatal `Io` error), we drop that peer from the active
//! set and continue with the survivors. One peer dropping must NOT fail
//! the whole spawn -- mirroring Hybrid's TCP rule.
//!
//! ## Threading modes (T14.2)
//!
//! See `CUSTOM.md` "Threading modes (T14.2)". In Single mode the driver
//! thread does inline reads + writes (the pre-E14 behaviour). In Multi
//! mode each peer gets a dedicated OS reader thread that drains the
//! socket and pushes decoded frames into a bounded mpsc; the driver's
//! `poll_receive` becomes a near-free `try_recv`. The reader uses
//! drop-on-full for Data items and blocking-send for Eot items, which
//! is what closes the T-impl.10 residual deadlock at high symmetric
//! rates: the reader thread never stalls on TCP, the peer's writer
//! never blocks forever on its end-of-test broadcast.

use std::collections::{HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use socket2::Socket;
use tungstenite::protocol::{Role, WebSocketContext};
use tungstenite::{
    client::IntoClientRequest, handshake::server::NoCallback, ClientHandshake, HandshakeError,
    Message, ServerHandshake, WebSocket,
};

use variant_base::logger::LoggerHandle;
use variant_base::types::{Qos, ReceivedUpdate, ThreadingMode};
use variant_base::Variant;

/// Internal record of an observed EOT marker on the data WebSocket
/// stream. The on-wire EOT exchange was retired in T15.8; this type
/// remains so peers running pre-T15.8 binaries that still emit EOT
/// markers on the data stream can be decoded and discarded without
/// surfacing as `Frame` parse errors. The decoded markers are no
/// longer surfaced to the driver.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct PeerEot {
    writer: String,
    eot_id: u64,
}

use crate::pairing::{DerivedEndpoints, PairRole, PeerDesc};
use crate::protocol::{self, Frame};

/// Read timeout applied to every per-peer underlying TCP socket. Short
/// enough to keep the poll loop responsive (so other peers' reads aren't
/// starved) yet long enough to avoid syscall churn when nothing is in
/// flight.
const READ_POLL_TIMEOUT: Duration = Duration::from_millis(1);

/// T17.5: write-side timeout for Single-mode outbound WebSocket
/// streams.
///
/// At symmetric saturation on QoS 3/4 (e.g. 1000 vpt x 100 Hz =
/// 100K msg/s on localhost) Single mode shares one thread between
/// `publish` and `poll_receive`. Without intervention, both peers
/// spend the publish phase inside tungstenite's blocking `write`,
/// neither side calls `poll_receive` to drain the kernel recv
/// buffer, and the kernel TCP send buffers fill on both sides --
/// the variant wedges until the runner kills it.
///
/// **Pre-T17.5 (T14.19 era)**: `SO_SNDTIMEO = 5 s` plus drop-the-peer
/// on the resulting `TimedOut` error. That unwedged the deadlock at
/// the cost of dropping the peer mid-operate -- delivery collapsed
/// to ~2.4% on the QoS-4 `1000x100hz` cell of the post-T16.16
/// heatmap, violating the new E17 contract (DESIGN.md § 6.5:
/// variants MUST deliver 100% of accepted writes at QoS 3/4,
/// blocking the publisher rather than dropping bytes).
///
/// **Post-T17.5**: the timeout is a **drain-interleave trigger**, not
/// a kill switch. On `TimedOut` / `WouldBlock` from tungstenite,
/// `send_to_peer_with_retry` drains the wedged peer's read side
/// (logging every frame via `LoggerHandle` and incrementing the
/// variant-base receive counter so the T15.11 internal-stall
/// watchdog stays calm), drains every OTHER active peer too, then
/// retries the send via `WebSocket::flush`. tungstenite has
/// already buffered the partial frame bytes in its internal
/// `out_buffer`, so `flush` resumes the write from wherever the
/// kernel stopped accepting -- no duplicate frame, no message
/// loss. The loop continues until the write lands or the peer
/// surfaces a non-transient fatal error.
///
/// The shorter timeout (100 ms vs. 5 s) keeps the interleave tight:
/// at 100K msg/s symmetric the drain step needs to happen
/// frequently enough that neither side's kernel buffer stays full
/// for long. 100 ms is well above `READ_POLL_TIMEOUT = 1 ms` so
/// transient bursts on a healthy LAN don't hit it, and well below
/// the 30 s watchdog threshold so even when both peers are
/// thoroughly back-pressured the receive counter has many
/// opportunities to advance per watchdog tick.
///
/// Applied to Single mode only. Multi mode runs a per-peer reader
/// thread that drains in parallel with the publisher; the wedge
/// does not occur and `SO_SNDTIMEO` would only invite spurious
/// drain churn. The `start_reader_threads` path explicitly clears
/// the timeout on the write clone (see `set_write_timeout(None)`
/// in `start_reader_threads`) to keep Multi mode's contract
/// unchanged.
const SINGLE_WRITE_TIMEOUT: Duration = Duration::from_millis(100);

/// Discovery / handshake timeout. If a peer never appears after this
/// duration, `connect` fails loudly rather than deadlocking the whole spawn.
const PEER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Grace duration for clean WebSocket close exchange before the
/// underlying TCP is forcibly torn down at `disconnect` time.
const DISCONNECT_GRACE: Duration = Duration::from_millis(200);

/// Join timeout for reader threads in `stop_reader_threads`. If a thread
/// is wedged inside `WebSocket::read` past this budget, we log a warning
/// and abandon it -- Rust will tear down on process exit. See T14.2.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Capacity of the Multi-mode lifecycle-only mpsc.
///
/// As of T14.10 the channel only carries lifecycle items (`Eot`,
/// `PeerDropped`) -- data `receive` events are written to JSONL
/// directly from the reader thread via the shared [`LoggerHandle`].
/// A small fixed bound is therefore sufficient: peers emit at most one
/// `Eot` per spawn plus an optional `PeerDropped`, so the channel can
/// never grow large in steady state. 256 leaves comfortable headroom
/// for many-peer fixtures and tests.
const LIFECYCLE_CHANNEL_CAPACITY: usize = 256;

/// Multi-mode write side: tungstenite framing context + the dedicated
/// write-only `TcpStream` clone. Held behind a Mutex to serialize
/// outbound frames (multiple writers would interleave bytes mid-frame
/// on the wire, which is illegal WebSocket framing).
struct MultiWriter {
    ctx: WebSocketContext,
    stream: TcpStream,
}

/// Per-mode IO state for a single peer.
///
/// `Single`: the original `WebSocket<TcpStream>` holds both halves; the
/// driver thread does inline reads + writes (existing behaviour).
///
/// `Multi`: writes go through `writer`; reads happen in a dedicated OS
/// thread (owned by the variant's `reader_threads` vec) that pushes
/// decoded frames into the shared `recv_tx` channel.
#[allow(clippy::large_enum_variant)]
enum PeerIo {
    Single(WebSocket<TcpStream>),
    Multi {
        /// Locked for the duration of an outbound `Message::Binary` /
        /// `Message::Close` write, so concurrent publishers never
        /// interleave WebSocket frame bytes on the wire.
        writer: Arc<Mutex<MultiWriter>>,
    },
}

/// A single WebSocket peer connection.
struct WsPeer {
    /// Peer's runner name (used to filter own EOT loopback and for log
    /// diagnostics).
    name: String,
    /// Local view of the peer address (informational).
    addr: SocketAddr,
    /// Mode-specific IO state. `Single` holds the full `WebSocket`
    /// (combined read+write); `Multi` holds only the write half.
    io: PeerIo,
}

/// Configuration for the WebSocket variant.
pub struct WebSocketConfig {
    /// Local listen address (used when at least one peer is a Server-role
    /// pair for this runner).
    pub listen_addr: SocketAddr,
    /// Peer descriptions resolved from `--peers` and pairing rules.
    pub peers: Vec<PeerDesc>,
    /// Active QoS for this spawn. The variant rejects 1 and 2.
    pub qos: Qos,
    /// OS-level receive-buffer size in kibibytes (1024-byte units).
    /// Applied via `SO_RCVBUF` on every underlying TCP socket
    /// immediately after the WS handshake completes; see T14.2.
    pub recv_buffer_kb: u32,
    /// Driver `values_per_tick`. Retained for compatibility with
    /// upstream callers (`from_derived`) and for potential future
    /// per-tick sizing; the Multi-mode mpsc was a function of this
    /// value pre-T14.10 but is now a fixed lifecycle-only bound. See
    /// `LIFECYCLE_CHANNEL_CAPACITY`.
    #[allow(dead_code)]
    pub values_per_tick: u32,
}

impl WebSocketConfig {
    pub fn from_derived(
        derived: DerivedEndpoints,
        qos: Qos,
        recv_buffer_kb: u32,
        values_per_tick: u32,
    ) -> Self {
        Self {
            listen_addr: derived.listen_addr,
            peers: derived.peers,
            qos,
            recv_buffer_kb,
            values_per_tick,
        }
    }
}

/// Item pushed by a Multi-mode reader thread into the shared channel.
///
/// As of T14.10 this is **lifecycle-only**: the reader thread writes
/// `receive` events for decoded data frames directly to JSONL via the
/// shared `LoggerHandle` and never queues them through the channel.
/// The channel therefore only carries end-of-test markers and per-peer
/// drop notifications, both of which must remain visible to the driver
/// thread (the driver logs `eot_received` itself and updates its peer
/// set on a drop).
enum ReaderItem {
    Eot {
        writer: String,
        eot_id: u64,
    },
    /// Reader observed a fatal per-peer error; the driver thread can
    /// log it and forget the peer. Carries the peer's runner name so
    /// the driver can correlate the drop.
    PeerDropped {
        peer: String,
        reason: String,
    },
}

/// Per-peer reader thread bookkeeping (Multi mode only).
struct ReaderThread {
    peer_name: String,
    handle: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
}

/// WebSocket variant implementing the Variant trait.
pub struct WebSocketVariant {
    runner: String,
    config: WebSocketConfig,
    peers: Vec<WsPeer>,
    /// `(writer, eot_id)` pairs already observed.
    seen_eots: HashSet<(String, u64)>,
    /// EOTs observed since the last `poll_peer_eots` call.
    pending_eots: VecDeque<PeerEot>,
    /// Threading mode captured at `connect` time so `start_reader_threads`
    /// / `stop_reader_threads` / `poll_receive` can branch consistently.
    threading_mode: ThreadingMode,
    /// Sender side of the Multi-mode shared receive channel. Cloned per
    /// reader thread; the variant retains an extra clone so the channel
    /// is not closed until `stop_reader_threads` runs.
    ///
    /// `None` in Single mode.
    recv_tx: Option<SyncSender<ReaderItem>>,
    /// Receiver side of the Multi-mode shared receive channel.
    /// `poll_receive` drains it via `try_recv`. `None` in Single mode.
    recv_rx: Option<Receiver<ReaderItem>>,
    /// Reader thread join handles (Multi mode only).
    reader_threads: Vec<ReaderThread>,
    /// Thread-safe handle to the driver's JSONL logger, attached by the
    /// driver between `connect` and `start_reader_threads` (T14.10).
    /// Cloned into each reader thread so it can write `receive` events
    /// directly off the driver thread. `None` until `attach_logger`
    /// runs; reader threads spawned before the handle arrives would
    /// fail to log, but the driver-side ordering guarantees this never
    /// happens.
    logger: Option<LoggerHandle>,
}

impl WebSocketVariant {
    pub fn new(runner: &str, config: WebSocketConfig) -> Self {
        Self {
            runner: runner.to_string(),
            config,
            peers: Vec::new(),
            seen_eots: HashSet::new(),
            pending_eots: VecDeque::new(),
            threading_mode: ThreadingMode::Single,
            recv_tx: None,
            recv_rx: None,
            reader_threads: Vec::new(),
            logger: None,
        }
    }

    /// Test hook: report how many reader threads are currently running
    /// (Multi-mode bookkeeping). Single mode always returns 0.
    #[cfg(test)]
    pub fn reader_thread_count(&self) -> usize {
        self.reader_threads.len()
    }

    /// Test hook: report the active threading mode.
    #[cfg(test)]
    pub fn active_threading_mode(&self) -> ThreadingMode {
        self.threading_mode
    }

    /// Record an observed EOT marker. Idempotent; filters out our own
    /// runner (defence-in-depth -- WS is per-pair so loopback is rare).
    fn record_eot(&mut self, writer: String, eot_id: u64) {
        if writer == self.runner {
            return;
        }
        if self.seen_eots.insert((writer.clone(), eot_id)) {
            self.pending_eots.push_back(PeerEot { writer, eot_id });
        }
    }

    /// Single-mode poll: visit every peer once, dispatch any received
    /// frame. Returns the first data update found, or `None` if no peer
    /// had a data frame ready this pass. Per-peer fatal errors drop the
    /// peer and the loop continues with the rest.
    fn poll_peers_once_single(&mut self) -> Option<ReceivedUpdate> {
        let mut keep: Vec<bool> = Vec::with_capacity(self.peers.len());
        let mut hit: Option<ReceivedUpdate> = None;
        let mut eots: Vec<(String, u64)> = Vec::new();

        for peer in self.peers.iter_mut() {
            if hit.is_some() {
                keep.push(true);
                continue;
            }
            let ws = match &mut peer.io {
                PeerIo::Single(ws) => ws,
                PeerIo::Multi { .. } => {
                    keep.push(true);
                    continue;
                }
            };
            match ws.read() {
                Ok(Message::Binary(bytes)) => match protocol::decode_frame(&bytes) {
                    Ok(Frame::Data(update)) => {
                        hit = Some(update);
                        keep.push(true);
                    }
                    Ok(Frame::Eot { writer, eot_id }) => {
                        eots.push((writer, eot_id));
                        keep.push(true);
                    }
                    Err(e) => {
                        eprintln!(
                            "warning: dropping WS peer {} ({}) after decode error: {:#}",
                            peer.name, peer.addr, e
                        );
                        keep.push(false);
                    }
                },
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    keep.push(true);
                }
                Ok(Message::Close(_)) => {
                    eprintln!(
                        "warning: WS peer {} ({}) sent Close frame; dropping",
                        peer.name, peer.addr
                    );
                    keep.push(false);
                }
                Ok(other) => {
                    eprintln!(
                        "warning: WS peer {} ({}) sent unexpected message {:?}; ignoring",
                        peer.name, peer.addr, other
                    );
                    keep.push(true);
                }
                Err(tungstenite::Error::Io(e)) if is_transient_io_error(&e) => {
                    keep.push(true);
                }
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => {
                    eprintln!(
                        "warning: WS peer {} ({}) closed; dropping",
                        peer.name, peer.addr
                    );
                    keep.push(false);
                }
                Err(e) => {
                    eprintln!(
                        "warning: dropping WS peer {} ({}) after read error: {:#}",
                        peer.name, peer.addr, e
                    );
                    keep.push(false);
                }
            }
        }

        if !keep.iter().all(|&k| k) {
            let mut idx = 0;
            self.peers.retain(|_| {
                let k = keep[idx];
                idx += 1;
                k
            });
        }

        for (writer, eot_id) in eots {
            self.record_eot(writer, eot_id);
        }

        hit
    }

    /// Multi-mode poll: drain lifecycle items from the shared receiver
    /// channel.
    ///
    /// As of T14.10 the channel is lifecycle-only: reader threads write
    /// `receive` events directly to JSONL via the shared `LoggerHandle`
    /// and never queue data frames here. This call therefore drains
    /// every available `Eot` / `PeerDropped` item and returns `None`.
    /// The driver still invokes `poll_receive` on every operate-phase
    /// iteration; that's what keeps lifecycle items flowing without
    /// requiring a separate driver hook.
    fn poll_peers_once_multi(&mut self) -> Option<ReceivedUpdate> {
        self.recv_rx.as_ref()?;
        loop {
            let next = self.recv_rx.as_ref()?.try_recv();
            match next {
                Ok(ReaderItem::Eot { writer, eot_id }) => {
                    self.record_eot(writer, eot_id);
                }
                Ok(ReaderItem::PeerDropped { peer, reason }) => {
                    eprintln!("warning: WS reader thread dropped peer {peer}: {reason}");
                    self.peers.retain(|p| p.name != peer);
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => return None,
            }
        }
    }

    /// Send a binary frame to every active peer. Drops a peer on a
    /// fatal write error; mirrors Hybrid TCP's broadcast behaviour.
    /// Routes to the mode-appropriate write path.
    ///
    /// **T17.5 (Single mode, QoS 3/4)**: tungstenite write surfaces
    /// a `Io(TimedOut)` / `Io(WouldBlock)` when the kernel TCP send
    /// buffer stays full for `SINGLE_WRITE_TIMEOUT`. Per the E17 /
    /// DESIGN.md § 6.5 strict-no-skip contract, this is **transient
    /// back-pressure** -- the publisher MUST block until the message
    /// is accepted. The retry path is: drain THIS peer's read side
    /// (logging every received frame via `LoggerHandle` and counting
    /// it into the variant-base receive counter so the T15.11
    /// internal-stall watchdog stays calm), drain any other active
    /// peers, then retry the send via `WebSocket::flush`.
    /// tungstenite has already buffered the partial frame bytes in
    /// its internal `out_buffer`; `flush` resumes the partial write
    /// from wherever the kernel stopped accepting, so no duplicate
    /// frame and no message loss. The loop continues until the send
    /// lands or the peer surfaces a genuine fatal error.
    ///
    /// **Pre-T17.5 (T14.19 era)**: the function dropped the peer on
    /// the first `TimedOut`, which left the spawn exiting cleanly
    /// but with near-zero delivery -- accepted before E17, violates
    /// the contract now. The retry-with-drain pattern restores 100%
    /// delivery at QoS 3/4 by paying for it with throughput
    /// collapse (the explicitly-acceptable failure mode per § 6.5).
    ///
    /// Genuine fatal errors (`ConnectionClosed`, `ConnectionReset`,
    /// `ConnectionAborted`, `AlreadyClosed`, decode error, etc.)
    /// still drop the peer with a warning. The decision is made via
    /// `is_transient_io_error` for IO errors; everything else falls
    /// through to the drop path.
    ///
    /// **Multi mode is unchanged**: writes have no timeout, so the
    /// per-peer reader thread is the back-pressure relief valve.
    fn broadcast_binary(&mut self, payload: Vec<u8>) -> Result<()> {
        // Process peers one at a time, popping each off `self.peers`
        // into a local scratch slot so the per-peer retry loop can
        // run `poll_peers_once_single` (which iterates over
        // `self.peers`) without borrow conflict. Survivors are
        // pushed back onto a holding vec and reassigned at the end.
        let mut survivors: Vec<WsPeer> = Vec::with_capacity(self.peers.len());
        while let Some(mut peer) = self.peers.pop() {
            match self.send_to_peer_with_retry(&mut peer, &payload) {
                Ok(()) => survivors.push(peer),
                Err(PeerSendError::Drop(reason)) => {
                    eprintln!(
                        "warning: dropping WS peer {} ({}) after write error: {reason:#}",
                        peer.name, peer.addr
                    );
                    // Peer dropped: do not push back.
                }
            }
        }
        // Restore the active set. Order is observable only to logs
        // and per-peer drains, both of which are order-insensitive.
        self.peers = survivors;
        self.peers.reverse();
        Ok(())
    }

    /// Send `payload` to a single peer, retrying on transient I/O
    /// back-pressure with an interleaved drain.
    ///
    /// In Single mode at QoS 3/4 a `TimedOut` / `WouldBlock` from
    /// tungstenite's underlying `write` means the kernel TCP send
    /// buffer is full. To unwedge the symmetric-saturation deadlock
    /// we drain the receive side of:
    ///
    /// 1. **The peer being written to** (via the locally-held
    ///    `WebSocket`). This is the critical step: every frame we
    ///    drain off this peer's read socket frees a byte of our
    ///    kernel recv buffer, which lets the peer's blocked write
    ///    make progress, which unwedges its publish loop so it can
    ///    drain ITS recv buffer, which finally lets OUR send
    ///    progress.
    /// 2. **Every other active peer in `self.peers`**, via
    ///    `poll_peers_once_single`. In the canonical 2-runner case
    ///    this is a no-op; larger fixtures rely on it to avoid
    ///    starving the unrelated connections while this peer is
    ///    back-pressured.
    ///
    /// Every drained data frame is logged directly via the
    /// `LoggerHandle` (same pattern as Multi mode's reader thread)
    /// so the strict "every received message is logged" contract
    /// from `metak-shared/overview.md` is preserved end-to-end.
    ///
    /// We retry the send via `WebSocket::flush`, not `send`:
    /// tungstenite already buffered the frame bytes in its internal
    /// `out_buffer` before the IO error, and `flush` resumes the
    /// partial write from wherever the kernel stopped accepting.
    /// Calling `send` again would queue a duplicate frame.
    ///
    /// In Multi mode the function attempts the send exactly once.
    /// No write timeout is installed, so back-pressure manifests as
    /// a truly blocking `write` that the per-peer reader thread
    /// takes care of via parallel drain. Non-transient errors still
    /// drop the peer.
    fn send_to_peer_with_retry(
        &mut self,
        peer: &mut WsPeer,
        payload: &[u8],
    ) -> std::result::Result<(), PeerSendError> {
        // First attempt is a `send` (write + flush). On retry after
        // a transient error, use `flush` only -- the payload is
        // already buffered in tungstenite and re-issuing `write`
        // would queue a duplicate frame.
        let mut first_attempt = true;
        loop {
            let result = match &mut peer.io {
                PeerIo::Single(ws) => {
                    if first_attempt {
                        ws.send(Message::Binary(payload.to_vec()))
                    } else {
                        ws.flush()
                    }
                }
                PeerIo::Multi { writer } => {
                    let mut guard = match writer.lock() {
                        Ok(g) => g,
                        Err(_) => {
                            return Err(PeerSendError::Drop(anyhow::anyhow!(
                                "WS Multi writer mutex poisoned for peer {} ({})",
                                peer.name,
                                peer.addr
                            )));
                        }
                    };
                    let MultiWriter { ctx, stream } = &mut *guard;
                    // Suppress clippy::result_large_err here:
                    // tungstenite::Error is intentionally large; we
                    // immediately convert into PeerSendError.
                    #[allow(clippy::result_large_err)]
                    let r = if first_attempt {
                        ctx.write(stream, Message::Binary(payload.to_vec()))
                            .and_then(|()| ctx.flush(stream))
                    } else {
                        ctx.flush(stream)
                    };
                    r
                }
            };
            first_attempt = false;

            match result {
                Ok(()) => return Ok(()),
                Err(tungstenite::Error::Io(io_err)) if is_transient_io_error(&io_err) => {
                    // T17.5 transient back-pressure. Step 1: drain
                    // the wedged peer's read side. Every byte we
                    // pull off this socket unblocks a corresponding
                    // byte of the peer's blocked write -- this is
                    // the critical step that breaks the symmetric-
                    // saturation deadlock in single mode.
                    drain_current_peer_into_logger(peer, self.logger.as_ref());
                    // Step 2: drain any other active peers so a
                    // many-peer fixture doesn't starve unrelated
                    // connections while this peer is back-pressured.
                    // The returned Option is dropped: if a peer's
                    // frame surfaced it was already routed by
                    // `poll_peers_once_single`'s normal flow. In
                    // the 2-runner case `self.peers` is empty here
                    // (the current peer was popped before this
                    // function was called) and this is a no-op.
                    let _ = self.poll_peers_once_single();
                    // Loop again. Next iteration uses `flush()` so
                    // tungstenite resumes its buffered frame from
                    // the partial-write position rather than
                    // appending a duplicate.
                    continue;
                }
                Err(e) => {
                    return Err(PeerSendError::Drop(anyhow::anyhow!(
                        "WS write error to peer {} ({}): {}",
                        peer.name,
                        peer.addr,
                        e
                    )));
                }
            }
        }
    }
}

/// Outcome of `send_to_peer_with_retry`. Successful sends return
/// `Ok(())`. The only failure mode surfaced to the broadcast loop
/// is "drop this peer", carrying the contextualised error for the
/// warning log line.
enum PeerSendError {
    Drop(anyhow::Error),
}

/// T17.5 helper: drain whatever frames are immediately available on
/// `peer`'s read side, logging each `Data` frame via the supplied
/// `LoggerHandle` (mirroring Multi mode's reader-thread logging
/// pattern). EOT frames on the data stream are silently discarded
/// (the on-wire EOT exchange was removed in T15.8; stale peers
/// might still emit them).
///
/// Returns as soon as the read socket signals `WouldBlock` /
/// `TimedOut` (the short `READ_POLL_TIMEOUT = 1 ms` set on the
/// underlying TCP recv timeout means this happens within ~1 ms of
/// the kernel recv buffer running dry). For Multi-mode peers this
/// is a no-op: the dedicated reader thread owns the read side.
///
/// When `logger` is `None` (e.g. unit tests that never call
/// `attach_logger`), drained frames are silently discarded -- the
/// retry loop still benefits from the kernel-buffer drain side
/// effect, which is the back-pressure-relief point of this helper.
fn drain_current_peer_into_logger(peer: &mut WsPeer, logger: Option<&LoggerHandle>) {
    let ws = match &mut peer.io {
        PeerIo::Single(ws) => ws,
        PeerIo::Multi { .. } => return,
    };
    loop {
        match ws.read() {
            Ok(Message::Binary(bytes)) => match protocol::decode_frame(&bytes) {
                Ok(Frame::Data(update)) => {
                    if let Some(logger) = logger {
                        // T18.3a: `record_receive` mirrors the legacy
                        // JSONL line into the driver's shared compact
                        // `EventBuffer`, so the digest-phase Parquet
                        // file captures every receive observed by the
                        // Single-mode drain path. Pre-T18.3a this
                        // helper called `log_receive` and the receive
                        // was missing from `*.compact.parquet`.
                        if let Err(e) = logger.record_receive(
                            &update.writer,
                            update.seq,
                            &update.path,
                            update.qos,
                            update.payload.len(),
                        ) {
                            eprintln!(
                                "warning: WS drain for peer {} ({}) failed to log receive \
                                 event: {e:#}; continuing",
                                peer.name, peer.addr
                            );
                        }
                    }
                }
                Ok(Frame::Eot { .. }) => {
                    // T15.8: EOT no longer carried on the data
                    // stream. Discard.
                }
                Err(e) => {
                    eprintln!(
                        "warning: WS drain for peer {} ({}) saw decode error: {e:#}; \
                         stopping drain",
                        peer.name, peer.addr
                    );
                    return;
                }
            },
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => return,
            Ok(_other) => {}
            Err(tungstenite::Error::Io(e)) if is_transient_io_error(&e) => return,
            Err(_) => return,
        }
    }
}

/// Apply `SO_RCVBUF = recv_buffer_kb * 1024` on the underlying TCP
/// socket. Some OSes silently cap the request; we log a warning if the
/// achieved size is materially smaller than requested but never fail
/// the connect path. See T14.2.
fn apply_recv_buffer_kb(stream: &TcpStream, recv_buffer_kb: u32, peer_addr: SocketAddr) {
    let clone = match stream.try_clone() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: SO_RCVBUF tuning skipped for {peer_addr}: TcpStream::try_clone \
                 failed: {e}"
            );
            return;
        }
    };
    let sock: Socket = clone.into();
    let requested = (recv_buffer_kb as usize).saturating_mul(1024);
    if let Err(e) = sock.set_recv_buffer_size(requested) {
        eprintln!(
            "warning: SO_RCVBUF set to {requested} bytes failed for {peer_addr}: {e}; \
             continuing with the kernel default"
        );
        return;
    }
    match sock.recv_buffer_size() {
        Ok(actual) => {
            if actual + actual < requested {
                eprintln!(
                    "warning: SO_RCVBUF for {peer_addr}: requested {requested} bytes, \
                     kernel granted {actual} bytes"
                );
            }
        }
        Err(e) => {
            eprintln!("warning: SO_RCVBUF readback failed for {peer_addr}: {e}");
        }
    }
}

/// T17.5: install `SO_SNDTIMEO = SINGLE_WRITE_TIMEOUT` on a
/// post-handshake WebSocket TCP stream when running in Single mode.
///
/// Originally added in T14.19 as a 5 s "kill switch" to break a
/// hard deadlock at high symmetric rates by dropping the peer on
/// timeout. Under E17 the semantics changed: the timeout is now a
/// **drain-interleave trigger** consumed by
/// `send_to_peer_with_retry` -- a 100 ms `TimedOut` / `WouldBlock`
/// signals "the kernel send buffer is full, go drain the recv
/// side", not "this peer is dead". See `SINGLE_WRITE_TIMEOUT` for
/// the full rationale.
///
/// Multi mode leaves the write timeout unset (the reader thread
/// drains in parallel and the wedge does not occur). Failure is
/// logged but never fatal: without the timeout the publisher
/// would block forever on the first symmetric-saturation wedge,
/// but the spawn does not break -- the runner-coordinated
/// termination state machine will eventually time it out at the
/// `default_timeout_secs` budget.
fn apply_single_mode_write_timeout(
    stream: &TcpStream,
    threading_mode: ThreadingMode,
    peer_addr: SocketAddr,
) {
    if threading_mode != ThreadingMode::Single {
        return;
    }
    if let Err(e) = stream.set_write_timeout(Some(SINGLE_WRITE_TIMEOUT)) {
        eprintln!(
            "[variant-websocket] T17.5: set_write_timeout({:?}) failed for {peer_addr}: {e}; \
             continuing without write-side timeout (Single mode may block forever under \
             symmetric saturation; default_timeout_secs will catch it)",
            SINGLE_WRITE_TIMEOUT
        );
    }
}

/// Background reader thread for a Multi-mode peer.
///
/// Owns the per-peer `WebSocket<TcpStream>` exclusively. Loops on
/// `WebSocket::read` with the short SO_RCVTIMEO previously installed
/// by `ws_client_connect` / `ws_server_accept` so the shutdown flag is
/// checked roughly every `READ_POLL_TIMEOUT`.
///
/// As of T14.10 the reader thread writes `receive` events for decoded
/// data frames **directly to JSONL** via the shared `LoggerHandle`.
/// The bounded mpsc is reserved for lifecycle items only (`Eot`,
/// `PeerDropped`). This lifts the high-rate delivery cliff that T14.2's
/// drop-on-full design imposed: every frame the reader parses off the
/// wire makes it into JSONL regardless of the driver's per-tick drain
/// cadence. Logger-mutex contention becomes the new bottleneck, but a
/// single line write is microseconds-cheap so the cliff moves far above
/// the 100 K msg/s symmetric workload that motivated T14.10.
fn reader_thread_main(
    peer_name: String,
    peer_addr: SocketAddr,
    mut ws: WebSocket<TcpStream>,
    tx: SyncSender<ReaderItem>,
    shutdown: Arc<AtomicBool>,
    logger: LoggerHandle,
) {
    while !shutdown.load(Ordering::Acquire) {
        match ws.read() {
            Ok(Message::Binary(bytes)) => match protocol::decode_frame(&bytes) {
                Ok(Frame::Data(update)) => {
                    // T14.10: write the `receive` event directly from
                    // the reader thread. The bounded mpsc no longer
                    // carries Data items, so the historical drop-on-
                    // full path is gone.
                    //
                    // T18.3a: switched from `log_receive` (legacy
                    // JSONL only) to `record_receive` so the row also
                    // lands in the driver's shared compact
                    // `EventBuffer`. Pre-T18.3a these receives were
                    // missing from `*.compact.parquet`.
                    if let Err(e) = logger.record_receive(
                        &update.writer,
                        update.seq,
                        &update.path,
                        update.qos,
                        update.payload.len(),
                    ) {
                        eprintln!(
                            "warning: WS reader thread for peer {peer_name} failed to log \
                             receive event: {e:#}; continuing"
                        );
                    }
                }
                Ok(Frame::Eot { writer, eot_id }) => {
                    if !push_eot_blocking(&tx, ReaderItem::Eot { writer, eot_id }, &shutdown) {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(ReaderItem::PeerDropped {
                        peer: peer_name.clone(),
                        reason: format!("decode error: {e:#}"),
                    });
                    return;
                }
            },
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => {
                let _ = tx.send(ReaderItem::PeerDropped {
                    peer: peer_name.clone(),
                    reason: format!("peer {peer_addr} sent Close frame"),
                });
                return;
            }
            Ok(_other) => {}
            Err(tungstenite::Error::Io(e)) if is_transient_io_error(&e) => {}
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                let _ = tx.send(ReaderItem::PeerDropped {
                    peer: peer_name.clone(),
                    reason: format!("peer {peer_addr} connection closed"),
                });
                return;
            }
            Err(e) => {
                let _ = tx.send(ReaderItem::PeerDropped {
                    peer: peer_name.clone(),
                    reason: format!("peer {peer_addr} read error: {e}"),
                });
                return;
            }
        }
    }
}

/// Push an `Eot` item into the shared channel, blocking briefly if it
/// is full. EOT markers are critical: dropping one forces the peer's
/// driver to wait the full `eot_timeout`. The shutdown flag breaks
/// the loop if the variant is being torn down.
fn push_eot_blocking(
    tx: &SyncSender<ReaderItem>,
    item: ReaderItem,
    shutdown: &Arc<AtomicBool>,
) -> bool {
    use std::sync::mpsc::TrySendError;
    let mut pending = item;
    loop {
        match tx.try_send(pending) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                if shutdown.load(Ordering::Acquire) {
                    return false;
                }
                pending = returned;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Open one TCP connection, then perform the WS client handshake. The
/// handshake itself is allowed `PEER_HANDSHAKE_TIMEOUT` total wall-clock.
fn ws_client_connect(addr: SocketAddr) -> Result<WebSocket<TcpStream>> {
    let deadline = Instant::now() + PEER_HANDSHAKE_TIMEOUT;
    let url = format!("ws://{}/bench", addr);
    let request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("invalid WS URL '{url}'"))?;

    let stream = loop {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(s) => break s,
            Err(e)
                if e.kind() == ErrorKind::ConnectionRefused
                    || e.kind() == ErrorKind::TimedOut
                    || e.kind() == ErrorKind::WouldBlock =>
            {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "TCP connect to peer {addr} timed out after {:?}: {e}",
                        PEER_HANDSHAKE_TIMEOUT
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("TCP connect to peer {addr} failed: {e}"));
            }
        }
    };

    stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY before WS handshake")?;
    stream
        .set_nonblocking(false)
        .context("failed to make TCP stream blocking before WS handshake")?;
    stream
        .set_read_timeout(Some(PEER_HANDSHAKE_TIMEOUT))
        .context("failed to set TCP read timeout for WS handshake")?;
    stream
        .set_write_timeout(Some(PEER_HANDSHAKE_TIMEOUT))
        .context("failed to set TCP write timeout for WS handshake")?;

    let (ws, _resp) = perform_client_handshake(request, stream)?;

    let s = ws.get_ref();
    s.set_write_timeout(None)
        .with_context(|| format!("failed to clear TCP write timeout for {addr}"))?;
    s.set_read_timeout(Some(READ_POLL_TIMEOUT))
        .with_context(|| format!("failed to set short TCP read timeout for {addr}"))?;
    Ok(ws)
}

fn perform_client_handshake(
    request: tungstenite::handshake::client::Request,
    stream: TcpStream,
) -> Result<(
    WebSocket<TcpStream>,
    tungstenite::handshake::client::Response,
)> {
    match tungstenite::client::client(request, stream) {
        Ok((ws, resp)) => Ok((ws, resp)),
        Err(HandshakeError::Interrupted(mid)) => drive_handshake_to_completion_client(mid),
        Err(HandshakeError::Failure(e)) => Err(anyhow::anyhow!("WS client handshake failed: {e}")),
    }
}

fn drive_handshake_to_completion_client(
    mut mid: tungstenite::handshake::MidHandshake<ClientHandshake<TcpStream>>,
) -> Result<(
    WebSocket<TcpStream>,
    tungstenite::handshake::client::Response,
)> {
    let deadline = Instant::now() + PEER_HANDSHAKE_TIMEOUT;
    loop {
        match mid.handshake() {
            Ok((ws, resp)) => return Ok((ws, resp)),
            Err(HandshakeError::Interrupted(m)) => {
                if Instant::now() >= deadline {
                    bail!("WS client handshake timed out");
                }
                std::thread::sleep(Duration::from_millis(5));
                mid = m;
            }
            Err(HandshakeError::Failure(e)) => {
                return Err(anyhow::anyhow!("WS client handshake failed: {e}"));
            }
        }
    }
}

fn ws_server_accept(stream: TcpStream, addr: SocketAddr) -> Result<WebSocket<TcpStream>> {
    stream
        .set_nodelay(true)
        .with_context(|| format!("failed to set TCP_NODELAY for accepted {addr}"))?;
    stream
        .set_nonblocking(false)
        .with_context(|| format!("failed to make accepted TCP stream blocking for {addr}"))?;
    stream
        .set_read_timeout(Some(PEER_HANDSHAKE_TIMEOUT))
        .with_context(|| format!("failed to set handshake read timeout for {addr}"))?;
    stream
        .set_write_timeout(Some(PEER_HANDSHAKE_TIMEOUT))
        .with_context(|| format!("failed to set handshake write timeout for {addr}"))?;

    let ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(HandshakeError::Interrupted(mid)) => drive_handshake_to_completion_server(mid)?,
        Err(HandshakeError::Failure(e)) => {
            return Err(anyhow::anyhow!("WS server handshake failed: {e}"));
        }
    };

    let s = ws.get_ref();
    s.set_write_timeout(None)
        .with_context(|| format!("failed to clear TCP write timeout for accepted {addr}"))?;
    s.set_read_timeout(Some(READ_POLL_TIMEOUT))
        .with_context(|| format!("failed to set short read timeout for accepted {addr}"))?;
    Ok(ws)
}

fn drive_handshake_to_completion_server(
    mut mid: tungstenite::handshake::MidHandshake<ServerHandshake<TcpStream, NoCallback>>,
) -> Result<WebSocket<TcpStream>> {
    let deadline = Instant::now() + PEER_HANDSHAKE_TIMEOUT;
    loop {
        match mid.handshake() {
            Ok(ws) => return Ok(ws),
            Err(HandshakeError::Interrupted(m)) => {
                if Instant::now() >= deadline {
                    bail!("WS server handshake timed out");
                }
                std::thread::sleep(Duration::from_millis(5));
                mid = m;
            }
            Err(HandshakeError::Failure(e)) => {
                return Err(anyhow::anyhow!("WS server handshake failed: {e}"));
            }
        }
    }
}

impl Variant for WebSocketVariant {
    fn name(&self) -> &str {
        "websocket"
    }

    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        &[ThreadingMode::Single, ThreadingMode::Multi]
    }

    fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()> {
        self.threading_mode = threading_mode;
        if matches!(self.config.qos, Qos::BestEffort | Qos::LatestValue) {
            bail!(
                "websocket variant does not support QoS {} (reliable QoS 3-4 only)",
                self.config.qos
            );
        }

        let server_pairs: Vec<PeerDesc> = self
            .config
            .peers
            .iter()
            .filter(|p| p.role == PairRole::Server)
            .cloned()
            .collect();
        let client_pairs: Vec<PeerDesc> = self
            .config
            .peers
            .iter()
            .filter(|p| p.role == PairRole::Client)
            .cloned()
            .collect();

        let listener = if !server_pairs.is_empty() {
            let l = TcpListener::bind(self.config.listen_addr).with_context(|| {
                format!(
                    "failed to bind WS TCP listener on {}",
                    self.config.listen_addr
                )
            })?;
            l.set_nonblocking(true)
                .context("failed to set WS listener non-blocking")?;
            Some(l)
        } else {
            None
        };

        let recv_buffer_kb = self.config.recv_buffer_kb;

        for peer in &client_pairs {
            let ws = ws_client_connect(peer.addr)
                .with_context(|| format!("failed WS client connect to {}", peer.addr))?;
            apply_recv_buffer_kb(ws.get_ref(), recv_buffer_kb, peer.addr);
            apply_single_mode_write_timeout(ws.get_ref(), threading_mode, peer.addr);
            self.peers.push(WsPeer {
                name: peer.name.clone(),
                addr: peer.addr,
                io: PeerIo::Single(ws),
            });
        }

        if let Some(listener) = listener {
            let deadline = Instant::now() + PEER_HANDSHAKE_TIMEOUT;
            let mut accepted_count = 0usize;
            while accepted_count < server_pairs.len() {
                if Instant::now() >= deadline {
                    bail!(
                        "timed out waiting for {} WS peer(s) to connect to {}",
                        server_pairs.len() - accepted_count,
                        self.config.listen_addr
                    );
                }
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let ws = ws_server_accept(stream, addr).with_context(|| {
                            format!("WS server handshake failed for inbound {addr}")
                        })?;
                        apply_recv_buffer_kb(ws.get_ref(), recv_buffer_kb, addr);
                        apply_single_mode_write_timeout(ws.get_ref(), threading_mode, addr);
                        let name = server_pairs
                            .iter()
                            .filter(|p| p.addr.ip() == addr.ip())
                            .nth(accepted_count_for_ip(&self.peers, addr.ip()))
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| format!("inbound-{addr}"));
                        self.peers.push(WsPeer {
                            name,
                            addr,
                            io: PeerIo::Single(ws),
                        });
                        accepted_count += 1;
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("WS accept error: {e}"));
                    }
                }
            }
        }

        Ok(())
    }

    fn attach_logger(&mut self, logger: LoggerHandle) {
        self.logger = Some(logger);
    }

    fn start_reader_threads(&mut self, mode: ThreadingMode) -> Result<()> {
        if mode == ThreadingMode::Single {
            return Ok(());
        }
        // T14.10: the mpsc carries lifecycle items only -- a small
        // fixed bound is sufficient. Receive events flow directly to
        // JSONL from the reader thread via the shared LoggerHandle.
        let (tx, rx) = sync_channel::<ReaderItem>(LIFECYCLE_CHANNEL_CAPACITY);
        self.recv_tx = Some(tx.clone());
        self.recv_rx = Some(rx);

        // The logger is only required when there are peers to spawn
        // reader threads for -- the zero-peers smoke path is exercised
        // by unit tests that drive `start_reader_threads` without
        // routing through `run_protocol`.
        let logger_handle = if self.peers.is_empty() {
            None
        } else {
            Some(self.logger.clone().context(
                "websocket variant has no LoggerHandle; driver must call attach_logger \
                 before start_reader_threads when peers are present",
            )?)
        };

        let old_peers = std::mem::take(&mut self.peers);
        for peer in old_peers {
            let role = role_for_peer(&self.config.peers, &peer.name);
            let peer_name = peer.name.clone();
            let peer_addr = peer.addr;

            let ws = match peer.io {
                PeerIo::Single(ws) => ws,
                PeerIo::Multi { .. } => {
                    bail!(
                        "internal error: peer {peer_name} already in Multi mode at \
                         start_reader_threads"
                    );
                }
            };

            let write_stream = ws
                .get_ref()
                .try_clone()
                .with_context(|| format!("try_clone TcpStream for peer {peer_name}"))?;
            let _ = write_stream.set_read_timeout(None);
            let _ = write_stream.set_write_timeout(None);

            let writer = Arc::new(Mutex::new(MultiWriter {
                ctx: WebSocketContext::new(role, None),
                stream: write_stream,
            }));
            let shutdown = Arc::new(AtomicBool::new(false));

            let tx_for_reader = tx.clone();
            let shutdown_for_reader = Arc::clone(&shutdown);
            let peer_name_for_thread = peer_name.clone();
            let logger_for_reader = logger_handle
                .as_ref()
                .expect("logger_handle is set whenever there are peers")
                .clone();
            let handle = std::thread::Builder::new()
                .name(format!("ws-reader-{peer_name}"))
                .spawn(move || {
                    reader_thread_main(
                        peer_name_for_thread,
                        peer_addr,
                        ws,
                        tx_for_reader,
                        shutdown_for_reader,
                        logger_for_reader,
                    );
                })
                .with_context(|| format!("spawn reader thread for peer {peer_name}"))?;

            self.peers.push(WsPeer {
                name: peer_name.clone(),
                addr: peer_addr,
                io: PeerIo::Multi { writer },
            });
            self.reader_threads.push(ReaderThread {
                peer_name,
                handle,
                shutdown,
            });
        }

        Ok(())
    }

    fn stop_reader_threads(&mut self) -> Result<()> {
        if self.reader_threads.is_empty() {
            return Ok(());
        }
        for rt in &self.reader_threads {
            rt.shutdown.store(true, Ordering::Release);
        }
        self.recv_tx = None;

        let drained = std::mem::take(&mut self.reader_threads);
        for rt in drained {
            let peer_name = rt.peer_name.clone();
            let handle = rt.handle;
            let (signal_tx, signal_rx) = std::sync::mpsc::channel::<()>();
            let watcher = std::thread::Builder::new()
                .name(format!("ws-reader-joiner-{peer_name}"))
                .spawn(move || {
                    let _ = handle.join();
                    let _ = signal_tx.send(());
                });
            match watcher {
                Ok(_watcher) => match signal_rx.recv_timeout(READER_JOIN_TIMEOUT) {
                    Ok(()) => {}
                    Err(_) => {
                        eprintln!(
                            "warning: WS reader thread for peer {peer_name} did not exit \
                             within {:?}; abandoning (process exit will reap it)",
                            READER_JOIN_TIMEOUT
                        );
                    }
                },
                Err(e) => {
                    eprintln!("warning: failed to spawn join-watcher for peer {peer_name}: {e}");
                }
            }
        }
        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        match qos {
            Qos::BestEffort | Qos::LatestValue => {
                bail!("websocket variant does not support QoS {qos} -- reliable QoS 3-4 only");
            }
            Qos::ReliableUdp | Qos::ReliableTcp => {
                let frame = protocol::encode_data(qos, seq, path, &self.runner, payload);
                self.broadcast_binary(frame)?;
            }
        }
        Ok(())
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        if self.threading_mode == ThreadingMode::Multi {
            return Ok(self.poll_peers_once_multi());
        }
        const POLL_BUDGET: u32 = 256;
        for _ in 0..POLL_BUDGET {
            let pending_before = self.pending_eots.len();
            if let Some(update) = self.poll_peers_once_single() {
                return Ok(Some(update));
            }
            if self.pending_eots.len() == pending_before {
                return Ok(None);
            }
        }
        Ok(None)
    }

    fn disconnect(&mut self) -> Result<()> {
        let close_deadline = Instant::now() + DISCONNECT_GRACE;
        let mut peers = std::mem::take(&mut self.peers);
        for peer in peers.iter_mut() {
            match &mut peer.io {
                PeerIo::Single(ws) => {
                    let _ = ws.close(None);
                }
                PeerIo::Multi { writer } => {
                    if let Ok(mut guard) = writer.lock() {
                        let MultiWriter { ctx, stream } = &mut *guard;
                        let _ = ctx.close(stream, None);
                    }
                }
            }
        }
        for peer in peers.iter_mut() {
            if let PeerIo::Single(ws) = &mut peer.io {
                while Instant::now() < close_deadline {
                    match ws.read() {
                        Ok(_) => continue,
                        Err(tungstenite::Error::ConnectionClosed)
                        | Err(tungstenite::Error::AlreadyClosed) => break,
                        Err(tungstenite::Error::Io(e)) if is_transient_io_error(&e) => {
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        for peer in peers.iter() {
            match &peer.io {
                PeerIo::Single(ws) => {
                    let _ = ws.get_ref().shutdown(Shutdown::Both);
                }
                PeerIo::Multi { writer } => {
                    if let Ok(guard) = writer.lock() {
                        let _ = guard.stream.shutdown(Shutdown::Both);
                    }
                }
            }
        }
        drop(peers);
        self.recv_tx = None;
        self.recv_rx = None;
        Ok(())
    }

    // T15.8: signal_end_of_test / poll_peer_eots removed. The on-wire
    // EOT exchange is no longer driven; runner-coordinated termination
    // (T15.4) plus variant-side idle detection (T15.5) is the sole
    // exit mechanism.
}

fn accepted_count_for_ip(accepted: &[WsPeer], ip: std::net::IpAddr) -> usize {
    accepted.iter().filter(|p| p.addr.ip() == ip).count()
}

fn role_for_peer(peer_descs: &[PeerDesc], peer_name: &str) -> Role {
    for d in peer_descs {
        if d.name == peer_name {
            return match d.role {
                PairRole::Client => Role::Client,
                PairRole::Server => Role::Server,
            };
        }
    }
    Role::Server
}

fn is_transient_io_error(e: &std::io::Error) -> bool {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        return true;
    }
    matches!(e.raw_os_error(), Some(997) | Some(10035) | Some(10060))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use variant_base::Logger;

    fn dummy_config(qos: Qos) -> WebSocketConfig {
        WebSocketConfig {
            listen_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            peers: Vec::new(),
            qos,
            recv_buffer_kb: 4096,
            values_per_tick: 100,
        }
    }

    /// Build a temporary `LoggerHandle` backed by a tmpdir-scoped JSONL
    /// file. Used by tests that exercise the Multi-mode reader-thread
    /// path post-T14.10 (the reader needs a logger handle to write
    /// `receive` events directly).
    fn temp_logger_handle() -> (LoggerHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = Logger::new(
            dir.path().to_str().expect("tmp path utf8"),
            "websocket-test",
            "self",
            "run01",
        )
        .expect("logger ok");
        (LoggerHandle::new(logger), dir)
    }

    #[test]
    fn name_returns_websocket() {
        let v = WebSocketVariant::new("r", dummy_config(Qos::ReliableTcp));
        assert_eq!(v.name(), "websocket");
    }

    #[test]
    fn supports_single_and_multi_threading_modes() {
        let v = WebSocketVariant::new("r", dummy_config(Qos::ReliableTcp));
        let modes = v.supported_threading_modes();
        assert!(modes.contains(&ThreadingMode::Single));
        assert!(modes.contains(&ThreadingMode::Multi));
        assert_eq!(modes.len(), 2);
    }

    #[test]
    fn publish_qos1_returns_error() {
        let mut v = WebSocketVariant::new("r", dummy_config(Qos::BestEffort));
        let err = v
            .publish("/p", &[0u8], Qos::BestEffort, 1)
            .expect_err("qos 1 must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not support") || msg.contains("reliable QoS"));
    }

    #[test]
    fn publish_qos2_returns_error() {
        let mut v = WebSocketVariant::new("r", dummy_config(Qos::LatestValue));
        let err = v
            .publish("/p", &[0u8], Qos::LatestValue, 1)
            .expect_err("qos 2 must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("QoS 2") || msg.contains("reliable"));
    }

    #[test]
    fn connect_with_qos1_errors() {
        let mut v = WebSocketVariant::new("r", dummy_config(Qos::BestEffort));
        let err = v
            .connect(variant_base::ThreadingMode::Single)
            .expect_err("connect must reject qos 1");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not support") || msg.contains("reliable"));
    }

    #[test]
    fn connect_records_threading_mode() {
        let mut v = WebSocketVariant::new("r", dummy_config(Qos::ReliableTcp));
        v.connect(ThreadingMode::Multi)
            .expect("connect with empty peers + reliable QoS succeeds");
        assert_eq!(v.active_threading_mode(), ThreadingMode::Multi);
    }

    // T15.8: removed tests `record_eot_dedupes`, `record_eot_filters_own_runner`,
    // and `record_eot_distinguishes_writers`. They exercised the
    // poll_peer_eots trait method that no longer exists.

    #[test]
    fn try_publish_qos3_returns_true_in_happy_path() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableUdp));
        let ok = v
            .try_publish("/p", &[0u8; 8], Qos::ReliableUdp, 1)
            .expect("try_publish must not error");
        assert!(ok);
    }

    #[test]
    fn try_publish_qos4_returns_true_in_happy_path() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        let ok = v
            .try_publish("/p", &[0u8; 8], Qos::ReliableTcp, 1)
            .expect("try_publish must not error");
        assert!(ok);
    }

    #[test]
    fn reader_thread_lifecycle_zero_peers() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.connect(ThreadingMode::Multi).expect("connect ok");
        assert_eq!(v.reader_thread_count(), 0);
        v.start_reader_threads(ThreadingMode::Multi)
            .expect("start_reader_threads ok with zero peers");
        assert_eq!(v.reader_thread_count(), 0);
        v.stop_reader_threads()
            .expect("stop_reader_threads ok with no threads");
        assert_eq!(v.reader_thread_count(), 0);
    }

    #[test]
    fn reader_thread_lifecycle_spawns_and_joins() {
        use std::net::TcpListener;
        use std::sync::atomic::AtomicU16;
        static PORT: AtomicU16 = AtomicU16::new(30501);
        let port = PORT.fetch_add(1, Ordering::SeqCst);
        let listen_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(listen_addr).expect("bind ok");
        listener.set_nonblocking(false).unwrap();

        let server_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let _ws = tungstenite::accept(stream).expect("server upgrade");
            std::thread::sleep(Duration::from_secs(2));
        });

        let stream =
            TcpStream::connect(listen_addr).expect("client connect to localhost test port");
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let url = format!("ws://{listen_addr}/bench");
        let (ws, _resp) =
            tungstenite::client::client(url.as_str(), stream).expect("client upgrade");
        let s = ws.get_ref();
        s.set_write_timeout(None).unwrap();
        s.set_read_timeout(Some(READ_POLL_TIMEOUT)).unwrap();

        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.config.peers.push(PeerDesc {
            name: "peer".to_string(),
            addr: listen_addr,
            role: PairRole::Client,
        });
        v.peers.push(WsPeer {
            name: "peer".to_string(),
            addr: listen_addr,
            io: PeerIo::Single(ws),
        });
        v.threading_mode = ThreadingMode::Multi;
        // T14.10: reader threads need a LoggerHandle so they can write
        // receive events directly to JSONL. Provide one backed by a
        // tmpdir-scoped file.
        let (logger_handle, _tmp_log_dir) = temp_logger_handle();
        v.attach_logger(logger_handle);

        v.start_reader_threads(ThreadingMode::Multi)
            .expect("start_reader_threads ok");
        assert_eq!(v.reader_thread_count(), 1);

        let join_started = Instant::now();
        v.stop_reader_threads().expect("stop_reader_threads ok");
        let elapsed = join_started.elapsed();
        assert!(
            elapsed < READER_JOIN_TIMEOUT + Duration::from_secs(1),
            "stop_reader_threads should join within ~{READER_JOIN_TIMEOUT:?} (took {elapsed:?})"
        );
        assert_eq!(v.reader_thread_count(), 0);

        let _ = server_thread.join();
    }

    /// T17.5: a Single-mode peer whose write hits the installed
    /// `SO_SNDTIMEO` MUST NOT be dropped from the active set. Per the
    /// E17 / DESIGN.md § 6.5 strict-no-skip contract, transient
    /// back-pressure at QoS 3/4 must block the writer rather than
    /// drop bytes.
    ///
    /// Pre-T17.5 (T14.19 era) this test verified the opposite:
    /// after `SO_SNDTIMEO` fired the peer was dropped, the active
    /// set went empty, and `broadcast_binary` returned `Ok(())` so
    /// the spawn could exit cleanly. That accepted near-zero
    /// delivery as the cost of unwedging the deadlock; E17
    /// rescinded the acceptance.
    ///
    /// Setup is identical: a server that accepts but never reads
    /// (so the variant's kernel send buffer fills). Post-T17.5 the
    /// variant blocks inside `send_to_peer_with_retry`'s drain-then-
    /// flush loop forever (the peer is genuinely stuck because the
    /// server never reads). To bound the test we spawn the
    /// broadcast on a worker thread, observe that:
    ///
    /// 1. The worker's iteration counter advances briefly (we get
    ///    SOME data into the kernel send buffer) then FREEZES while
    ///    the worker is inside the retry loop -- proof that the
    ///    publisher is blocked, not returning Ok with the peer
    ///    dropped or skipping the message.
    /// 2. After we tear the server down (forcing a non-transient
    ///    error on the socket), the worker exits and reports the
    ///    final peer count == 0 -- proof that the genuine-fatal-
    ///    error path still drops correctly.
    #[test]
    fn t17_5_broadcast_blocks_and_keeps_peer_under_back_pressure() {
        use socket2::SockRef;
        use std::net::TcpListener;
        use std::sync::atomic::AtomicU16;

        static PORT: AtomicU16 = AtomicU16::new(30801);
        let port = PORT.fetch_add(1, Ordering::SeqCst);
        let listen_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(listen_addr).expect("bind ok");
        listener.set_nonblocking(false).unwrap();

        // Server: accept + upgrade, then go idle until the test
        // signals shutdown. Never reads, so the variant's kernel
        // send buffer fills and trips SO_SNDTIMEO.
        let (server_shutdown_tx, server_shutdown_rx) = std::sync::mpsc::channel::<()>();
        let server_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let ws = tungstenite::accept(stream).expect("server upgrade");
            let _ = server_shutdown_rx.recv();
            drop(ws);
        });

        let stream =
            TcpStream::connect(listen_addr).expect("client connect to localhost test port");
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let url = format!("ws://{listen_addr}/bench");
        let (ws, _resp) =
            tungstenite::client::client(url.as_str(), stream).expect("client upgrade");
        let s = ws.get_ref();
        s.set_read_timeout(Some(READ_POLL_TIMEOUT)).unwrap();
        // Short test write timeout for fast feedback. Production
        // uses 100 ms (T17.5); the test uses 50 ms.
        s.set_write_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let sock = SockRef::from(s);
        sock.set_send_buffer_size(1024).unwrap();

        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.config.peers.push(PeerDesc {
            name: "peer".to_string(),
            addr: listen_addr,
            role: PairRole::Client,
        });
        v.peers.push(WsPeer {
            name: "peer".to_string(),
            addr: listen_addr,
            io: PeerIo::Single(ws),
        });
        v.threading_mode = ThreadingMode::Single;

        let payload = vec![0xAAu8; 8192];
        let (iter_tx, iter_rx) = std::sync::mpsc::channel::<u32>();
        let (peer_count_tx, peer_count_rx) = std::sync::mpsc::channel::<usize>();
        let worker = std::thread::spawn(move || {
            let max_iters: u32 = 10_000;
            for i in 0..max_iters {
                if iter_tx.send(i).is_err() {
                    break;
                }
                if v.broadcast_binary(payload.clone()).is_err() {
                    break;
                }
            }
            drop(iter_tx);
            let _ = peer_count_tx.send(v.peers.len());
        });

        // Phase 1: wait briefly for the counter to start advancing.
        let mut last_seen: u32 = 0;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            match iter_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(i) => last_seen = i,
                Err(_) => break,
            }
        }
        // Phase 2: confirm the counter has frozen.
        let before_freeze = last_seen;
        let freeze_start = Instant::now();
        while freeze_start.elapsed() < Duration::from_millis(500) {
            if let Ok(i) = iter_rx.recv_timeout(Duration::from_millis(50)) {
                last_seen = i;
            }
        }
        assert_eq!(
            last_seen, before_freeze,
            "T17.5 contract violation: broadcast_binary iteration count \
             advanced from {before_freeze} to {last_seen} during a 500ms \
             freeze window under sustained back-pressure. Per DESIGN.md \
             § 6.5, QoS 3/4 must block the publisher rather than dropping \
             bytes or returning skips."
        );

        // Tear the server down. The worker's blocked retry loop
        // will observe a non-transient error and exit cleanly.
        let _ = server_shutdown_tx.send(());
        let _ = server_thread.join();
        let _ = worker.join();

        // Confirm the genuine-fatal-error path still drops the
        // peer correctly. Final count should be zero.
        let final_count = peer_count_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker must signal final peer count");
        assert_eq!(
            final_count, 0,
            "after teardown the wedged peer must be dropped via the \
             genuine-error path (ConnectionReset/Closed); got {final_count}"
        );
    }

    // T15.8: removed test `t14_10_poll_multi_drains_lifecycle_items_only`.
    // It exercised the poll_peer_eots trait method that no longer exists.

    /// T14.10: `poll_peers_once_multi` must process a `PeerDropped`
    /// lifecycle item by removing the named peer from the active set.
    /// We bind a localhost listener + auto-accept thread to produce a
    /// connected `TcpStream` for the synthetic Multi-mode peer; the
    /// stream's IO is irrelevant here -- the test only verifies the
    /// `peers.retain` side effect.
    #[test]
    fn t14_10_poll_multi_processes_peer_dropped() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.threading_mode = ThreadingMode::Multi;
        let (tx, rx) = sync_channel::<ReaderItem>(LIFECYCLE_CHANNEL_CAPACITY);
        v.recv_tx = Some(tx.clone());
        v.recv_rx = Some(rx);

        // Cheap connected TcpStream for the synthetic peer's writer.
        // The stream is never read or written through; we just need to
        // satisfy the `MultiWriter::stream` field.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let local_addr = listener.local_addr().expect("local_addr");
        let accept_handle = std::thread::spawn(move || listener.accept());
        let stream = TcpStream::connect(local_addr).expect("loopback connect");
        let _ = accept_handle.join();

        v.peers.push(WsPeer {
            name: "alice".to_string(),
            addr: local_addr,
            io: PeerIo::Multi {
                writer: Arc::new(Mutex::new(MultiWriter {
                    ctx: WebSocketContext::new(Role::Client, None),
                    stream,
                })),
            },
        });

        tx.send(ReaderItem::PeerDropped {
            peer: "alice".to_string(),
            reason: "test-induced".to_string(),
        })
        .expect("send peer_dropped");

        let data_hit = v.poll_peers_once_multi();
        assert!(data_hit.is_none());
        assert!(
            v.peers.iter().all(|p| p.name != "alice"),
            "PeerDropped lifecycle item must remove the peer from the active set"
        );
    }

    /// T14.10: the variant's `attach_logger` hook stores the handle so
    /// subsequent `start_reader_threads` calls can clone it into
    /// spawned reader threads. Verified observationally via the
    /// internal `logger` field after the hook runs.
    #[test]
    fn t14_10_attach_logger_stores_handle() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        assert!(v.logger.is_none(), "logger starts empty");
        let (handle, _tmp) = temp_logger_handle();
        v.attach_logger(handle);
        assert!(
            v.logger.is_some(),
            "attach_logger must persist the handle for use by reader threads"
        );
    }

    // ----- T18.3a: receive events from both call sites land in the
    //               shared compact `EventBuffer` -----

    /// Build a `LoggerHandle` with a shared compact-buffer sink wired
    /// in, mirroring how the driver constructs it in `run_protocol`.
    /// Returns the handle, the shared `Arc<Mutex<CompactBuffers>>` for
    /// assertion, and the holding tempdir.
    fn temp_logger_handle_with_compact(
        legacy_jsonl: bool,
    ) -> (
        LoggerHandle,
        std::sync::Arc<std::sync::Mutex<variant_base::CompactBuffers>>,
        tempfile::TempDir,
    ) {
        use std::sync::{Arc as StdArc, Mutex as StdMutex};
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = Logger::new(
            dir.path().to_str().expect("tmp path utf8"),
            "websocket-test",
            "self",
            "run01",
        )
        .expect("logger ok");
        let mut handle = LoggerHandle::new(logger);
        let buffers: StdArc<StdMutex<variant_base::CompactBuffers>> =
            StdArc::new(StdMutex::new(variant_base::CompactBuffers::new()));
        handle.attach_compact_sink(StdArc::clone(&buffers), legacy_jsonl);
        (handle, buffers, dir)
    }

    /// T18.3a -- Single-mode call site: `drain_current_peer_into_logger`
    /// pulls a frame off the WS socket and emits the receive via the
    /// `LoggerHandle`. Post-T18.3a the row must land in the shared
    /// compact buffer too, not only the legacy JSONL stream.
    ///
    /// Setup: stand up a real WS server-side socket, hand the client-
    /// side `WebSocket<TcpStream>` to a `WsPeer` in Single mode, push a
    /// data frame from the server, then call
    /// `drain_current_peer_into_logger`. Assert the compact buffer
    /// gained exactly one Receive row whose seq / writer / path match
    /// what the server sent.
    #[test]
    fn t18_3a_single_mode_drain_pushes_into_compact_buffer() {
        use std::net::TcpListener;
        // Bind to an ephemeral OS-assigned port to avoid collisions
        // with parallel tests in the same binary.
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind ok");
        let listen_addr = listener.local_addr().expect("local_addr");
        listener.set_nonblocking(false).unwrap();

        // Server thread sends one binary data frame and waits a moment
        // before letting the connection drop.
        let server_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut ws = tungstenite::accept(stream).expect("server upgrade");
            let body = protocol::encode_data(Qos::ReliableTcp, 42, "/p", "alice", &[1u8; 16]);
            ws.send(tungstenite::Message::Binary(body))
                .expect("server send");
            // Keep the connection up briefly so the client drain can
            // pull the frame before the socket dies.
            std::thread::sleep(Duration::from_millis(200));
        });

        let stream =
            TcpStream::connect(listen_addr).expect("client connect to localhost test port");
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let url = format!("ws://{listen_addr}/bench");
        let (ws, _resp) =
            tungstenite::client::client(url.as_str(), stream).expect("client upgrade");
        // Use a slightly longer read timeout than the default so the
        // first `ws.read()` inside the drain helper has enough budget
        // to surface the inbound frame.
        let s = ws.get_ref();
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        let mut peer = WsPeer {
            name: "alice".to_string(),
            addr: listen_addr,
            io: PeerIo::Single(ws),
        };

        let (handle, compact, _tmp) = temp_logger_handle_with_compact(false);
        drain_current_peer_into_logger(&mut peer, Some(&handle));

        let buf = compact.lock().unwrap();
        assert_eq!(
            buf.len(),
            1,
            "T17.5 drain helper must push the receive into the compact buffer; got {} rows",
            buf.len()
        );
        assert_eq!(buf.kind[0], variant_base::EventKind::Receive as u8);
        assert_eq!(buf.seq[0], 42);
        assert_eq!(buf.bytes[0], 16);
        assert_eq!(buf.qos[0], 4);
        assert_eq!(buf.peers.dict(), &["alice".to_string()]);
        assert_eq!(buf.paths.dict(), &["/p".to_string()]);

        let _ = server_thread.join();
    }

    /// T18.3a -- Multi-mode call site: a dedicated reader thread pulls
    /// frames off the WS socket and emits each receive via the
    /// `LoggerHandle`. Post-T18.3a every such row must also land in
    /// the shared compact buffer.
    ///
    /// Setup is closer to `reader_thread_lifecycle_spawns_and_joins`:
    /// stand up a real server, hand the client-side WS to
    /// `start_reader_threads` in Multi mode, push a few data frames
    /// from the server, wait for the reader to consume them, then
    /// shut the reader thread down and assert the compact buffer
    /// captured exactly the rows we sent.
    #[test]
    fn t18_3a_multi_mode_reader_thread_pushes_into_compact_buffer() {
        use std::net::TcpListener;
        // Bind to an ephemeral OS-assigned port to avoid collisions
        // with parallel tests in the same binary.
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind ok");
        let listen_addr = listener.local_addr().expect("local_addr");
        listener.set_nonblocking(false).unwrap();

        // Server sends three data frames then sleeps so the client
        // reader has time to consume before the connection ends.
        let server_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut ws = tungstenite::accept(stream).expect("server upgrade");
            for seq in 1u64..=3 {
                let body = protocol::encode_data(Qos::ReliableTcp, seq, "/p", "alice", &[7u8; 32]);
                ws.send(tungstenite::Message::Binary(body))
                    .expect("server send");
            }
            std::thread::sleep(Duration::from_millis(500));
        });

        let stream =
            TcpStream::connect(listen_addr).expect("client connect to localhost test port");
        stream.set_nodelay(true).unwrap();
        let url = format!("ws://{listen_addr}/bench");
        let (ws, _resp) =
            tungstenite::client::client(url.as_str(), stream).expect("client upgrade");
        let s = ws.get_ref();
        s.set_write_timeout(None).unwrap();
        s.set_read_timeout(Some(READ_POLL_TIMEOUT)).unwrap();

        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.config.peers.push(PeerDesc {
            name: "alice".to_string(),
            addr: listen_addr,
            role: PairRole::Client,
        });
        v.peers.push(WsPeer {
            name: "alice".to_string(),
            addr: listen_addr,
            io: PeerIo::Single(ws),
        });
        v.threading_mode = ThreadingMode::Multi;

        let (handle, compact, _tmp) = temp_logger_handle_with_compact(false);
        v.attach_logger(handle);
        v.start_reader_threads(ThreadingMode::Multi)
            .expect("start_reader_threads ok");

        // Poll the compact buffer until the reader thread has pushed
        // all three rows or a wallclock budget elapses.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let len = compact.lock().unwrap().len();
            if len >= 3 {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "compact buffer never received the 3 frames from reader thread; saw {} rows",
                    len
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        v.stop_reader_threads().expect("stop_reader_threads ok");

        let buf = compact.lock().unwrap();
        assert_eq!(
            buf.len(),
            3,
            "Multi-mode reader thread must push every receive into the compact buffer"
        );
        assert_eq!(buf.kind, vec![variant_base::EventKind::Receive as u8; 3]);
        assert_eq!(buf.seq, vec![1, 2, 3]);
        assert!(buf.bytes.iter().all(|b| *b == 32));
        assert_eq!(buf.peers.dict(), &["alice".to_string()]);
        assert_eq!(buf.paths.dict(), &["/p".to_string()]);

        let _ = server_thread.join();
    }
}
