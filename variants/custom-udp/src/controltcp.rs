/// T14.18: per-peer-pair TCP control side-channel for EOT exchange.
///
/// QoS-independent TCP connection per peer pair, established at
/// `connect()` time. Carries ONLY length-prefixed control frames
/// (EOT markers and `bye` shutdown signals). Because the control
/// connection is a separate kernel socket from the data path
/// (multicast UDP for QoS 1-3 / TCP per-pair for QoS 4), an EOT
/// pushed on it cannot be dropped by data-path saturation (UDP recv
/// buffer overrun, TCP send-buffer fill, ...).
///
/// ## Pairing
///
/// Lower-sorted-name peer is the **server** (binds + listens). Higher-
/// sorted peer is the **client** (connects). Same convention as
/// Hybrid/QUIC/WebSocket. One bidirectional connection per peer pair.
///
/// ## Threading
///
/// - **Multi mode**: one dedicated OS thread per control connection
///   reads length-prefixed frames in a blocking loop with
///   `MULTI_MODE_READ_TIMEOUT` and pushes decoded EOT markers onto
///   the existing T14.16 `lifecycle_tx` channel.
/// - **Single mode**: control socket is blocking with
///   `SINGLE_MODE_READ_TIMEOUT = 1ms`. The variant's `poll_receive`
///   polls each control peer inline; no additional threads.
///
/// ## Lifecycle teardown
///
/// At `disconnect()`: send a length-prefixed `bye` frame, half-close
/// the write side, drain the read side until peer closes or
/// `--eot-timeout-secs` elapses, then drop. Same shape as the
/// websocket variant's EOT drain.
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::protocol::{self, EotFrame};

/// Short read timeout used for the control socket in Single mode.
pub const SINGLE_MODE_READ_TIMEOUT: Duration = Duration::from_millis(1);

/// Read timeout for the Multi-mode control reader thread.
pub const MULTI_MODE_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// Maximum payload size we will accept on the control connection.
pub const MAX_CONTROL_FRAME_BYTES: usize = 4096;

/// Bounded retry budget for the client-side `TcpStream::connect`.
pub const CONTROL_CONNECT_BUDGET: Duration = Duration::from_secs(30);

/// Maximum time to wait for a server-side accept.
pub const CONTROL_ACCEPT_BUDGET: Duration = Duration::from_secs(30);

/// Maximum time to wait for a reader thread to join during teardown.
pub const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

const FRAME_TAG_EOT: u8 = 0x01;
const FRAME_TAG_BYE: u8 = 0x02;

/// Per-peer control wiring derived from `--control-base-port` + the
/// `--peers` map. See `main.rs::derive_control_endpoints`.
#[derive(Debug, Clone)]
pub struct ControlPeerEndpoint {
    pub peer_name: String,
    pub peer_addr: SocketAddr,
    pub role: ControlRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRole {
    /// We accept the inbound connection.
    Server,
    /// We dial the peer.
    Client,
}

/// One peer's control connection: a single bidirectional `TcpStream`.
pub struct ControlPeer {
    pub peer_name: String,
    pub peer_addr: SocketAddr,
    write_stream: TcpStream,
    read_stream: Option<TcpStream>,
    read_buf: Vec<u8>,
}

impl ControlPeer {
    pub fn from_stream(
        stream: TcpStream,
        peer_name: String,
        peer_addr: SocketAddr,
    ) -> Result<Self> {
        stream
            .set_nodelay(true)
            .with_context(|| format!("set TCP_NODELAY on control stream to {peer_addr}"))?;
        stream
            .set_nonblocking(false)
            .with_context(|| format!("set blocking on control stream to {peer_addr}"))?;
        let read_stream = stream
            .try_clone()
            .with_context(|| format!("try_clone control stream to {peer_addr}"))?;
        read_stream
            .set_read_timeout(Some(SINGLE_MODE_READ_TIMEOUT))
            .with_context(|| format!("set read timeout on control stream to {peer_addr}"))?;
        Ok(Self {
            peer_name,
            peer_addr,
            write_stream: stream,
            read_stream: Some(read_stream),
            read_buf: Vec::new(),
        })
    }

    /// Take the read clone for use by a Multi-mode reader thread.
    pub fn take_read_clone(&mut self) -> Option<TcpStream> {
        self.read_stream.take()
    }

