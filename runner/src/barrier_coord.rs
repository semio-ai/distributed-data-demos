//! Reliable ready/done barrier transport over TCP per peer pair (T15.10).
//!
//! Before T15.10, ready/done barriers used the same UDP multicast socket
//! as discovery, clock-sync, and probe responses. Under symmetric same-host
//! stress (1000 vpt x 100 Hz x 2 directions ~~ 200K msgs/s on the variant
//! data plane, plus multicast loopback for the coord socket) the kernel's
//! per-socket UDP recv buffer overflows during the variant-transition
//! window between spawns. Lost barrier datagrams have no application-level
//! retransmit -- the 500 ms broadcast tick keeps re-sending the same
//! payload but the receiver has already exited its 2 s linger by the time
//! the kernel queue drains, so the loss is permanent and the barrier times
//! out at 120 s.
//!
//! T15.10 moves the barrier quorum signal to a dedicated long-lived
//! TCP-per-peer-pair channel mirroring the T14.24 resume_manifest pattern
//! and the T15.3 progress_coord pattern. TCP retransmit handles loss for
//! free; length-prefixed framing is immune to UDP truncation; per-peer
//! state is bounded by the connection lifetime (one connection per pair
//! from after-discovery until shutdown).
//!
//! Wire model:
//!
//! - **Pairing** -- same rule as T14.24 / T15.3: for peer pair `(a, b)`
//!   with `a < b` lexicographically by runner name, `a` accepts and `b`
//!   connects. Self-pairs do not exchange.
//! - **Port derivation** -- each runner listens on
//!   `base_port + BARRIER_TCP_OFFSET + runner_index`. The constant
//!   [`BARRIER_TCP_OFFSET`] (96) is chosen to leave a clear gap above
//!   the progress-coord range (`base + 64 + index`) and resume_manifest
//!   range (`base + 32 + index`).
//! - **Handshake** -- the connecting side writes a single length-prefixed
//!   UTF-8 frame carrying its runner name (`[u32 BE length][name bytes]`,
//!   256-byte cap). The accepting side reads this frame, verifies the
//!   name is in the expected runners set, and installs the stream as
//!   the peer's writer.
//! - **Framing** -- subsequent frames are length-prefixed JSON encodings
//!   of `Message::Ready` or `Message::Done`. Frames above
//!   [`BARRIER_FRAME_MAX_BYTES`] (16 KiB; far above any plausible Ready
//!   or Done) are rejected defensively.
//! - **Inbox model** -- each peer's reader thread appends inbound
//!   barrier messages to a per-peer `Vec`. The barrier loops in
//!   `Coordinator::ready_barrier` / `done_barrier` drain the inboxes
//!   on every poll and match against the active variant. Messages not
//!   matching the active variant are dropped silently (the post-T15.10
//!   protocol does NOT need T-coord.1b stale-Done re-emission on the
//!   TCP channel because TCP delivery is reliable -- a peer either
//!   receives a Done or the TCP connection is closed; there is no
//!   intermediate "datagram dropped, peer waits forever" case).
//!
//! Lifecycle: a `BarrierCoordinator` is constructed after discovery
//! (which populates `peer_hosts`), started via `start()` before the
//! Phase 2 spawn loop, used across every spawn, and shut down via
//! `shutdown()` before runner exit. In single-runner mode every method
//! is a no-op.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::message::Message;

/// Offset added to the base UDP coordination port to derive each
/// runner's TCP listener port for the T15.10 ready/done barrier
/// exchange.
///
/// Layout (zero-based `index` for each runner in the config's `runners`
/// array):
/// - UDP coord: `base + index`
/// - Resume manifest TCP (T14.24): `base + 32 + index`
/// - Progress TCP (T15.3): `base + 64 + index`
/// - Barrier TCP (T15.10): `base + 96 + index`
///
/// All four ranges sit inside the same low ephemeral region operators
/// already permit for UDP coordination, so no new firewall rules are
/// required.
pub const BARRIER_TCP_OFFSET: u16 = 96;

