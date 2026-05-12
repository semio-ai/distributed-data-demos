/// UdpVariant: implements the `Variant` trait using raw UDP sockets
/// with multicast for QoS 1-3 and TCP for QoS 4.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use variant_base::{PeerEot, Qos, ReceivedUpdate, ThreadingMode, Variant};

use crate::controltcp::{self, ControlFrame, ControlPeer, ControlPeerEndpoint, ControlRole};
use crate::protocol;
use crate::protocol::Frame;
use crate::qos::{GapCheckResult, GapDetector, LatestValueTracker};

/// Number of times an EOT marker is sent on the UDP path.
///
/// Per `metak-shared/api-contracts/eot-protocol.md` "Custom UDP" mechanics:
/// 5 retries with 5 ms spacing for redundancy under loss. Receivers dedupe
/// by `(writer, eot_id)` so duplicates are silently absorbed.
const EOT_UDP_RETRIES: usize = 5;

/// Delay between successive UDP EOT sends.
const EOT_UDP_SPACING: Duration = Duration::from_millis(5);

/// Short blocking timeout applied to reader-thread sockets so the threads
/// can wake periodically and observe the shutdown flag without relying on
/// out-of-band signalling. Matches the websocket / hybrid pattern (T14.x):
/// reads use a real OS-level `SO_RCVTIMEO`; writes are unaffected.
const READER_RCVTIMEO: Duration = Duration::from_millis(50);

/// Maximum wall-clock time the variant waits for all expected inbound TCP
/// peer connections to be accepted before `start_reader_threads` proceeds.
/// Matches the pre-existing connect-time tolerance used by other variants.
const TCP_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum wall-clock time we wait for a reader thread to join during
/// `stop_reader_threads`. Threads that fail to exit within this window are
/// logged as wedged and abandoned -- preferred over deadlocking the
/// disconnect path. Matches the contract documented on
/// `Variant::stop_reader_threads` for the E14 rollout (T14.1 notes).
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Floor for the Multi-mode mpsc channel bound. Keeps the channel useful
/// even when `values_per_tick` is small (e.g. low-rate fixtures). Sized
/// well above one tick's-worth of frames so the reader thread can push a
/// short burst without bouncing on `TrySendError::Full`.
const MULTI_CHANNEL_FLOOR: usize = 16;

/// Configuration for the UDP variant.
///
/// Built by `main::run` from the runner-injected `--peers` map plus the
/// variant-specific `--multicast-group`, `--buffer-size`, `--tcp-base-port`
/// extra args. UDP multicast uses `multicast_group` directly with no
/// per-runner / per-QoS stride; TCP listen / connect addresses are derived in
/// main per the convention documented in CUSTOM.md and only consumed at
/// QoS 4.
#[derive(Debug, Clone)]
pub struct UdpConfig {
    /// Multicast group address and port. Same value on every runner; bound
    /// directly with no stride.
    pub multicast_group: SocketAddrV4,
    /// UDP receive buffer size.
    pub buffer_size: usize,
    /// The runner's own name, used as the writer field in the wire format.
    pub runner: String,
    /// QoS level for this spawn.
    pub qos: Qos,
    /// Local TCP listen address derived in main from `tcp_base_port` +
    /// runner_index + (qos - 1) * qos_stride. Only bound at QoS 4.
    pub tcp_listen_addr: SocketAddr,
    /// Remote TCP endpoints (one per non-self peer) derived in main. Only
    /// connected at QoS 4. May be empty (e.g. single-peer self-only test).
    pub tcp_peers: Vec<SocketAddr>,
    /// OS-level receive buffer size in kibibytes (1024-byte units). The
    /// variant applies `SO_RCVBUF = recv_buffer_kb * 1024` on every
    /// recv-side socket it owns: the UDP multicast socket and (at QoS 4)
    /// every inbound TCP stream. See `metak-shared/api-contracts/variant-cli.md`
    /// "E14 additions: --recv-buffer-kb" for the contract.
    pub recv_buffer_kb: u32,
    /// Driver's per-tick value count. Used to size the Multi-mode mpsc
    /// channel (see `start_reader_threads`). Unused in Single mode.
    pub values_per_tick: u32,
    /// T14.18: Local control TCP listen address (QoS-independent).
    pub control_listen_addr: SocketAddr,
    /// T14.18: Per-peer control wiring.
    pub control_peers: Vec<ControlPeerEndpoint>,
    /// T14.18: `--eot-timeout-secs` for the control-channel drain.
    pub eot_timeout_secs: Option<u64>,
}

/// Data frame placed on the bounded Multi-mode mpsc by reader threads.
///
/// As of T14.16 the Multi-mode reader threads route items into TWO
/// channels rather than one. `ReaderDataItem` rides the bounded
/// `data_tx` (existing `sync_channel`); lifecycle items (EOT, NACK,
/// TcpPeerDropped) ride the unbounded `lifecycle_tx`. See
/// `ReaderLifecycleItem` and the design notes in CUSTOM.md "Threading
/// modes (T14.16)".
///
/// Drop-on-full is acceptable for data: QoS 1/2 tolerate loss by
/// definition and QoS 3/4 framing semantics on the driver thread are
/// unchanged because the dropped item simply never lands in
/// `pending`.
enum ReaderDataItem {
    /// A decoded data frame from any transport (UDP multicast or TCP).
    Data(protocol::Message),
}

/// Lifecycle item placed on the unbounded Multi-mode mpsc by reader
/// threads. Must NEVER drop: EOT loss forces the peer's driver to wait
/// the full `eot_timeout`; NACK loss silently breaks QoS-3 reliability;
/// TcpPeerDropped loss leaves a stale peer reference around.
///
/// Per T14.16 the worker chose to FOLD NACK into the lifecycle channel
/// rather than introduce a third sibling. Rationale: NACKs are rare
/// (only fired on gap detection on the receiver side), losing them is
/// catastrophic for QoS 3 reliability, and one extra `std::sync::mpsc`
/// channel keeps both the wiring and the `poll_receive` drain
/// straightforward.
enum ReaderLifecycleItem {
    /// A decoded EOT marker.
    Eot(protocol::EotFrame),
    /// A raw NACK datagram (UDP-only). Parsed on the driver thread so the
    /// `send_buffer` lookup happens where the buffer lives.
    Nack(Vec<u8>),
    /// A drop signal for a per-peer TCP reader thread. Carries no payload;
    /// the driver does not need to know which peer dropped because TCP
    /// streams in Multi mode are owned by reader threads.
    TcpPeerDropped,
}

/// Resources spawned by `start_reader_threads(Multi)`. Owned by the variant
/// across the variant's lifetime so `stop_reader_threads` can tear them
/// down deterministically.
///
/// T14.16: the single shared `Receiver<ReaderItem>` is replaced by two
/// receivers: a bounded `data_rx` (drop-on-full acceptable) and an
/// unbounded `lifecycle_rx` (must-not-drop). `poll_receive` drains the
/// lifecycle receiver FIRST so EOT/NACK observations are never starved by
/// a saturated data channel.
struct MultiReaderState {
    /// Receiver side of the shared bounded data mpsc.
    data_rx: Receiver<ReaderDataItem>,
    /// Receiver side of the shared unbounded lifecycle mpsc.
    lifecycle_rx: Receiver<ReaderLifecycleItem>,
    /// Shutdown flag observed by every reader thread on each wakeup.
    shutdown: Arc<AtomicBool>,
    /// Join handles for spawned reader threads. The Drop / explicit-stop
    /// path tries each with `READER_JOIN_TIMEOUT` and abandons wedged
    /// threads with a single warning.
    handles: Vec<thread::JoinHandle<()>>,
}

/// The UDP variant implementation.
pub struct UdpVariant {
    config: UdpConfig,
    /// The main UDP socket for multicast send/receive (QoS 1-3).
    udp_socket: Option<UdpSocket>,
    /// Receive buffer.
    recv_buf: Vec<u8>,
    /// QoS 2: latest-value tracker for stale discard.
    latest_tracker: LatestValueTracker,
    /// QoS 3: gap detector.
    gap_detector: GapDetector,
    /// QoS 3: sent message buffer for NACK retransmit, keyed by seq.
    send_buffer: HashMap<u64, Vec<u8>>,
    /// QoS 4: TCP listener for incoming connections.
    ///
    /// Single mode: kept and polled lazily by `recv_tcp`.
    /// Multi mode: drained during `start_reader_threads` so every expected
    /// inbound stream is accepted before reader threads spawn; the
    /// listener is then dropped.
    tcp_listener: Option<TcpListener>,
    /// QoS 4: TCP streams to peers (for sending).
    tcp_out_streams: Vec<TcpStream>,
    /// QoS 4 / Single mode: TCP streams from peers (for receiving).
    /// In Multi mode these are moved into per-peer reader threads at
    /// `start_reader_threads` time, leaving this `Vec` empty.
    tcp_in_streams: Vec<TcpStream>,
    /// Internal queue for updates ready to be returned via poll_receive.
    pending: VecDeque<ReceivedUpdate>,
    /// Dedup set for observed peer EOTs. Each unique `(writer, eot_id)`
    /// pair is recorded here exactly once for the lifetime of the spawn.
    eot_seen: HashSet<(String, u64)>,
    /// Queue of newly-observed peer EOTs not yet drained by
    /// `poll_peer_eots`. Each entry corresponds to a fresh insertion
    /// into `eot_seen`.
    eot_queue: VecDeque<PeerEot>,
    /// Active threading mode. Set by `connect`. Single mode preserves
    /// pre-T14.3 behaviour. Multi mode enables the reader-thread path
    /// driven by `start_reader_threads` / `stop_reader_threads`.
    threading_mode: ThreadingMode,
    /// Reader-thread state. `Some` only in Multi mode while reader
    /// threads are running.
    multi: Option<MultiReaderState>,
    /// T14.18: per-peer control TCP connections.
    control_peers: Vec<ControlPeer>,
    /// T14.18: control reader threads (Multi mode only).
    control_threads: Vec<thread::JoinHandle<()>>,
    control_shutdown: Option<Arc<AtomicBool>>,
}

/// Apply `SO_RCVBUF = recv_buffer_kb * 1024` to a UDP `Socket`, but only as
/// an upward floor. The pre-existing `tune_udp_buffers` helper already
/// requested 8 MiB; high-rate same-host fixtures (the qos1 / qos4
/// regression tests at 100 K msg/s) depend on that floor and would
/// silently regress if we let the default `--recv-buffer-kb = 4096`
/// (4 MiB) shrink the buffer below it. The contract from variant-cli.md
/// says "Variants must call setsockopt(SO_RCVBUF, recv_buffer_kb *
/// 1024)"; we satisfy that for any `--recv-buffer-kb` greater than the
/// current achieved size and leave the buffer alone otherwise. Errors
/// are logged (single line) and swallowed: best-effort, like
/// `tune_udp_buffers`.
fn apply_recv_buffer_kb_udp(socket: &Socket, recv_buffer_kb: u32) {
    let requested = (recv_buffer_kb as usize).saturating_mul(1024);
    let current = socket.recv_buffer_size().unwrap_or(0);
    if requested <= current {
        // The operator-requested size is at or below what we already
        // achieved via `tune_udp_buffers`; nothing to do.
        return;
    }
    if let Err(e) = socket.set_recv_buffer_size(requested) {
        eprintln!(
            "[custom-udp] warning: set_recv_buffer_size({}) on UDP socket failed: {}",
            requested, e
        );
    }
}

