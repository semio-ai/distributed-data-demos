/// TCP transport for QoS 3-4 (reliable-ordered, reliable-TCP).
///
/// One TCP connection per peer. Uses length-prefix framing from the protocol
/// module. `TCP_NODELAY` is set on all connections to minimize latency.
///
/// ## Truly-blocking writes, polled reads via `SO_RCVTIMEO`
///
/// Each peer's underlying `TcpStream` is split via `try_clone` into a write
/// handle and a read handle. The socket itself stays in **blocking mode**
/// (we never call `set_nonblocking(true)` on it), so writes through the
/// write handle truly block under kernel back-pressure — the back-pressure
/// signal we want to measure for this benchmark. Back-pressure is part of
/// TCP's reliability story; bypassing it (e.g. with non-blocking writes
/// plus app-side retry-and-drop) would distort the comparison against
/// `custom-udp`'s NACK approach. See `CUSTOM.md`.
///
/// To make reads pollable without flipping the socket-wide `FIONBIO` flag
/// (which on Windows would silently un-block the write side too — see
/// CUSTOM.md), we install a short `SO_RCVTIMEO` on the read handle via
/// `set_read_timeout(READ_POLL_TIMEOUT)`. Reads return `WouldBlock`
/// (Unix) or `TimedOut` (Windows) when no data is in flight, allowing
/// the protocol loop to interleave UDP and other peers' reads. Writes
/// remain blocking.
///
/// `write_with_retry` is kept as a defence-in-depth safety net for the
/// case where the socket is somehow non-blocking (it never is in normal
/// operation, but the retry budget is large — 10 s — so we behave like a
/// true blocking write for any realistic transient).
///
/// ## Per-peer fault tolerance on read AND write
///
/// At cross-machine high throughput, individual peer streams may return
/// `ConnectionAborted` / `ConnectionReset` or unexpected EOF — typically as
/// a downstream effect of one side bailing on a `WouldBlock`. The read
/// loop and the broadcast loop must NOT propagate such errors up: each
/// logs a single warning, drops that peer's stream, and continues so the
/// spawn as a whole still completes.
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Wall-clock budget for the TCP write `WouldBlock` retry loop. Used as
/// a safety net in case the socket *is* somehow non-blocking — under
/// normal operation the socket stays in blocking mode and `write` blocks
/// without ever returning `WouldBlock`. Set very large so we behave like
/// a true blocking write for any realistic transient back-pressure but
/// still escape if something pathological happens.
const TCP_WRITE_RETRY_BUDGET: Duration = Duration::from_secs(10);

/// Read timeout for per-peer TCP read polling. Short enough to keep the
/// poll loop responsive (so UDP and other peers' reads aren't starved),
/// long enough that we don't churn the syscall when nothing is in
/// flight. The poll loop iterates over all peers per tick; each peer's
/// read can spend up to this long waiting before yielding back.
const READ_POLL_TIMEOUT: Duration = Duration::from_millis(1);

/// A single TCP peer connection.
///
/// Holds two handles to the same underlying blocking socket. `write_stream`
/// is used for `write_all`, which truly blocks under kernel back-pressure
/// when the send buffer fills. `read_stream` carries a short
/// `SO_RCVTIMEO` (`READ_POLL_TIMEOUT`) so `try_recv_framed` returns
/// `WouldBlock`/`TimedOut` quickly when no data is in flight, without
/// flipping the socket-wide non-blocking flag (which would defeat the
/// blocking writes — see module docs). Both handles refer to the same
/// kernel socket; closing or shutting down one tears down both.
pub struct TcpPeer {
    pub addr: SocketAddr,
    write_stream: TcpStream,
    read_stream: TcpStream,
    /// Buffer for accumulating partial reads.
    read_buf: Vec<u8>,
}

impl TcpPeer {
    /// Build a `TcpPeer` from an existing connection.
    ///
    /// The input stream is cloned via `try_clone` to obtain two handles to
    /// the same blocking socket. The read handle is given a short
    /// `SO_RCVTIMEO` so reads can be polled; writes inherit the socket's
    /// blocking behaviour and apply true back-pressure on send-buffer
    /// fill. See module docs.
    fn from_stream(stream: TcpStream, addr: SocketAddr) -> Result<Self> {
        // Clone gives us two independent handles to the same socket. The
        // socket itself stays in blocking mode (we never call
        // `set_nonblocking(true)`), so writes through the write handle
        // truly block under kernel back-pressure — the back-pressure
        // signal we want to measure for this benchmark.
        //
        // To make reads pollable without flipping the socket-wide
        // `FIONBIO` flag (which on Windows would silently un-block the
        // write side too), we use `set_read_timeout(SHORT)` on the read
        // handle: reads return `WouldBlock` / `TimedOut` if no data
        // arrives within the timeout. Writes remain blocking. See module
        // docs.
        let read_stream = stream
            .try_clone()
            .with_context(|| format!("failed to clone TCP stream for {addr}"))?;
        let write_stream = stream;

        read_stream
            .set_read_timeout(Some(READ_POLL_TIMEOUT))
            .with_context(|| format!("failed to set TCP read timeout for {addr}"))?;

        Ok(Self {
            addr,
            write_stream,
            read_stream,
            read_buf: Vec::new(),
        })
    }

