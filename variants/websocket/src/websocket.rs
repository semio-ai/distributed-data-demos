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

use variant_base::types::{Qos, ReceivedUpdate, ThreadingMode};
use variant_base::{PeerEot, Variant};

use crate::pairing::{DerivedEndpoints, PairRole, PeerDesc};
use crate::protocol::{self, Frame};

/// Read timeout applied to every per-peer underlying TCP socket. Short
/// enough to keep the poll loop responsive (so other peers' reads aren't
/// starved) yet long enough to avoid syscall churn when nothing is in
/// flight.
const READ_POLL_TIMEOUT: Duration = Duration::from_millis(1);

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

/// Minimum per-peer mpsc capacity. The formula `4 * values_per_tick *
/// peer_count` may yield a tiny number (or zero) for unit tests with
/// no peers; floor at 16 so the channel always has breathing room.
const MIN_CHANNEL_CAPACITY: usize = 16;

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
    /// Driver `values_per_tick` -- used to size the Multi-mode bounded
    /// mpsc channel (`4 * values_per_tick * peer_count`, floored at 16).
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
/// Frames are decoded by the reader (CPU work moves off the driver
/// thread); EOT markers and data updates take separate paths inside
/// `poll_receive`.
enum ReaderItem {
    Data(ReceivedUpdate),
    Eot { writer: String, eot_id: u64 },
    /// Reader observed a fatal per-peer error; the driver thread can
    /// log it and forget the peer. Carries the peer's runner name so
    /// the driver can correlate the drop.
    PeerDropped { peer: String, reason: String },
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

