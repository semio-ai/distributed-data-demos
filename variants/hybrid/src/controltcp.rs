/// T14.18: per-peer-pair TCP control side-channel for EOT exchange.
///
/// This module establishes a QoS-independent TCP connection per peer
/// pair at `connect()` time. The connection carries ONLY control
/// frames (currently length-prefixed EOT markers). Because the control
/// connection is a separate kernel socket from the data path (UDP
/// multicast for QoS 1-2, TCP per-pair for QoS 3-4), an EOT marker
/// pushed on it cannot be dropped by the kernel's UDP recv buffer
/// overrun nor blocked behind a saturated data-path TCP send buffer.
///
/// ## Pairing
///
/// Lower-sorted-name peer is the **server** (binds + listens). Higher-
/// sorted peer is the **client** (connects). This matches the
/// Hybrid/QUIC/WebSocket convention so the pairing logic stays
/// uniform across variants. One bidirectional connection per peer pair.
///
/// ## Threading
///
/// In Multi mode, the variant spawns one dedicated OS thread per
/// control connection. The thread does blocking `read_exact` of
/// length-prefixed frames (4-byte BE length) and pushes decoded EOT
/// markers onto the existing T14.16 lifecycle channel.
///
/// In Single mode, the control socket is left in BLOCKING mode with a
/// short `SO_RCVTIMEO` (1 ms). The variant's `poll_receive` calls
/// `try_recv_frame` once per pass, which returns `Ok(None)` quickly
/// when nothing is in flight and `Ok(Some(frame))` when a complete
/// EOT frame has accumulated. This keeps Single mode's "no extra
/// threads" property (data path remains single-threaded; the only
/// extra socket is the control fd polled inline).
///
/// ## Lifecycle teardown
///
/// At `close()` we send a length-prefixed `bye` frame, half-close the
/// write side, drain the read side until the peer closes or
/// `--eot-timeout-secs` elapses, then drop the stream. Same shape as
/// the websocket variant's EOT drain.
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::protocol::{self, Frame};
use crate::reader::HubLifecycleMessage;

/// Short read timeout used for the control socket in Single mode. Long
/// enough that the kernel doesn't churn on the syscall when nothing is
/// in flight; short enough that `poll_receive` returns promptly when
/// the control fd has no data.
pub const SINGLE_MODE_READ_TIMEOUT: Duration = Duration::from_millis(1);

/// Read timeout for the Multi-mode control reader thread. Same
/// rationale as `TCP_READER_TIMEOUT` in reader.rs: long enough that
/// the syscall doesn't churn, short enough to wake up periodically to
/// observe the shutdown flag.
pub const MULTI_MODE_READ_TIMEOUT: Duration = Duration::from_millis(200);

/// Maximum payload size we will accept on the control connection. EOT
/// frames are <100 bytes; cap at 4 KiB to bound any worst-case allocation.
pub const MAX_CONTROL_FRAME_BYTES: usize = 4096;

/// Bounded retry budget for `TcpStream::connect` on the control
/// connection. The lower-sorted peer must have bound + started
/// listening before the higher-sorted peer's connect succeeds; both
/// pass the runner's ready barrier near-simultaneously, so we need a
/// short retry window for the race.
pub const CONTROL_CONNECT_BUDGET: Duration = Duration::from_secs(30);

/// Maximum time to wait for a server-side accept on the control
/// listener. Same scale as `CONTROL_CONNECT_BUDGET`.
pub const CONTROL_ACCEPT_BUDGET: Duration = Duration::from_secs(30);

/// Maximum time to wait for the reader thread to join during teardown.
pub const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Tag byte distinguishing the kinds of control frame on the wire.
/// `bye` is sent by both sides during disconnect to signal "I am done
/// with this control channel; you may close." Receivers treat it as
/// EOF for drain purposes.
const FRAME_TAG_EOT: u8 = 0x01;
const FRAME_TAG_BYE: u8 = 0x02;