    /// Write a length-prefixed framed message to this peer.
    ///
    /// Truly blocking under back-pressure (the socket is in blocking mode
    /// and `SO_RCVTIMEO` does not affect writes). The bounded retry in
    /// `write_with_retry` is a defence-in-depth safety net for the case
    /// where the socket is somehow non-blocking — see module docs.
    pub fn send_framed(&mut self, data: &[u8]) -> Result<()> {
        write_with_retry(&mut self.write_stream, data)
            .with_context(|| format!("failed to write to TCP peer {}", self.addr))
    }

    /// Try to read the next complete framed message (non-blocking).
    ///
    /// Returns `Ok(Some(msg))` when a complete frame is available.
    /// Returns `Ok(None)` when no complete frame is available yet (no data,
    /// or only a partial frame buffered).
    /// Returns `Err` for *fatal* per-peer errors (`ConnectionAborted`,
    /// `ConnectionReset`, unexpected EOF, malformed framing) — the caller
    /// is expected to drop this peer and continue with the others rather
    /// than failing the whole spawn.
    pub fn try_recv_framed(&mut self) -> Result<Option<Vec<u8>>> {
        // Read whatever is available into the buffer.
        let mut tmp = [0u8; 65536];
        match self.read_stream.read(&mut tmp) {
            Ok(0) => {
                // Clean EOF from the peer's side. Not necessarily wrong
                // (e.g. peer finished and closed) but treat as fatal-for-
                // this-peer so the caller drops the stream.
                anyhow::bail!("TCP peer {} closed connection (EOF)", self.addr);
            }
            Ok(n) => {
                self.read_buf.extend_from_slice(&tmp[..n]);
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // No data within `READ_POLL_TIMEOUT`. Not an error —
                // just means the peer didn't have anything to send right
                // now. (`WouldBlock` is what Unix returns when the
                // socket is in non-blocking mode; `TimedOut` is what
                // Windows returns when `SO_RCVTIMEO` elapses on a
                // blocking socket. We accept both.)
            }
            Err(e) => {
                // ConnectionAborted, ConnectionReset, or other I/O error.
                // Caller drops this peer; spawn continues.
                return Err(anyhow::anyhow!(
                    "TCP read error from {}: {} ({:?})",
                    self.addr,
                    e,
                    e.kind()
                ));
            }
        }

        // Try to extract a complete frame: 4-byte length prefix + payload.
        if self.read_buf.len() < 4 {
            return Ok(None);
        }
        let msg_len = u32::from_be_bytes(self.read_buf[0..4].try_into().unwrap()) as usize;
        let total = 4 + msg_len;
        if self.read_buf.len() < total {
            return Ok(None);
        }

        // Extract the message and shrink the buffer.
        let msg = self.read_buf[4..total].to_vec();
        self.read_buf.drain(..total);

        Ok(Some(msg))
    }

    /// Shut down both directions of the underlying socket so the peer
    /// observes a clean teardown.
    fn shutdown(&self) {
        let _ = self.write_stream.shutdown(Shutdown::Both);
    }
}

/// Manages TCP connections to all peers and the local listener.
pub struct TcpTransport {
    listener: TcpListener,
    /// Outbound connections we initiated to peers.
    outbound: Vec<TcpPeer>,
    /// Inbound connections accepted from peers.
    inbound: Vec<TcpPeer>,
}

impl TcpTransport {
    /// Create a TCP listener on the given address.
    pub fn new(listen_addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(listen_addr)
            .with_context(|| format!("failed to bind TCP listener on {}", listen_addr))?;
        listener
            .set_nonblocking(true)
            .context("failed to set TCP listener non-blocking")?;

        Ok(Self {
            listener,
            outbound: Vec::new(),
            inbound: Vec::new(),
        })
    }