    /// Multi-mode poll: drain one item from the shared receiver channel.
    /// Reader threads have already decoded frames; this is a near-free
    /// `try_recv` on the driver thread.
    fn poll_peers_once_multi(&mut self) -> Option<ReceivedUpdate> {
        if self.recv_rx.is_none() {
            return None;
        }
        loop {
            let next = {
                let rx = match self.recv_rx.as_ref() {
                    Some(rx) => rx,
                    None => return None,
                };
                rx.try_recv()
            };
            match next {
                Ok(ReaderItem::Data(update)) => return Some(update),
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

    /// Send a binary frame to every active peer. Drops a peer on a fatal
    /// write error; mirrors Hybrid TCP's broadcast behaviour. Routes to
    /// the mode-appropriate write path.
    fn broadcast_binary(&mut self, payload: Vec<u8>) -> Result<()> {
        let mut keep: Vec<bool> = Vec::with_capacity(self.peers.len());
        let mut last_err: Option<anyhow::Error> = None;

        for peer in self.peers.iter_mut() {
            let result = match &mut peer.io {
                PeerIo::Single(ws) => ws.send(Message::Binary(payload.clone())).map_err(|e| {
                    anyhow::anyhow!("WS write error to peer {} ({}): {}", peer.name, peer.addr, e)
                }),
                PeerIo::Multi { writer } => {
                    let mut guard = writer
                        .lock()
                        .map_err(|_| anyhow::anyhow!("WS Multi writer mutex poisoned"))?;
                    let MultiWriter { ctx, stream } = &mut *guard;
                    ctx.write(stream, Message::Binary(payload.clone()))
                        .and_then(|()| ctx.flush(stream))
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "WS write error to peer {} ({}): {}",
                                peer.name,
                                peer.addr,
                                e
                            )
                        })
                }
            };
            match result {
                Ok(()) => keep.push(true),
                Err(e) => {
                    eprintln!(
                        "warning: dropping WS peer {} ({}) after write error: {:#}",
                        peer.name, peer.addr, e
                    );
                    keep.push(false);
                    last_err = Some(e);
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

        if self.peers.is_empty() {
            if let Some(e) = last_err {
                return Err(e.context("all WS peers dropped after write errors"));
            }
        }
        Ok(())
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

/// Background reader thread for a Multi-mode peer.
///
/// Owns the per-peer `WebSocket<TcpStream>` exclusively. Loops on
/// `WebSocket::read` with the short SO_RCVTIMEO previously installed
/// by `ws_client_connect` / `ws_server_accept` so the shutdown flag is
/// checked roughly every `READ_POLL_TIMEOUT`. Each decoded Data frame
/// is **dropped on full** so the reader NEVER blocks on the channel
/// (this is what breaks the T-impl.10 symmetric-flood deadlock).
/// EOT frames are pushed with a blocking-send fallback so the
/// synchronization marker is not silently lost when the channel is
/// briefly busy.
fn reader_thread_main(
    peer_name: String,
    peer_addr: SocketAddr,
    mut ws: WebSocket<TcpStream>,
    tx: SyncSender<ReaderItem>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        match ws.read() {
            Ok(Message::Binary(bytes)) => match protocol::decode_frame(&bytes) {
                Ok(Frame::Data(update)) => {
                    if !push_data_drop_on_full(&tx, ReaderItem::Data(update)) {
                        return;
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
            Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::AlreadyClosed) => {
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

/// Push a `Data` item into the shared channel, dropping on full.
///
/// On `TrySendError::Full` we drop the item and keep going. This is
/// the critical Multi-mode property that breaks the T-impl.10
/// symmetric-flood deadlock: the reader thread NEVER blocks on the
/// channel for data, so the kernel TCP recv buffer keeps draining,
/// which keeps the peer's writer from blocking on its end-of-test
/// broadcast. The bounded channel still acts as a useful OOM bound for
/// memory growth.
///
/// Returns `false` if the receiver was dropped (variant torn down).
fn push_data_drop_on_full(tx: &SyncSender<ReaderItem>, item: ReaderItem) -> bool {
    use std::sync::mpsc::TrySendError;
    match tx.try_send(item) {
        Ok(()) => true,
        Err(TrySendError::Full(_dropped)) => true,
        Err(TrySendError::Disconnected(_)) => false,
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

    fn start_reader_threads(&mut self, mode: ThreadingMode) -> Result<()> {
        if mode == ThreadingMode::Single {
            return Ok(());
        }
        let peer_count = self.peers.len().max(1);
        let bound = (self.config.values_per_tick as usize)
            .saturating_mul(4)
            .saturating_mul(peer_count)
            .max(MIN_CHANNEL_CAPACITY);
        let (tx, rx) = sync_channel::<ReaderItem>(bound);
        self.recv_tx = Some(tx.clone());
        self.recv_rx = Some(rx);

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
            let handle = std::thread::Builder::new()
                .name(format!("ws-reader-{peer_name}"))
                .spawn(move || {
                    reader_thread_main(
                        peer_name_for_thread,
                        peer_addr,
                        ws,
                        tx_for_reader,
                        shutdown_for_reader,
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
                    eprintln!(
                        "warning: failed to spawn join-watcher for peer {peer_name}: {e}"
                    );
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

    fn signal_end_of_test(&mut self) -> Result<u64> {
        let eot_id: u64 = rand::random();
        let frame = protocol::encode_eot(&self.runner, eot_id);
        self.broadcast_binary(frame)
            .context("failed to broadcast WS EOT marker")?;
        Ok(eot_id)
    }

    fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
        let drained: Vec<PeerEot> = self.pending_eots.drain(..).collect();
        Ok(drained)
    }
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
    match e.raw_os_error() {
        Some(997) => true,
        Some(10035) => true,
        Some(10060) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn dummy_config(qos: Qos) -> WebSocketConfig {
        WebSocketConfig {
            listen_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            peers: Vec::new(),
            qos,
            recv_buffer_kb: 4096,
            values_per_tick: 100,
        }
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

    #[test]
    fn record_eot_dedupes() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.record_eot("alice".to_string(), 42);
        v.record_eot("alice".to_string(), 42);
        let drained = v.poll_peer_eots().unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].writer, "alice");
        assert_eq!(drained[0].eot_id, 42);
        assert!(v.poll_peer_eots().unwrap().is_empty());
    }

    #[test]
    fn record_eot_filters_own_runner() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.record_eot("self".to_string(), 7);
        assert!(v.poll_peer_eots().unwrap().is_empty());
    }

    #[test]
    fn record_eot_distinguishes_writers() {
        let mut v = WebSocketVariant::new("self", dummy_config(Qos::ReliableTcp));
        v.record_eot("alice".to_string(), 1);
        v.record_eot("bob".to_string(), 2);
        let drained = v.poll_peer_eots().unwrap();
        assert_eq!(drained.len(), 2);
        let names: HashSet<&str> = drained.iter().map(|e| e.writer.as_str()).collect();
        assert!(names.contains("alice"));
        assert!(names.contains("bob"));
    }

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
}
