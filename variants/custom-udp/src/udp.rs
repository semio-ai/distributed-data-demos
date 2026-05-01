/// UdpVariant: implements the `Variant` trait using raw UDP sockets
/// with multicast for QoS 1-3 and TCP for QoS 4.
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use variant_base::{Qos, ReceivedUpdate, Variant};

use crate::protocol;
use crate::qos::{GapCheckResult, GapDetector, LatestValueTracker};

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
        for peer_addr in &self.config.tcp_peers {
            match TcpStream::connect(peer_addr) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    stream.set_nonblocking(true)?;
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

        // Read from all incoming TCP streams.
        let mut new_in_streams = Vec::new();
        for mut stream in self.tcp_in_streams.drain(..) {
            let mut keep = true;
            // Try to read a length-prefixed message.
            let mut len_buf = [0u8; 4];
            match stream.read_exact(&mut len_buf) {
                Ok(()) => {
                    let total_len = u32::from_be_bytes(len_buf) as usize;
                    if total_len > self.config.buffer_size {
                        eprintln!("[custom-udp] TCP message too large: {}", total_len);
                        keep = false;
                    } else {
                        let mut msg_buf = vec![0u8; total_len];
                        msg_buf[..4].copy_from_slice(&len_buf);
                        match stream.read_exact(&mut msg_buf[4..]) {
                            Ok(()) => match protocol::decode(&msg_buf) {
                                Ok(msg) => {
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
                                Err(e) => {
                                    eprintln!("[custom-udp] TCP decode error: {}", e);
                                }
                            },
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(_) => {
                                keep = false;
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    keep = false;
                }
            }
            if keep {
                new_in_streams.push(stream);
            }
        }
        self.tcp_in_streams = new_in_streams;

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

    fn connect(&mut self) -> Result<()> {
        self.setup_udp()?;

        if self.config.qos == Qos::ReliableTcp {
            self.setup_tcp()?;
        }

        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        let encoded = protocol::encode(qos, seq, path, &self.config.runner, payload)?;

        match qos {
            Qos::BestEffort | Qos::LatestValue => {
                // Send via multicast UDP.
                let socket = self
                    .udp_socket
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("UDP socket not connected"))?;
                let target: SocketAddr = SocketAddr::V4(self.config.multicast_group);
                loop {
                    match socket.send_to(&encoded, target) {
                        Ok(_) => break,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::yield_now();
                            continue;
                        }
                        Err(e) => return Err(e).context("UDP send failed"),
                    }
                }
            }
            Qos::ReliableUdp => {
                // Send via multicast UDP and buffer for NACK retransmit.
                let socket = self
                    .udp_socket
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("UDP socket not connected"))?;
                let target: SocketAddr = SocketAddr::V4(self.config.multicast_group);
                loop {
                    match socket.send_to(&encoded, target) {
                        Ok(_) => break,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::yield_now();
                            continue;
                        }
                        Err(e) => return Err(e).context("UDP send failed"),
                    }
                }

                // Buffer for retransmit. Limit buffer to last 10000 messages.
                self.send_buffer.insert(seq, encoded);
                if self.send_buffer.len() > 10000 {
                    // Remove oldest entries. Since seq is monotonically increasing,
                    // remove anything below seq - 10000.
                    if seq > 10000 {
                        let cutoff = seq - 10000;
                        self.send_buffer.retain(|&k, _| k > cutoff);
                    }
                }
            }
            Qos::ReliableTcp => {
                // Send via TCP to all connected peers.
                let mut failed_indices = Vec::new();
                for (i, stream) in self.tcp_out_streams.iter_mut().enumerate() {
                    if stream.write_all(&encoded).is_err() {
                        failed_indices.push(i);
                    }
                }
                // Remove failed streams (in reverse to preserve indices).
                for &i in failed_indices.iter().rev() {
                    self.tcp_out_streams.remove(i);
                }
                if self.tcp_out_streams.is_empty() && !self.config.tcp_peers.is_empty() {
                    // All TCP peers disconnected but we had peers configured.
                    // Fall through silently; the runner will detect missing data.
                }
            }
        }

        Ok(())
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
        if variant.connect().is_ok() {
            assert!(variant.disconnect().is_ok());
        }
    }

    #[test]
    fn poll_receive_before_connect_returns_none() {
        let mut variant = UdpVariant::new(default_config(Qos::BestEffort));
        let result = variant.poll_receive().unwrap();
        assert!(result.is_none());
    }
}