/// Per-peer control wiring derived from `--control-base-port` + the
/// `--peers` map. See `main.rs::derive_control_endpoints`.
#[derive(Debug, Clone)]
pub struct ControlPeerEndpoint {
    pub peer_name: String,
    /// For `Role::Server` this is informational. For `Role::Client`
    /// this is the peer's listen addr we dial.
    pub peer_addr: SocketAddr,
    pub role: ControlRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRole {
    /// We are the server side (we accept the inbound connection).
    Server,
    /// We are the client side (we dial the peer).
    Client,
}

/// One peer's control connection: a single bidirectional `TcpStream`.
///
/// In Single mode the variant owns this directly and polls it via
/// `try_recv_frame`. In Multi mode the variant hands the read side off
/// to a dedicated reader thread via `take_read_clone`; the write side
/// stays on the variant for outbound EOT sends.
pub struct ControlPeer {
    /// Peer's runner name (used for diagnostics and for the server-side
    /// to look up which peer just connected; not currently used on the
    /// wire).
    pub peer_name: String,
    /// Peer's control endpoint (server: peer's accepted addr; client:
    /// peer's connect target).
    pub peer_addr: SocketAddr,
    /// Write-side handle. Stays in blocking mode (writes never need to
    /// be polled; EOT frames are tiny so write_all never realistically
    /// blocks).
    write_stream: TcpStream,
    /// Read-side handle. In Single mode held here and polled inline
    /// via `try_recv_frame`. In Multi mode `take_read_clone` extracts
    /// it for the reader thread.
    read_stream: Option<TcpStream>,
    /// Accumulating buffer for partial reads (Single mode only).
    read_buf: Vec<u8>,
}

impl ControlPeer {
    /// Build a `ControlPeer` from an established `TcpStream`.
    ///
    /// `TCP_NODELAY` is set, the read clone is given
    /// `SINGLE_MODE_READ_TIMEOUT` so it returns `WouldBlock` /
    /// `TimedOut` promptly when no data is in flight. Multi mode then
    /// overrides this timeout before handing the clone to its reader
    /// thread.
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
    /// After this returns `Some(stream)`, `try_recv_frame` will
    /// return `Ok(None)` (the variant no longer owns the read side).
    pub fn take_read_clone(&mut self) -> Option<TcpStream> {
        self.read_stream.take()
    }

    /// Try to read one complete length-prefixed control frame
    /// (Single mode polling).
    ///
    /// Returns `Ok(Some(frame))` when a complete frame is buffered.
    /// Returns `Ok(None)` when no complete frame is available yet
    /// (no data, partial data, or `SINGLE_MODE_READ_TIMEOUT` elapsed
    /// without bytes).
    /// Returns `Err` for fatal per-peer errors (`ConnectionAborted`,
    /// `ConnectionReset`, EOF, malformed framing); the caller should
    /// drop the peer.
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
                // No data within SINGLE_MODE_READ_TIMEOUT. Fine; try
                // to extract from whatever we may already have buffered
                // (e.g. one full frame from a previous pass).
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

    /// Send a `bye` marker so the peer knows we are done. Errors are
    /// swallowed (the peer may have already closed).
    pub fn send_bye(&mut self) {
        let _ = self.send_frame(&encode_bye());
    }

    /// Half-close the write side. Following `send_bye`, the peer's
    /// `read_exact` will return EOF after consuming any in-flight
    /// frames.
    pub fn shutdown_write(&self) {
        let _ = self.write_stream.shutdown(Shutdown::Write);
    }

    /// Drain the read side until the peer closes or `deadline` elapses.
    /// Frames that arrive during the drain are decoded and pushed to
    /// `out` so the caller can still apply them (e.g. a last EOT that
    /// arrived after our own `bye`).
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
                    // Nothing for now; small sleep to avoid spinning.
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => {
                    // Peer closed or error; nothing more to drain.
                    return frames;
                }
            }
        }
    }

    /// Close the underlying stream.
    pub fn shutdown(&self) {
        let _ = self.write_stream.shutdown(Shutdown::Both);
    }
}

/// A decoded control frame.
#[derive(Debug, Clone)]
pub enum ControlFrame {
    /// An EOT marker. Same `(writer, eot_id)` payload as data-path EOTs.
    Eot { writer: String, eot_id: u64 },
    /// A "bye" marker indicating the peer is done sending and about to
    /// close. Receivers treat this as logical EOF on the control
    /// channel.
    Bye,
}

/// Encode an EOT control frame.
///
/// Layout: `[tag = 0x01][protocol-level EOT encoding]`. The inner
/// payload reuses `protocol::encode_eot` (the same `(writer, eot_id)`
/// shape carried over multicast/TCP today) so the variant has a
/// single source of truth for EOT serialisation.
pub fn encode_eot_frame(writer: &str, eot_id: u64) -> Vec<u8> {
    let inner = protocol::encode_eot(writer, eot_id);
    let mut out = Vec::with_capacity(1 + inner.len());
    out.push(FRAME_TAG_EOT);
    out.extend_from_slice(&inner);
    out
}

/// Encode a `bye` control frame.
pub fn encode_bye() -> Vec<u8> {
    vec![FRAME_TAG_BYE]
}