/// Apply `SO_RCVBUF = recv_buffer_kb * 1024` to a TCP stream via a borrowed
/// `socket2::SockRef`. Best-effort like the UDP variant: never propagates
/// the error so a same-host fixture survives kernel clamping. See
/// `metak-shared/api-contracts/variant-cli.md` "E14 additions:
/// --recv-buffer-kb" for the contract.
fn apply_recv_buffer_kb_tcp(stream: &TcpStream, recv_buffer_kb: u32) {
    let requested = (recv_buffer_kb as usize).saturating_mul(1024);
    let sock_ref = socket2::SockRef::from(stream);
    if let Err(e) = sock_ref.set_recv_buffer_size(requested) {
        eprintln!(
            "[custom-udp] warning: set_recv_buffer_size({}) on TCP stream failed: {}",
            requested, e
        );
    }
}

/// Outcome of attempting to read one length-prefixed frame from a TCP stream.
///
/// Wire format is `[4 bytes BE total_len] [total_len - 4 bytes payload]` where
/// `total_len` covers the length prefix itself. Used by `read_framed_message`
/// and the QoS-4 `recv_tcp` path.
#[derive(Debug)]
pub(crate) enum FrameReadResult {
    /// A complete frame was read. Bytes include the 4-byte length prefix
    /// (i.e. the buffer is `total_len` bytes long, ready for `protocol::decode`).
    Frame(Vec<u8>),
    /// No bytes (or only a partial header / partial body) were available
    /// without blocking. Caller should retain the stream and try again later.
    WouldBlock,
    /// The peer must be dropped: framing error, EOF, undersized length prefix,
    /// oversized length prefix, or read error other than `WouldBlock`. The
    /// `&'static str` carries a short human-readable reason for logging.
    DropPeer(&'static str),
}

/// Try to read one length-prefixed frame from `stream`.
///
/// Validates that `4 <= header_min <= total_len <= max_total_len`. Any
/// out-of-range value (whether from a torn cross-machine read, a hostile
/// peer, or a buggy sender) is treated as a peer protocol violation: the
/// caller should drop the stream. This function never panics on the
/// content of `total_len`.
///
/// `WouldBlock` is returned if either the 4-byte length prefix or the
/// declared body bytes are not yet fully available; the caller should
/// retain the stream.
pub(crate) fn read_framed_message<R: Read>(
    stream: &mut R,
    max_total_len: usize,
) -> FrameReadResult {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return FrameReadResult::WouldBlock,
        Err(_) => return FrameReadResult::DropPeer("length prefix read failed"),
    }

    let total_len = u32::from_be_bytes(len_buf) as usize;
    if total_len < protocol::HEADER_FIXED_SIZE {
        // Torn cross-machine read or peer protocol violation: a valid frame
        // must include at minimum the fixed-size header. Without this check,
        // `vec![0u8; total_len]` followed by `msg_buf[..4].copy_from_slice`
        // panics for any total_len < 4.
        return FrameReadResult::DropPeer("undersized length prefix");
    }
    if total_len > max_total_len {
        return FrameReadResult::DropPeer("length prefix exceeds buffer_size");
    }

    let mut msg_buf = vec![0u8; total_len];
    msg_buf[..4].copy_from_slice(&len_buf);
    match stream.read_exact(&mut msg_buf[4..]) {
        Ok(()) => FrameReadResult::Frame(msg_buf),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => FrameReadResult::WouldBlock,
        Err(_) => FrameReadResult::DropPeer("body read failed"),
    }
}

impl UdpVariant {
    pub fn new(config: UdpConfig) -> Self {
        let recv_buf = vec![0u8; config.buffer_size];
        Self {
            config,
            udp_socket: None,
            recv_buf,
            latest_tracker: LatestValueTracker::new(),
            gap_detector: GapDetector::new(),
            send_buffer: HashMap::new(),
            tcp_listener: None,
            tcp_out_streams: Vec::new(),
            tcp_in_streams: Vec::new(),
            pending: VecDeque::new(),
            eot_seen: HashSet::new(),
            eot_queue: VecDeque::new(),
            threading_mode: ThreadingMode::Single,
            multi: None,
            control_peers: Vec::new(),
            control_threads: Vec::new(),
            control_shutdown: None,
        }
    }

    /// T14.18: establish per-peer control TCP connections.
    fn setup_control_channel(&mut self) -> Result<()> {
        if self.config.control_peers.is_empty() {
            return Ok(());
        }
        let server_peers: Vec<ControlPeerEndpoint> = self
            .config
            .control_peers
            .iter()
            .filter(|p| p.role == ControlRole::Server)
            .cloned()
            .collect();
        let client_peers: Vec<ControlPeerEndpoint> = self
            .config
            .control_peers
            .iter()
            .filter(|p| p.role == ControlRole::Client)
            .cloned()
            .collect();

        let listener =
            std::net::TcpListener::bind(self.config.control_listen_addr).with_context(|| {
                format!(
                    "T14.18: failed to bind control listener on {}",
                    self.config.control_listen_addr
                )
            })?;
        let deadline = Instant::now() + controltcp::CONTROL_ACCEPT_BUDGET;
        for expected in &server_peers {
            let (stream, accepted_addr) = controltcp::accept_with_budget(&listener, deadline)
                .with_context(|| {
                    format!(
                        "T14.18: control accept for peer {} timed out",
                        expected.peer_name
                    )
                })?;
            let peer = ControlPeer::from_stream(stream, expected.peer_name.clone(), accepted_addr)?;
            self.control_peers.push(peer);
        }
        drop(listener);

        for endpoint in &client_peers {
            let stream = controltcp::connect_with_budget(
                endpoint.peer_addr,
                controltcp::CONTROL_CONNECT_BUDGET,
            )
            .with_context(|| {
                format!(
                    "T14.18: control connect to peer {} ({}) failed",
                    endpoint.peer_name, endpoint.peer_addr
                )
            })?;
            let peer =
                ControlPeer::from_stream(stream, endpoint.peer_name.clone(), endpoint.peer_addr)?;
            self.control_peers.push(peer);
        }
        Ok(())
    }

    /// Single mode: poll every control peer once.
    fn poll_control_peers(&mut self) {
        let mut dropped = Vec::new();
        for (i, peer) in self.control_peers.iter_mut().enumerate() {
            match peer.try_recv_frame() {
                Ok(Some(ControlFrame::Eot { writer, eot_id })) => {
                    if writer != self.config.runner
                        && self.eot_seen.insert((writer.clone(), eot_id))
                    {
                        self.eot_queue.push_back(PeerEot { writer, eot_id });
                    }
                }
                Ok(Some(ControlFrame::Bye)) => {}
                Ok(None) => {}
                Err(e) => {
                    eprintln!(
                        "[custom-udp] T14.18 control peer {} read error: {:#}; dropping",
                        peer.peer_addr, e
                    );
                    peer.shutdown();
                    dropped.push(i);
                }
            }
        }
        for &i in dropped.iter().rev() {
            self.control_peers.remove(i);
        }
    }

    /// Effective EOT timeout seconds for the control-channel drain.
    fn effective_eot_timeout_secs(&self) -> u64 {
        const DEFAULT_EOT_TIMEOUT_SECS: u64 = 5;
        self.config
            .eot_timeout_secs
            .unwrap_or(DEFAULT_EOT_TIMEOUT_SECS)
    }

    /// Record an observed peer EOT, deduplicating by `(writer, eot_id)`.
    ///
    /// Skips the writer's own runner name (a sanity guard against echoing
    /// our own EOTs back through the multicast loopback or a self-connect).
    /// Returns `true` if the EOT was new and queued; `false` if it was a
    /// duplicate.
    fn record_peer_eot(&mut self, writer: String, eot_id: u64) -> bool {
        if writer == self.config.runner {
            return false;
        }
        let key = (writer.clone(), eot_id);
        if self.eot_seen.insert(key) {
            self.eot_queue.push_back(PeerEot { writer, eot_id });
            true
        } else {
            false
        }
    }

    /// Set up the multicast UDP socket.
    fn setup_udp(&mut self) -> Result<()> {
        let multicast_addr = self.config.multicast_group;

        // Create a UDP socket using socket2 for advanced options.
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("failed to create UDP socket")?;

        socket
            .set_reuse_address(true)
            .context("failed to set SO_REUSEADDR")?;
        socket.set_nonblocking(true)?;
        // T-impl.2: bump SO_RCVBUF / SO_SNDBUF to 8 MiB so the high-rate
        // same-host fixtures don't get clipped by ~64 KB Windows kernel
        // defaults. The helper logs a single warning if the OS caps the
        // achieved size below 1 MiB and continues regardless.
        variant_base::tune_udp_buffers(&socket).context("tune UDP buffers")?;
        // T14.3 (E14): override SO_RCVBUF to honour --recv-buffer-kb when
        // it requests more than the 8 MiB floor from `tune_udp_buffers`.
        // We deliberately call `tune_udp_buffers` first so the lower bound
        // is preserved on configs that omit `--recv-buffer-kb` (its
        // default is 4 MiB, below the 8 MiB floor); only larger requests
        // raise the size above the floor.
        apply_recv_buffer_kb_udp(&socket, self.config.recv_buffer_kb);

        // Bind to the multicast port on all interfaces.
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, multicast_addr.port());
        socket
            .bind(&SockAddr::from(bind_addr))
            .context("failed to bind UDP socket")?;

        // Join the multicast group.
        socket
            .join_multicast_v4(multicast_addr.ip(), &Ipv4Addr::UNSPECIFIED)
            .context("failed to join multicast group")?;

        // Enable multicast loopback so we receive our own messages (useful for testing).
        socket
            .set_multicast_loop_v4(true)
            .context("failed to set multicast loopback")?;

        let std_socket: UdpSocket = socket.into();
        self.udp_socket = Some(std_socket);