/// Per-pair connect / accept poll timeout for the barrier exchange
/// listener. Short enough to retry every few hundred ms when the peer's
/// listener has not bound yet; long enough that the connect thread does
/// not spin in a tight loop. The overall startup budget is bounded by
/// [`BARRIER_STARTUP_BUDGET`].
const BARRIER_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// I/O read/write timeout once a peer pair has connected. Each barrier
/// frame is small (~150 bytes); a few seconds is plenty for any
/// reasonable LAN. A timeout here marks the peer's stream unhealthy;
/// the next attempted publish fails and the pair drops out of the
/// connected set. The barrier loops fall back to the overall barrier
/// deadline.
const BARRIER_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on a single received barrier frame. 16 KiB is well above
/// any plausible Ready / Done JSON (a few hundred bytes today) and
/// bounds worst-case memory if a peer lies about its length prefix.
const BARRIER_FRAME_MAX_BYTES: u32 = 16 * 1024;

/// Overall budget for the initial accept-and-connect handshake with
/// every peer. After this budget elapses, peers that have not yet
/// connected are left in the unhealthy set; subsequent barrier calls
/// will time out for those peers via their own deadline. Generous
/// enough to absorb the longest plausible process-startup skew between
/// runners on different machines.
const BARRIER_STARTUP_BUDGET: Duration = Duration::from_secs(15);

/// Per-peer mailbox for inbound barrier messages. Reader threads append;
/// barrier loops drain. The inbox is small in practice (typically one
/// matching message per peer per barrier) but unbounded to avoid losing
/// a fast peer's broadcast while the local side is still inside a
/// previous barrier's linger.
type Inbox = Arc<Mutex<Vec<Message>>>;

/// Background coordinator that maintains a long-lived TCP connection
/// to every other runner for ready/done barrier signalling.
///
/// Construct via [`BarrierCoordinator::new`], then call
/// [`BarrierCoordinator::start`] once `peer_hosts` is known (after
/// discovery). The `Coordinator` in `protocol.rs` consults this handle
/// inside `ready_barrier` / `done_barrier` when it is installed,
/// preferring the TCP path over the legacy UDP path. Call
/// [`BarrierCoordinator::shutdown`] before runner exit.
///
/// In single-runner mode the coordinator is a no-op: `start` returns
/// immediately, `broadcast` is an early return, and `shutdown` has
/// nothing to do.
pub struct BarrierCoordinator {
    /// This runner's name.
    name: String,
    /// Ordered runner list (used to derive each peer's listener port).
    runners_order: Vec<String>,
    /// All expected runner names (used to filter accepted peer names).
    expected: HashSet<String>,
    /// Base UDP coordination port; the barrier port is derived from
    /// this plus [`BARRIER_TCP_OFFSET`] plus the per-runner index.
    base_port: u16,
    /// Whether this is single-runner mode (no peers => no transport).
    single_runner: bool,
    /// Per-peer writer state, populated by `start()`. Keyed by peer
    /// runner name. Each entry owns a `TcpStream` clone wrapped in a
    /// mutex so `broadcast()` can serialise outbound frames per peer.
    writers: Mutex<HashMap<String, Arc<Mutex<TcpStream>>>>,
    /// Per-peer reader thread join handles.
    reader_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Signalled from `shutdown` to tell reader threads to exit. Reads
    /// are polled at the I/O timeout boundary; once true, the reader
    /// stops looping on its next wakeup.
    stop: Arc<AtomicBool>,
    /// Per-peer inbox of received Ready/Done messages. The barrier
    /// loops drain this on every poll.
    inboxes: Mutex<HashMap<String, Inbox>>,
}