/// Decode a single control frame from `payload` (the bytes after the
/// 4-byte length prefix).
pub fn decode_control_frame(payload: &[u8]) -> Result<ControlFrame> {
    if payload.is_empty() {
        anyhow::bail!("empty control frame");
    }
    match payload[0] {
        FRAME_TAG_EOT => {
            // The inner payload is the standard EOT wire format.
            let inner = &payload[1..];
            match protocol::decode_frame(inner) {
                Ok(Frame::Eot { writer, eot_id }) => Ok(ControlFrame::Eot { writer, eot_id }),
                Ok(Frame::Data(_)) => {
                    anyhow::bail!("control frame tagged EOT but payload decoded as Data");
                }
                Err(e) => Err(e).context("decode inner EOT payload of control frame"),
            }
        }
        FRAME_TAG_BYE => Ok(ControlFrame::Bye),
        other => anyhow::bail!("unknown control frame tag: 0x{:02X}", other),
    }
}

/// Try to extract one complete frame from a partial-read buffer. Drains
/// the prefix + payload on success; leaves the buffer alone otherwise.
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

/// Spawn a Multi-mode reader thread for one control peer. The thread
/// reads length-prefixed frames in a blocking loop with
/// `MULTI_MODE_READ_TIMEOUT` and pushes decoded EOT markers onto the
/// existing T14.16 lifecycle channel.
pub fn spawn_control_reader(
    mut read_stream: TcpStream,
    peer_label: String,
    lifecycle_tx: Sender<HubLifecycleMessage>,
    shutdown: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    read_stream
        .set_read_timeout(Some(MULTI_MODE_READ_TIMEOUT))
        .with_context(|| format!("set read timeout for control reader {peer_label}"))?;
    let label = peer_label.clone();
    let handle = thread::Builder::new()
        .name(format!("hybrid-control-reader-{peer_label}"))
        .spawn(move || control_reader_loop(&mut read_stream, label, lifecycle_tx, shutdown))
        .with_context(|| format!("spawn control reader thread {peer_label}"))?;
    Ok(handle)
}

fn control_reader_loop(
    stream: &mut TcpStream,
    label: String,
    lifecycle_tx: Sender<HubLifecycleMessage>,
    shutdown: Arc<AtomicBool>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut tmp = [0u8; 1024];
    while !shutdown.load(Ordering::SeqCst) {
        match stream.read(&mut tmp) {
            Ok(0) => {
                // Clean EOF -- peer closed the control connection.
                if !shutdown.load(Ordering::SeqCst) {
                    eprintln!(
                        "[variant-hybrid] control reader {label}: peer closed (EOF); thread exits"
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
                eprintln!("[variant-hybrid] control reader {label}: read error: {e}; thread exits");
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
                    "[variant-hybrid] control reader {label}: invalid frame_len {frame_len}; thread exits"
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
                    // Unbounded send -- never blocks, never drops.
                    let _ = lifecycle_tx.send(HubLifecycleMessage::Eot { writer, eot_id });
                }
                Ok(ControlFrame::Bye) => {
                    // Peer signalled it is done sending. Continue
                    // reading until EOF in case more frames are in
                    // flight after the bye.
                }
                Err(e) => {
                    eprintln!(
                        "[variant-hybrid] control reader {label}: decode error: {e:#}; thread exits"
                    );
                    return;
                }
            }
        }
    }
}

/// Listen for one inbound control connection from a single peer, with
/// a bounded deadline. Returns the accepted `TcpStream` on success.
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
/// `TimedOut`/`WouldBlock`. Mirrors the data-path TCP connect retry
/// (see `tcp.rs::connect_with_retry`) so both runners can race past
/// the ready barrier without one's connect failing before the other's
/// listener is bound.
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
    fn extract_one_frame_drains_buffer_on_success() {
        let mut buf: Vec<u8> = Vec::new();
        let eot = encode_eot_frame("bob", 1);
        let mut framed: Vec<u8> = Vec::new();
        framed.extend_from_slice(&(eot.len() as u32).to_be_bytes());
        framed.extend_from_slice(&eot);
        buf.extend_from_slice(&framed);

        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
        let frame = extract_one_frame(&mut buf, addr)
            .expect("must extract")
            .expect("frame present");
        match frame {
            ControlFrame::Eot { writer, eot_id } => {
                assert_eq!(writer, "bob");
                assert_eq!(eot_id, 1);
            }
            other => panic!("expected Eot, got {other:?}"),
        }
        assert!(buf.is_empty(), "buffer must be drained after extraction");
    }

    #[test]
    fn extract_one_frame_partial_data_returns_none() {
        let mut buf: Vec<u8> = vec![0, 0];
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
        let result = extract_one_frame(&mut buf, addr).expect("partial must be Ok(None)");
        assert!(result.is_none());
        assert_eq!(buf.len(), 2, "buffer must be preserved on Ok(None)");
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
    /// frame from client to server, and read it back via `try_recv_frame`.
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

        // Server reads.
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
}