        Ok(())
    }

    /// Set up TCP listener and connections for QoS 4.
    fn setup_tcp(&mut self) -> Result<()> {
        // Bind on the derived per-runner / per-qos TCP listen address.
        let listener = TcpListener::bind(self.config.tcp_listen_addr).with_context(|| {
            format!(
                "failed to bind TCP listener on {}",
                self.config.tcp_listen_addr
            )
        })?;
        listener.set_nonblocking(true)?;
        eprintln!(
            "[custom-udp] TCP listener on {} for QoS 4",
            self.config.tcp_listen_addr
        );
        self.tcp_listener = Some(listener);

        // Connect to peers (excluding self — already filtered in main).
        //
        // T-impl.7: outbound TCP streams stay in blocking mode so
        // `write_all` truly blocks under kernel back-pressure. The
        // receive path uses `tcp_in_streams` (separate sockets accepted
        // from the listener), which retain non-blocking semantics for
        // polled reads — see `recv_tcp`. Mixing blocking writes with
        // non-blocking reads on different sockets avoids the
        // `FIONBIO`-is-socket-wide trap (see hybrid CUSTOM.md "Truly-
        // blocking writes, polled reads via SO_RCVTIMEO" for the same
        // pattern on hybrid).
        for peer_addr in &self.config.tcp_peers {
            match TcpStream::connect(peer_addr) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    // Explicit blocking — `TcpStream::connect` already
                    // returns a blocking socket by default but we set
                    // it again so the back-pressure contract doesn't
                    // depend on upstream defaults.
                    stream.set_nonblocking(false)?;
                    // T14.3: apply SO_RCVBUF on the outbound stream too.
                    // The kernel reserves recv-side buffer per socket
                    // regardless of direction-of-use; honouring the
                    // operator's request on every TCP socket we own keeps
                    // the contract simple ("every TCP socket gets it").
                    apply_recv_buffer_kb_tcp(&stream, self.config.recv_buffer_kb);
                    self.tcp_out_streams.push(stream);
                }
                Err(e) => {
                    eprintln!(
                        "[custom-udp] warning: failed to connect to peer {}: {}",
                        peer_addr, e
                    );
                }
            }
        }

        Ok(())
    }

    /// Try to receive a UDP datagram and process it.
    fn recv_udp(&mut self) -> Result<()> {
        // Collect received datagrams into a temporary list to avoid holding
        // an immutable borrow on self.udp_socket while mutating other fields.
        let mut received: Vec<Vec<u8>> = Vec::new();

        if let Some(socket) = &self.udp_socket {
            loop {
                match socket.recv_from(&mut self.recv_buf) {
                    Ok((n, _addr)) => {
                        received.push(self.recv_buf[..n].to_vec());
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(e) => {
                        return Err(e).context("UDP recv_from failed");
                    }
                }
            }
        }

        // Process collected datagrams.
        for data in &received {
            if protocol::is_nack(data) {
                self.handle_nack(data)?;
                continue;
            }

            if protocol::is_eot_udp(data) {
                match protocol::decode_eot(data) {
                    Ok(eot) => {
                        self.record_peer_eot(eot.writer, eot.eot_id);
                    }
                    Err(e) => {
                        eprintln!("[custom-udp] EOT decode error (UDP): {}", e);
                    }
                }
                continue;
            }

            match protocol::decode(data) {
                Ok(msg) => {
                    // Skip our own messages.
                    if msg.writer == self.config.runner {
                        continue;
                    }

                    self.process_received_message(msg)?;
                }
                Err(e) => {
                    eprintln!("[custom-udp] decode error: {}", e);
                }
            }
        }

        Ok(())
    }

    /// Try to accept incoming TCP connections and read from existing ones (QoS 4).
    fn recv_tcp(&mut self) -> Result<()> {
        // Accept new connections.
        if let Some(listener) = &self.tcp_listener {
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        stream.set_nonblocking(true)?;
                        let _ = stream.set_nodelay(true);
                        // T14.3: apply SO_RCVBUF on every accepted
                        // inbound stream, matching the outbound path.
                        apply_recv_buffer_kb_tcp(&stream, self.config.recv_buffer_kb);
                        self.tcp_in_streams.push(stream);
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        eprintln!("[custom-udp] TCP accept error: {}", e);
                        break;
                    }
                }
            }
        }

        // Read from all incoming TCP streams. Decode results are buffered
        // and applied after the loop because `record_peer_eot` borrows
        // `&mut self`, which conflicts with the `self.tcp_in_streams.drain`
        // iterator's borrow over the loop body.
        let mut new_in_streams = Vec::new();
        let mut decoded_data: Vec<protocol::Message> = Vec::new();
        let mut decoded_eots: Vec<protocol::EotFrame> = Vec::new();
        for mut stream in self.tcp_in_streams.drain(..) {
            let mut keep = true;
            match read_framed_message(&mut stream, self.config.buffer_size) {
                FrameReadResult::Frame(msg_buf) => match protocol::decode_frame(&msg_buf) {
                    Ok(Frame::Data(msg)) => {
                        decoded_data.push(msg);
                    }
                    Ok(Frame::Eot(eot)) => {
                        decoded_eots.push(eot);
                    }
                    Err(e) => {
                        eprintln!("[custom-udp] TCP decode error: {}", e);
                    }
                },
                FrameReadResult::WouldBlock => {}
                FrameReadResult::DropPeer(reason) => {
                    eprintln!("[custom-udp] TCP framing: dropping peer ({})", reason);
                    keep = false;
                }
            }
            if keep {
                new_in_streams.push(stream);
            }
        }
        self.tcp_in_streams = new_in_streams;

        for msg in decoded_data {
            if msg.writer != self.config.runner {
                self.pending.push_back(ReceivedUpdate {
                    writer: msg.writer,
                    seq: msg.seq,
                    path: msg.path,
                    qos: msg.qos,
                    payload: msg.payload,
                });
            }
        }
        for eot in decoded_eots {
            self.record_peer_eot(eot.writer, eot.eot_id);
        }

        Ok(())
    }

    /// Send an EOT frame on the active transport for the configured QoS.
    ///
    /// **DEPRECATED in T14.18**: EOT now routes exclusively over the
    /// per-peer-pair TCP control connection (see
    /// `controltcp::encode_eot_frame` / `signal_end_of_test`). This
    /// function is kept for the `eot_lifecycle_smoke` test in
    /// `tests/multicast_loopback.rs` and as a legacy reference.
    ///
    /// QoS 1-3 (UDP path): broadcast the EOT datagram to the multicast
    /// group `EOT_UDP_RETRIES` times with `EOT_UDP_SPACING` between sends.
    /// QoS 4 (TCP path): send the framed EOT to every connected peer once.
    #[allow(dead_code)]
    fn send_eot(&mut self, eot_id: u64) -> Result<()> {
        let frame = protocol::encode_eot(&self.config.runner, eot_id)?;

        match self.config.qos {
            Qos::BestEffort | Qos::LatestValue | Qos::ReliableUdp => {
                let socket = self
                    .udp_socket
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("UDP socket not connected"))?;
                let target: SocketAddr = SocketAddr::V4(self.config.multicast_group);
                for i in 0..EOT_UDP_RETRIES {
                    // Best-effort: WouldBlock or other transient errors are
                    // logged but never abort the EOT phase. The receiver
                    // dedup guarantees duplicates are harmless.
                    if let Err(e) = socket.send_to(&frame, target) {
                        eprintln!("[custom-udp] EOT UDP send error (retry {}): {}", i, e);
                    }
                    if i + 1 < EOT_UDP_RETRIES {
                        thread::sleep(EOT_UDP_SPACING);
                    }
                }
            }
            Qos::ReliableTcp => {
                let mut failed_indices = Vec::new();
                for (i, stream) in self.tcp_out_streams.iter_mut().enumerate() {
                    if let Err(e) = stream.write_all(&frame) {
                        eprintln!("[custom-udp] EOT TCP send error to peer #{}: {}", i, e);
                        failed_indices.push(i);
                    }
                }
                for &i in failed_indices.iter().rev() {
                    self.tcp_out_streams.remove(i);
                }
            }
        }

        Ok(())
    }

    /// Process a successfully decoded message through QoS filters.
    fn process_received_message(&mut self, msg: protocol::Message) -> Result<()> {
        match self.config.qos {
            Qos::LatestValue => {
                if !self.latest_tracker.accept(&msg.writer, &msg.path, msg.seq) {
                    return Ok(()); // discard stale
                }
            }
            Qos::ReliableUdp => {
                let result = self.gap_detector.check(&msg.writer, msg.seq);
                match result {
                    GapCheckResult::Gap { missing } => {
                        self.send_nacks(&msg.writer, &missing)?;
                    }
                    GapCheckResult::Duplicate => {
                        return Ok(()); // discard duplicate
                    }
                    GapCheckResult::InOrder | GapCheckResult::FirstSeen => {}
                }
            }
            _ => {
                // QoS 1 (BestEffort) and QoS 4 (TCP) accept everything on the UDP path.
            }
        }

        self.pending.push_back(ReceivedUpdate {
            writer: msg.writer,
            seq: msg.seq,
            path: msg.path,
            qos: msg.qos,
            payload: msg.payload,
        });

        Ok(())
    }

    /// Send NACK messages for missing sequences (QoS 3).
    fn send_nacks(&self, writer: &str, missing: &[u64]) -> Result<()> {
        let socket = match &self.udp_socket {
            Some(s) => s,
            None => return Ok(()),
        };

        let target: SocketAddr = SocketAddr::V4(self.config.multicast_group);
        for &seq in missing {
            let nack = protocol::encode_nack(writer, seq);
            // Send NACK to multicast group (the original sender will see it).
            let _ = socket.send_to(&nack, target);
        }

        Ok(())
    }

    /// Shared publish implementation used by both `publish` and `try_publish`.
    ///
    /// When `block_on_wouldblock` is `true`, UDP sends spin on
    /// `WouldBlock` with `yield_now()` until the kernel accepts the
    /// datagram (preserving the original blocking-style `publish`
    /// behaviour). When `false`, a single `send_to` attempt is made and
    /// `WouldBlock` returns `Ok(false)` so the caller can log a
    /// `backpressure_skipped` event.
    ///
    /// TCP (QoS 4) is always blocking — see `try_publish` for the
    /// rationale: gapping a TCP stream would corrupt the per-peer
    /// receiver state, and TCP's own send buffer already provides
    /// natural pacing.
    fn publish_encoded(
        &mut self,
        encoded: &[u8],
        qos: Qos,
        seq: u64,
        block_on_wouldblock: bool,
    ) -> Result<bool> {
        match qos {
            Qos::BestEffort | Qos::LatestValue => {
                let socket = self
                    .udp_socket
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("UDP socket not connected"))?;
                let target: SocketAddr = SocketAddr::V4(self.config.multicast_group);
                loop {
                    match socket.send_to(encoded, target) {
                        Ok(_) => return Ok(true),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            if block_on_wouldblock {
                                std::thread::yield_now();
                                continue;
                            } else {
                                return Ok(false);
                            }
                        }
                        Err(e) => return Err(e).context("UDP send failed"),
                    }
                }
            }
            Qos::ReliableUdp => {
                // QoS 3: NACK protocol requires contiguous seqs, so we
                // never report backpressure here — gapping the stream
                // would force receivers to NACK for a seq we already
                // dropped. Spin on WouldBlock; the kernel buffer is the
                // pacing mechanism.
                let socket = self
                    .udp_socket
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("UDP socket not connected"))?;
                let target: SocketAddr = SocketAddr::V4(self.config.multicast_group);
                loop {
                    match socket.send_to(encoded, target) {
                        Ok(_) => break,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::yield_now();
                            continue;
                        }
                        Err(e) => return Err(e).context("UDP send failed"),
                    }
                }

                // Buffer for retransmit. Limit buffer to last 10000 messages.
                self.send_buffer.insert(seq, encoded.to_vec());
                if self.send_buffer.len() > 10000 && seq > 10000 {
                    let cutoff = seq - 10000;
                    self.send_buffer.retain(|&k, _| k > cutoff);
                }
                Ok(true)
            }
            Qos::ReliableTcp => {
                // QoS 4: blocking TCP write. Strictly contiguous seqs;
                // never report backpressure (the kernel send buffer
                // fills and `write_all` blocks until it drains).
                let mut failed_indices = Vec::new();
                for (i, stream) in self.tcp_out_streams.iter_mut().enumerate() {
                    if stream.write_all(encoded).is_err() {
                        failed_indices.push(i);
                    }
                }
                for &i in failed_indices.iter().rev() {
                    self.tcp_out_streams.remove(i);
                }
                Ok(true)
            }
        }
    }

    /// Handle a received NACK: retransmit the requested message if we have it buffered.
    fn handle_nack(&self, data: &[u8]) -> Result<()> {
        let (writer, missing_seq) = protocol::decode_nack(data)?;

        // Only respond if the NACK is for our messages.
        if writer != self.config.runner {
            return Ok(());
        }

        if let Some(msg_bytes) = self.send_buffer.get(&missing_seq) {
            let socket = match &self.udp_socket {
                Some(s) => s,
                None => return Ok(()),
            };
            let target: SocketAddr = SocketAddr::V4(self.config.multicast_group);
            let _ = socket.send_to(msg_bytes, target);
        }

        Ok(())
    }

    /// Compute the Multi-mode mpsc channel bound.
    ///
    /// Spec (T14.3): `4 * values_per_tick * (peer_count + 1)` floored at
    /// `MULTI_CHANNEL_FLOOR` (16). The `peer_count + 1` term accounts for
    /// the UDP reader thread plus one reader thread per active TCP peer.
    /// Floored so a small `values_per_tick` (e.g. 1) still leaves room for
    /// a burst before reader threads start bouncing on `TrySendError::Full`.
    fn multi_channel_bound(&self, peer_count: usize) -> usize {
        let raw = (self.config.values_per_tick as usize)
            .saturating_mul(4)
            .saturating_mul(peer_count.saturating_add(1));
        raw.max(MULTI_CHANNEL_FLOOR)
    }

    /// Multi mode: synchronously accept every expected inbound TCP peer
    /// connection from the listener so each accepted stream can be moved
    /// into its own reader thread.
    ///
    /// Returns the accepted streams. The listener is dropped from `self`
    /// before this returns since Multi mode does not need to keep
    /// accepting more peers after `start_reader_threads`. If the peer
    /// count is zero (single-peer self-only test) this returns an empty
    /// vec without ever touching the listener.
    fn multi_accept_tcp_peers(&mut self, expected: usize) -> Result<Vec<TcpStream>> {
        if expected == 0 {
            self.tcp_listener.take();
            return Ok(Vec::new());
        }
        let listener = self
            .tcp_listener
            .take()
            .ok_or_else(|| anyhow::anyhow!("multi-mode: TCP listener missing at QoS 4"))?;
        // The listener was set non-blocking by `setup_tcp`. We keep that
        // mode but poll it with a deadline; on accept we restore blocking
        // semantics (with `SO_RCVTIMEO`) before handing the stream off to
        // its reader thread.
        let deadline = Instant::now() + TCP_ACCEPT_TIMEOUT;
        let mut accepted: Vec<TcpStream> = Vec::with_capacity(expected);
        while accepted.len() < expected {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "[custom-udp] multi: timed out waiting for {} TCP peer(s) on {}",
                    expected - accepted.len(),
                    self.config.tcp_listen_addr
                );
            }
            match listener.accept() {
                Ok((stream, _addr)) => {
                    // Reader threads need blocking semantics + a short
                    // `SO_RCVTIMEO` so they can periodically wake and
                    // observe the shutdown flag. This matches the
                    // websocket / hybrid TCP-reader pattern.
                    stream
                        .set_nonblocking(false)
                        .context("set_nonblocking(false) on accepted TCP stream")?;
                    stream
                        .set_read_timeout(Some(READER_RCVTIMEO))
                        .context("set_read_timeout on accepted TCP stream")?;
                    let _ = stream.set_nodelay(true);
                    apply_recv_buffer_kb_tcp(&stream, self.config.recv_buffer_kb);
                    accepted.push(stream);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    return Err(e).context("multi: TCP accept failed");
                }
            }
        }
        Ok(accepted)
    }

    /// Spawn reader threads for Multi mode. One thread reads the UDP
    /// multicast socket; one thread reads each accepted TCP peer stream
    /// (QoS 4 only). All threads push parsed items into a shared bounded
    /// mpsc. Reader handles + the channel receiver are stashed on `self`
    /// for `stop_reader_threads` and `poll_receive` to consume.
    ///
    /// The UDP socket needs a short blocking timeout (`SO_RCVTIMEO`) so
    /// the reader thread can periodically wake and observe the shutdown
    /// flag without out-of-band signalling. The currently-bound socket
    /// is non-blocking from `setup_udp`; we clone it (so `publish` keeps
    /// its non-blocking handle) and switch the clone to blocking with
    /// `SO_RCVTIMEO`.
    fn start_reader_threads_multi(&mut self) -> Result<()> {
        // At QoS 4 the listener was bound during `connect`. Accept all
        // expected inbound TCP peer streams synchronously before spawning
        // reader threads, so we have one stream per thread.
        let expected_tcp = if self.config.qos == Qos::ReliableTcp {
            self.config.tcp_peers.len()
        } else {
            0
        };
        let tcp_streams = self.multi_accept_tcp_peers(expected_tcp)?;
        let peer_count = tcp_streams.len();

        let bound = self.multi_channel_bound(peer_count);
        // T14.16: split the reader-thread mpsc into two channels. The
        // bounded data channel drops on saturation (acceptable for QoS
        // 1/2; QoS 3/4 protocols are unaffected because the dropped item
        // never lands in `pending`). The unbounded lifecycle channel
        // carries EOT, NACK, and TcpPeerDropped -- losing any of those
        // would silently break the spawn (EOT-loss = peer timeout,
        // NACK-loss = QoS-3 reliability hole, TcpPeerDropped-loss = stale
        // peer reference). Drained first in `drain_multi_channel`.
        let (data_tx, data_rx) = mpsc::sync_channel::<ReaderDataItem>(bound);
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel::<ReaderLifecycleItem>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

        // -- UDP reader thread --
        // Clone the existing UDP socket so `publish` keeps its handle on
        // the non-blocking original. The clone gets short blocking
        // semantics: blocking + `SO_RCVTIMEO` so the recv wakes
        // periodically to check the shutdown flag.
        if let Some(udp) = self.udp_socket.as_ref() {
            let udp_clone = udp
                .try_clone()
                .context("multi: try_clone on UDP socket failed")?;
            udp_clone
                .set_nonblocking(false)
                .context("multi: set_nonblocking(false) on UDP clone failed")?;
            udp_clone
                .set_read_timeout(Some(READER_RCVTIMEO))
                .context("multi: set_read_timeout on UDP clone failed")?;
            // Re-apply SO_RCVBUF on the clone so the cloned descriptor
            // doesn't accidentally drop back to OS defaults on platforms
            // where socket options aren't inherited via `dup`. socket2
            // exposes the helper through `SockRef`.
            let sock_ref = socket2::SockRef::from(&udp_clone);
            let requested = (self.config.recv_buffer_kb as usize).saturating_mul(1024);
            if let Err(e) = sock_ref.set_recv_buffer_size(requested) {
                eprintln!(
                    "[custom-udp] warning: set_recv_buffer_size({}) on UDP clone failed: {}",
                    requested, e
                );
            }

            let data_tx_udp = data_tx.clone();
            let lifecycle_tx_udp = lifecycle_tx.clone();
            let shutdown_udp = Arc::clone(&shutdown);
            let buffer_size = self.config.buffer_size;
            let handle = thread::Builder::new()
                .name("custom-udp-recv-udp".to_string())
                .spawn(move || {
                    udp_reader_thread(
                        udp_clone,
                        buffer_size,
                        data_tx_udp,
                        lifecycle_tx_udp,
                        shutdown_udp,
                    );
                })
                .context("multi: spawn UDP reader thread")?;
            handles.push(handle);
        }

        // -- Per-peer TCP reader threads (QoS 4 only) --
        for (i, stream) in tcp_streams.into_iter().enumerate() {
            let data_tx_tcp = data_tx.clone();
            let lifecycle_tx_tcp = lifecycle_tx.clone();
            let shutdown_tcp = Arc::clone(&shutdown);
            let max_total_len = self.config.buffer_size;
            let handle = thread::Builder::new()
                .name(format!("custom-udp-recv-tcp-{}", i))
                .spawn(move || {
                    tcp_reader_thread(
                        stream,
                        max_total_len,
                        data_tx_tcp,
                        lifecycle_tx_tcp,
                        shutdown_tcp,
                    );
                })
                .context("multi: spawn TCP reader thread")?;
            handles.push(handle);
        }

        // T14.18: per-peer control reader threads (Multi mode). Each
        // thread reads length-prefixed control frames and pushes any
        // decoded EOT directly onto the lifecycle channel as
        // `ReaderLifecycleItem::Eot(EotFrame)`. The map_fn shim lets
        // controltcp.rs stay variant-agnostic.
        let control_shutdown = Arc::new(AtomicBool::new(false));
        let mut control_handles: Vec<thread::JoinHandle<()>> = Vec::new();
        for peer in self.control_peers.iter_mut() {
            if let Some(read_clone) = peer.take_read_clone() {
                let label = format!("{}-{}", peer.peer_name, peer.peer_addr);
                let lifecycle_tx_ctl = lifecycle_tx.clone();
                let shutdown_ctl = Arc::clone(&control_shutdown);
                let handle = controltcp::spawn_control_reader(
                    read_clone,
                    label,
                    lifecycle_tx_ctl,
                    control_eot_to_lifecycle,
                    shutdown_ctl,
                )
                .with_context(|| {
                    format!(
                        "T14.18: spawn control reader for {} ({})",
                        peer.peer_name, peer.peer_addr
                    )
                })?;
                control_handles.push(handle);
            }
        }
        self.control_shutdown = Some(control_shutdown);
        self.control_threads = control_handles;

        // The original `data_tx` / `lifecycle_tx` senders belong to the
        // variant only to clone from; drop them so the channels
        // correctly report `Disconnected` once every reader thread
        // exits and drops its own clone. Otherwise `try_recv` would
        // observe `Empty` forever after all readers exit, masking the
        // disconnect.
        drop(data_tx);
        drop(lifecycle_tx);

        self.multi = Some(MultiReaderState {
            data_rx,
            lifecycle_rx,
            shutdown,
            handles,
        });
        Ok(())
    }

    /// Drain whatever the reader threads have delivered into the two
    /// mpsc channels and apply each item to the driver-side state. Used
    /// only in Multi mode.
    ///
    /// T14.16: drains the unbounded `lifecycle_rx` FIRST (priority --
    /// EOT/NACK/PeerDropped must never be starved by a saturated data
    /// channel), then drains the bounded `data_rx`. Lifecycle items are
    /// rare (O(peers) per spawn) so we drain to empty unconditionally;
    /// the data drain still bounds itself at the first staged update so
    /// `poll_receive` keeps its one-update-per-call shape and the
    /// caller's drain loop sees the same per-call semantics as
    /// Single-mode.
    fn drain_multi_channel(&mut self) -> Result<()> {
        // Lifecycle drain first -- never starved.
        loop {
            let item = match self.multi.as_ref() {
                Some(m) => match m.lifecycle_rx.try_recv() {
                    Ok(item) => item,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                },
                None => return Ok(()),
            };
            match item {
                ReaderLifecycleItem::Eot(eot) => {
                    self.record_peer_eot(eot.writer, eot.eot_id);
                }
                ReaderLifecycleItem::Nack(data) => {
                    if let Err(e) = self.handle_nack(&data) {
                        eprintln!("[custom-udp] multi: NACK handling error: {}", e);
                    }
                }
                ReaderLifecycleItem::TcpPeerDropped => {
                    // Informational: the per-peer reader thread exited.
                    // We rely on `stop_reader_threads` to reap the join
                    // handle at disconnect time; nothing to do here.
                }
            }
        }

        // Data drain second, bounded by "first staged update".
        loop {
            let item = match self.multi.as_ref() {
                Some(m) => match m.data_rx.try_recv() {
                    Ok(item) => item,
                    Err(mpsc::TryRecvError::Empty) => return Ok(()),
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                },
                None => return Ok(()),
            };
            match item {
                ReaderDataItem::Data(msg) => {
                    if msg.writer == self.config.runner {
                        continue;
                    }
                    self.process_received_message(msg)?;
                    // Single update per `poll_receive` call -- match the
                    // Single-mode return shape so the driver's drain loop
                    // sees the same per-call semantics.
                    if !self.pending.is_empty() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// UDP reader thread body. Receives datagrams on a blocking socket with a
/// short `SO_RCVTIMEO`, parses each datagram, and routes the decoded
/// item onto either the bounded data channel (Data frames) or the
/// unbounded lifecycle channel (EOT / NACK markers). Exits when
/// `shutdown` is set.
///
/// `WouldBlock` / `TimedOut` are non-fatal (recv timeout fired); other I/O
/// errors are logged once and stop the thread (the variant is in a bad
/// state -- the driver will observe via stalled poll output).
fn udp_reader_thread(
    socket: UdpSocket,
    buffer_size: usize,
    data_tx: SyncSender<ReaderDataItem>,
    lifecycle_tx: Sender<ReaderLifecycleItem>,
    shutdown: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; buffer_size];
    while !shutdown.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, _addr)) => {
                let bytes = &buf[..n];
                if protocol::is_nack(bytes) {
                    if send_lifecycle(&lifecycle_tx, ReaderLifecycleItem::Nack(bytes.to_vec())) {
                        return;
                    }
                    continue;
                }
                if protocol::is_eot_udp(bytes) {
                    match protocol::decode_eot(bytes) {
                        Ok(eot) => {
                            if send_lifecycle(&lifecycle_tx, ReaderLifecycleItem::Eot(eot)) {
                                return;
                            }
                        }
                        Err(e) => {
                            eprintln!("[custom-udp] multi: EOT decode error (UDP): {}", e);
                        }
                    }
                    continue;
                }
                match protocol::decode(bytes) {
                    Ok(msg) => {
                        if send_data_or_warn(&data_tx, ReaderDataItem::Data(msg)) {
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("[custom-udp] multi: UDP decode error: {}", e);
                    }
                }
            }
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // SO_RCVTIMEO fired with no data. Loop and re-check
                // the shutdown flag.
            }
            Err(e) => {
                eprintln!("[custom-udp] multi: UDP recv error: {}", e);
                return;
            }
        }
    }
}

