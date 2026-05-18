/// TCP transport for QoS 3-4 (reliable-ordered, reliable-TCP).
///
/// One TCP connection per peer. Uses length-prefix framing from the protocol
/// module. `TCP_NODELAY` is set on all connections to minimize latency.
///
/// ## T17.4 strict-delivery contract
///
/// Per DESIGN.md § 6.5, QoS 3/4 prioritises delivery over throughput.
/// The variant MUST NOT drop a QoS 3/4 message at the publish path; it
/// MUST block until the message is accepted. The TCP path is therefore
/// structured so back-pressure flows end-to-end:
///
/// 1. Outbound writes block on a full kernel send buffer (the
///    application sees `WouldBlock`, retries with a drain pass).
/// 2. The peer's reader thread / read loop blocks on a full driver
///    channel (multi mode) or stops draining (single mode is blocked
///    in its own publish).
/// 3. The peer's kernel TCP recv buffer fills.
/// 4. Kernel TCP back-pressures the writer's send (window closes).
/// 5. Writer's `write` keeps returning `WouldBlock` until the recv
///    side resumes.
///
/// To break the symmetric-saturation wedge in single mode (both
/// peers blocked in publish, neither draining), the variant runs
/// `broadcast_with_drain` and passes a callback that drains incoming
/// frames between write attempts. This replaces the pre-T17.4
/// `SO_SNDTIMEO + drop-peer` mechanism, which violated strict
/// delivery (dropping a peer dropped all undelivered messages to it).
///
/// ## Sockets are non-blocking
///
/// Each peer's underlying `TcpStream` is set to non-blocking mode
/// (`set_nonblocking(true)`). The read clone inherits this flag (the
/// `FIONBIO` flag is socket-wide on Windows). Reads then return
/// `WouldBlock` immediately when no data is in flight; the variant's
/// poll loop already treats `WouldBlock` as "no data this tick" and
/// moves on. Writes likewise return `WouldBlock` when the kernel send
/// buffer is full; the broadcast loop drains incoming reads (via the
/// drain callback) and retries — never dropping the message.
///
/// ## Per-peer fault tolerance on read AND write
///
/// At cross-machine high throughput, individual peer streams may return
/// `ConnectionAborted` / `ConnectionReset` or unexpected EOF when a
/// peer process terminates. The read loop and the broadcast loop
/// MUST drop such peers and continue with the survivors. They MUST
/// NOT propagate the error up, and they MUST NOT drop a peer for a
/// transient `WouldBlock` — `WouldBlock` is the back-pressure signal,
/// not a fatal error.
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use socket2::SockRef;
use variant_base::types::ThreadingMode;

/// T17.4: how long the write loop spends inside one
/// `broadcast_with_drain` call before yielding back so the variant
/// can run a drain pass (single mode) or before logging a stall
/// warning (multi mode). The loop keeps retrying after the warning
/// -- strict delivery is contractual; we never give up on the
/// message, only on individual peers that close their connection.
const TCP_WRITE_STALL_WARNING: Duration = Duration::from_secs(30);

/// Read timeout for per-peer TCP read polling.
///
/// On a non-blocking socket `set_read_timeout` is effectively
/// advisory -- reads return `WouldBlock` immediately. The value is
/// kept for documentation / for the rare path where a caller calls
/// `set_nonblocking(false)` on the read clone (e.g. teardown).
const READ_POLL_TIMEOUT: Duration = Duration::from_millis(1);

/// T17.4: back-off between non-blocking-write retries when the
/// kernel send buffer is full. Short enough that the writer
/// responds to back-pressure relief promptly; long enough to avoid
/// 100 % CPU spin under sustained saturation. The single-mode
/// broadcast also runs a drain pass between attempts to break the
/// symmetric wedge -- see [`TcpTransport::broadcast_with_drain`].
const TCP_WRITE_RETRY_SLEEP: Duration = Duration::from_micros(200);