    /// Connect to a peer at the given address. Sets `TCP_NODELAY` and
    /// arranges blocking writes plus a short read timeout for polled reads.
    pub fn connect_to_peer(&mut self, addr: SocketAddr) -> Result<()> {
        let stream = TcpStream::connect(addr)
            .with_context(|| format!("failed to connect TCP to peer {}", addr))?;
        // `TcpStream::connect` returns a blocking socket by default, but
        // be explicit so the back-pressure semantics don't depend on
        // upstream defaults.
        stream
            .set_nonblocking(false)
            .context("failed to make outbound TCP stream blocking")?;
        stream
            .set_nodelay(true)
            .context("failed to set TCP_NODELAY on outbound")?;
        let peer = TcpPeer::from_stream(stream, addr)?;
        self.outbound.push(peer);
        Ok(())
    }

    /// Accept any pending inbound connections (non-blocking).
    pub fn accept_pending(&mut self) -> Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    // The listener is non-blocking. Per-platform behaviour
                    // varies on whether `accept` inherits that flag — make
                    // the new per-peer stream blocking explicitly so writes
                    // really do block on back-pressure. `from_stream` then
                    // installs `set_read_timeout` for the read side.
                    stream
                        .set_nonblocking(false)
                        .context("failed to make accepted TCP stream blocking")?;
                    stream
                        .set_nodelay(true)
                        .context("failed to set TCP_NODELAY on inbound")?;
                    let peer = TcpPeer::from_stream(stream, addr)?;
                    self.inbound.push(peer);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    return Err(anyhow::anyhow!("TCP accept error: {}", e));
                }
            }
        }
        Ok(())
    }

    /// Send a framed message to all outbound peers.
    ///
    /// Writes are blocking (see module docs). If a peer's write fails, the
    /// peer is dropped and we continue with the others — same fault-tolerance
    /// rule we apply to reads. The first error encountered is returned only
    /// when ALL peers have been dropped (i.e. there is no longer anyone left
    /// to publish to).
    pub fn broadcast(&mut self, data: &[u8]) -> Result<()> {
        let mut last_err: Option<anyhow::Error> = None;
        let mut keep: Vec<bool> = Vec::with_capacity(self.outbound.len());
        for peer in &mut self.outbound {
            match peer.send_framed(data) {
                Ok(()) => keep.push(true),
                Err(e) => {
                    eprintln!(
                        "warning: dropping TCP outbound peer {} after write error: {:#}",
                        peer.addr, e
                    );
                    peer.shutdown();
                    keep.push(false);
                    last_err = Some(e);
                }
            }
        }
        if !keep.iter().all(|&k| k) {
            let mut idx = 0;
            self.outbound.retain(|_| {
                let k = keep[idx];
                idx += 1;
                k
            });
        }
        if self.outbound.is_empty() && self.inbound.is_empty() {
            if let Some(e) = last_err {
                return Err(e.context("all TCP peers dropped after write errors"));
            }
        }
        Ok(())
    }

    /// Try to receive the next framed message from any peer (inbound or
    /// outbound). Returns `Ok(None)` when no complete message is available.
    ///
    /// Per-peer fatal errors (`ConnectionAborted`, `ConnectionReset`, EOF,
    /// malformed framing) are absorbed at this layer: the offending peer is
    /// dropped from the active set with a single warning, and we move on.
    /// One peer disconnecting must NOT fail the whole spawn — see module
    /// docs.
    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        // Accept any new inbound connections first.
        self.accept_pending()?;

        if let Some(msg) = poll_peer_set(&mut self.inbound, "inbound") {
            return Ok(Some(msg));
        }
        if let Some(msg) = poll_peer_set(&mut self.outbound, "outbound") {
            return Ok(Some(msg));
        }

        Ok(None)
    }

    /// Test-only access to the count of currently-active outbound peers.
    #[cfg(test)]
    pub fn outbound_count(&self) -> usize {
        self.outbound.len()
    }

    /// Close all connections.
    pub fn close(self) -> Result<()> {
        // Streams and the listener are dropped when self goes out of scope.
        for peer in &self.outbound {
            peer.shutdown();
        }
        for peer in &self.inbound {
            peer.shutdown();
        }
        drop(self.listener);
        drop(self.outbound);
        drop(self.inbound);
        Ok(())
    }
}

/// Trait abstracting the per-call `write` so the retry loop can be
/// exercised without a real TCP socket.
trait ByteWrite {
    fn write_once(&mut self, data: &[u8]) -> io::Result<usize>;
}

impl ByteWrite for TcpStream {
    fn write_once(&mut self, data: &[u8]) -> io::Result<usize> {
        Write::write(self, data)
    }
}