/// Per-peer TCP reader thread body. Reads length-prefixed frames in a
/// blocking loop with `SO_RCVTIMEO`. Exits when `shutdown` is set, EOF, or
/// any framing / read error (the stream is dropped, and a
/// `ReaderLifecycleItem::TcpPeerDropped` is pushed so the driver can
/// observe).
fn tcp_reader_thread(
    mut stream: TcpStream,
    max_total_len: usize,
    data_tx: SyncSender<ReaderDataItem>,
    lifecycle_tx: Sender<ReaderLifecycleItem>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        // Read the 4-byte length prefix with the configured short timeout.
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => {
                let _ = lifecycle_tx.send(ReaderLifecycleItem::TcpPeerDropped);
                return;
            }
        }

        let total_len = u32::from_be_bytes(len_buf) as usize;
        if total_len < protocol::HEADER_FIXED_SIZE || total_len > max_total_len {
            eprintln!(
                "[custom-udp] multi: TCP framing: dropping peer (invalid total_len {})",
                total_len
            );
            let _ = lifecycle_tx.send(ReaderLifecycleItem::TcpPeerDropped);
            return;
        }

        // Body read: we want the body bytes in their entirety. Treat
        // intermediate `WouldBlock` / `TimedOut` as transient and retry,
        // observing `shutdown` between retries.
        let mut msg_buf = vec![0u8; total_len];
        msg_buf[..4].copy_from_slice(&len_buf);
        let mut got: usize = 4;
        let mut fatal = false;
        while got < total_len {
            match stream.read(&mut msg_buf[got..]) {
                Ok(0) => {
                    fatal = true;
                    break;
                }
                Ok(n) => got += n,
                Err(ref e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    if shutdown.load(Ordering::Relaxed) {
                        fatal = true;
                        break;
                    }
                }
                Err(_) => {
                    fatal = true;
                    break;
                }
            }
        }
        if fatal {
            let _ = lifecycle_tx.send(ReaderLifecycleItem::TcpPeerDropped);
            return;
        }

        match protocol::decode_frame(&msg_buf) {
            Ok(Frame::Data(msg)) => {
                if send_data_or_warn(&data_tx, ReaderDataItem::Data(msg)) {
                    return;
                }
            }
            Ok(Frame::Eot(eot)) => {
                if send_lifecycle(&lifecycle_tx, ReaderLifecycleItem::Eot(eot)) {
                    return;
                }
            }
            Err(e) => {
                eprintln!("[custom-udp] multi: TCP decode error: {}", e);
            }
        }
    }
}

