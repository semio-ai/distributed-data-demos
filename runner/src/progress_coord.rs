//! Cross-runner progress exchange transport (T15.3, epic E15).
//!
//! Each runner exchanges per-spawn [`Message::ProgressUpdate`] frames
//! with every other runner over a long-lived TCP per-peer-pair
//! connection. The transport mirrors T14.24's resume_manifest TCP
//! pattern (length-prefixed JSON frame; lower-sorted-name accepts,
//! higher connects; port derived from `--port + PROGRESS_TCP_OFFSET +
//! peer_index`) but uses a **dedicated** connection rather than reusing
//! the resume_manifest one. Rationale: the resume_manifest exchange is
//! one-shot during Phase 1.25 and closes after a single round-trip,
//! while progress exchange runs continuously across every Phase 2
//! spawn. Building a fresh connection here keeps the lifetimes
//! independent and lets the resume_manifest path stay as a simple
//! request/response.
//!
//! Wire model:
//!
//! - **Pairing** -- same rule as T14.24: for peer pair `(a, b)` with
//!   `a < b` (lexicographically by runner name), `a` accepts the
//!   inbound connection and `b` makes the outbound. Self-pairs do not
//!   exchange.
//! - **Port derivation** -- each runner listens on
//!   `base_port + PROGRESS_TCP_OFFSET + runner_index`. The constant
//!   [`PROGRESS_TCP_OFFSET`] (64) is chosen to leave a clear gap above
//!   the resume_manifest range (`base + 32 + index`, capped by
//!   `runners.len()`).
//! - **Framing** -- one length-prefixed JSON frame per snapshot:
//!   `[u32 BE length][JSON bytes]`. Frames above
//!   [`PROGRESS_FRAME_MAX_BYTES`] (64 KiB; far above any plausible
//!   `ProgressUpdate`) are rejected defensively.
//! - **Cadence** -- the spawn loop calls [`ProgressCoordinator::publish`]
//!   each tick (~1 Hz). The publisher does its own per-peer write on
//!   the caller's thread; reads run on a dedicated reader thread per
//!   peer connection that updates the shared
//!   [`RemoteProgressViewHandle`] in place.
//!
//! Lifecycle: a [`ProgressCoordinator`] is constructed after discovery
//! has populated `peer_hosts`, started via [`ProgressCoordinator::start`]
//! before the Phase 2 spawn loop, used across every spawn, and shut
//! down via [`ProgressCoordinator::shutdown`] before runner exit. Errors
//! on individual peer pairs are logged and the affected pair is marked
//! unhealthy; the runner continues with the peers it could reach. This
//! is a best-effort observability channel -- losing it does not abort
//! the run (T15.4's safety-net `max_spawn_secs` still terminates a
//! stuck spawn).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Result};

use crate::message::Message;
use crate::progress::{ProgressUpdateRef, RemoteProgressViewHandle};

/// Offset added to the base UDP coordination port to derive each
/// runner's TCP listener port for the T15.3 progress-update exchange.
///
/// The resume_manifest range sits at `base + 32 + index` for
/// `index in [0, runners.len())`. Placing progress at `base + 64 +
/// index` leaves a 32-port gap which is comfortably above any
/// realistic runner count (no benchmark today exceeds 8 runners). All
/// three ranges -- UDP coord at `base + index`, resume_manifest at
/// `base + 32 + index`, and progress at `base + 64 + index` -- sit
/// inside the same low ephemeral region operators already need to
/// permit for UDP coordination, so no new firewall rules are required.
pub const PROGRESS_TCP_OFFSET: u16 = 64;

/// Per-pair connect / accept poll timeout for the progress exchange
/// listener. Short enough to retry every few hundred ms when the
/// peer's listener has not bound yet; long enough that the connect
/// thread does not spin in a tight loop. The overall startup budget
/// is bounded by [`PROGRESS_STARTUP_BUDGET`].
const PROGRESS_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// I/O read/write timeout once a peer pair has connected. Each frame
/// is tiny (~200 bytes); a few seconds is plenty for any reasonable
/// LAN. A timeout here marks the pair unhealthy; the writer drops the
/// frame, and the reader thread will likewise exit on EOF and the
/// pair stops exchanging.
const PROGRESS_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on a single received progress frame. 64 KiB is well above
/// any plausible `ProgressUpdate` JSON (a few hundred bytes today) and
/// bounds worst-case memory if a peer lies about its length prefix.
const PROGRESS_FRAME_MAX_BYTES: u32 = 64 * 1024;