/// Write `data` to `writer`, retrying on `WouldBlock` for up to
/// `TCP_WRITE_RETRY_BUDGET`. Yields the thread between attempts to give
/// the kernel a chance to drain the send buffer. Behaves like a blocking
/// `write_all` for the caller.
///
/// Returns `Ok(())` once every byte has been written. Returns `Err` if the
/// budget is exhausted while still hitting `WouldBlock`, or if the write
/// fails with any other error. A budget exhaustion is the back-pressure
/// signal — the caller surfaces it instead of silently dropping data.
fn write_with_retry<W: ByteWrite>(writer: &mut W, data: &[u8]) -> Result<()> {
    let deadline = Instant::now() + TCP_WRITE_RETRY_BUDGET;
    let mut written = 0usize;
    while written < data.len() {
        match writer.write_once(&data[written..]) {
            Ok(0) => {
                return Err(anyhow::anyhow!(
                    "TCP write returned 0 bytes after {} of {} written",
                    written,
                    data.len()
                ));
            }
            Ok(n) => {
                written += n;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "TCP write still WouldBlock after {:?} budget ({} of {} bytes written)",
                        TCP_WRITE_RETRY_BUDGET,
                        written,
                        data.len()
                    ));
                }
                std::thread::yield_now();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // Standard EINTR retry — does not consume the budget.
            }
            Err(e) => return Err(anyhow::anyhow!("TCP write error: {}", e)),
        }
    }
    Ok(())
}