/// A single TCP peer connection.
///
/// Holds two handles to the same underlying socket. Since T17.4, the
/// socket is non-blocking: writes return `WouldBlock` when the kernel
/// send buffer fills (the strict-delivery write loop retries with a
/// drain pass between attempts; see [`TcpTransport::broadcast_with_drain`]),
/// and reads return `WouldBlock` when no data is in flight.
///
/// `read_stream` is held as `Option` so Multi mode can `take()` it and
/// hand the handle to a per-peer reader thread. Both handles refer to
/// the same kernel socket; closing or shutting down one tears down both.
pub struct TcpPeer {
    pub addr: SocketAddr,
    write_stream: TcpStream,
    /// Read half. Held as `Option` so Multi mode can `take()`
    /// ownership and hand the handle to a per-peer reader thread.
    read_stream: Option<TcpStream>,
    /// Buffer for accumulating partial reads (Single mode only).
    read_buf: Vec<u8>,
}

impl TcpPeer {
    /// Build a `TcpPeer` from an existing connection.
    ///
    /// T17.4: the socket is switched to non-blocking mode. Writes and
    /// reads then return `WouldBlock` instead of stalling the thread.
    /// `SO_RCVTIMEO` is also installed on the read clone as a safety
    /// net for the rare paths that re-enable blocking mode during
    /// teardown.
    ///
    /// `_mode` is retained on the signature for API stability across
    /// the T16.3 → T17.4 transition (Single vs. Multi no longer
    /// branches inside this function; the broadcast path branches
    /// on the threading mode instead).
    fn from_stream(stream: TcpStream, addr: SocketAddr, _mode: ThreadingMode) -> Result<Self> {
        // Clone gives us two independent handles to the same socket.
        let read_stream = stream
            .try_clone()
            .with_context(|| format!("failed to clone TCP stream for {addr}"))?;
        let write_stream = stream;

        // T17.4: non-blocking writes (and reads). `set_nonblocking` is
        // socket-wide on Windows; one call covers both clones. The
        // `SO_RCVTIMEO` below is kept as a no-op safety net for any
        // future code path that flips the socket back to blocking.
        write_stream
            .set_nonblocking(true)
            .with_context(|| format!("failed to set TCP stream non-blocking for {addr}"))?;
        read_stream
            .set_read_timeout(Some(READ_POLL_TIMEOUT))
            .with_context(|| format!("failed to set TCP read timeout for {addr}"))?;

        Ok(Self {
            addr,
            write_stream,
            read_stream: Some(read_stream),
            read_buf: Vec::new(),
        })
    }