/// Overall budget for the initial accept-and-connect handshake with
/// every peer. After this budget elapses, peers that have not yet
/// connected are left in the unhealthy set; the runner proceeds
/// without their view. Generous enough to absorb the longest plausible
/// process-startup skew between runners on different machines.
const PROGRESS_STARTUP_BUDGET: Duration = Duration::from_secs(15);

/// Background coordinator that maintains a long-lived TCP connection
/// to every other runner, sending `ProgressUpdate` frames from this
/// runner and folding incoming frames into a shared
/// [`RemoteProgressViewHandle`].
///
/// Construct via [`ProgressCoordinator::new`], then call
/// [`ProgressCoordinator::start`] once `peer_hosts` is known (after
/// discovery). [`ProgressCoordinator::publish`] is called on every
/// progress tick from the spawn loop. [`ProgressCoordinator::shutdown`]
/// stops reader threads and closes connections cleanly before runner
/// exit.
///
/// In single-runner mode the coordinator is a no-op: `start` returns
/// immediately, `publish` is a quick early return, and `shutdown` has
/// nothing to do.
pub struct ProgressCoordinator {
    /// This runner's name.
    name: String,
    /// Ordered runner list (used to derive each peer's listener port).
    runners_order: Vec<String>,
    /// All expected runner names (used to filter accepted peer names).
    expected: std::collections::HashSet<String>,
    /// Base UDP coordination port; the progress port is derived from
    /// this plus [`PROGRESS_TCP_OFFSET`] plus the per-runner index.
    base_port: u16,
    /// Whether this is single-runner mode (no peers => no transport).
    single_runner: bool,
    /// Per-peer writer state, populated by `start()`. Keyed by peer
    /// runner name. Each entry owns a `TcpStream` clone wrapped in a
    /// mutex so `publish()` can serialise outbound frames per peer.
    writers: Mutex<HashMap<String, Arc<Mutex<TcpStream>>>>,
    /// Per-peer reader thread join handles.
    reader_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Signalled from `shutdown` to tell reader threads to exit. Reads
    /// are polled at the I/O timeout boundary; once true, the reader
    /// stops looping on its next wakeup.
    stop: Arc<AtomicBool>,
    /// Shared view that reader threads populate and the spawn loop
    /// reads via T15.4 (and via tests today).
    view: RemoteProgressViewHandle,
}