    /// Single-mode polling: try to read one complete frame.
    pub fn try_recv_frame(&mut self) -> Result<Option<ControlFrame>> {
        let read = match self.read_stream.as_mut() {
            Some(r) => r,
            None => return Ok(None),
        };
        let mut tmp = [0u8; 1024];
        match read.read(&mut tmp) {
            Ok(0) => anyhow::bail!("control peer {} closed (EOF)", self.peer_addr),
            Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // No data in the short window; try to extract from
                // whatever we may already have buffered.
            }
            Err(e) => {
                anyhow::bail!(
                    "control read error from {}: {} ({:?})",
                    self.peer_addr,
                    e,
                    e.kind()
                );
            }
        }

        extract_one_frame(&mut self.read_buf, self.peer_addr)
    }

    /// Write a length-prefixed control frame.
    pub fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        let len: u32 = frame
            .len()
            .try_into()
            .with_context(|| format!("control frame too large: {} bytes", frame.len()))?;
        let mut out = Vec::with_capacity(4 + frame.len());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(frame);
        self.write_stream
            .write_all(&out)
            .with_context(|| format!("control write to {} failed", self.peer_addr))
    }

    /// Send a `bye` marker so the peer knows we are done.
    pub fn send_bye(&mut self) {
        let _ = self.send_frame(&encode_bye());
    }

    /// Half-close the write side.
    pub fn shutdown_write(&self) {
        let _ = self.write_stream.shutdown(Shutdown::Write);
    }

    /// Drain the read side until the peer closes or `deadline` elapses.
    pub fn drain_until_closed(&mut self, deadline: Instant) -> Vec<ControlFrame> {
        let mut frames = Vec::new();
        loop {
            if Instant::now() >= deadline {
                return frames;
            }
            match self.try_recv_frame() {
                Ok(Some(frame)) => {
                    let is_bye = matches!(frame, ControlFrame::Bye);
                    frames.push(frame);
                    if is_bye {
                        return frames;
                    }
                }
                Ok(None) => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return frames,
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.write_stream.shutdown(Shutdown::Both);
    }
}

/// A decoded control frame.
#[derive(Debug, Clone)]
pub enum ControlFrame {
    Eot { writer: String, eot_id: u64 },
    Bye,
}

/// Encode an EOT control frame.
///
/// Layout: `[tag = 0x01][protocol-level EOT encoding]`. The inner
/// payload reuses `protocol::encode_eot` (same `(writer, eot_id)`
/// shape carried over multicast/TCP pre-T14.18).
pub fn encode_eot_frame(writer: &str, eot_id: u64) -> Vec<u8> {
    // `protocol::encode_eot` itself returns `Result<Vec<u8>>`; the
    // only error path is "writer too long for u16", which is
    // exceedingly unlikely for a runner name. Surface the underlying
    // bytes here; an oversize writer would already have surfaced via
    // the data-path code.
    let inner = protocol::encode_eot(writer, eot_id).expect("encode_eot");
    let mut out = Vec::with_capacity(1 + inner.len());
    out.push(FRAME_TAG_EOT);
    out.extend_from_slice(&inner);
    out
}

/// Encode a `bye` control frame.
pub fn encode_bye() -> Vec<u8> {
    vec![FRAME_TAG_BYE]
}

/// Decode a single control frame.
pub fn decode_control_frame(payload: &[u8]) -> Result<ControlFrame> {
    if payload.is_empty() {
        anyhow::bail!("empty control frame");
    }
    match payload[0] {
        FRAME_TAG_EOT => {
            let inner = &payload[1..];
            let eot: EotFrame =
                protocol::decode_eot(inner).context("decode inner EOT payload of control frame")?;
            Ok(ControlFrame::Eot {
                writer: eot.writer,
                eot_id: eot.eot_id,
            })
        }
        FRAME_TAG_BYE => Ok(ControlFrame::Bye),
        other => anyhow::bail!("unknown control frame tag: 0x{:02X}", other),
    }
}

fn extract_one_frame(buf: &mut Vec<u8>, peer_addr: SocketAddr) -> Result<Option<ControlFrame>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let frame_len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
    if frame_len == 0 {
        anyhow::bail!("control peer {peer_addr} sent zero-length frame");
    }
    if frame_len > MAX_CONTROL_FRAME_BYTES {
        anyhow::bail!(
            "control peer {peer_addr} sent oversized frame: {} > {}",
            frame_len,
            MAX_CONTROL_FRAME_BYTES
        );
    }
    let total = 4 + frame_len;
    if buf.len() < total {
        return Ok(None);
    }
    let payload: Vec<u8> = buf[4..total].to_vec();
    buf.drain(..total);
    Ok(Some(decode_control_frame(&payload)?))
}