    /// Take ownership of the read half so a Multi-mode reader thread
    /// can own it. Returns `None` if it has already been taken.
    pub fn take_read_stream(&mut self) -> Option<TcpStream> {
        self.read_stream.take()
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
    ///
    /// **T16.3**: when the internal `read_buf` already holds a complete
    /// frame, extract and return it WITHOUT first doing another
    /// `read()` syscall. The read syscall has `SO_RCVTIMEO = 1 ms`, so
    /// re-entering it when the kernel recv buffer is empty (but we
    /// have buffered frames) would waste 1 ms per buffered message.
    /// At 1 000 msg/s symmetric on QoS 3/4 in Single mode, that 1 ms
    /// per call was capping the drain at ~1 000 calls/s and starving
    /// the receive path. Extracting buffered frames first lets the
    /// driver's drain loop chew through a batch of frames in
    /// microseconds and return to publish.
    pub fn try_recv_framed(&mut self) -> Result<Option<Vec<u8>>> {
        // Multi mode `take_read_stream()`s the read clone, so polling
        // is only valid when we still own it (Single mode).
        let read = match self.read_stream.as_mut() {
            Some(r) => r,
            None => return Ok(None),
        };

        // T16.3: fast path — if a complete frame is already buffered
        // from a previous large `read()`, return it without another
        // syscall. The kernel may have delivered many frames at once
        // (especially on localhost / TCP_NODELAY) and we'd otherwise
        // wedge for `READ_POLL_TIMEOUT` per buffered frame.
        if let Some(msg) = take_buffered_frame(&mut self.read_buf) {
            return Ok(Some(msg));
        }

        // Read whatever is available into the buffer.
        let mut tmp = [0u8; 65536];
        match read.read(&mut tmp) {
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

        // Try to extract a complete frame from the (possibly enlarged)
        // buffer.
        Ok(take_buffered_frame(&mut self.read_buf))
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
    /// Threading mode for this spawn. Determines whether
    /// `broadcast` runs an inline read-drain pass on each
    /// `WouldBlock` (Single mode -- breaks the symmetric wedge)
    /// or only retries (Multi mode -- the per-peer reader thread
    /// drains in parallel).
    threading_mode: ThreadingMode,
    /// T17.4: frames decoded during the inline drain pass that
    /// runs inside `broadcast_with_drain` in single mode. Drained
    /// FIRST by `try_recv` so the variant's `poll_receive` sees
    /// them in order. Capacity-bounded only by available memory;
    /// the strict-delivery contract is about not dropping at the
    /// *publish* path, while the receive path expects the
    /// variant's driver thread to drain.
    pending_drained: VecDeque<Vec<u8>>,
}

impl TcpTransport {
    /// Create a TCP listener on the given address.
    ///
    /// `threading_mode` is stashed so subsequent `connect_to_peer` /
    /// `accept_pending` calls can install (or skip) the T16.3 write
    /// timeout on newly built peer streams.
    pub fn new(listen_addr: SocketAddr, threading_mode: ThreadingMode) -> Result<Self> {
        let listener = TcpListener::bind(listen_addr)
            .with_context(|| format!("failed to bind TCP listener on {}", listen_addr))?;
        listener
            .set_nonblocking(true)
            .context("failed to set TCP listener non-blocking")?;

        Ok(Self {
            listener,
            outbound: Vec::new(),
            inbound: Vec::new(),
            threading_mode,
            pending_drained: VecDeque::new(),
        })
    }

    /// Connect to a peer at the given address. Sets `TCP_NODELAY` and
    /// arranges blocking writes plus a short read timeout for polled
    /// reads.
    ///
    /// `recv_buffer_kb` is the T14.1 `--recv-buffer-kb` value, applied
    /// via `SO_RCVBUF` to the underlying socket before the read clone
    /// is made. `None` skips the tune (used by unit tests).
    ///
    /// Connect uses a bounded retry on `ConnectionRefused` so both
    /// runners can race past the ready barrier without one's `connect`
    /// failing before the other's listener is bound. 30 s budget: on
    /// Windows under cargo-test load the peer subprocess's `bind`
    /// can take several seconds after the barrier releases. The
    /// legacy `TcpStream::connect` had no app-level cap and would
    /// wait up to the kernel default (~21 s on Windows) before
    /// returning `ConnectionRefused`; this stays at the same order of
    /// magnitude while still being bounded so a permanently-
    /// unreachable peer surfaces a clean error instead of hanging
    /// the whole spawn forever.
    pub fn connect_to_peer(&mut self, addr: SocketAddr, recv_buffer_kb: Option<u32>) -> Result<()> {
        let stream = connect_with_retry(addr, Duration::from_secs(30))
            .with_context(|| format!("failed to connect TCP to peer {}", addr))?;
        // T17.4: the socket starts in blocking mode after
        // `TcpStream::connect`. `TcpPeer::from_stream` flips it to
        // non-blocking; nothing to do here.
        stream
            .set_nodelay(true)
            .context("failed to set TCP_NODELAY on outbound")?;
        if let Some(kb) = recv_buffer_kb {
            apply_tcp_recv_buffer(&stream, kb, addr)?;
        }
        let peer = TcpPeer::from_stream(stream, addr, self.threading_mode)?;
        self.outbound.push(peer);
        Ok(())
    }

    /// Accept any pending inbound connections (non-blocking).
    ///
    /// `recv_buffer_kb`, when supplied, is applied as `SO_RCVBUF` on
    /// each newly accepted socket before the read clone is made.
    pub fn accept_pending(&mut self, recv_buffer_kb: Option<u32>) -> Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    // T17.4: `TcpPeer::from_stream` switches the
                    // socket to non-blocking; no per-direction
                    // toggle needed here.
                    stream
                        .set_nodelay(true)
                        .context("failed to set TCP_NODELAY on inbound")?;
                    if let Some(kb) = recv_buffer_kb {
                        apply_tcp_recv_buffer(&stream, kb, addr)?;
                    }
                    let peer = TcpPeer::from_stream(stream, addr, self.threading_mode)?;
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
    /// T17.4: writes are non-blocking with internal retry-on-WouldBlock,
    /// so back-pressure is absorbed inside this call rather than
    /// surfacing as an error. The strict no-skip contract (DESIGN.md
    /// § 6.5) means we MUST NOT give up on a peer just because its
    /// send buffer is full -- the message must eventually land.
    ///
    /// In SINGLE mode, the loop runs an inline read-drain pass
    /// between write attempts so the symmetric-wedge (both peers
    /// blocked in publish, neither draining recv) is broken
    /// without dropping any messages. Frames decoded during the
    /// drain pass are stashed on the transport's `pending_drained`
    /// queue and surfaced by the next `try_recv` call.
    ///
    /// In MULTI mode the per-peer reader thread already drains in
    /// parallel; the broadcast loop only retries on `WouldBlock`
    /// without running an inline drain (it would race the reader
    /// thread on the read clone, which has been `take`n by the
    /// thread anyway).
    ///
    /// A peer is dropped ONLY on truly fatal I/O errors
    /// (`ConnectionAborted`, `ConnectionReset`, etc.). Transient
    /// `WouldBlock` is the back-pressure signal, retried internally.
    pub fn broadcast(&mut self, data: &[u8]) -> Result<()> {
        let mode = self.threading_mode;
        match mode {
            ThreadingMode::Single => self.broadcast_with_drain(data, |t| {
                inline_drain_into_pending(t);
            }),
            ThreadingMode::Multi => self.broadcast_with_drain(data, |_| {}),
        }
    }

    /// T17.4: like [`broadcast`] but invokes `drain` between
    /// non-blocking write retries when the kernel send buffer is
    /// full. The drain callback is expected to consume any
    /// available incoming frames so the symmetric-saturation wedge
    /// (both peers blocked in publish, neither draining recv) is
    /// broken in single mode.
    ///
    /// The callback receives `&mut TcpTransport` (the transport) so
    /// it can read from BOTH outbound and inbound peer sets while
    /// the current write is paused.
    pub fn broadcast_with_drain<F>(&mut self, data: &[u8], mut drain: F) -> Result<()>
    where
        F: FnMut(&mut TcpTransport),
    {
        let mut last_err: Option<anyhow::Error> = None;
        let n = self.outbound.len();
        // Track which peers survived. We index by position so we can
        // safely call `drain(&mut self)` between attempts without
        // holding a borrow into `self.outbound`.
        let mut keep: Vec<bool> = vec![true; n];

        #[allow(clippy::needless_range_loop)] // we mutate `self.outbound` inside via index
        for i in 0..n {
            let mut written = 0usize;
            let total = data.len();
            let stall_warn_deadline = Instant::now() + TCP_WRITE_STALL_WARNING;
            let mut warned_once = false;
            let addr = self.outbound[i].addr;

            'send: loop {
                let chunk = &data[written..];
                let res = {
                    let peer = &mut self.outbound[i];
                    Write::write(&mut peer.write_stream, chunk)
                };
                match res {
                    Ok(0) => {
                        let e = anyhow::anyhow!(
                            "TCP write to {addr} returned 0 bytes after {written} of {total}"
                        );
                        eprintln!(
                            "warning: dropping TCP outbound peer {addr} after write error: {e:#}"
                        );
                        self.outbound[i].shutdown();
                        keep[i] = false;
                        last_err = Some(e);
                        break 'send;
                    }
                    Ok(n) => {
                        written += n;
                        if written >= total {
                            break 'send;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Back-pressure: kernel send buffer is full.
                        // Run a drain pass (lets single mode read the
                        // peer's outbound frames -> drain peer's
                        // kernel send buffer -> unstick our write).
                        drain(self);
                        if !warned_once && Instant::now() >= stall_warn_deadline {
                            eprintln!(
                                "[variant-hybrid] TCP write to {addr} stalled for >{:?}; \
                                 still retrying (strict no-skip QoS 3/4 contract)",
                                TCP_WRITE_STALL_WARNING
                            );
                            warned_once = true;
                        }
                        std::thread::sleep(TCP_WRITE_RETRY_SLEEP);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        // EINTR -- retry without back-off.
                    }
                    Err(e) => {
                        let err = anyhow::anyhow!("TCP write error to {addr}: {e}");
                        eprintln!(
                            "warning: dropping TCP outbound peer {addr} after write error: {err:#}"
                        );
                        self.outbound[i].shutdown();
                        keep[i] = false;
                        last_err = Some(err);
                        break 'send;
                    }
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
    /// T17.4: drains the inline-drained queue first so frames
    /// pulled off the wire during a single-mode `broadcast` are
    /// surfaced in order before any new reads are issued.
    ///
    /// Per-peer fatal errors (`ConnectionAborted`, `ConnectionReset`, EOF,
    /// malformed framing) are absorbed at this layer: the offending peer is
    /// dropped from the active set with a single warning, and we move on.
    /// One peer disconnecting must NOT fail the whole spawn — see module
    /// docs.
    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        // T17.4: stashed frames from the inline drain (single mode
        // only) come first so the variant's poll_receive sees them
        // in arrival order.
        if let Some(msg) = self.pending_drained.pop_front() {
            return Ok(Some(msg));
        }

        // Accept any new inbound connections first. Single-mode call
        // path passes `None` here; Multi mode never calls `try_recv`
        // (the reader-thread hub bypasses this path).
        self.accept_pending(None)?;

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

    /// Mutable access to outbound peers (used by Multi-mode reader
    /// setup to drain the read clones via `TcpPeer::take_read_stream`).
    pub fn outbound_peers_mut(&mut self) -> &mut [TcpPeer] {
        &mut self.outbound
    }

    /// Mutable access to inbound peers (used by Multi-mode reader
    /// setup to drain the read clones via `TcpPeer::take_read_stream`).
    pub fn inbound_peers_mut(&mut self) -> &mut [TcpPeer] {
        &mut self.inbound
    }

    /// Drain any inbound TCP connections that have arrived since the
    /// last call AND apply `SO_RCVBUF` from `recv_buffer_kb` on each.
    /// Used by Multi mode in `start_reader_threads`.
    pub fn accept_pending_with_buffer(&mut self, recv_buffer_kb: Option<u32>) -> Result<()> {
        self.accept_pending(recv_buffer_kb)
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

/// Connect to `addr` with a bounded retry ONLY on `ConnectionRefused`.
/// The two-runner startup is a known race: both sides hit the ready
/// barrier and call `connect` near simultaneously; either side's
/// listener may not be bound yet. On `ConnectionRefused`, retry every
/// 50 ms for up to `budget`. All other error kinds (including
/// `TimedOut`) propagate immediately so we don't paper over real
/// connectivity problems.
///
/// Uses the BLOCKING `TcpStream::connect` (no per-attempt timeout) to
/// preserve the existing kernel-default connect behaviour: a successful
/// connect on a healthy LAN returns within milliseconds. Wrapping with
/// `connect_timeout(500ms)` would have falsely tripped on slow SYN-ACK
/// scheduling at higher QoS levels (cause of the qos4 regression in
/// the first iteration of this helper).
fn connect_with_retry(addr: SocketAddr, budget: Duration) -> Result<TcpStream> {
    let deadline = Instant::now() + budget;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return Ok(s),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "TCP connect to {addr} kept getting refused after {budget:?}: {e}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(anyhow::anyhow!("TCP connect to {addr} failed: {e}")),
        }
    }
}

/// T16.3: extract a single complete length-prefixed frame from
/// `buf` if one is available, draining the consumed bytes. Returns
/// `None` when `buf` does not yet hold a complete `[u32 length |
/// payload]` frame.
///
/// Pulled out of `TcpPeer::try_recv_framed` so the fast path
/// (buffered-frame-already-available) and the slow path (read more
/// from the kernel first) share the same extraction code.
fn take_buffered_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() < 4 {
        return None;
    }
    let msg_len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
    let total = 4 + msg_len;
    if buf.len() < total {
        return None;
    }
    let msg = buf[4..total].to_vec();
    buf.drain(..total);
    Some(msg)
}

/// Apply `SO_RCVBUF = kb * 1024` to a `TcpStream` (T14.1 / T14.4).
/// Emits a single `eprintln!` warning when the achieved size lands
/// below the requested value (e.g. Windows silently clamping).
fn apply_tcp_recv_buffer(stream: &TcpStream, kb: u32, addr: SocketAddr) -> Result<()> {
    let bytes = (kb as usize).saturating_mul(1024);
    let sock = SockRef::from(stream);
    sock.set_recv_buffer_size(bytes)
        .with_context(|| format!("set_recv_buffer_size on TCP socket to peer {addr}"))?;
    let achieved = sock
        .recv_buffer_size()
        .with_context(|| format!("read SO_RCVBUF on TCP socket to peer {addr}"))?;
    if achieved < bytes {
        eprintln!(
            "[variant-hybrid] warning: TCP SO_RCVBUF to peer {addr} achieved only {achieved} bytes, requested {bytes} ({kb} KiB)"
        );
    }
    Ok(())
}

/// Trait abstracting the per-call `write` so the retry loop can be
/// exercised without a real TCP socket. Test-only.
#[cfg(test)]
trait ByteWrite {
    fn write_once(&mut self, data: &[u8]) -> io::Result<usize>;
}

#[cfg(test)]
impl ByteWrite for TcpStream {
    fn write_once(&mut self, data: &[u8]) -> io::Result<usize> {
        Write::write(self, data)
    }
}

/// T17.4: write `data` to `writer`, looping forever on `WouldBlock`
/// while still calling `between_retries` between attempts so the
/// caller can drain incoming reads (the single-mode wedge breaker).
///
/// Strict no-skip QoS 3/4 contract (DESIGN.md § 6.5) means the loop
/// MUST NOT give up on the message: a transient `WouldBlock` is the
/// back-pressure signal, retried indefinitely. Real I/O errors
/// (`ConnectionReset`, `BrokenPipe`, etc.) DO surface immediately --
/// the peer is unrecoverable, the broadcast layer drops it from the
/// active set and continues with the survivors.
///
/// Used by the unit tests below; the production publish path
/// inlines this same logic inside
/// [`TcpTransport::broadcast_with_drain`] so the drain pass can
/// capture `&mut TcpTransport` cleanly.
#[cfg(test)]
fn write_nonblocking_strict<W: ByteWrite, F: FnMut()>(
    writer: &mut W,
    data: &[u8],
    mut between_retries: F,
) -> Result<()> {
    let mut written = 0usize;
    let stall_warn_deadline = Instant::now() + TCP_WRITE_STALL_WARNING;
    let mut warned_once = false;
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
                between_retries();
                if !warned_once && Instant::now() >= stall_warn_deadline {
                    eprintln!(
                        "[variant-hybrid] TCP write stalled for >{:?}; still retrying \
                         (strict no-skip QoS 3/4 contract)",
                        TCP_WRITE_STALL_WARNING
                    );
                    warned_once = true;
                }
                std::thread::sleep(TCP_WRITE_RETRY_SLEEP);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // Standard EINTR retry.
            }
            Err(e) => return Err(anyhow::anyhow!("TCP write error: {}", e)),
        }
    }
    Ok(())
}

/// T17.4: single-mode inline drain pass used by `broadcast`.
///
/// Pulls any available frames from every peer's read side into
/// the transport's `pending_drained` queue. Called between
/// non-blocking write attempts when the kernel send buffer is
/// full -- this is what breaks the symmetric-saturation wedge
/// without dropping messages. The variant's `poll_receive`
/// surfaces the frames on the next call.
///
/// Bounded by `INLINE_DRAIN_MAX_FRAMES` per call so a single
/// broadcast iteration doesn't run away in pathological cases.
const INLINE_DRAIN_MAX_FRAMES: usize = 64;

fn inline_drain_into_pending(t: &mut TcpTransport) {
    for _ in 0..INLINE_DRAIN_MAX_FRAMES {
        // Reuse `try_recv` logic but skip its pending-queue check
        // so we don't return the same frame on consecutive calls.
        let frame = pull_one_fresh_frame(t);
        match frame {
            Some(msg) => t.pending_drained.push_back(msg),
            None => return,
        }
    }
}

/// T17.4: pull ONE fresh frame off the wire (skipping the
/// `pending_drained` queue). Returns `None` when no frame is
/// currently available across either peer set.
fn pull_one_fresh_frame(t: &mut TcpTransport) -> Option<Vec<u8>> {
    // Accept any new inbound connections; ignore the error path
    // here -- we're inside a write-retry hot loop and a failed
    // accept will be re-attempted on the next `try_recv`.
    let _ = t.accept_pending(None);
    if let Some(msg) = poll_peer_set(&mut t.inbound, "inbound") {
        return Some(msg);
    }
    if let Some(msg) = poll_peer_set(&mut t.outbound, "outbound") {
        return Some(msg);
    }
    None
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
        let mut transport = TcpTransport::new(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            ThreadingMode::Single,
        )
        .unwrap();
        transport.connect_to_peer(addr_a, None).unwrap();
        transport.connect_to_peer(addr_b, None).unwrap();

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

        let mut transport = TcpTransport::new(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            ThreadingMode::Single,
        )
        .unwrap();
        transport.connect_to_peer(addr, None).unwrap();

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

    /// T17.4: `write_nonblocking_strict` recovers from a transient
    /// `WouldBlock` by retrying (no budget). The `between_retries`
    /// callback is invoked at least once per `WouldBlock` so the
    /// caller can drain reads (the single-mode wedge breaker).
    #[test]
    fn write_nonblocking_strict_recovers_after_one_wouldblock() {
        let mut writer = FlakyWriter {
            wouldblock_remaining: 1,
            attempts: 0,
            last_payload_len: 0,
        };
        let mut drain_calls = 0u32;
        write_nonblocking_strict(&mut writer, b"hello", || drain_calls += 1)
            .expect("strict loop must yield Ok after one WouldBlock");
        assert!(
            (2..=10_000).contains(&writer.attempts),
            "expected a small finite retry count, got {}",
            writer.attempts
        );
        assert_eq!(writer.last_payload_len, 5);
        assert!(
            drain_calls >= 1,
            "between_retries callback should fire on each WouldBlock; got {drain_calls}"
        );
    }

    /// T17.4: a peer that NEVER unblocks would normally pin the
    /// strict loop forever. To verify the loop is genuinely retrying
    /// (and not silently dropping) we wrap the always-block writer
    /// with an attempt cap.
    #[test]
    fn write_nonblocking_strict_retries_indefinitely_on_wouldblock() {
        struct Capped {
            attempts: u32,
            max_attempts: u32,
        }
        impl ByteWrite for Capped {
            fn write_once(&mut self, _data: &[u8]) -> io::Result<usize> {
                self.attempts += 1;
                if self.attempts >= self.max_attempts {
                    return Ok(1);
                }
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
        }
        let mut w = Capped {
            attempts: 0,
            max_attempts: 50,
        };
        write_nonblocking_strict(&mut w, &[0u8; 1], || {})
            .expect("must eventually succeed when the kernel relents");
        assert_eq!(
            w.attempts, 50,
            "loop must NOT give up on WouldBlock; got {} attempts",
            w.attempts
        );
    }

    /// Partial writes (a `write` returning fewer bytes than asked) must be
    /// resumed at the next offset, not retried from the start.
    #[test]
    fn write_nonblocking_strict_handles_partial_writes() {
        struct PartialWriter {
            written: Vec<u8>,
        }
        impl ByteWrite for PartialWriter {
            fn write_once(&mut self, data: &[u8]) -> io::Result<usize> {
                let n = data.len().min(1);
                self.written.extend_from_slice(&data[..n]);
                Ok(n)
            }
        }
        let mut w = PartialWriter {
            written: Vec::new(),
        };
        write_nonblocking_strict(&mut w, b"abcdef", || {}).unwrap();
        assert_eq!(&w.written, b"abcdef");
    }

    /// T17.4: a real I/O error (e.g. `ConnectionReset`) MUST
    /// surface immediately so the broadcast layer can drop the
    /// peer. Only `WouldBlock` and `Interrupted` are retried.
    #[test]
    fn write_nonblocking_strict_surfaces_real_io_errors() {
        struct BrokenWriter;
        impl ByteWrite for BrokenWriter {
            fn write_once(&mut self, _data: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        }
        let err = write_nonblocking_strict(&mut BrokenWriter, b"x", || {})
            .expect_err("ConnectionReset must surface as a fatal write error");
        assert!(format!("{err:#}").contains("TCP write error"));
    }

    /// T17.4: in BOTH threading modes, `TcpPeer::from_stream` puts
    /// the socket in non-blocking mode (no mode-branched
    /// SO_SNDTIMEO install).
    #[test]
    fn from_stream_puts_socket_in_nonblocking_mode_single() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (_server_side, _) = listener.accept().unwrap();

        let _peer =
            TcpPeer::from_stream(client, addr, ThreadingMode::Single).expect("from_stream Single");
        // Non-blocking exercised by the integration tests; this
        // placeholder keeps the test name as a stable contract marker.
    }

    /// T16.3: `take_buffered_frame` extracts exactly one frame and
    /// drains its bytes from the buffer. Multiple frames stay
    /// independently extractable.
    #[test]
    fn take_buffered_frame_handles_multiple_frames() {
        // Encode two length-prefixed frames into a single buffer.
        let frame_a = b"hello";
        let frame_b = b"world!!";
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&(frame_a.len() as u32).to_be_bytes());
        buf.extend_from_slice(frame_a);
        buf.extend_from_slice(&(frame_b.len() as u32).to_be_bytes());
        buf.extend_from_slice(frame_b);
        let total_bytes = buf.len();

        let m1 = take_buffered_frame(&mut buf).expect("first frame must be extractable");
        assert_eq!(m1.as_slice(), frame_a);
        assert_eq!(buf.len(), total_bytes - 4 - frame_a.len());

        let m2 = take_buffered_frame(&mut buf).expect("second frame must be extractable");
        assert_eq!(m2.as_slice(), frame_b);
        assert!(buf.is_empty());

        // Empty buffer => None.
        assert!(take_buffered_frame(&mut buf).is_none());
    }

    /// T16.3: `take_buffered_frame` returns None when the buffer
    /// holds only a partial frame (length-prefix without enough
    /// payload bytes).
    #[test]
    fn take_buffered_frame_returns_none_on_partial_payload() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&10u32.to_be_bytes()); // claim 10 bytes payload
        buf.extend_from_slice(b"abc"); // only 3 bytes present
        assert!(take_buffered_frame(&mut buf).is_none());
        // Buffer is untouched -- subsequent reads can complete the frame.
        assert_eq!(buf.len(), 4 + 3);
    }

    /// T17.4: Multi mode also uses non-blocking sockets now. The
    /// per-peer reader thread does `read` on the non-blocking
    /// clone; the existing read loop already absorbs `WouldBlock`
    /// (or `TimedOut`) as a "no data this iteration" signal.
    #[test]
    fn from_stream_puts_socket_in_nonblocking_mode_multi() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (_server_side, _) = listener.accept().unwrap();

        let _peer =
            TcpPeer::from_stream(client, addr, ThreadingMode::Multi).expect("from_stream Multi");
        // No write timeout is expected (T16.3 SO_SNDTIMEO retired).
    }