impl ProgressCoordinator {
    /// Build a coordinator. The `view` handle is what readers will
    /// update; callers retain a clone to read snapshots from.
    pub fn new(
        name: String,
        runners_order: Vec<String>,
        base_port: u16,
        view: RemoteProgressViewHandle,
    ) -> Self {
        let expected: std::collections::HashSet<String> = runners_order.iter().cloned().collect();
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
            view,
        }
    }

    /// Whether this coordinator has any peers to exchange with.
    pub fn is_single_runner(&self) -> bool {
        self.single_runner
    }

    /// Open per-peer TCP connections to every other runner using
    /// `peer_hosts` for host resolution. Lower-sorted-name accepts;
    /// higher connects. Blocks until every peer pair is connected, or
    /// the [`PROGRESS_STARTUP_BUDGET`] elapses. Peers that never appear
    /// are silently dropped (their snapshots will simply never enter
    /// the view); the runner is expected to make decisions on the
    /// peers it could reach.
    ///
    /// Spawns one reader thread per accepted/connected peer that runs
    /// for the lifetime of the coordinator. Errors on individual peers
    /// are logged to stderr and do not abort startup.
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
            .checked_add(PROGRESS_TCP_OFFSET)
            .and_then(|p| p.checked_add(my_index as u16))
            .ok_or_else(|| {
                anyhow!(
                    "base port {} + offset {} + index {} overflows u16",
                    self.base_port,
                    PROGRESS_TCP_OFFSET,
                    my_index
                )
            })?;
        let listener_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, my_listener_port);
        let listener = TcpListener::bind(listener_addr).map_err(|e| {
            anyhow!(
                "progress_coord: failed to bind TCP listener on {}: {}",
                listener_addr,
                e
            )
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| anyhow!("progress_coord: set_nonblocking failed: {}", e))?;

        let mut accept_pending: std::collections::HashSet<String> =
            to_accept.iter().cloned().collect();
        let mut connect_pending: std::collections::HashSet<String> =
            to_connect.iter().cloned().collect();

        let deadline = Instant::now() + PROGRESS_STARTUP_BUDGET;
        while !accept_pending.is_empty() || !connect_pending.is_empty() {
            if Instant::now() >= deadline {
                let still_missing: Vec<String> = accept_pending
                    .iter()
                    .chain(connect_pending.iter())
                    .cloned()
                    .collect();
                eprintln!(
                    "[runner:{}] progress_coord: startup budget {}s elapsed; \
                     proceeding without peer(s): {:?}",
                    self.name,
                    PROGRESS_STARTUP_BUDGET.as_secs(),
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
                    .checked_add(PROGRESS_TCP_OFFSET)
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
    /// Called both from the accept path (`accept_and_handshake`) and
    /// the connect path (`connect_and_handshake`).
    fn install_peer(&self, peer_name: &str, stream: TcpStream) {
        // Apply per-frame I/O timeouts so a wedged peer cannot block
        // the writer or reader indefinitely.
        let _ = stream.set_read_timeout(Some(PROGRESS_IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(PROGRESS_IO_TIMEOUT));

        // Clone the stream so we can hand the reader half to a
        // dedicated thread while keeping the writer half in the
        // coordinator's writer map. `TcpStream::try_clone` is the
        // standard way to get an independent fd referring to the same
        // socket.
        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[runner:{}] progress_coord: try_clone for peer '{}' failed: {}; \
                     reader thread not started",
                    self.name, peer_name, e
                );
                // Without a reader we can still send; install the
                // writer and continue.
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

        let stop = self.stop.clone();
        let view = self.view.clone();
        let runner_name_for_log = self.name.clone();
        let peer_for_thread = peer_name.to_string();
        let handle = std::thread::spawn(move || {
            run_reader_loop(
                reader_stream,
                view,
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

    /// Send a snapshot of the local progress tracker to every connected
    /// peer. Best-effort: per-peer write errors mark that peer's stream
    /// unhealthy (removed from the writer map) and are logged once. The
    /// caller continues regardless. In single-runner mode this is a
    /// no-op.
    ///
    /// Called at ~1 Hz from the spawn loop while a child is active.
    pub fn publish(
        &self,
        spawn: &str,
        phase: &str,
        sent: u64,
        received: u64,
        eot_sent: bool,
        eot_received: bool,
    ) {
        if self.single_runner {
            return;
        }
        let ts = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.9fZ")
            .to_string();
        let msg = Message::ProgressUpdate {
            runner: self.name.clone(),
            spawn: spawn.to_string(),
            phase: phase.to_string(),
            sent,
            received,
            eot_sent,
            eot_received,
            ts,
        };
        let payload = msg.to_bytes();

        // Snapshot the current writer set so we drop the lock before
        // the per-peer writes. Each value is an `Arc<Mutex<TcpStream>>`
        // which we can clone cheaply.
        let writers_snapshot: Vec<(String, Arc<Mutex<TcpStream>>)> = self
            .writers
            .lock()
            .expect("writers mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut failed: Vec<String> = Vec::new();
        for (peer, stream_arc) in writers_snapshot {
            let mut stream = stream_arc.lock().expect("per-peer stream mutex poisoned");
            if let Err(e) = write_progress_frame(&mut stream, &payload) {
                eprintln!(
                    "[runner:{}] progress_coord: write to peer '{}' failed: {}; \
                     removing peer from writer set",
                    self.name, peer, e
                );
                failed.push(peer);
            }
        }
        if !failed.is_empty() {
            let mut writers = self.writers.lock().expect("writers mutex poisoned");
            for peer in failed {
                writers.remove(&peer);
            }
        }
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

    /// Return a clone of the shared view handle (for callers that
    /// constructed the coordinator without retaining their own copy).
    /// Currently unused -- the runner threads through its own view
    /// clone -- but exposed so T15.4's idle detector can take a view
    /// off the coordinator if its construction shape changes.
    #[allow(dead_code)]
    pub fn view(&self) -> RemoteProgressViewHandle {
        self.view.clone()
    }
}

impl Drop for ProgressCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reader loop: read length-prefixed frames until EOF or `stop`.
fn run_reader_loop(
    mut stream: TcpStream,
    view: RemoteProgressViewHandle,
    stop: Arc<AtomicBool>,
    runner_name: &str,
    peer_name: &str,
) {
    while !stop.load(Ordering::Relaxed) {
        match read_progress_frame(&mut stream) {
            Ok(bytes) => {
                let msg = match Message::from_bytes(&bytes) {
                    Some(m) => m,
                    None => {
                        eprintln!(
                            "[runner:{}] progress_coord: malformed frame from peer '{}'; \
                             ignoring",
                            runner_name, peer_name
                        );
                        continue;
                    }
                };
                match msg {
                    Message::ProgressUpdate {
                        runner,
                        spawn,
                        phase,
                        sent,
                        received,
                        eot_sent,
                        eot_received,
                        ts,
                    } => {
                        view.apply_update(
                            ProgressUpdateRef {
                                runner: &runner,
                                spawn: &spawn,
                                phase: &phase,
                                sent,
                                received,
                                eot_sent,
                                eot_received,
                                ts: &ts,
                            },
                            SystemTime::now(),
                        );
                    }
                    other => {
                        eprintln!(
                            "[runner:{}] progress_coord: unexpected message type from peer \
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
                    "[runner:{}] progress_coord: read error from peer '{}': {}; \
                     reader thread exiting",
                    runner_name, peer_name, e
                );
                return;
            }
        }
    }
}

/// Length-prefixed write of a serialised payload over a connected
/// stream. Mirrors `write_manifest_frame` in `protocol.rs`.
fn write_progress_frame(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let len: u32 = payload.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

/// Read one length-prefixed payload from a stream. Mirrors
/// `read_manifest_frame` in `protocol.rs` but uses a smaller cap.
fn read_progress_frame(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes);
    if len > PROGRESS_FRAME_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "progress frame length {} exceeds cap {}",
                len, PROGRESS_FRAME_MAX_BYTES
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
        .map_err(|e| anyhow!("progress_coord connect: bad address '{}': {}", addr, e))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, PROGRESS_CONNECT_TIMEOUT)
        .map_err(|e| anyhow!("progress_coord connect: {} not reachable: {}", addr, e))?;
    stream
        .set_read_timeout(Some(PROGRESS_IO_TIMEOUT))
        .map_err(|e| anyhow!("progress_coord connect: set_read_timeout failed: {}", e))?;
    stream
        .set_write_timeout(Some(PROGRESS_IO_TIMEOUT))
        .map_err(|e| anyhow!("progress_coord connect: set_write_timeout failed: {}", e))?;

    // Handshake: send our name as raw UTF-8 bytes (length-prefixed),
    // distinct from a JSON `Message` frame so the accept side can
    // distinguish them. Names are simple identifiers; we cap at 256
    // bytes defensively.
    let name_bytes = self_name.as_bytes();
    if name_bytes.len() > 256 {
        return Err(anyhow!(
            "progress_coord connect: self name too long ({} bytes)",
            name_bytes.len()
        ));
    }
    let len: u32 = name_bytes.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| anyhow!("progress_coord connect: handshake write failed: {}", e))?;
    stream
        .write_all(name_bytes)
        .map_err(|e| anyhow!("progress_coord connect: handshake name write failed: {}", e))?;
    stream
        .flush()
        .map_err(|e| anyhow!("progress_coord connect: handshake flush failed: {}", e))?;

    Ok(stream)
}

/// One-shot handshake on the accepting side: read the peer's runner
/// name from the first length-prefixed frame, verify it is in the
/// expected set, and return the stream paired with the peer's name.
fn accept_and_handshake(
    mut stream: TcpStream,
    expected: &std::collections::HashSet<String>,
) -> Result<(String, TcpStream)> {
    // The accepted stream inherits the listener's nonblocking flag on
    // some platforms (notably Linux); explicitly switch to blocking
    // here so `read_exact` blocks instead of returning WouldBlock. The
    // per-frame I/O timeout bounds the wait.
    stream.set_nonblocking(false).map_err(|e| {
        anyhow!(
            "progress_coord accept: set_nonblocking(false) failed: {}",
            e
        )
    })?;
    stream
        .set_read_timeout(Some(PROGRESS_IO_TIMEOUT))
        .map_err(|e| anyhow!("progress_coord accept: set_read_timeout failed: {}", e))?;
    stream
        .set_write_timeout(Some(PROGRESS_IO_TIMEOUT))
        .map_err(|e| anyhow!("progress_coord accept: set_write_timeout failed: {}", e))?;

    let mut len_bytes = [0u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .map_err(|e| anyhow!("progress_coord accept: handshake len read failed: {}", e))?;
    let len = u32::from_be_bytes(len_bytes);
    if len > 256 {
        return Err(anyhow!(
            "progress_coord accept: handshake name length {} exceeds cap 256",
            len
        ));
    }
    let mut name_buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut name_buf)
        .map_err(|e| anyhow!("progress_coord accept: handshake name read failed: {}", e))?;
    let peer_name = String::from_utf8(name_buf)
        .map_err(|e| anyhow!("progress_coord accept: peer name not UTF-8: {}", e))?;
    if !expected.contains(&peer_name) {
        return Err(anyhow!(
            "progress_coord accept: peer '{}' is not in expected runners set",
            peer_name
        ));
    }
    Ok((peer_name, stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU16;

    /// Allocate a unique port for each test to avoid conflicts. The
    /// base ports here (35000+) sit well above the `protocol::tests`
    /// allocator (29800+) and any range its monotonic-increment
    /// counter could reach within a single workspace test invocation.
    /// We reserve 16-port slots per test so the
    /// `base + PROGRESS_TCP_OFFSET + index` derivation always fits.
    fn next_test_port() -> u16 {
        static PORT_COUNTER: AtomicU16 = AtomicU16::new(35000);
        PORT_COUNTER.fetch_add(16, Ordering::Relaxed)
    }

    #[test]
    fn single_runner_start_is_immediate() {
        let view = RemoteProgressViewHandle::new();
        let pc = ProgressCoordinator::new(
            "solo".into(),
            vec!["solo".into()],
            next_test_port(),
            view.clone(),
        );
        assert!(pc.is_single_runner());
        let hosts = HashMap::new();
        pc.start(&hosts).expect("single-runner start is a no-op");
        // Publish must be a quick no-op too.
        pc.publish("any", "operate", 1, 2, false, false);
        // View remains empty (no peers to report).
        assert_eq!(view.snapshot().peer_count(), 0);
        pc.shutdown();
    }

    #[test]
    fn two_runner_progress_exchange_round_trips() {
        let port = next_test_port();
        let runners = vec!["alice".to_string(), "bob".to_string()];
        let mut hosts = HashMap::new();
        hosts.insert("alice".to_string(), "127.0.0.1".to_string());
        hosts.insert("bob".to_string(), "127.0.0.1".to_string());

        let view_a = RemoteProgressViewHandle::new();
        let view_b = RemoteProgressViewHandle::new();

        let pc_a = Arc::new(ProgressCoordinator::new(
            "alice".into(),
            runners.clone(),
            port,
            view_a.clone(),
        ));
        let pc_b = Arc::new(ProgressCoordinator::new(
            "bob".into(),
            runners.clone(),
            port,
            view_b.clone(),
        ));

        let pc_a_clone = pc_a.clone();
        let hosts_a = hosts.clone();
        let start_a = std::thread::spawn(move || pc_a_clone.start(&hosts_a));
        let pc_b_clone = pc_b.clone();
        let hosts_b = hosts.clone();
        let start_b = std::thread::spawn(move || pc_b_clone.start(&hosts_b));
        start_a.join().unwrap().unwrap();
        start_b.join().unwrap().unwrap();

        // Each side publishes one update; wait briefly for the
        // reader threads on the other side to fold them in.
        pc_a.publish("sp1", "operate", 11, 22, false, false);
        pc_b.publish("sp1", "operate", 33, 44, false, false);

        // Poll for arrival within a generous bound.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let a_sees_b = view_a.snapshot().snapshot_for("bob", "sp1");
            let b_sees_a = view_b.snapshot().snapshot_for("alice", "sp1");
            if a_sees_b.is_some() && b_sees_a.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for cross-runner ProgressUpdate exchange. \
                     a_sees_b={:?} b_sees_a={:?}",
                    a_sees_b, b_sees_a
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let a_snap = view_a.snapshot().snapshot_for("bob", "sp1").unwrap();
        assert_eq!(a_snap.sent, 33);
        assert_eq!(a_snap.received, 44);
        assert_eq!(a_snap.phase, "operate");
        let b_snap = view_b.snapshot().snapshot_for("alice", "sp1").unwrap();
        assert_eq!(b_snap.sent, 11);
        assert_eq!(b_snap.received, 22);
        assert_eq!(b_snap.phase, "operate");

        pc_a.shutdown();
        pc_b.shutdown();
    }
}