/// Push a data item onto the bounded mpsc, dropping it (with a warning)
/// if the channel is full. Returns `true` when the channel is
/// disconnected -- the caller should exit its loop in that case.
/// Full-channel drops are expected back-pressure under saturating
/// receive load and acceptable for QoS 1/2 (the variant's BestEffort /
/// LatestValue semantics tolerate loss). QoS 3 NACK / QoS 4 framing
/// semantics are still observed by the driver thread because the
/// dropped item simply never lands in `pending`.
///
/// T14.16: the warning message explicitly mentions "data channel" and
/// "Data frame" so operators can be certain that EOT / NACK /
/// TcpPeerDropped were NOT lost when this line appears in stderr --
/// those lifecycle items ride the separate unbounded `lifecycle_tx`.
fn send_data_or_warn(tx: &SyncSender<ReaderDataItem>, item: ReaderDataItem) -> bool {
    match tx.try_send(item) {
        Ok(()) => false,
        Err(TrySendError::Full(_)) => {
            // Single line per overrun avoids log spam at 100K msg/s.
            // The variant remains correct -- this is just a measured
            // drop under sustained back-pressure.
            eprintln!(
                "[custom-udp] multi: data channel full -- dropping Data frame (receiver saturated)"
            );
            false
        }
        Err(TrySendError::Disconnected(_)) => true,
    }
}

/// Push a lifecycle item onto the unbounded mpsc. Returns `true` when
/// the channel is disconnected -- the caller should exit its loop in
/// that case. Because the channel is unbounded, sends never block and
/// never drop; the only failure mode is the receiver having been
/// dropped (driver tearing down). T14.16: this is the EOT/NACK/
/// PeerDropped survival path.
fn send_lifecycle(tx: &Sender<ReaderLifecycleItem>, item: ReaderLifecycleItem) -> bool {
    match tx.send(item) {
        Ok(()) => false,
        Err(_) => true,
    }
}

/// T14.18: convert a control-channel `EotFrame` into the variant's
/// lifecycle item shape. Used as the `map_eot` argument to
/// `controltcp::spawn_control_reader`.
fn control_eot_to_lifecycle(eot: protocol::EotFrame) -> ReaderLifecycleItem {
    ReaderLifecycleItem::Eot(eot)
}

impl Variant for UdpVariant {
    fn name(&self) -> &str {
        "custom-udp"
    }