/// Multi-mode reader thread function. Pushes decoded EOT markers onto
/// the existing lifecycle channel via the variant's
/// `ReaderLifecycleItem::Eot` shape.
pub fn spawn_control_reader<T>(
    mut read_stream: TcpStream,
    peer_label: String,
    lifecycle_tx: Sender<T>,
    map_eot: fn(EotFrame) -> T,
    shutdown: Arc<AtomicBool>,
) -> Result<JoinHandle<()>>
where
    T: Send + 'static,
{
    read_stream
        .set_read_timeout(Some(MULTI_MODE_READ_TIMEOUT))
        .with_context(|| format!("set read timeout for control reader {peer_label}"))?;
    let label = peer_label.clone();
    let handle = thread::Builder::new()
        .name(format!("custom-udp-control-reader-{peer_label}"))
        .spawn(move || {
            control_reader_loop(&mut read_stream, label, lifecycle_tx, map_eot, shutdown)
        })
        .with_context(|| format!("spawn control reader thread {peer_label}"))?;
    Ok(handle)
}

fn control_reader_loop<T>(
    stream: &mut TcpStream,
    label: String,
    lifecycle_tx: Sender<T>,
    map_eot: fn(EotFrame) -> T,
    shutdown: Arc<AtomicBool>,
) where
    T: Send + 'static,
{
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut tmp = [0u8; 1024];
    while !shutdown.load(Ordering::SeqCst) {
        match stream.read(&mut tmp) {
            Ok(0) => {
                if !shutdown.load(Ordering::SeqCst) {
                    eprintln!(
                        "[custom-udp] control reader {label}: peer closed (EOF); thread exits"
                    );
                }
                return;
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                eprintln!("[custom-udp] control reader {label}: read error: {e}; thread exits");
                return;
            }
        }
        loop {
            if buf.len() < 4 {
                break;
            }
            let frame_len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
            if frame_len == 0 || frame_len > MAX_CONTROL_FRAME_BYTES {
                eprintln!(
                    "[custom-udp] control reader {label}: invalid frame_len {frame_len}; thread exits"
                );
                return;
            }
            let total = 4 + frame_len;
            if buf.len() < total {
                break;
            }
            let payload: Vec<u8> = buf[4..total].to_vec();
            buf.drain(..total);
            match decode_control_frame(&payload) {
                Ok(ControlFrame::Eot { writer, eot_id }) => {
                    let _ = lifecycle_tx.send(map_eot(EotFrame { writer, eot_id }));
                }
                Ok(ControlFrame::Bye) => {
                    // Peer signalled done; keep reading until EOF in
                    // case of in-flight frames.
                }
                Err(e) => {
                    eprintln!(
                        "[custom-udp] control reader {label}: decode error: {e:#}; thread exits"
                    );
                    return;
                }
            }
        }
    }
}

/// Accept one inbound control connection with a bounded deadline.
pub fn accept_with_budget(
    listener: &TcpListener,
    deadline: Instant,
) -> Result<(TcpStream, SocketAddr)> {
    listener
        .set_nonblocking(true)
        .context("set TCP listener non-blocking for control accept")?;
    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                stream
                    .set_nonblocking(false)
                    .context("restore blocking on accepted control stream")?;
                return Ok((stream, addr));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for control connection accept");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e).context("control listener accept failed"),
        }
    }
}

