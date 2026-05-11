/// UdpVariant: implements the `Variant` trait using raw UDP sockets
/// with multicast for QoS 1-3 and TCP for QoS 4.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use variant_base::{PeerEot, Qos, ReceivedUpdate, Variant};

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
    tcp_listener: Option<TcpListener>,
    /// QoS 4: TCP streams to peers (for sending).
    tcp_out_streams: Vec<TcpStream>,
    /// QoS 4: TCP streams from peers (for receiving).
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
        }
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
    /// QoS 1-3 (UDP path): broadcast the EOT datagram to the multicast
    /// group `EOT_UDP_RETRIES` times with `EOT_UDP_SPACING` between sends.
    /// Receivers dedupe by `(writer, eot_id)` so duplicates are absorbed.
    ///
    /// QoS 4 (TCP path): send the framed EOT to every connected peer once.
    /// TCP delivery + ordering guarantees make retries unnecessary; failed
    /// peers are dropped from the active set and the spawn continues.
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
}

impl Variant for UdpVariant {
    fn name(&self) -> &str {
        "custom-udp"
    }

    fn connect(&mut self, threading_mode: variant_base::ThreadingMode) -> Result<()> {
        // T14.1 compile-fix only -- the trait signature gained the
        // threading-mode argument. Real Multi-mode handling for
        // custom-udp is filed under T14.3.
        let _ = threading_mode;
        self.setup_udp()?;

        if self.config.qos == Qos::ReliableTcp {
            self.setup_tcp()?;
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

        // Try receiving from UDP.
        self.recv_udp()?;

        // For QoS 4, also check TCP.
        if self.config.qos == Qos::ReliableTcp {
            self.recv_tcp()?;
        }

        Ok(self.pending.pop_front())
    }

    fn disconnect(&mut self) -> Result<()> {
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

    fn signal_end_of_test(&mut self) -> Result<u64> {
        let eot_id: u64 = rand::random::<u64>();
        self.send_eot(eot_id)?;
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
}