/// Poll every peer in `peers` once. If a peer yields a complete message,
/// return it. If a peer reports a fatal error, log a single warning, mark
/// that peer for removal, and keep polling the rest. Survivors stay in the
/// set.
fn poll_peer_set(peers: &mut Vec<TcpPeer>, label: &str) -> Option<Vec<u8>> {
    let mut keep: Vec<bool> = Vec::with_capacity(peers.len());
    let mut hit: Option<Vec<u8>> = None;

    for peer in peers.iter_mut() {
        if hit.is_some() {
            // Already found a message this pass; stop polling but keep
            // recording the rest as alive so we don't accidentally drop
            // them.
            keep.push(true);
            continue;
        }
        match peer.try_recv_framed() {
            Ok(Some(msg)) => {
                hit = Some(msg);
                keep.push(true);
            }
            Ok(None) => keep.push(true),
            Err(e) => {
                eprintln!(
                    "warning: dropping TCP {label} peer {} after read error: {:#}",
                    peer.addr, e
                );
                peer.shutdown();
                keep.push(false);
            }
        }
    }

    if !keep.iter().all(|&k| k) {
        let mut idx = 0;
        peers.retain(|_| {
            let k = keep[idx];
            idx += 1;
            k
        });
    }

    hit
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Spin up two ephemeral TCP listeners on localhost, connect to both,
    /// then forcibly reset one of the inbound peer connections and verify
    /// the read loop drops only the broken peer (the other survives).
    #[test]
    fn try_recv_drops_one_peer_on_connection_error_keeps_other() {
        // Listener that *we* control — the variant under test connects to
        // it like a normal peer.
        let listener_a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr_a = listener_a.local_addr().unwrap();

        let listener_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr_b = listener_b.local_addr().unwrap();

        // The TcpTransport under test owns its own listener (which we won't
        // use in this test) and dials `listener_a` and `listener_b`.
        let mut transport = TcpTransport::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        transport.connect_to_peer(addr_a).unwrap();
        transport.connect_to_peer(addr_b).unwrap();

        // Accept the inbound side of each connection on the test listeners.
        let (peer_a_stream, _) = listener_a.accept().unwrap();
        let (peer_b_stream, _) = listener_b.accept().unwrap();

        // Tear down peer A: shutdown both halves and drop. On stable Rust
        // we don't have access to `set_linger` (unstable `tcp_linger`
        // feature) to force an RST, but a clean shutdown produces an EOF
        // on the variant's read side, which our code also treats as
        // fatal-for-this-peer (see `try_recv_framed`'s `Ok(0)` arm). Either
        // failure mode (RST → ConnectionReset, or FIN → EOF) exercises the
        // same fault-tolerance branch in `poll_peer_set`.
        let _ = peer_a_stream.shutdown(Shutdown::Both);
        drop(peer_a_stream);

        // Give the kernel a moment to deliver the RST/EOF to the variant's
        // read side. Poll up to ~500ms.
        let start = std::time::Instant::now();
        let mut dropped = false;
        while start.elapsed() < std::time::Duration::from_millis(500) {
            // Drive the read loop. Any complete message would be `Some`,
            // but in this test there are no writes, so we just wait for
            // the dropped-peer warning to take effect.
            let _ = transport.try_recv().expect("try_recv must not fail");
            if transport.outbound_count() == 1 {
                dropped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            dropped,
            "expected the broken peer to be dropped within 500ms; outbound_count={}",
            transport.outbound_count()
        );

        // The surviving peer (B) is still in the active set.
        assert_eq!(
            transport.outbound_count(),
            1,
            "exactly one outbound peer should remain after the other was dropped"
        );

        // Keep peer_b_stream alive until the end so it doesn't also get
        // torn down before the assertion.
        drop(peer_b_stream);
    }

    /// The read loop must keep polling and not return an error to the
    /// caller when a single peer goes bad — even when there is also a
    /// healthy peer that has nothing to deliver. Regression-protect the
    /// "do not propagate per-peer errors" rule.
    #[test]
    fn try_recv_returns_ok_when_a_peer_errors() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut transport = TcpTransport::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        transport.connect_to_peer(addr).unwrap();

        let (peer_stream, _) = listener.accept().unwrap();
        // Clean shutdown produces EOF on the variant's read side; our code
        // treats that as fatal-for-this-peer (see `try_recv_framed`'s
        // `Ok(0)` arm). `set_linger` would force an RST instead but is
        // gated behind the unstable `tcp_linger` feature on stable Rust.
        let _ = peer_stream.shutdown(Shutdown::Both);
        drop(peer_stream);

        // Even though the peer is gone, try_recv must return Ok (with
        // None or Some) — never propagate the per-peer error.
        let start = std::time::Instant::now();
        loop {
            let res = transport.try_recv();
            assert!(
                res.is_ok(),
                "try_recv must not surface per-peer errors, got: {:?}",
                res.as_ref().err()
            );
            if transport.outbound_count() == 0 {
                break;
            }
            if start.elapsed() > std::time::Duration::from_millis(500) {
                panic!("peer was never dropped from the set");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Test shim: returns `WouldBlock` for the first N attempts, then
    /// reports a successful write of all bytes. Counts attempts so we can
    /// assert the retry actually happened and wasn't infinite.
    struct FlakyWriter {
        wouldblock_remaining: u32,
        attempts: u32,
        last_payload_len: usize,
    }

    impl ByteWrite for FlakyWriter {
        fn write_once(&mut self, data: &[u8]) -> io::Result<usize> {
            self.attempts += 1;
            if self.wouldblock_remaining > 0 {
                self.wouldblock_remaining -= 1;
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                self.last_payload_len = data.len();
                Ok(data.len())
            }
        }
    }

    /// Always returns `WouldBlock`. Used to verify the retry loop bails
    /// when the budget is exhausted instead of spinning forever.
    struct AlwaysBlockWriter {
        attempts: u32,
    }

    impl ByteWrite for AlwaysBlockWriter {
        fn write_once(&mut self, _data: &[u8]) -> io::Result<usize> {
            self.attempts += 1;
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    #[test]
    fn write_with_retry_recovers_after_one_wouldblock() {
        let mut writer = FlakyWriter {
            wouldblock_remaining: 1,
            attempts: 0,
            last_payload_len: 0,
        };
        write_with_retry(&mut writer, b"hello")
            .expect("retry path must yield Ok after one WouldBlock");
        assert!(
            (2..=10_000).contains(&writer.attempts),
            "expected a small finite retry count, got {}",
            writer.attempts
        );
        assert_eq!(writer.last_payload_len, 5);
    }

    #[test]
    fn write_with_retry_bails_after_budget_exhausted() {
        let mut writer = AlwaysBlockWriter { attempts: 0 };
        let err = write_with_retry(&mut writer, b"x")
            .expect_err("retry loop must surface error when budget is exhausted");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("WouldBlock"),
            "error should mention WouldBlock, got: {msg}"
        );
        assert!(writer.attempts >= 1);
    }

    /// Partial writes (a `write` returning fewer bytes than asked) must be
    /// resumed at the next offset, not retried from the start.
    #[test]
    fn write_with_retry_handles_partial_writes() {
        struct PartialWriter {
            written: Vec<u8>,
        }
        impl ByteWrite for PartialWriter {
            fn write_once(&mut self, data: &[u8]) -> io::Result<usize> {
                // Write at most 1 byte at a time.
                let n = data.len().min(1);
                self.written.extend_from_slice(&data[..n]);
                Ok(n)
            }
        }
        let mut w = PartialWriter {
            written: Vec::new(),
        };
        write_with_retry(&mut w, b"abcdef").unwrap();
        assert_eq!(&w.written, b"abcdef");
    }
}