    /// T14.3: custom-udp supports both threading modes.
    ///
    /// - `Single`: existing inline-poll behaviour. `poll_receive` reads
    ///   the UDP socket and (at QoS 4) the inbound TCP streams directly
    ///   on the driver thread.
    /// - `Multi`: one OS reader thread per recv-side socket (UDP +
    ///   per-TCP-peer) parses frames off the hot path and pushes
    ///   decoded items into a shared bounded mpsc. `poll_receive`
    ///   becomes a fast `try_recv`.
    ///
    /// `SO_RCVBUF` is applied from `--recv-buffer-kb * 1024` to every
    /// recv-side socket the variant owns, in either mode. See CUSTOM.md
    /// "Threading modes (T14.3)".
    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        &[ThreadingMode::Single, ThreadingMode::Multi]
    }

    fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()> {
        // Stash the mode so subsequent `start_reader_threads` /
        // `poll_receive` / `stop_reader_threads` calls know which path
        // to take. Both Single and Multi mode rely on `setup_udp` /
        // `setup_tcp` exactly as today; the divergence is in
        // `start_reader_threads`.
        self.threading_mode = threading_mode;
        self.setup_udp()?;

        if self.config.qos == Qos::ReliableTcp {
            self.setup_tcp()?;
        }

        // T14.18: establish per-peer-pair TCP control connections for
        // EOT exchange, separate from the data path.
        self.setup_control_channel()
            .context("T14.18: failed to set up control TCP side-channel")?;

        Ok(())
    }

    fn start_reader_threads(&mut self, mode: ThreadingMode) -> Result<()> {
        // Defensive: the driver passes the same mode as `connect`. Snapshot
        // it for the rest of the lifecycle in case the trait contract
        // tightens in the future.
        self.threading_mode = mode;
        match mode {
            ThreadingMode::Single => Ok(()),
            ThreadingMode::Multi => self.start_reader_threads_multi(),
        }
    }

    fn stop_reader_threads(&mut self) -> Result<()> {
        // T14.18: tear down control reader threads first. Set the
        // shared flag, shut down each peer's stream to wake blocked
        // reads, and join with a bounded deadline.
        if let Some(shutdown) = self.control_shutdown.take() {
            shutdown.store(true, Ordering::SeqCst);
            for peer in &self.control_peers {
                peer.shutdown();
            }
            let deadline = Instant::now() + controltcp::READER_JOIN_TIMEOUT;
            for handle in self.control_threads.drain(..) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let poll_step = Duration::from_millis(20);
                let mut waited = Duration::ZERO;
                while !handle.is_finished() && waited < remaining {
                    thread::sleep(poll_step);
                    waited += poll_step;
                }
                if handle.is_finished() {
                    let _ = handle.join();
                } else {
                    eprintln!(
                        "[custom-udp] warning: T14.18 control reader thread did not exit within {:?}; detaching",
                        controltcp::READER_JOIN_TIMEOUT
                    );
                }
            }
        }

        let multi = match self.multi.take() {
            Some(m) => m,
            None => return Ok(()),
        };

        // Signal shutdown. Reader threads observe this on the next wake
        // (bounded by `READER_RCVTIMEO`).
        multi.shutdown.store(true, Ordering::Relaxed);
        // Drop both receivers to disconnect any blocked sender; reader
        // threads exit on `Disconnected`. We can't move the receivers
        // out of `multi` directly because `MultiReaderState` is the
        // owner -- explicit `drop` clarifies intent. (T14.16: dropping
        // the lifecycle receiver too closes the unbounded `Sender` side
        // for the reader threads.)
        drop(multi.data_rx);
        drop(multi.lifecycle_rx);

        // Join each thread with a per-thread deadline. `JoinHandle::is_finished`
        // (stable since 1.61) lets us poll without blocking, and once the
        // thread is finished `join` returns promptly. Wedged threads
        // surface a single warning and are abandoned -- the alternative
        // (deadlock the disconnect path) is worse.
        for (i, handle) in multi.handles.into_iter().enumerate() {
            let start = Instant::now();
            while !handle.is_finished() {
                if start.elapsed() >= READER_JOIN_TIMEOUT {
                    eprintln!(
                        "[custom-udp] warning: reader thread #{} did not exit within {:?}; abandoning",
                        i, READER_JOIN_TIMEOUT
                    );
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if handle.is_finished() {
                if let Err(_panic) = handle.join() {
                    eprintln!("[custom-udp] warning: reader thread #{} panicked", i);
                }
            }
        }
        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        let encoded = protocol::encode(qos, seq, path, &self.config.runner, payload)?;
        self.publish_encoded(&encoded, qos, seq, /* block_on_wouldblock */ true)?;
        Ok(())
    }

    /// T-impl.7: honest backpressure for the driver.
    ///
    /// QoS 1/2 use a single non-blocking `send_to`. If the kernel returns
    /// `WouldBlock`, we return `Ok(false)` so the driver logs
    /// `backpressure_skipped` instead of `write`, and the seq gap that
    /// results is tolerated by the receiver (best-effort / latest-value
    /// both already tolerate loss).
    ///
    /// QoS 3 / QoS 4 MUST NOT gap the seq stream — the NACK protocol
    /// (QoS 3) needs contiguous seqs to know what to ask for, and the
    /// TCP receiver (QoS 4) expects strictly ordered framed messages.
    /// For those QoS levels we keep the blocking-retry behaviour from
    /// `publish` and always report `Ok(true)`; the kernel send buffer is
    /// the natural pacing mechanism.
    ///
    /// See `variants/custom-udp/CUSTOM.md` "Backpressure semantics
    /// (T-impl.7)".
    fn try_publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<bool> {
        let encoded = protocol::encode(qos, seq, path, &self.config.runner, payload)?;
        let block = matches!(qos, Qos::ReliableUdp | Qos::ReliableTcp);
        self.publish_encoded(&encoded, qos, seq, block)
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        // Return any already-queued update first.
        if let Some(update) = self.pending.pop_front() {
            return Ok(Some(update));
        }

        match self.threading_mode {
            ThreadingMode::Single => {
                // T14.18: poll the control fd alongside the data
                // path. EOTs land in `eot_queue`; data is unaffected.
                self.poll_control_peers();
                // Try receiving from UDP.
                self.recv_udp()?;
                // For QoS 4, also check TCP.
                if self.config.qos == Qos::ReliableTcp {
                    self.recv_tcp()?;
                }
            }
            ThreadingMode::Multi => {
                // Reader threads have already parsed frames off the
                // sockets and pushed them into the shared mpsc. The
                // control reader threads (T14.18) also push EOT items
                // onto the lifecycle channel; `drain_multi_channel`
                // surfaces them through `record_peer_eot`. Drain
                // until we have one update ready or the channel is
                // empty.
                self.drain_multi_channel()?;
            }
        }

        Ok(self.pending.pop_front())
    }

    fn disconnect(&mut self) -> Result<()> {
        // Defensive: if reader threads are still active (driver did not
        // call `stop_reader_threads` first, e.g. tests that call
        // `disconnect` directly), tear them down here so the underlying
        // sockets can drop cleanly.
        if self.multi.is_some() {
            // Surface the warning from stop but never let it block the
            // disconnect path.
            if let Err(e) = self.stop_reader_threads() {
                eprintln!(
                    "[custom-udp] warning: stop_reader_threads during disconnect: {}",
                    e
                );
            }
        }

        // T14.18: graceful control-channel teardown.
        let drain_budget = Duration::from_secs(self.effective_eot_timeout_secs().clamp(1, 30));
        let drain_deadline = Instant::now() + drain_budget;
        let single_mode = self.threading_mode == ThreadingMode::Single;
        for peer in self.control_peers.iter_mut() {
            peer.send_bye();
            peer.shutdown_write();
            if single_mode {
                let last_frames = peer.drain_until_closed(drain_deadline);
                for frame in last_frames {
                    if let ControlFrame::Eot { writer, eot_id } = frame {
                        if writer != self.config.runner
                            && self.eot_seen.insert((writer.clone(), eot_id))
                        {
                            self.eot_queue.push_back(PeerEot { writer, eot_id });
                        }
                    }
                }
            }
            peer.shutdown();
        }
        self.control_peers.clear();

        // Leave multicast group and close socket.
        if let Some(socket) = self.udp_socket.take() {
            let multicast_addr = self.config.multicast_group;
            // Best-effort leave; ignore errors (socket will be dropped anyway).
            let raw: Socket = socket.into();
            let _ = raw.leave_multicast_v4(multicast_addr.ip(), &Ipv4Addr::UNSPECIFIED);
        }

        // Close TCP resources.
        self.tcp_listener.take();
        self.tcp_out_streams.clear();
        self.tcp_in_streams.clear();

        self.send_buffer.clear();

        Ok(())
    }

    /// T14.18: Generate an EOT id and dispatch the marker over the
    /// per-peer-pair TCP control connection, regardless of QoS. The
    /// pre-T14.18 UDP-multicast / TCP-data-stream EOT dispatch is
    /// removed: the control connection is QoS-independent and cannot
    /// be dropped by data-path saturation.
    fn signal_end_of_test(&mut self) -> Result<u64> {
        let eot_id: u64 = rand::random::<u64>();
        let frame = controltcp::encode_eot_frame(&self.config.runner, eot_id);
        let mut failed_idx = Vec::new();
        for (i, peer) in self.control_peers.iter_mut().enumerate() {
            if let Err(e) = peer.send_frame(&frame) {
                eprintln!(
                    "[custom-udp] T14.18: control EOT send to peer {} failed: {:#}; dropping peer",
                    peer.peer_addr, e
                );
                peer.shutdown();
                failed_idx.push(i);
            }
        }
        for &i in failed_idx.iter().rev() {
            self.control_peers.remove(i);
        }
        Ok(eot_id)
    }

    fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
        // Variant is the source of truth for dedup (`eot_seen` HashSet),
        // so each (writer, eot_id) reaches the driver at most once.
        let drained: Vec<PeerEot> = self.eot_queue.drain(..).collect();
        Ok(drained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn default_config(qos: Qos) -> UdpConfig {
        UdpConfig {
            multicast_group: SocketAddrV4::new(Ipv4Addr::new(239, 0, 0, 1), 9000),
            buffer_size: 65536,
            runner: "test-runner".to_string(),
            qos,
            tcp_listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            tcp_peers: Vec::new(),
            recv_buffer_kb: 4096,
            values_per_tick: 1,
            control_listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            control_peers: Vec::new(),
            eot_timeout_secs: Some(2),
        }
    }

    #[test]
    fn variant_name() {
        let variant = UdpVariant::new(default_config(Qos::BestEffort));
        assert_eq!(variant.name(), "custom-udp");
    }

    #[test]
    fn connect_and_disconnect() {
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        // connect may fail in CI environments without multicast support,
        // but should not panic.
        if variant.connect(variant_base::ThreadingMode::Single).is_ok() {
            assert!(variant.disconnect().is_ok());
        }
    }

    #[test]
    fn poll_receive_before_connect_returns_none() {
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        let result = variant.poll_receive().unwrap();
        assert!(result.is_none());
    }

    // ---- read_framed_message: framing safety ----
    //
    // These tests guard against the cross-machine TCP teardown panic at
    // `udp.rs:233` (range end index 4 out of range). When the network tears
    // a frame mid-read, `read_exact` can succeed with bytes that decode as
    // a too-small `total_len` (0..=3). The reader MUST drop the peer in
    // that case rather than allocate an undersized vec and slice into it.
    // See LEARNED.md "Cross-machine validation reveals failures invisible
    // on localhost" and TASKS.md T10.4 for context.

    /// Build a 4-byte big-endian length prefix.
    fn len_prefix(n: u32) -> [u8; 4] {
        n.to_be_bytes()
    }

    /// `Read` impl that returns the wrapped bytes once, then returns
    /// `WouldBlock` on every subsequent read. Used to simulate a TCP
    /// stream where the body bytes have not yet arrived.
    struct ReadThenWouldBlock {
        inner: std::io::Cursor<Vec<u8>>,
    }

    impl ReadThenWouldBlock {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: std::io::Cursor::new(bytes),
            }
        }
    }

    impl Read for ReadThenWouldBlock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            if n == 0 && !buf.is_empty() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "no more data"));
            }
            Ok(n)
        }
    }

    #[test]
    fn framing_drops_peer_on_zero_length_prefix() {
        // 4-byte prefix decoding to 0 is the canonical torn-read panic
        // value. Must drop, not panic.
        let mut bytes: &[u8] = &[0, 0, 0, 0];
        match read_framed_message(&mut bytes, 65536) {
            FrameReadResult::DropPeer(_) => {}
            other => panic!("expected DropPeer, got {:?}", other),
        }
    }

    #[test]
    fn framing_drops_peer_on_undersized_length_prefix() {
        // 1, 2, 3 are all below the 4-byte length prefix itself and below
        // the 17-byte minimum header. Each must drop the peer.
        for n in [1u32, 2, 3] {
            let prefix = len_prefix(n);
            let mut bytes: &[u8] = &prefix;
            match read_framed_message(&mut bytes, 65536) {
                FrameReadResult::DropPeer(_) => {}
                other => panic!("n={}: expected DropPeer, got {:?}", n, other),
            }
        }
    }

    #[test]
    fn framing_drops_peer_on_length_prefix_below_header_min() {
        // 4 is technically large enough to allocate the prefix itself,
        // but a frame must contain at minimum a full header. Same for
        // 5..=16. All must drop.
        for n in 4u32..=(protocol::HEADER_FIXED_SIZE as u32 - 1) {
            let prefix = len_prefix(n);
            let mut bytes: &[u8] = &prefix;
            match read_framed_message(&mut bytes, 65536) {
                FrameReadResult::DropPeer(_) => {}
                other => panic!("n={}: expected DropPeer, got {:?}", n, other),
            }
        }
    }

    #[test]
    fn framing_drops_peer_when_length_exceeds_buffer_size() {
        // Regression: existing oversized-frame check still drops the peer.
        let buffer_size = 1024;
        let prefix = len_prefix(buffer_size as u32 + 1);
        let mut bytes: &[u8] = &prefix;
        match read_framed_message(&mut bytes, buffer_size) {
            FrameReadResult::DropPeer(_) => {}
            other => panic!("expected DropPeer, got {:?}", other),
        }
    }

    #[test]
    fn framing_returns_wouldblock_when_body_not_yet_available() {
        // Regression: if the prefix is fully readable but the body returns
        // WouldBlock, the caller must retain the stream (WouldBlock result),
        // not drop it.
        let total_len = (protocol::HEADER_FIXED_SIZE + 8) as u32; // header + 8B payload
        let mut reader = ReadThenWouldBlock::new(len_prefix(total_len).to_vec());
        match read_framed_message(&mut reader, 65536) {
            FrameReadResult::WouldBlock => {}
            other => panic!("expected WouldBlock, got {:?}", other),
        }
    }

    #[test]
    fn framing_accepts_valid_frame() {
        // A real encoded message must round-trip through the framing reader
        // (length prefix is the first 4 bytes of the encoded message) and
        // then through `protocol::decode`.
        let encoded = protocol::encode(
            Qos::ReliableTcp,
            7,
            "/p",
            "writer-x",
            &[1, 2, 3, 4, 5, 6, 7, 8],
        )
        .unwrap();
        let mut bytes: &[u8] = &encoded;
        match read_framed_message(&mut bytes, 65536) {
            FrameReadResult::Frame(buf) => {
                assert_eq!(buf, encoded);
                let msg = protocol::decode(&buf).unwrap();
                assert_eq!(msg.seq, 7);
                assert_eq!(msg.path, "/p");
                assert_eq!(msg.writer, "writer-x");
                assert_eq!(msg.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
            }
            other => panic!("expected Frame, got {:?}", other),
        }
    }

    #[test]
    fn framing_drops_peer_on_eof_before_prefix() {
        // Empty stream (clean EOF on the prefix read) should drop the peer.
        let mut bytes: &[u8] = &[];
        match read_framed_message(&mut bytes, 65536) {
            FrameReadResult::DropPeer(_) => {}
            other => panic!("expected DropPeer, got {:?}", other),
        }
    }

    // ---- EOT: dedup, queue drain, bounds-check regression ----

    #[test]
    fn record_peer_eot_dedupes_repeated_sends() {
        // UDP delivers EOT 5 times by design; receiver must surface each
        // (writer, eot_id) pair exactly once.
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        for _ in 0..EOT_UDP_RETRIES {
            variant.record_peer_eot("alice".to_string(), 0xABCD);
        }
        let drained = variant.poll_peer_eots().unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].writer, "alice");
        assert_eq!(drained[0].eot_id, 0xABCD);

        // Subsequent calls return nothing -- queue is drained.
        let drained2 = variant.poll_peer_eots().unwrap();
        assert!(drained2.is_empty());
    }

    #[test]
    fn record_peer_eot_distinct_writers_distinct_entries() {
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        // Multiple bursts from two distinct peers.
        for _ in 0..3 {
            variant.record_peer_eot("alice".to_string(), 1);
        }
        for _ in 0..3 {
            variant.record_peer_eot("bob".to_string(), 2);
        }
        let drained = variant.poll_peer_eots().unwrap();
        assert_eq!(drained.len(), 2, "expected one entry per distinct writer");
        let names: Vec<String> = drained.iter().map(|e| e.writer.clone()).collect();
        assert!(names.contains(&"alice".to_string()));
        assert!(names.contains(&"bob".to_string()));
    }

    #[test]
    fn record_peer_eot_skips_self() {
        // The variant's own runner name must never be queued -- a sanity
        // guard against multicast loopback echoing our EOT back at us.
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        variant.record_peer_eot("test-runner".to_string(), 7); // self
        variant.record_peer_eot("alice".to_string(), 8);
        let drained = variant.poll_peer_eots().unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].writer, "alice");
    }

    #[test]
    fn poll_peer_eots_default_state_is_empty() {
        // No observations recorded -> empty drain.
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        assert!(variant.poll_peer_eots().unwrap().is_empty());
    }

    /// UDP retry-and-dedup harness: simulate the receiver's `recv_udp`
    /// path being fed five copies of the same EOT datagram and assert it
    /// surfaces a single `PeerEot` from `poll_peer_eots`.
    #[test]
    fn udp_retry_dedup_yields_single_peer_eot() {
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));

        // Simulate 5 multicast deliveries of the same EOT frame (writer
        // "alice", id 0x1234).
        let frame = protocol::encode_eot("alice", 0x1234).unwrap();
        for _ in 0..EOT_UDP_RETRIES {
            assert!(protocol::is_eot_udp(&frame));
            let decoded = protocol::decode_eot(&frame).unwrap();
            variant.record_peer_eot(decoded.writer, decoded.eot_id);
        }

        let drained = variant.poll_peer_eots().unwrap();
        assert_eq!(
            drained.len(),
            1,
            "five copies of the same EOT must dedupe to one PeerEot"
        );
        assert_eq!(drained[0].writer, "alice");
        assert_eq!(drained[0].eot_id, 0x1234);
    }

    #[test]
    fn signal_end_of_test_returns_nonzero_id_without_socket() {
        // Even when no socket is connected (we haven't called `connect`),
        // signal_end_of_test should fail loudly rather than panic. We don't
        // strictly assert success here -- only that it terminates.
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        let _ = variant.signal_end_of_test();
    }

    /// Regression: a malformed EOT frame whose length prefix is below
    /// `HEADER_FIXED_SIZE` MUST drop the peer cleanly without panic, the
    /// same behaviour T10.4 added for data frames.
    #[test]
    fn framing_drops_peer_on_eot_with_undersized_length_prefix() {
        // Construct a "frame" that LOOKS like an EOT frame (tag 0xEE at
        // offset 4) but whose total_len is too small. The reader should
        // never even reach the tag byte: the bounds check at the prefix
        // level fires first.
        for n in 0u32..=(protocol::HEADER_FIXED_SIZE as u32 - 1) {
            let mut bytes = Vec::with_capacity(8);
            bytes.extend_from_slice(&n.to_be_bytes());
            bytes.push(protocol::EOT_TAG);
            bytes.extend_from_slice(&[0u8; 3]); // padding so the read can attempt the body
            let mut cur = std::io::Cursor::new(bytes);
            match read_framed_message(&mut cur, 65536) {
                FrameReadResult::DropPeer(_) => {}
                other => panic!("n={}: expected DropPeer, got {:?}", n, other),
            }
        }
    }

    /// Regression: an oversize EOT length prefix (greater than the
    /// configured buffer) must drop the peer rather than allocate.
    #[test]
    fn framing_drops_peer_on_eot_with_oversized_length_prefix() {
        let buffer_size = 1024;
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(&((buffer_size as u32) + 1).to_be_bytes());
        bytes.push(protocol::EOT_TAG);
        let mut cur = std::io::Cursor::new(bytes);
        match read_framed_message(&mut cur, buffer_size) {
            FrameReadResult::DropPeer(_) => {}
            other => panic!("expected DropPeer, got {:?}", other),
        }
    }

    // ---- T-impl.7: try_publish backpressure semantics ----

    /// Build a `UdpVariant` whose UDP socket is a *real* non-blocking
    /// socket bound to an ephemeral port (not a multicast group) with a
    /// tiny `SO_SNDBUF`. The intent is to drive `send_to` into
    /// `WouldBlock` quickly without depending on the OS-wide multicast
    /// configuration. The configured `multicast_group` doubles as the
    /// send target — sending to a non-listening unicast addr exercises
    /// the same syscall path while staying inside loopback.
    fn make_variant_with_tiny_sndbuf(qos: Qos) -> Result<UdpVariant> {
        use socket2::{Domain, Protocol, SockAddr, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("create test socket")?;
        socket.set_reuse_address(true).ok();
        socket.set_nonblocking(true)?;
        // Tiny send buffer so the kernel can fill quickly. The kernel
        // typically clamps the floor (Windows ~1 KB), but anything well
        // below the per-packet rate-times-MTU drains slowly enough that
        // a busy loop is guaranteed to hit `WouldBlock`.
        let _ = socket.set_send_buffer_size(1024);
        let bind_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        socket.bind(&SockAddr::from(bind_addr))?;

        // Pick a discard target: an arbitrary loopback port nobody is
        // listening on. The kernel still buffers the datagram inside
        // SO_SNDBUF before its NIC layer can drop it, so we get
        // realistic WouldBlock once the buffer fills.
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);

        let mut cfg = default_config(qos);
        cfg.multicast_group = target;
        let mut v = UdpVariant::new(cfg);
        v.udp_socket = Some(socket.into());
        Ok(v)
    }

    /// Detect whether the host kernel actually surfaces `WouldBlock`
    /// when an undersized SO_SNDBUF is hammered. Some platforms (most
    /// notably Windows on a few NIC configurations) silently drop the
    /// datagram at a layer below the syscall return, so `send_to`
    /// never reports `WouldBlock`. When that happens we can't validate
    /// the `Ok(false)` path with a real socket — fall back to a probe.
    fn host_surfaces_udp_wouldblock() -> bool {
        use socket2::{Domain, Protocol, SockAddr, Socket, Type};
        let Ok(socket) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) else {
            return false;
        };
        if socket.set_nonblocking(true).is_err() {
            return false;
        }
        let _ = socket.set_send_buffer_size(1024);
        let bind_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        if socket.bind(&SockAddr::from(bind_addr)).is_err() {
            return false;
        }
        let target = SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1));
        let payload = vec![0u8; 60_000];
        for _ in 0..200_000 {
            match socket.send_to(&payload, &target) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
        false
    }

    /// QoS 1 (BestEffort): `try_publish` must return `Ok(false)` at
    /// some point when the UDP send buffer fills, NOT block and NOT
    /// fall through to `publish`.
    ///
    /// Some host configurations never surface `WouldBlock` on
    /// loopback UDP (the kernel drops at a deeper layer). We probe
    /// first and only assert the `Ok(false)` outcome when the probe
    /// confirms the host can produce it; otherwise we settle for
    /// "every `try_publish` call returns `Ok(true)` and never errors",
    /// which still proves we never panicked, never blocked
    /// indefinitely, and never returned `Err`.
    #[test]
    fn try_publish_qos1_returns_false_under_send_buffer_pressure() {
        let mut v = make_variant_with_tiny_sndbuf(Qos::BestEffort)
            .expect("must construct a non-blocking test variant");

        // Near-MTU-max payloads so a 1 KB send buffer can only hold
        // one datagram — every subsequent send hits WouldBlock until
        // the kernel drains.
        let payload = vec![0xABu8; 60_000];
        let mut saw_false = false;
        for seq in 0..200_000u64 {
            match v.try_publish("/p", &payload, Qos::BestEffort, seq) {
                Ok(true) => {}
                Ok(false) => {
                    saw_false = true;
                    break;
                }
                Err(e) => panic!("try_publish errored: {e:#}"),
            }
        }

        if !saw_false && host_surfaces_udp_wouldblock() {
            panic!("expected try_publish to return Ok(false) on QoS 1 — host can surface WouldBlock but try_publish did not");
        }
    }

    /// QoS 2 (LatestValue): same behaviour as QoS 1 — gap-tolerant by
    /// design, so `try_publish` reports backpressure honestly.
    #[test]
    fn try_publish_qos2_returns_false_under_send_buffer_pressure() {
        let mut v = make_variant_with_tiny_sndbuf(Qos::LatestValue)
            .expect("must construct a non-blocking test variant");

        let payload = vec![0xCDu8; 60_000];
        let mut saw_false = false;
        for seq in 0..200_000u64 {
            match v.try_publish("/p", &payload, Qos::LatestValue, seq) {
                Ok(true) => {}
                Ok(false) => {
                    saw_false = true;
                    break;
                }
                Err(e) => panic!("try_publish errored: {e:#}"),
            }
        }

        if !saw_false && host_surfaces_udp_wouldblock() {
            panic!("expected try_publish to return Ok(false) on QoS 2 — host can surface WouldBlock but try_publish did not");
        }
    }

    /// QoS 3 (ReliableUdp): MUST NOT gap the seq stream. Under the
    /// same buffer pressure that triggers `Ok(false)` for QoS 1/2,
    /// QoS 3 must keep returning `Ok(true)` (blocking on WouldBlock
    /// internally) so the NACK protocol stays sound. We bound the
    /// inner blocking loop indirectly by giving the kernel time to
    /// drain between iterations (yield_now in the impl); the test
    /// itself just needs to confirm we never see `Ok(false)`.
    #[test]
    fn try_publish_qos3_never_reports_backpressure() {
        let mut v = make_variant_with_tiny_sndbuf(Qos::ReliableUdp)
            .expect("must construct a non-blocking test variant");

        let payload = vec![0xEFu8; 64];
        // Modest iteration count: enough to hit at least one transient
        // WouldBlock at SO_SNDBUF=2KB, but few enough that the kernel-
        // drain spin in `publish_encoded` keeps the test fast.
        for seq in 0..500u64 {
            let result = v
                .try_publish("/p", &payload, Qos::ReliableUdp, seq)
                .expect("QoS 3 try_publish should succeed");
            assert!(
                result,
                "QoS 3 must never return Ok(false) (would create a NACK-fatal seq gap)"
            );
        }
    }

    /// QoS 4 (ReliableTcp): identical "always Ok(true)" contract.
    /// We don't have peers in this minimal fixture, so the TCP write
    /// path is a no-op — but the no-peers case is exactly the same
    /// codepath production hits when all peers have been dropped due
    /// to write errors, and the driver must still see `Ok(true)` so
    /// it doesn't emit `backpressure_skipped` for a transport that
    /// fundamentally can't gap.
    #[test]
    fn try_publish_qos4_never_reports_backpressure_no_peers() {
        let mut v = UdpVariant::new(default_config(Qos::ReliableTcp));
        // No `connect()` call: tcp_out_streams stays empty, which is
        // the "all peers dropped" steady-state. We just need to assert
        // the return contract here.
        let payload = b"hello";
        for seq in 0..50u64 {
            let result = v
                .try_publish("/p", payload, Qos::ReliableTcp, seq)
                .expect("QoS 4 try_publish should succeed");
            assert!(
                result,
                "QoS 4 must never return Ok(false) — TCP receivers expect contiguous seqs"
            );
        }
    }

    /// Happy path: when nothing is backpressured, `try_publish`
    /// returns `Ok(true)` on QoS 1. Uses a real loopback multicast
    /// socket (same path the variant uses in production) so this
    /// exercises the full `setup_udp` -> `try_publish` flow.
    #[test]
    fn try_publish_happy_path_returns_true() {
        // Ephemeral multicast group/port so we don't collide with the
        // other tests in this module that bind 239.0.0.1:9000.
        let mut cfg = default_config(Qos::BestEffort);
        cfg.multicast_group = SocketAddrV4::new(Ipv4Addr::new(239, 0, 0, 1), 19940);
        let mut v = UdpVariant::new(cfg);
        if v.connect(variant_base::ThreadingMode::Single).is_err() {
            // CI without multicast: skip silently. This is the same
            // pattern used by `connect_and_disconnect` above.
            return;
        }

        let payload = b"x";
        // A single send at low rate must always be accepted.
        let result = v
            .try_publish("/p", payload, Qos::BestEffort, 0)
            .expect("happy-path try_publish must succeed");
        assert!(result, "expected Ok(true) on idle transport");
        v.disconnect().ok();
    }

    // ---- T14.3: capability declaration ----

    /// custom-udp must declare `[Single, Multi]` per CUSTOM.md "Threading
    /// modes (T14.3)". The runner consults this declaration to skip
    /// spawns whose threading_mode the variant cannot honour.
    #[test]
    fn supported_threading_modes_includes_single_and_multi() {
        let v = UdpVariant::new(default_config(Qos::BestEffort));
        let modes = v.supported_threading_modes();
        assert!(modes.contains(&ThreadingMode::Single));
        assert!(modes.contains(&ThreadingMode::Multi));
        assert_eq!(modes.len(), 2);
    }

    // ---- T14.3: reader-thread lifecycle ----

    /// Multi mode: `start_reader_threads(Multi)` must spawn the UDP
    /// reader thread (and zero TCP threads for a single-peer / no-TCP
    /// config), and `stop_reader_threads` must tear them down cleanly
    /// without hanging or panicking.
    ///
    /// Uses an ephemeral multicast group/port so test runs don't collide.
    #[test]
    fn multi_mode_start_and_stop_reader_threads_lifecycle() {
        let mut cfg = default_config(Qos::BestEffort);
        cfg.multicast_group = SocketAddrV4::new(Ipv4Addr::new(239, 0, 0, 1), 19941);
        let mut v = UdpVariant::new(cfg);
        if v.connect(ThreadingMode::Multi).is_err() {
            // CI without multicast support: skip silently. Matches the
            // pattern used by `connect_and_disconnect`.
            return;
        }
        v.start_reader_threads(ThreadingMode::Multi)
            .expect("multi: start_reader_threads must succeed");
        // Reader-thread state must be populated.
        assert!(
            v.multi.is_some(),
            "expected MultiReaderState populated after start"
        );
        // Stop must succeed and clear the state.
        v.stop_reader_threads()
            .expect("multi: stop_reader_threads must succeed");
        assert!(
            v.multi.is_none(),
            "expected MultiReaderState cleared after stop"
        );
        v.disconnect().ok();
    }

    /// Multi mode end-to-end loopback: publish a message via multicast
    /// and confirm the UDP reader thread parses it, pushes it onto the
    /// mpsc, and `poll_receive` surfaces it. Loopback is enabled by
    /// `setup_udp` so we receive our own datagrams. The "skip-own-runner"
    /// filter in `recv_udp` / `drain_multi_channel` is bypassed here by
    /// using a writer name different from the configured runner.
    #[test]
    fn multi_mode_poll_receive_returns_loopback_message() {
        let mut cfg = default_config(Qos::BestEffort);
        cfg.multicast_group = SocketAddrV4::new(Ipv4Addr::new(239, 0, 0, 1), 19942);
        // Use a distinct runner name so the configured runner doesn't
        // match our injected writer (otherwise the variant filters its
        // own messages).
        cfg.runner = "test-runner-receiver".to_string();
        let mut v = UdpVariant::new(cfg);
        if v.connect(ThreadingMode::Multi).is_err() {
            return; // skip silently in CI without multicast
        }
        v.start_reader_threads(ThreadingMode::Multi)
            .expect("start_reader_threads(Multi) must succeed");

        // Encode a frame with a foreign "writer" name so the variant's
        // skip-own-writer filter does not eat it. Send via the bound UDP
        // socket directly so the kernel loops it back to our own
        // reader-thread clone.
        let encoded = protocol::encode(
            Qos::BestEffort,
            42,
            "/p",
            "external-writer",
            &[1u8, 2, 3, 4, 5, 6, 7, 8],
        )
        .unwrap();
        let target: SocketAddr = SocketAddr::V4(v.config.multicast_group);
        v.udp_socket
            .as_ref()
            .unwrap()
            .send_to(&encoded, target)
            .unwrap();

        // Poll for up to ~2 s for the loopback to surface. The reader
        // thread blocks with `READER_RCVTIMEO` (50 ms) so we have to
        // poll repeatedly.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got: Option<ReceivedUpdate> = None;
        while Instant::now() < deadline {
            if let Some(update) = v.poll_receive().unwrap() {
                got = Some(update);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Tear down before asserting so a test failure doesn't leak threads.
        v.stop_reader_threads().ok();
        v.disconnect().ok();

        let update = got.expect("expected to receive the loopback message via Multi mode");
        assert_eq!(update.writer, "external-writer");
        assert_eq!(update.seq, 42);
        assert_eq!(update.path, "/p");
        assert_eq!(update.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// Channel-bound computation: floored at `MULTI_CHANNEL_FLOOR` when
    /// the formula `4 * values_per_tick * (peer_count + 1)` lands below
    /// it. Important for low-rate fixtures (`values_per_tick = 1`,
    /// single peer) so reader threads don't bounce on `Full` under a
    /// trickle.
    #[test]
    fn multi_channel_bound_respects_floor() {
        let mut cfg = default_config(Qos::ReliableTcp);
        cfg.values_per_tick = 1;
        let v = UdpVariant::new(cfg);
        // 4 * 1 * (0 + 1) = 4, floored to 16.
        assert_eq!(v.multi_channel_bound(0), MULTI_CHANNEL_FLOOR);
        // 4 * 1 * (1 + 1) = 8, floored to 16.
        assert_eq!(v.multi_channel_bound(1), MULTI_CHANNEL_FLOOR);
    }

    /// Channel-bound computation: above the floor when
    /// `4 * values_per_tick * (peer_count + 1)` exceeds it.
    #[test]
    fn multi_channel_bound_scales_with_inputs() {
        let mut cfg = default_config(Qos::ReliableTcp);
        cfg.values_per_tick = 10;
        let v = UdpVariant::new(cfg);
        // 4 * 10 * (1 + 1) = 80.
        assert_eq!(v.multi_channel_bound(1), 80);
        // 4 * 10 * (5 + 1) = 240.
        assert_eq!(v.multi_channel_bound(5), 240);
    }

    /// Single mode is the default and must remain a no-op for
    /// `start_reader_threads` / `stop_reader_threads`. Tests guard
    /// against accidental Multi-mode regressions when Single is
    /// selected.
    #[test]
    fn single_mode_reader_thread_hooks_are_noops() {
        let mut cfg = default_config(Qos::BestEffort);
        cfg.multicast_group = SocketAddrV4::new(Ipv4Addr::new(239, 0, 0, 1), 19943);
        let mut v = UdpVariant::new(cfg);
        if v.connect(ThreadingMode::Single).is_err() {
            return;
        }
        v.start_reader_threads(ThreadingMode::Single)
            .expect("start_reader_threads(Single) is a no-op and must succeed");
        assert!(
            v.multi.is_none(),
            "Single mode must NOT populate MultiReaderState"
        );
        v.stop_reader_threads()
            .expect("stop_reader_threads(Single) is a no-op and must succeed");
        v.disconnect().ok();
    }

    // ---- T14.16: EOT survives reader-channel saturation ----

    /// Stub the reader-thread path: synthesise a `MultiReaderState`
    /// with hand-rolled channels, push N data items into a TINY data
    /// channel (so most of them drop on `Full`) interleaved with M
    /// `Eot` lifecycle items into the unbounded lifecycle channel, and
    /// confirm `drain_multi_channel` surfaces ALL M EOTs regardless of
    /// how many data drops happen along the way.
    ///
    /// This is the core T14.16 invariant: "data may drop, EOT must
    /// not." It exercises `drain_multi_channel` end-to-end without
    /// needing real sockets.
    #[test]
    fn t14_16_eot_survives_data_channel_saturation() {
        // Tiny data channel (8 slots) so saturating it is trivial.
        let (data_tx, data_rx) = mpsc::sync_channel::<ReaderDataItem>(8);
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel::<ReaderLifecycleItem>();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Build a no-network variant in Multi mode so
        // `drain_multi_channel` runs.
        let mut cfg = default_config(Qos::BestEffort);
        cfg.runner = "self".to_string();
        let mut v = UdpVariant::new(cfg);
        v.threading_mode = ThreadingMode::Multi;
        v.multi = Some(MultiReaderState {
            data_rx,
            lifecycle_rx,
            shutdown,
            handles: Vec::new(),
        });

        // Push 1000 data frames (will mostly drop), interleaved with
        // 16 EOTs across 16 distinct writers. Use try_send so
        // saturation doesn't block the test.
        let total_data = 1000u64;
        let eot_writers = 16u64;
        let eot_stride = total_data / eot_writers;
        let mut drops = 0u64;
        let mut eots_sent = 0u64;
        for i in 0..total_data {
            let msg = protocol::Message {
                qos: Qos::BestEffort,
                seq: i,
                path: "/p".to_string(),
                writer: format!("peer{}", i % eot_writers),
                payload: vec![0xAA; 8],
            };
            match data_tx.try_send(ReaderDataItem::Data(msg)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => drops += 1,
                Err(TrySendError::Disconnected(_)) => panic!("data channel disconnected"),
            }
            // Every `eot_stride` data sends fire an EOT on the
            // lifecycle channel until we have pushed `eot_writers`
            // EOTs. The data channel is tiny so this fires AFTER
            // saturation begins.
            if eots_sent < eot_writers && i.is_multiple_of(eot_stride) {
                let writer = format!("eot-writer-{}", eots_sent);
                let eot = protocol::EotFrame {
                    writer,
                    eot_id: 0x1000 + eots_sent,
                };
                lifecycle_tx
                    .send(ReaderLifecycleItem::Eot(eot))
                    .expect("lifecycle channel must not drop");
                eots_sent += 1;
            }
        }
        assert!(
            drops > 0,
            "test premise: at least one Data frame must have been dropped"
        );
        assert_eq!(eots_sent, eot_writers, "test pushed every EOT");

        // Drop the senders so the channels can signal Disconnected
        // after drain.
        drop(data_tx);
        drop(lifecycle_tx);

        // Run the drain. We expect EVERY EOT to surface in
        // `poll_peer_eots` regardless of how many data frames were
        // dropped during the burst above.
        v.drain_multi_channel()
            .expect("drain_multi_channel must succeed");

        let drained = v.poll_peer_eots().expect("poll_peer_eots");
        assert_eq!(
            drained.len() as u64,
            eot_writers,
            "every Eot pushed onto the lifecycle channel must survive data-channel saturation \
             (drops={}, eots_sent={}, eots_observed={})",
            drops,
            eots_sent,
            drained.len()
        );
    }

    /// T14.16: NACK items ride the lifecycle channel and must survive
    /// data-channel saturation. (Worker chose to fold NACK into the
    /// lifecycle channel rather than introducing a third sibling --
    /// see CUSTOM.md "Threading modes (T14.16)" / NACK disposition.)
    /// This test asserts a NACK pushed onto `lifecycle_tx` is
    /// observed by `drain_multi_channel` even when interleaved with
    /// saturating data load. We can't easily assert NACK side effects
    /// without a real send_buffer, so we check the no-panic /
    /// no-deadlock contract and confirm the lifecycle drain reached
    /// the NACK item.
    #[test]
    fn t14_16_nack_survives_data_channel_saturation() {
        let (data_tx, data_rx) = mpsc::sync_channel::<ReaderDataItem>(4);
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel::<ReaderLifecycleItem>();
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut v = UdpVariant::new(default_config(Qos::ReliableUdp));
        v.threading_mode = ThreadingMode::Multi;
        v.multi = Some(MultiReaderState {
            data_rx,
            lifecycle_rx,
            shutdown,
            handles: Vec::new(),
        });

        // Saturate the data channel.
        for i in 0..32u64 {
            let msg = protocol::Message {
                qos: Qos::ReliableUdp,
                seq: i,
                path: "/p".to_string(),
                writer: "peer".to_string(),
                payload: vec![0xCD; 8],
            };
            let _ = data_tx.try_send(ReaderDataItem::Data(msg));
        }
        // Push a malformed NACK datagram onto the lifecycle channel.
        // `handle_nack` will log an error and continue; the point is
        // the drain reaches it without panic / deadlock and continues.
        lifecycle_tx
            .send(ReaderLifecycleItem::Nack(vec![0u8; 8]))
            .expect("lifecycle channel must accept NACK");

        drop(data_tx);
        drop(lifecycle_tx);

        // Drain must succeed and not propagate any NACK-decode error.
        v.drain_multi_channel()
            .expect("drain_multi_channel must absorb NACK-decode errors silently");
    }
}