impl BarrierCoordinator {
    /// Build a coordinator. Construction does not perform any network
    /// I/O; call `start()` once `peer_hosts` is known.
    pub fn new(name: String, runners_order: Vec<String>, base_port: u16) -> Self {
        let expected: HashSet<String> = runners_order.iter().cloned().collect();
        let single_runner = runners_order.len() == 1 && runners_order[0] == name;
        Self {
            name,
            runners_order,
            expected,
            base_port,
            single_runner,
            writers: Mutex::new(HashMap::new()),
            reader_handles: Mutex::new(Vec::new()),
            stop: Arc::new(AtomicBool::new(false)),
            inboxes: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this coordinator is in single-runner mode (no peers, no
    /// transport).
    pub fn is_single_runner(&self) -> bool {
        self.single_runner
    }

    /// Set of peers we are currently connected to. Used by the barrier
    /// loops to decide whether the TCP path is usable for this run; if
    /// a peer is missing from the connected set, the loop reports a
    /// barrier timeout for that peer the same way the UDP path would.
    pub fn connected_peers(&self) -> HashSet<String> {
        self.writers
            .lock()
            .expect("writers mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Open per-peer TCP connections to every other runner using
    /// `peer_hosts` for host resolution. Lower-sorted-name accepts;
    /// higher connects. Blocks until every peer pair is connected, or
    /// the [`BARRIER_STARTUP_BUDGET`] elapses. Peers that never appear
    /// are silently dropped from the connected set; the subsequent
    /// barrier calls will time out for those peers via their own
    /// deadline.
    ///
    /// Spawns one reader thread per accepted/connected peer that runs
    /// for the lifetime of the coordinator.
    ///
    /// In single-runner mode this returns `Ok(())` immediately.
    pub fn start(&self, peer_hosts: &HashMap<String, String>) -> Result<()> {
        if self.single_runner {
            return Ok(());
        }

        let mut peers: Vec<String> = self
            .runners_order
            .iter()
            .filter(|n| **n != self.name)
            .cloned()
            .collect();
        peers.sort();

        let mut to_accept: Vec<String> = Vec::new();
        let mut to_connect: Vec<String> = Vec::new();
        for peer in &peers {
            if self.name.as_str() < peer.as_str() {
                to_accept.push(peer.clone());
            } else {
                to_connect.push(peer.clone());
            }
        }

        // Always bind our listener -- even when `to_accept` is empty
        // (i.e. we are the highest-sorted name) -- so the cost is one
        // fd and we have a stable contract for tests.
        let my_index = self
            .runners_order
            .iter()
            .position(|r| r == &self.name)
            .unwrap_or(0);
        let my_listener_port = self
            .base_port
            .checked_add(BARRIER_TCP_OFFSET)
            .and_then(|p| p.checked_add(my_index as u16))
            .ok_or_else(|| {
                anyhow!(
                    "base port {} + offset {} + index {} overflows u16",
                    self.base_port,
                    BARRIER_TCP_OFFSET,
                    my_index
                )
            })?;
        let listener_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, my_listener_port);
        let listener = TcpListener::bind(listener_addr).map_err(|e| {
            anyhow!(
                "barrier_coord: failed to bind TCP listener on {}: {}",
                listener_addr,
                e
            )
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| anyhow!("barrier_coord: set_nonblocking failed: {}", e))?;

        let mut accept_pending: HashSet<String> = to_accept.iter().cloned().collect();
        let mut connect_pending: HashSet<String> = to_connect.iter().cloned().collect();

        let deadline = Instant::now() + BARRIER_STARTUP_BUDGET;
        while !accept_pending.is_empty() || !connect_pending.is_empty() {
            if Instant::now() >= deadline {
                let still_missing: Vec<String> = accept_pending
                    .iter()
                    .chain(connect_pending.iter())
                    .cloned()
                    .collect();
                eprintln!(
                    "[runner:{}] barrier_coord: startup budget {}s elapsed; \
                     proceeding without peer(s): {:?}",
                    self.name,
                    BARRIER_STARTUP_BUDGET.as_secs(),
                    still_missing
                );
                break;
            }

            // 1. Outbound connects.
            let targets: Vec<String> = connect_pending.iter().cloned().collect();
            for peer in targets {
                let peer_index = match self.runners_order.iter().position(|r| r == &peer) {
                    Some(i) => i,
                    None => continue,
                };
                let peer_port = match self
                    .base_port
                    .checked_add(BARRIER_TCP_OFFSET)
                    .and_then(|p| p.checked_add(peer_index as u16))
                {
                    Some(p) => p,
                    None => continue,
                };
                let peer_host = match peer_hosts.get(&peer) {
                    Some(h) => h.clone(),
                    None => continue,
                };
                let addr = format!("{}:{}", peer_host, peer_port);
                match connect_and_handshake(&addr, &self.name) {
                    Ok(stream) => {
                        self.install_peer(&peer, stream);
                        connect_pending.remove(&peer);
                    }
                    Err(_e) => {
                        // Most common case: peer's listener not bound
                        // yet. Retry on the next outer-loop iteration.
                    }
                }
            }

            // 2. Inbound accepts.
            for _ in 0..peers.len() {
                if accept_pending.is_empty() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _peer_addr)) => {
                        match accept_and_handshake(stream, &self.expected) {
                            Ok((peer_name, stream)) => {
                                if accept_pending.contains(&peer_name) {
                                    self.install_peer(&peer_name, stream);
                                    accept_pending.remove(&peer_name);
                                } else {
                                    // Unexpected peer / duplicate; drop.
                                }
                            }
                            Err(_e) => {
                                // Stream-level error; peer will retry.
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }

            if accept_pending.is_empty() && connect_pending.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // The listener is no longer needed once every accept has
        // landed (or the budget elapsed). Drop it explicitly.
        drop(listener);
        Ok(())
    }

    /// Install a fresh stream as the writer + start a reader thread.
    /// Called from both the accept and connect paths.
    fn install_peer(&self, peer_name: &str, stream: TcpStream) {
        // Apply per-frame I/O timeouts so a wedged peer cannot block
        // the writer or reader indefinitely.
        let _ = stream.set_read_timeout(Some(BARRIER_IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(BARRIER_IO_TIMEOUT));
        // Disable Nagle so per-barrier frames are flushed promptly --
        // a 200 ms Nagle delay matters for a 100 ms barrier tick.
        let _ = stream.set_nodelay(true);

        // Clone the stream so we can hand the reader half to a
        // dedicated thread while keeping the writer half in the
        // coordinator's writer map.
        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[runner:{}] barrier_coord: try_clone for peer '{}' failed: {}; \
                     reader thread not started",
                    self.name, peer_name, e
                );
                self.writers
                    .lock()
                    .expect("writers mutex poisoned")
                    .insert(peer_name.to_string(), Arc::new(Mutex::new(stream)));
                return;
            }
        };
        self.writers
            .lock()
            .expect("writers mutex poisoned")
            .insert(peer_name.to_string(), Arc::new(Mutex::new(stream)));

        // Per-peer inbox. Created here on first install so the barrier
        // loops can find it by peer name.
        let inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
        self.inboxes
            .lock()
            .expect("inboxes mutex poisoned")
            .insert(peer_name.to_string(), inbox.clone());

        let stop = self.stop.clone();
        let runner_name_for_log = self.name.clone();
        let peer_for_thread = peer_name.to_string();
        let handle = std::thread::spawn(move || {
            run_reader_loop(
                reader_stream,
                inbox,
                stop,
                &runner_name_for_log,
                &peer_for_thread,
            );
        });
        self.reader_handles
            .lock()
            .expect("reader_handles mutex poisoned")
            .push(handle);
    }

    /// Broadcast a barrier message (`Message::Ready` or `Message::Done`)
    /// to every connected peer. Per-peer write errors mark that peer's
    /// stream unhealthy (removed from the writer map). Returns the set
    /// of peers we successfully wrote to.
    ///
    /// In single-runner mode this is a no-op and returns an empty set.
    pub fn broadcast(&self, msg: &Message) -> HashSet<String> {
        if self.single_runner {
            return HashSet::new();
        }
        let payload = msg.to_bytes();

        let writers_snapshot: Vec<(String, Arc<Mutex<TcpStream>>)> = self
            .writers
            .lock()
            .expect("writers mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut ok: HashSet<String> = HashSet::new();
        let mut failed: Vec<String> = Vec::new();
        for (peer, stream_arc) in writers_snapshot {
            let mut stream = stream_arc.lock().expect("per-peer stream mutex poisoned");
            match write_barrier_frame(&mut stream, &payload) {
                Ok(()) => {
                    ok.insert(peer);
                }
                Err(e) => {
                    eprintln!(
                        "[runner:{}] barrier_coord: write to peer '{}' failed: {}; \
                         removing peer from writer set",
                        self.name, peer, e
                    );
                    failed.push(peer);
                }
            }
        }
        if !failed.is_empty() {
            let mut writers = self.writers.lock().expect("writers mutex poisoned");
            for peer in failed {
                writers.remove(&peer);
            }
        }
        ok
    }

    /// Drain the inbox for `peer`. Returns an empty `Vec` if the peer
    /// has no inbox (not yet connected, or already shut down). The
    /// inbox is cleared on every drain so subsequent calls return only
    /// newly arrived messages.
    ///
    /// Production code uses [`Self::drain_all_inboxes`]; this
    /// per-peer accessor is retained for tests that want to assert
    /// per-peer arrival counts.
    #[allow(dead_code)]
    pub fn drain_inbox(&self, peer: &str) -> Vec<Message> {
        let inboxes = self.inboxes.lock().expect("inboxes mutex poisoned");
        match inboxes.get(peer) {
            Some(inbox) => {
                let mut g = inbox.lock().expect("inbox mutex poisoned");
                std::mem::take(&mut *g)
            }
            None => Vec::new(),
        }
    }

    /// Drain inboxes for all connected peers in one call. Useful for
    /// barrier loops that want to scan every peer's inbox per tick
    /// without holding the inboxes map locked between drains.
    pub fn drain_all_inboxes(&self) -> HashMap<String, Vec<Message>> {
        let inboxes_snapshot: Vec<(String, Inbox)> = {
            let inboxes = self.inboxes.lock().expect("inboxes mutex poisoned");
            inboxes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        let mut out: HashMap<String, Vec<Message>> = HashMap::new();
        for (peer, inbox) in inboxes_snapshot {
            let mut g = inbox.lock().expect("inbox mutex poisoned");
            let drained = std::mem::take(&mut *g);
            if !drained.is_empty() {
                out.insert(peer, drained);
            }
        }
        out
    }

    /// Stop reader threads, close connections, and join. Idempotent;
    /// safe to call from `Drop`. In single-runner mode this is a
    /// no-op.
    pub fn shutdown(&self) {
        if self.single_runner {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        // Close every writer. Closing the writer half causes the reader
        // half on the peer to see EOF promptly; closing our reader is
        // handled inside the reader thread's own loop via `stop`.
        {
            let mut writers = self.writers.lock().expect("writers mutex poisoned");
            for (_peer, stream_arc) in writers.drain() {
                if let Ok(stream) = stream_arc.lock() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }
        // Join readers. They wake on the I/O timeout, observe `stop`,
        // and exit.
        let handles: Vec<JoinHandle<()>> = self
            .reader_handles
            .lock()
            .expect("reader_handles mutex poisoned")
            .drain(..)
            .collect();
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for BarrierCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reader loop: read length-prefixed frames until EOF or `stop`. Folds
/// parseable Ready/Done messages into the per-peer inbox. Other
/// message types are dropped (they should not appear on this channel
/// per the protocol contract).
fn run_reader_loop(
    mut stream: TcpStream,
    inbox: Inbox,
    stop: Arc<AtomicBool>,
    runner_name: &str,
    peer_name: &str,
) {
    while !stop.load(Ordering::Relaxed) {
        match read_barrier_frame(&mut stream) {
            Ok(bytes) => {
                let msg = match Message::from_bytes(&bytes) {
                    Some(m) => m,
                    None => {
                        eprintln!(
                            "[runner:{}] barrier_coord: malformed frame from peer '{}'; \
                             ignoring",
                            runner_name, peer_name
                        );
                        continue;
                    }
                };
                match msg {
                    Message::Ready { .. } | Message::Done { .. } => {
                        inbox.lock().expect("inbox mutex poisoned").push(msg);
                    }
                    other => {
                        eprintln!(
                            "[runner:{}] barrier_coord: unexpected message type from peer \
                             '{}': {:?}",
                            runner_name, peer_name, other
                        );
                    }
                }
            }
            Err(e) => {
                let kind = e.kind();
                if kind == std::io::ErrorKind::TimedOut || kind == std::io::ErrorKind::WouldBlock {
                    // I/O timeout simply lets the loop poll `stop`
                    // again; not an error.
                    continue;
                }
                if kind == std::io::ErrorKind::UnexpectedEof || stop.load(Ordering::Relaxed) {
                    // Peer closed or we are shutting down. Quiet exit.
                    return;
                }
                eprintln!(
                    "[runner:{}] barrier_coord: read error from peer '{}': {}; \
                     reader thread exiting",
                    runner_name, peer_name, e
                );
                return;
            }
        }
    }
}

/// Length-prefixed write of a serialised payload over a connected
/// stream. Mirrors `write_progress_frame` in `progress_coord.rs`.
fn write_barrier_frame(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let len: u32 = payload.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

/// Read one length-prefixed payload from a stream.
fn read_barrier_frame(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes);
    if len > BARRIER_FRAME_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "barrier frame length {} exceeds cap {}",
                len, BARRIER_FRAME_MAX_BYTES
            ),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// One-shot handshake on the connecting side: send our runner name as
/// a single length-prefixed frame so the accepting side can map the
/// stream to a peer. Returns the connected stream on success.
fn connect_and_handshake(addr: &str, self_name: &str) -> Result<TcpStream> {
    let socket_addr: SocketAddr = addr
        .parse()
        .map_err(|e| anyhow!("barrier_coord connect: bad address '{}': {}", addr, e))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, BARRIER_CONNECT_TIMEOUT)
        .map_err(|e| anyhow!("barrier_coord connect: {} not reachable: {}", addr, e))?;
    stream
        .set_read_timeout(Some(BARRIER_IO_TIMEOUT))
        .map_err(|e| anyhow!("barrier_coord connect: set_read_timeout failed: {}", e))?;
    stream
        .set_write_timeout(Some(BARRIER_IO_TIMEOUT))
        .map_err(|e| anyhow!("barrier_coord connect: set_write_timeout failed: {}", e))?;
    let _ = stream.set_nodelay(true);

    let name_bytes = self_name.as_bytes();
    if name_bytes.len() > 256 {
        return Err(anyhow!(
            "barrier_coord connect: self name too long ({} bytes)",
            name_bytes.len()
        ));
    }
    let len: u32 = name_bytes.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| anyhow!("barrier_coord connect: handshake write failed: {}", e))?;
    stream
        .write_all(name_bytes)
        .map_err(|e| anyhow!("barrier_coord connect: handshake name write failed: {}", e))?;
    stream
        .flush()
        .map_err(|e| anyhow!("barrier_coord connect: handshake flush failed: {}", e))?;

    Ok(stream)
}

/// One-shot handshake on the accepting side: read the peer's runner
/// name from the first length-prefixed frame, verify it is in the
/// expected set, and return the stream paired with the peer's name.
fn accept_and_handshake(
    mut stream: TcpStream,
    expected: &HashSet<String>,
) -> Result<(String, TcpStream)> {
    // The accepted stream inherits the listener's nonblocking flag on
    // some platforms (notably Linux); explicitly switch to blocking
    // here so `read_exact` blocks instead of returning WouldBlock. The
    // per-frame I/O timeout bounds the wait.
    stream
        .set_nonblocking(false)
        .map_err(|e| anyhow!("barrier_coord accept: set_nonblocking(false) failed: {}", e))?;
    stream
        .set_read_timeout(Some(BARRIER_IO_TIMEOUT))
        .map_err(|e| anyhow!("barrier_coord accept: set_read_timeout failed: {}", e))?;
    stream
        .set_write_timeout(Some(BARRIER_IO_TIMEOUT))
        .map_err(|e| anyhow!("barrier_coord accept: set_write_timeout failed: {}", e))?;

    let mut len_bytes = [0u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .map_err(|e| anyhow!("barrier_coord accept: handshake len read failed: {}", e))?;
    let len = u32::from_be_bytes(len_bytes);
    if len > 256 {
        return Err(anyhow!(
            "barrier_coord accept: handshake name length {} exceeds cap 256",
            len
        ));
    }
    let mut name_buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut name_buf)
        .map_err(|e| anyhow!("barrier_coord accept: handshake name read failed: {}", e))?;
    let peer_name = String::from_utf8(name_buf)
        .map_err(|e| anyhow!("barrier_coord accept: peer name not UTF-8: {}", e))?;
    if !expected.contains(&peer_name) {
        return Err(anyhow!(
            "barrier_coord accept: peer '{}' is not in expected runners set",
            peer_name
        ));
    }
    Ok((peer_name, stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU16;

    /// Allocate a unique port for each test to avoid conflicts. Reserve
    /// 16-port slots per test so the `base + BARRIER_TCP_OFFSET +
    /// index` derivation always fits.
    fn next_test_port() -> u16 {
        static PORT_COUNTER: AtomicU16 = AtomicU16::new(40000);
        PORT_COUNTER.fetch_add(16, Ordering::Relaxed)
    }

    #[test]
    fn single_runner_start_is_immediate() {
        let bc = BarrierCoordinator::new("solo".into(), vec!["solo".into()], next_test_port());
        assert!(bc.is_single_runner());
        let hosts = HashMap::new();
        bc.start(&hosts).expect("single-runner start is a no-op");
        // Broadcast must be a quick no-op too.
        let msg = Message::Ready {
            name: "solo".into(),
            variant: "v".into(),
            run: "r".into(),
        };
        assert!(bc.broadcast(&msg).is_empty());
        bc.shutdown();
    }

    #[test]
    fn two_runner_barrier_exchange_round_trips() {
        let port = next_test_port();
        let runners = vec!["alice".to_string(), "bob".to_string()];
        let mut hosts = HashMap::new();
        hosts.insert("alice".to_string(), "127.0.0.1".to_string());
        hosts.insert("bob".to_string(), "127.0.0.1".to_string());

        let bc_a = Arc::new(BarrierCoordinator::new(
            "alice".into(),
            runners.clone(),
            port,
        ));
        let bc_b = Arc::new(BarrierCoordinator::new("bob".into(), runners.clone(), port));

        let bc_a_clone = bc_a.clone();
        let hosts_a = hosts.clone();
        let start_a = std::thread::spawn(move || bc_a_clone.start(&hosts_a));
        let bc_b_clone = bc_b.clone();
        let hosts_b = hosts.clone();
        let start_b = std::thread::spawn(move || bc_b_clone.start(&hosts_b));
        start_a.join().unwrap().unwrap();
        start_b.join().unwrap().unwrap();

        // Each side broadcasts one Ready; the other side should receive
        // it on its peer's inbox.
        let ready_a = Message::Ready {
            name: "alice".into(),
            variant: "v1".into(),
            run: "r1".into(),
        };
        let ready_b = Message::Ready {
            name: "bob".into(),
            variant: "v1".into(),
            run: "r1".into(),
        };
        bc_a.broadcast(&ready_a);
        bc_b.broadcast(&ready_b);

        // Poll for arrival within a generous bound. Wide enough to
        // absorb scheduling jitter when this test runs in parallel
        // with the broader workspace test set on Windows; the
        // happy-path completes well under 200 ms on an idle host.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let a_inbox = bc_a.drain_inbox("bob");
            let b_inbox = bc_b.drain_inbox("alice");
            if !a_inbox.is_empty() && !b_inbox.is_empty() {
                // Verify content of one frame.
                assert!(matches!(
                    a_inbox[0],
                    Message::Ready { ref name, ref variant, .. } if name == "bob" && variant == "v1"
                ));
                assert!(matches!(
                    b_inbox[0],
                    Message::Ready { ref name, ref variant, .. } if name == "alice" && variant == "v1"
                ));
                break;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for cross-runner Ready exchange over barrier TCP");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        bc_a.shutdown();
        bc_b.shutdown();
    }

    /// Stress: many barrier rounds in sequence, each side broadcasts
    /// Ready and Done for each round, and both sides must drain a
    /// matching count from the other's inbox without loss.
    #[test]
    fn many_barriers_in_sequence_have_no_loss() {
        let port = next_test_port();
        let runners = vec!["alice".to_string(), "bob".to_string()];
        let mut hosts = HashMap::new();
        hosts.insert("alice".to_string(), "127.0.0.1".to_string());
        hosts.insert("bob".to_string(), "127.0.0.1".to_string());

        let bc_a = Arc::new(BarrierCoordinator::new(
            "alice".into(),
            runners.clone(),
            port,
        ));
        let bc_b = Arc::new(BarrierCoordinator::new("bob".into(), runners.clone(), port));

        let bc_a_clone = bc_a.clone();
        let hosts_a = hosts.clone();
        let start_a = std::thread::spawn(move || bc_a_clone.start(&hosts_a));
        let bc_b_clone = bc_b.clone();
        let hosts_b = hosts.clone();
        let start_b = std::thread::spawn(move || bc_b_clone.start(&hosts_b));
        start_a.join().unwrap().unwrap();
        start_b.join().unwrap().unwrap();

        // 100 rounds, each round = (Ready_a, Ready_b, Done_a, Done_b).
        // After all rounds bob's inbox from alice must contain exactly
        // 100 Ready frames + 100 Done frames; ditto for alice's inbox
        // from bob.
        const ROUNDS: usize = 100;
        for i in 0..ROUNDS {
            let variant = format!("v{i}");
            let run = "r1".to_string();
            bc_a.broadcast(&Message::Ready {
                name: "alice".into(),
                variant: variant.clone(),
                run: run.clone(),
            });
            bc_b.broadcast(&Message::Ready {
                name: "bob".into(),
                variant: variant.clone(),
                run: run.clone(),
            });
            bc_a.broadcast(&Message::Done {
                name: "alice".into(),
                variant: variant.clone(),
                run: run.clone(),
                status: "success".into(),
                exit_code: 0,
            });
            bc_b.broadcast(&Message::Done {
                name: "bob".into(),
                variant,
                run,
                status: "success".into(),
                exit_code: 0,
            });
        }

        // Drain inboxes until both sides have observed 2 * ROUNDS
        // messages from the peer.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut a_from_b: Vec<Message> = Vec::new();
        let mut b_from_a: Vec<Message> = Vec::new();
        while (a_from_b.len() < 2 * ROUNDS || b_from_a.len() < 2 * ROUNDS)
            && Instant::now() < deadline
        {
            a_from_b.extend(bc_a.drain_inbox("bob"));
            b_from_a.extend(bc_b.drain_inbox("alice"));
            if a_from_b.len() < 2 * ROUNDS || b_from_a.len() < 2 * ROUNDS {
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        assert_eq!(
            a_from_b.len(),
            2 * ROUNDS,
            "alice should observe exactly {} frames from bob, got {}",
            2 * ROUNDS,
            a_from_b.len()
        );
        assert_eq!(
            b_from_a.len(),
            2 * ROUNDS,
            "bob should observe exactly {} frames from alice, got {}",
            2 * ROUNDS,
            b_from_a.len()
        );

        bc_a.shutdown();
        bc_b.shutdown();
    }
}