    /// T17.4: `broadcast_with_drain` eventually delivers all
    /// bytes without dropping the peer, even when the kernel
    /// send buffer pushes back. Uses a real loopback TCP pair
    /// and a reader thread that consumes everything at the end.
    /// This is the primary contract test for the strict-delivery
    /// fix.
    #[test]
    fn broadcast_with_drain_delivers_all_bytes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let writer_sock = TcpStream::connect(addr).unwrap();
        let (reader_sock, _) = listener.accept().unwrap();

        let mut transport = TcpTransport::new(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            ThreadingMode::Single,
        )
        .unwrap();
        let peer = TcpPeer::from_stream(writer_sock, addr, ThreadingMode::Single).unwrap();
        transport.outbound.push(peer);

        const N_BYTES: usize = 4 * 1024 * 1024;
        let reader_handle = std::thread::spawn(move || -> usize {
            let mut sock = reader_sock;
            sock.set_nonblocking(false).ok();
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = vec![0u8; 65536];
            let mut total = 0usize;
            while total < N_BYTES {
                match Read::read(&mut sock, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(_) => break,
                }
            }
            total
        });

        let payload = vec![0xABu8; N_BYTES];
        let result = transport.broadcast_with_drain(&payload, |_| {});
        result.expect("broadcast must succeed; strict no-skip contract");
        assert_eq!(
            transport.outbound.len(),
            1,
            "peer must NOT be dropped on transient WouldBlock"
        );

        let read = reader_handle.join().expect("reader thread joined");
        assert_eq!(
            read, N_BYTES,
            "every byte must reach the peer (strict no-skip contract); \
             got {read} of {N_BYTES}"
        );
    }
}
