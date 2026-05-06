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

use std::collections::{HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tungstenite::{
    client::IntoClientRequest, handshake::server::NoCallback, ClientHandshake, HandshakeError,
    Message, ServerHandshake, WebSocket,
};

use variant_base::types::{Qos, ReceivedUpdate};
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

/// A single WebSocket peer connection.
struct WsPeer {
    /// Peer's runner name (used to filter own EOT loopback and for log
    /// diagnostics).
    name: String,
    /// Local view of the peer address (informational).
    addr: SocketAddr,
    /// The sync tungstenite WebSocket. Holds the TCP stream internally;
    /// dropping this drops the socket.
    ws: WebSocket<TcpStream>,
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
}

impl WebSocketConfig {
    pub fn from_derived(derived: DerivedEndpoints, qos: Qos) -> Self {
        Self {
            listen_addr: derived.listen_addr,
            peers: derived.peers,
            qos,
        }
    }
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
}

impl WebSocketVariant {
    pub fn new(runner: &str, config: WebSocketConfig) -> Self {
        Self {
            runner: runner.to_string(),
            config,
            peers: Vec::new(),
            seen_eots: HashSet::new(),
            pending_eots: VecDeque::new(),
        }
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

    /// Poll every active peer once, dispatching any received frame.
    /// Returns the first data update found, or `None` if no peer had a
    /// data frame ready this pass. Per-peer fatal errors drop the peer
    /// and the loop continues with the rest.
    fn poll_peers_once(&mut self) -> Option<ReceivedUpdate> {
        let mut keep: Vec<bool> = Vec::with_capacity(self.peers.len());
        let mut hit: Option<ReceivedUpdate> = None;
        // Capture EOT observations into a local buffer so we don't borrow
        // self twice (the receive loop borrows &mut self.peers; record_eot
        // borrows &mut self too).
        let mut eots: Vec<(String, u64)> = Vec::new();

        for peer in self.peers.iter_mut() {
            if hit.is_some() {
                keep.push(true);
                continue;
            }
            match peer.ws.read() {
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
                    // tungstenite handles pong responses internally; we just keep going.
                    keep.push(true);
                }
                Ok(Message::Close(_)) => {
                    // Peer requested close; treat as fatal-for-this-peer.
                    eprintln!(
                        "warning: WS peer {} ({}) sent Close frame; dropping",
                        peer.name, peer.addr
                    );
                    keep.push(false);
                }
                Ok(other) => {
                    // Text or Frame variant: not expected. Skip.
                    eprintln!(
                        "warning: WS peer {} ({}) sent unexpected message {:?}; ignoring",
                        peer.name, peer.addr, other
                    );
                    keep.push(true);
                }
                Err(tungstenite::Error::Io(e)) if is_transient_io_error(&e) => {
                    // Read-poll deadline elapsed (or transient Windows
                    // overlapped-I/O race); nothing buffered. Not fatal.
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

    /// Send a binary frame to every active peer. Drops a peer on a fatal
    /// write error; mirrors Hybrid TCP's broadcast behaviour.
    fn broadcast_binary(&mut self, payload: Vec<u8>) -> Result<()> {
        let mut keep: Vec<bool> = Vec::with_capacity(self.peers.len());
        let mut last_err: Option<anyhow::Error> = None;

        for peer in self.peers.iter_mut() {
            // `Message::binary` clones into a fresh `Vec`; we clone here so we
            // can hand a Vec to tungstenite per peer.
            match peer.ws.send(Message::Binary(payload.clone())) {
                Ok(()) => keep.push(true),
                Err(e) => {
                    eprintln!(
                        "warning: dropping WS peer {} ({}) after write error: {:#}",
                        peer.name, peer.addr, e
                    );
                    keep.push(false);
                    last_err = Some(anyhow::anyhow!("WS write error: {}", e));
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

/// Open one TCP connection, then perform the WS client handshake. The
/// handshake itself is allowed `PEER_HANDSHAKE_TIMEOUT` total wall-clock.
fn ws_client_connect(addr: SocketAddr) -> Result<WebSocket<TcpStream>> {
    let deadline = Instant::now() + PEER_HANDSHAKE_TIMEOUT;
    let url = format!("ws://{}/bench", addr);
    let request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("invalid WS URL '{url}'"))?;

    // Open TCP. `TcpStream::connect_timeout` requires a single SocketAddr.
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

    // For the handshake itself we want short-blocking semantics so the
    // upgrade completes promptly. After the handshake we re-arm the
    // read timeout for poll behaviour.
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

    // After handshake: blocking writes (clear write_timeout), short read
    // timeout for the poll loop. `set_write_timeout(None)` restores the
    // socket to fully-blocking writes -- the back-pressure signal we
    // want to measure.
    let s = ws.get_ref();
    s.set_write_timeout(None)
        .with_context(|| format!("failed to clear TCP write timeout for {addr}"))?;
    s.set_read_timeout(Some(READ_POLL_TIMEOUT))
        .with_context(|| format!("failed to set short TCP read timeout for {addr}"))?;
    Ok(ws)
}

/// Drive `tungstenite::client` to completion, handling the
/// `Interrupted` mid-handshake retry.
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

/// Drive `tungstenite::server::accept` to completion against a freshly
/// accepted TCP stream.
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

    fn connect(&mut self) -> Result<()> {
        // Defensive: the variant only supports reliable QoS levels (3 and 4).
        // `main` should already have rejected 1/2 before we got here, but
        // re-check so the trait is robust on its own.
        if matches!(self.config.qos, Qos::BestEffort | Qos::LatestValue) {
            bail!(
                "websocket variant does not support QoS {} (reliable QoS 3-4 only)",
                self.config.qos
            );
        }

        // Partition peers by role.
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

        // Set up the TCP listener if any peer expects us to accept.
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

        // Connect to every peer where we are the client (in parallel-ish:
        // we just dial sequentially with a per-peer retry loop). Each
        // peer's TCP listener may not be bound yet -- we tolerate
        // ConnectionRefused while it brings up its socket.
        for peer in &client_pairs {
            let ws = ws_client_connect(peer.addr)
                .with_context(|| format!("failed WS client connect to {}", peer.addr))?;
            self.peers.push(WsPeer {
                name: peer.name.clone(),
                addr: peer.addr,
                ws,
            });
        }

        // Accept the inbound side of every server pair, polling the
        // non-blocking listener.
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
                        // Match the inbound socket back to its peer name by
                        // remote IP. With multiple same-host peers this is
                        // ambiguous, but each accepted connection still gets
                        // a slot; we resolve order by accept order against
                        // server_pairs sorted-by-name.
                        let name = server_pairs
                            .iter()
                            .filter(|p| p.addr.ip() == addr.ip())
                            .nth(accepted_count_for_ip(&self.peers, addr.ip()))
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| format!("inbound-{addr}"));
                        self.peers.push(WsPeer { name, addr, ws });
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
        // Each pass through poll_peers_once dispatches at most one frame
        // per peer. If the first pass returns Data, we're done. If it only
        // dispatched non-data (EOTs) on some peers, we keep iterating with a
        // bounded budget so the data behind those EOTs isn't masked.
        const POLL_BUDGET: u32 = 256;
        for _ in 0..POLL_BUDGET {
            // Snapshot whether we had any pending EOTs before this pass; if
            // poll_peers_once produces a new EOT we retry, otherwise we
            // surface None.
            let pending_before = self.pending_eots.len();
            if let Some(update) = self.poll_peers_once() {
                return Ok(Some(update));
            }
            if self.pending_eots.len() == pending_before {
                return Ok(None);
            }
        }
        Ok(None)
    }

    fn disconnect(&mut self) -> Result<()> {
        // Send a clean WS close to every peer with a small grace window,
        // then forcibly tear down the underlying TCP. We do not block the
        // spawn forever waiting for peers to acknowledge -- the EOT phase
        // already gave them a deterministic boundary.
        let close_deadline = Instant::now() + DISCONNECT_GRACE;
        let mut peers = std::mem::take(&mut self.peers);
        for peer in peers.iter_mut() {
            // Best-effort close.
            let _ = peer.ws.close(None);
        }
        // Drain any pending writes / read the peer's close response with
        // bounded effort.
        for peer in peers.iter_mut() {
            while Instant::now() < close_deadline {
                match peer.ws.read() {
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
        // Forcibly shut down the underlying TCP regardless of WS state.
        for peer in peers.iter() {
            let s = peer.ws.get_ref();
            let _ = s.shutdown(Shutdown::Both);
        }
        drop(peers);
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

/// Helper: count how many already-accepted peers share the given IP, so
/// we can pick the next-by-sorted-name candidate when multiple peers
/// share a host.
fn accepted_count_for_ip(accepted: &[WsPeer], ip: std::net::IpAddr) -> usize {
    accepted.iter().filter(|p| p.addr.ip() == ip).count()
}

/// Decide whether an `io::Error` from the underlying TCP socket should be
/// treated as a transient "no data available" signal (keep peer alive) vs
/// a fatal connection error (drop peer).
///
/// On Unix, `SO_RCVTIMEO` produces `WouldBlock`. On Windows it produces
/// `TimedOut` (`WSAETIMEDOUT` -> 10060). However, Windows can also surface
/// `ERROR_IO_PENDING` (997) when an overlapped read is mid-flight and the
/// timeout fires, or `WSAEWOULDBLOCK` (10035) on certain code paths --
/// neither indicates a genuine connection failure. Treat them all as
/// transient so a low-traffic stream isn't spuriously dropped.
fn is_transient_io_error(e: &std::io::Error) -> bool {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        return true;
    }
    match e.raw_os_error() {
        Some(997) => true,   // ERROR_IO_PENDING
        Some(10035) => true, // WSAEWOULDBLOCK
        Some(10060) => true, // WSAETIMEDOUT
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
        }
    }

    #[test]
    fn name_returns_websocket() {
        let v = WebSocketVariant::new("r", dummy_config(Qos::ReliableTcp));
        assert_eq!(v.name(), "websocket");
    }

    #[test]
    fn publish_qos1_returns_error() {
        let mut v = WebSocketVariant::new("r", dummy_config(Qos::BestEffort));
        let err = v
            .publish("/p", &[0u8], Qos::BestEffort, 1)
            .expect_err("qos 1 must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not support") || msg.contains("reliable QoS"),
            "unexpected error message: {msg}"
        );
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
        let err = v.connect().expect_err("connect must reject qos 1");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not support") || msg.contains("reliable"));
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
        // Subsequent poll: nothing new.
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
}