/// Connect to `addr` with bounded retry on `ConnectionRefused`/
/// `TimedOut`/`WouldBlock`. Same pattern as `tcp::connect_with_retry`
/// in the hybrid variant.
pub fn connect_with_budget(addr: SocketAddr, budget: Duration) -> Result<TcpStream> {
    let deadline = Instant::now() + budget;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return Ok(s),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "control connect to {addr} failed within {budget:?}: {e}"
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(anyhow::anyhow!("control connect to {addr} failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn eot_frame_roundtrip() {
        let encoded = encode_eot_frame("alice", 0xCAFE_BABE_DEAD_BEEFu64);
        let payload = &encoded[..];
        let decoded = decode_control_frame(payload).expect("must decode");
        match decoded {
            ControlFrame::Eot { writer, eot_id } => {
                assert_eq!(writer, "alice");
                assert_eq!(eot_id, 0xCAFE_BABE_DEAD_BEEFu64);
            }
            other => panic!("expected Eot, got {other:?}"),
        }
    }

    #[test]
    fn bye_frame_roundtrip() {
        let encoded = encode_bye();
        let decoded = decode_control_frame(&encoded).expect("bye must decode");
        assert!(matches!(decoded, ControlFrame::Bye));
    }

    #[test]
    fn decode_rejects_empty_payload() {
        assert!(decode_control_frame(&[]).is_err());
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        assert!(decode_control_frame(&[0xFFu8, 0, 0, 0]).is_err());
    }

    #[test]
    fn extract_one_frame_oversized_errors() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&((MAX_CONTROL_FRAME_BYTES as u32 + 1).to_be_bytes()));
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
        assert!(extract_one_frame(&mut buf, addr).is_err());
    }

    #[test]
    fn extract_one_frame_zero_length_errors() {
        let mut buf: Vec<u8> = vec![0, 0, 0, 0];
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
        assert!(extract_one_frame(&mut buf, addr).is_err());
    }

    /// End-to-end loopback: stand up a listener, dial it, send an EOT
    /// frame from client to server, read it back via `try_recv_frame`.
    #[test]
    fn end_to_end_loopback_eot() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let server_addr = listener.local_addr().unwrap();

        let join = std::thread::spawn(move || {
            let (stream, addr) = listener.accept().unwrap();
            ControlPeer::from_stream(stream, "client".to_string(), addr).unwrap()
        });

        let client_stream =
            connect_with_budget(server_addr, Duration::from_secs(2)).expect("connect must succeed");
        let mut client =
            ControlPeer::from_stream(client_stream, "server".to_string(), server_addr).unwrap();
        let mut server_peer = join.join().unwrap();

        let frame = encode_eot_frame("client-runner", 0x1234);
        client.send_frame(&frame).expect("client send");

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got: Option<ControlFrame> = None;
        while Instant::now() < deadline {
            match server_peer.try_recv_frame() {
                Ok(Some(f)) => {
                    got = Some(f);
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(2)),
                Err(e) => panic!("server read errored: {e:#}"),
            }
        }
        let got = got.expect("server must receive the frame");
        match got {
            ControlFrame::Eot { writer, eot_id } => {
                assert_eq!(writer, "client-runner");
                assert_eq!(eot_id, 0x1234);
            }
            other => panic!("expected Eot, got {other:?}"),
        }
    }

    /// Saturation test: simulate the entire data path being dropped
    /// (no data sockets at all) and assert that an EOT frame pushed
    /// on the control channel is still observable end-to-end.
    #[test]
    fn eot_survives_with_no_data_path() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let server_addr = listener.local_addr().unwrap();

        let join = std::thread::spawn(move || {
            let (stream, addr) = listener.accept().unwrap();
            ControlPeer::from_stream(stream, "client".to_string(), addr).unwrap()
        });

        let client_stream =
            connect_with_budget(server_addr, Duration::from_secs(2)).expect("connect");
        let mut client =
            ControlPeer::from_stream(client_stream, "server".to_string(), server_addr).unwrap();
        let mut server_peer = join.join().unwrap();

        // Send 5 EOT frames from 5 distinct writers (simulating the
        // peer broadcast in a 6-peer cluster).
        for i in 0..5u64 {
            let frame = encode_eot_frame(&format!("writer-{i}"), 0x100 + i);
            client.send_frame(&frame).unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut received: Vec<(String, u64)> = Vec::new();
        while received.len() < 5 && Instant::now() < deadline {
            match server_peer.try_recv_frame() {
                Ok(Some(ControlFrame::Eot { writer, eot_id })) => {
                    received.push((writer, eot_id));
                }
                Ok(Some(ControlFrame::Bye)) => {}
                Ok(None) => thread::sleep(Duration::from_millis(2)),
                Err(e) => panic!("server read error: {e:#}"),
            }
        }
        assert_eq!(received.len(), 5);
    }
}
