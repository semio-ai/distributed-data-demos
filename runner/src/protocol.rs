use crate::clock_sync::{respond_to_probe, ClockSyncEngine};
use crate::local_addrs::canonical_peer_host;
use crate::message::Message;
use anyhow::{bail, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BROADCAST_INTERVAL: Duration = Duration::from_millis(500);
const RECV_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_MSG_SIZE: usize = 4096;

/// Multicast group for runner coordination (organization-local scope).
const COORDINATION_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 77, 66, 55);

/// Coordinator manages the UDP coordination protocol for runner synchronization.
pub struct Coordinator {
    /// This runner's name.
    name: String,
    /// All expected runner names.
    expected: HashSet<String>,
    /// The ordered runners list (to determine leader).
    runners_order: Vec<String>,
    /// Config hash for verification.
    config_hash: String,
    /// Run identifier for filtering stale messages from previous runs.
    run: String,
    /// This runner's proposed log subfolder.
    proposed_log_subdir: String,
    /// Whether this runner was launched with `--resume`.
    resume: bool,
    /// UDP socket (None in single-runner mode since no network I/O is needed).
    /// Wrapped in `Arc` so the `ClockSyncEngine` can share ownership without
    /// reopening the port.
    socket: Option<Arc<Socket>>,
    /// Addresses of all peer runners (including self for multicast, excluding
    /// self for unicast fallback). Each runner gets its own port to avoid
    /// Windows same-port delivery issues.
    peer_addrs: Vec<SocketAddr>,
    /// Peer host strings captured during discovery, keyed by runner name.
    /// Same-host peers (local interface IP or `127.0.0.1` source) are stored
    /// as the literal `"127.0.0.1"`. Always contains an entry for this
    /// runner itself (`127.0.0.1`). Wrapped in a Mutex so `discover()` can
    /// populate it through a shared reference.
    peer_hosts: Mutex<HashMap<String, String>>,
    /// Whether this is single-runner mode.
    single_runner: bool,
}

impl Coordinator {
    /// Create a new coordinator.
    ///
    /// In single-runner mode (only this runner in the expected set), no socket
    /// is created and all protocol methods return immediately.
    ///
    /// `log_subdir` is this runner's proposed log subfolder name. During
    /// discovery the leader's proposal (first runner in the config list) is
    /// adopted by all runners.
    pub fn new(
        name: String,
        runners: &[String],
        config_hash: String,
        port: u16,
        log_subdir: String,
        run: String,
        resume: bool,
    ) -> Result<Self> {
        let expected: HashSet<String> = runners.iter().cloned().collect();
        let single_runner = runners.len() == 1 && runners[0] == name;

        // Each runner gets its own port: base_port + index in runners list.
        // This avoids Windows issues where multiple processes on the same
        // UDP port don't reliably deliver packets to each other.
        let my_index = runners.iter().position(|r| r == &name).unwrap_or(0);
        let my_port = port + my_index as u16;

        // Build the list of all peer addresses to send to.
        // Each runner gets its own port (base + index). We send to each
        // peer's port via:
        //   1. Multicast group (works cross-machine on any LAN)
        //   2. Localhost fallback (always works for same-machine)
        let mut peer_addrs: Vec<SocketAddr> = Vec::new();
        for i in 0..runners.len() {
            let peer_port = port + i as u16;
            // Multicast for cross-machine discovery.
            peer_addrs.push(SocketAddr::V4(SocketAddrV4::new(
                COORDINATION_MULTICAST,
                peer_port,
            )));
            // Localhost fallback for same-machine runners.
            peer_addrs.push(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                peer_port,
            )));
        }

        let socket = if single_runner {
            None
        } else {
            Some(Arc::new(create_coordination_socket(my_port)?))
        };

        // Self-populate the peer_hosts map. Single-runner mode never receives
        // discovery messages from peers, so this is also the final state.
        // Multi-runner mode adds peer entries as Discover messages arrive.
        let mut peer_hosts: HashMap<String, String> = HashMap::new();
        peer_hosts.insert(name.clone(), "127.0.0.1".to_string());

        Ok(Coordinator {
            name,
            expected,
            runners_order: runners.to_vec(),
            config_hash,
            run,
            proposed_log_subdir: log_subdir,
            resume,
            socket,
            peer_addrs,
            peer_hosts: Mutex::new(peer_hosts),
            single_runner,
        })
    }

    /// Snapshot of the peer host map captured during discovery.
    ///
    /// Keys are runner names; values are the canonical host strings used in
    /// the runner-injected `--peers` CLI argument. Same-host peers appear as
    /// `"127.0.0.1"`. The local runner is always present.
    pub fn peer_hosts(&self) -> HashMap<String, String> {
        self.peer_hosts.lock().unwrap().clone()
    }

    /// Whether this coordinator is in single-runner mode (no peers, no socket).
    pub fn is_single_runner(&self) -> bool {
        self.single_runner
    }

    /// Build a `ClockSyncEngine` that shares the coordination socket with this
    /// coordinator. Returns `None` in single-runner mode (no socket exists).
    ///
    /// The engine and the coordinator must NOT be used concurrently from
    /// different threads — the runner's main loop only invokes one or the
    /// other at a time, so single-threaded sequential use is safe.
    pub fn clock_sync_engine(&self) -> Option<ClockSyncEngine> {
        let socket = self.socket.as_ref()?.clone();
        Some(ClockSyncEngine::new(
            self.name.clone(),
            socket,
            self.peer_addrs.clone(),
        ))
    }

    /// Run the discovery phase.
    ///
    /// Broadcasts Discover messages until all expected runners have been seen
    /// with matching config hashes. After all peers are found, continues
    /// broadcasting for a linger period so slower peers can also complete
    /// their discovery.
    ///
    /// Returns the agreed-upon log subfolder name. The leader (first runner in
    /// the config's `runners` list) decides the subfolder; all other runners
    /// adopt the leader's proposal.
    ///
    /// In single-runner mode, returns own proposal immediately.
    pub fn discover(&self) -> Result<String> {
        if self.single_runner {
            return Ok(self.proposed_log_subdir.clone());
        }

        let socket = self.socket.as_deref().unwrap();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(self.name.clone());

        // Track the leader's proposed log subfolder.
        let leader = &self.runners_order[0];
        let mut leader_log_subdir: Option<String> = if *leader == self.name {
            Some(self.proposed_log_subdir.clone())
        } else {
            None
        };

        let msg = Message::Discover {
            name: self.name.clone(),
            config_hash: self.config_hash.clone(),
            log_subdir: self.proposed_log_subdir.clone(),
            resume: self.resume,
        };

        loop {
            self.send(socket, &msg)?;

            let deadline = std::time::Instant::now() + BROADCAST_INTERVAL;
            while std::time::Instant::now() < deadline {
                if let Some((received, src_ip)) = self.recv_from(socket) {
                    // Accept any message type as proof that a peer exists.
                    // This handles the race where a fast peer has already
                    // moved past discovery and is sending Ready/Done messages.
                    let peer_name = match &received {
                        Message::Discover {
                            name,
                            config_hash,
                            log_subdir,
                            resume,
                        } => {
                            if self.expected.contains(name) && *config_hash != self.config_hash {
                                bail!(
                                    "config hash mismatch from runner '{}': expected {}, got {}",
                                    name,
                                    &self.config_hash[..8],
                                    &config_hash[..config_hash.len().min(8)]
                                );
                            }
                            // Resume-mode agreement: every peer must report the
                            // same `resume` flag value. Mixing resume and fresh
                            // runs in the same coordination group is incoherent
                            // (the fresh runner would create a new log subfolder
                            // while the resume runner would reuse an existing
                            // one).
                            if self.expected.contains(name) && *resume != self.resume {
                                bail!(
                                    "resume-flag mismatch from runner '{}': expected {}, got {}",
                                    name,
                                    self.resume,
                                    resume
                                );
                            }
                            // Capture the leader's log subfolder proposal.
                            if name == leader && leader_log_subdir.is_none() {
                                leader_log_subdir = Some(log_subdir.clone());
                            }
                            Some(name.clone())
                        }
                        Message::ResumeManifest { ref name, .. } => Some(name.clone()),
                        Message::Ready { ref name, .. } => Some(name.clone()),
                        Message::Done { ref name, .. } => Some(name.clone()),
                        Message::ProbeRequest { from, to, id, t1 } => {
                            // Always-respond rule: even mid-discovery, a peer
                            // probing us must get a prompt reply. Discovery
                            // is rare here (peers usually probe after Phase
                            // 1.5) but is included for completeness.
                            if to == &self.name {
                                let _ = respond_to_probe(
                                    socket,
                                    &self.peer_addrs,
                                    &self.name,
                                    from,
                                    *id,
                                    t1,
                                );
                            }
                            None
                        }
                        Message::ProbeResponse { .. } => None,
                    };
                    if let Some(name) = peer_name {
                        if self.expected.contains(&name) {
                            seen.insert(name.clone());
                            // Capture the peer's host. Same-host peers (local
                            // interface or 127.0.0.1 source) collapse to
                            // "127.0.0.1". Skip self-loopback echoes -- self
                            // was pre-populated with "127.0.0.1" at construction.
                            if name != self.name {
                                let host = canonical_peer_host(src_ip);
                                let mut guard = self.peer_hosts.lock().unwrap();
                                guard.entry(name).or_insert(host);
                            }
                        }
                    }
                }
            }

            // Discovery completes only when every expected runner has been
            // seen AND has an entry in peer_hosts (which is populated above
            // for peers and at construction for self).
            let hosts_known = {
                let guard = self.peer_hosts.lock().unwrap();
                self.expected.iter().all(|n| guard.contains_key(n))
            };
            if seen == self.expected && hosts_known {
                // Linger: keep broadcasting Discover for 2 more seconds so
                // slower peers can complete their discovery phase.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Also drain incoming messages during linger to keep
                    // the socket buffer clean.
                    self.drain_and_answer_probe(socket);
                    std::thread::sleep(BROADCAST_INTERVAL);
                }

                // Return the leader's log subfolder. We always have it at
                // this point because (a) if we are the leader we set it
                // above, or (b) we received the leader's Discover message.
                return Ok(
                    leader_log_subdir.expect("leader log_subdir should be known after discovery")
                );
            }
        }
    }

    /// Ready barrier for a specific variant.
    ///
    /// Broadcasts Ready and waits until all runners have signaled ready.
    /// In single-runner mode, returns immediately.
    pub fn ready_barrier(&self, variant_name: &str) -> Result<()> {
        if self.single_runner {
            return Ok(());
        }

        let socket = self.socket.as_deref().unwrap();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(self.name.clone());

        let msg = Message::Ready {
            name: self.name.clone(),
            variant: variant_name.to_string(),
            run: self.run.clone(),
        };

        loop {
            self.send(socket, &msg)?;

            let deadline = std::time::Instant::now() + BROADCAST_INTERVAL;
            while std::time::Instant::now() < deadline {
                match self.recv(socket) {
                    Some(Message::Ready { name, variant, run }) => {
                        if variant == variant_name
                            && run == self.run
                            && self.expected.contains(&name)
                        {
                            seen.insert(name);
                        }
                    }
                    Some(Message::ProbeRequest { from, to, id, t1 }) => {
                        if to == self.name {
                            let _ = respond_to_probe(
                                socket,
                                &self.peer_addrs,
                                &self.name,
                                &from,
                                id,
                                &t1,
                            );
                        }
                    }
                    _ => {}
                }
            }

            if seen == self.expected {
                // Linger: keep broadcasting Ready for 2 more seconds so
                // slower peers can complete their barrier.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Drain incoming messages to keep the socket buffer clean.
                    self.drain_and_answer_probe(socket);
                    std::thread::sleep(BROADCAST_INTERVAL);
                }
                return Ok(());
            }
        }
    }

    /// Done barrier for a specific variant.
    ///
    /// Broadcasts Done with this runner's outcome and waits until all runners
    /// have reported. Returns a map of runner_name -> (status, exit_code).
    /// In single-runner mode, returns immediately with own result.
    pub fn done_barrier(
        &self,
        variant_name: &str,
        status: &str,
        exit_code: i32,
    ) -> Result<HashMap<String, (String, i32)>> {
        let mut results: HashMap<String, (String, i32)> = HashMap::new();
        results.insert(self.name.clone(), (status.to_string(), exit_code));

        if self.single_runner {
            return Ok(results);
        }

        let socket = self.socket.as_deref().unwrap();
        let msg = Message::Done {
            name: self.name.clone(),
            variant: variant_name.to_string(),
            run: self.run.clone(),
            status: status.to_string(),
            exit_code,
        };

        loop {
            self.send(socket, &msg)?;

            let deadline = std::time::Instant::now() + BROADCAST_INTERVAL;
            while std::time::Instant::now() < deadline {
                match self.recv(socket) {
                    Some(Message::Done {
                        name,
                        variant,
                        run,
                        status: s,
                        exit_code: c,
                    }) => {
                        if variant == variant_name
                            && run == self.run
                            && self.expected.contains(&name)
                        {
                            results.insert(name, (s, c));
                        }
                    }
                    Some(Message::ProbeRequest { from, to, id, t1 }) => {
                        if to == self.name {
                            let _ = respond_to_probe(
                                socket,
                                &self.peer_addrs,
                                &self.name,
                                &from,
                                id,
                                &t1,
                            );
                        }
                    }
                    _ => {}
                }
            }

            if results.len() == self.expected.len() {
                // Linger: keep broadcasting Done for 2 more seconds so
                // slower peers can complete their barrier.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Drain incoming messages to keep the socket buffer clean.
                    self.drain_and_answer_probe(socket);
                    std::thread::sleep(BROADCAST_INTERVAL);
                }
                return Ok(results);
            }
        }
    }

    /// Exchange `ResumeManifest` messages with all peers (Phase 1.25).
    ///
    /// Each runner has already computed its local `complete_jobs` list
    /// (effective_names whose log file exists locally and is non-empty).
    /// This method broadcasts the local manifest, listens for one from
    /// every peer in `runners`, and returns a `HashMap<runner_name,
    /// complete_jobs>` containing every peer's report keyed by name.
    /// This runner's own manifest is also included in the returned map.
    ///
    /// Periodic re-broadcast every 500 ms mirrors the discovery loss-
    /// recovery pattern. Probe requests addressed to this runner are still
    /// answered while waiting (the always-respond rule). In single-runner
    /// mode this method is a no-op and returns a map containing only the
    /// caller's own manifest.
    pub fn exchange_resume_manifest(
        &self,
        local_complete_jobs: Vec<String>,
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut all: HashMap<String, Vec<String>> = HashMap::new();
        all.insert(self.name.clone(), local_complete_jobs.clone());

        if self.single_runner {
            return Ok(all);
        }

        let socket = self.socket.as_deref().unwrap();
        let msg = Message::ResumeManifest {
            name: self.name.clone(),
            run: self.run.clone(),
            complete_jobs: local_complete_jobs,
        };

        loop {
            self.send(socket, &msg)?;

            let deadline = std::time::Instant::now() + BROADCAST_INTERVAL;
            while std::time::Instant::now() < deadline {
                match self.recv(socket) {
                    Some(Message::ResumeManifest {
                        name,
                        run,
                        complete_jobs,
                    }) => {
                        // Defensive: drop messages from a different run id.
                        // After discovery agreement these should not exist,
                        // but a stale broadcast from a previous run could
                        // theoretically arrive on the wire.
                        if run == self.run && self.expected.contains(&name) {
                            all.entry(name).or_insert(complete_jobs);
                        }
                    }
                    Some(Message::ProbeRequest { from, to, id, t1 }) => {
                        if to == self.name {
                            let _ = respond_to_probe(
                                socket,
                                &self.peer_addrs,
                                &self.name,
                                &from,
                                id,
                                &t1,
                            );
                        }
                    }
                    _ => {}
                }
            }

            if all.len() == self.expected.len() {
                // Linger: keep broadcasting so slower peers can collect
                // ours after they finish their own waits.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    self.drain_and_answer_probe(socket);
                    std::thread::sleep(BROADCAST_INTERVAL);
                }
                return Ok(all);
            }
        }
    }

    /// Send a message to all peer runner ports via UDP broadcast.
    fn send(&self, socket: &Socket, msg: &Message) -> Result<()> {
        let data = msg.to_bytes();
        for addr in &self.peer_addrs {
            // Ignore send errors for individual peers (they may not be up yet).
            let _ = socket.send_to(&data, &(*addr).into());
        }
        Ok(())
    }

    /// Drain a single inbound message while still answering probe requests.
    /// Used during linger phases where we just want to keep the socket buffer
    /// clean but must not drop probes silently.
    fn drain_and_answer_probe(&self, socket: &Socket) {
        if let Some(Message::ProbeRequest { from, to, id, t1 }) = self.recv(socket) {
            if to == self.name {
                let _ = respond_to_probe(socket, &self.peer_addrs, &self.name, &from, id, &t1);
            }
        }
    }

    /// Try to receive a message from the socket. Returns None on timeout or
    /// parse failure.
    fn recv(&self, socket: &Socket) -> Option<Message> {
        self.recv_from(socket).map(|(msg, _src)| msg)
    }

    /// Try to receive a message and the source address from the socket.
    /// Returns None on timeout, parse failure, or if the source address is
    /// not an IPv4/IPv6 address.
    fn recv_from(&self, socket: &Socket) -> Option<(Message, IpAddr)> {
        let mut buf = [std::mem::MaybeUninit::uninit(); MAX_MSG_SIZE];
        match socket.recv_from(&mut buf) {
            Ok((n, sock_addr)) => {
                // SAFETY: socket.recv_from guarantees the first `n` bytes are initialized.
                let data: Vec<u8> = buf[..n]
                    .iter()
                    .map(|b| unsafe { b.assume_init() })
                    .collect();
                let msg = Message::from_bytes(&data)?;
                let src_ip = sockaddr_to_ip(&sock_addr)?;
                Some((msg, src_ip))
            }
            Err(_) => None,
        }
    }
}

/// Extract an `IpAddr` from a socket2 `SockAddr`.
fn sockaddr_to_ip(sa: &SockAddr) -> Option<IpAddr> {
    sa.as_socket().map(|sock| sock.ip())
}

/// Create a UDP socket for runner coordination.
///
/// Each runner gets a unique port (base + index), so there is no port
/// contention between processes. The socket joins a multicast group for
/// cross-machine discovery and also accepts localhost datagrams for
/// same-machine fallback.
fn create_coordination_socket(port: u16) -> Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_read_timeout(Some(RECV_TIMEOUT))?;
    socket.set_nonblocking(false)?;

    // Bind to INADDR_ANY so we receive both multicast and localhost traffic.
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

    // Join the coordination multicast group to receive cross-machine messages.
    socket.join_multicast_v4(&COORDINATION_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_loop_v4(true)?;

    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Allocate a unique port for each test to avoid conflicts when tests run in parallel.
    fn next_test_port() -> u16 {
        static PORT_COUNTER: AtomicU16 = AtomicU16::new(29800);
        PORT_COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    /// Serialize multicast-using tests on Windows.
    ///
    /// Multiple tests joining the same multicast group simultaneously (each
    /// with two-thread coordination loops) can exhaust Windows multicast
    /// resources and cause `recv_from` to drop packets reliably enough that
    /// the `discover()` bail-on-mismatch tests never see the peer's first
    /// Discover and hang indefinitely. Per-test unique ports avoid port
    /// collisions but not the global multicast-membership pressure. Holding
    /// this mutex around the test body ensures only one multicast cohort is
    /// active at a time. Single-runner tests do not need this since they
    /// do not bind a socket.
    fn multicast_test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn single_runner_discover_is_immediate() {
        let coord = Coordinator::new(
            "local".into(),
            &["local".to_string()],
            "somehash".into(),
            0, // port unused in single-runner
            "run01-20260415_120000".into(),
            "run01".into(),
            false,
        )
        .unwrap();
        assert!(coord.single_runner);
        let log_subdir = coord.discover().unwrap();
        assert_eq!(log_subdir, "run01-20260415_120000");
        // Self-population: peer_hosts contains this runner mapped to 127.0.0.1.
        let hosts = coord.peer_hosts();
        assert_eq!(hosts.get("local"), Some(&"127.0.0.1".to_string()));
        assert_eq!(hosts.len(), 1);
    }

    #[test]
    fn single_runner_ready_barrier_is_immediate() {
        let coord = Coordinator::new(
            "local".into(),
            &["local".to_string()],
            "somehash".into(),
            0,
            "run01-20260415_120000".into(),
            "run01".into(),
            false,
        )
        .unwrap();
        coord.ready_barrier("test-variant").unwrap();
    }

    #[test]
    fn single_runner_done_barrier_returns_own_result() {
        let coord = Coordinator::new(
            "local".into(),
            &["local".to_string()],
            "somehash".into(),
            0,
            "run01-20260415_120000".into(),
            "run01".into(),
            false,
        )
        .unwrap();
        let results = coord.done_barrier("test-variant", "success", 0).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results.get("local"), Some(&("success".to_string(), 0)));
    }

    #[test]
    fn two_runner_localhost_coordination() {
        let _guard = multicast_test_lock();
        let port = next_test_port();

        let hash = "testhash123".to_string();
        let runners = vec!["runner_a".to_string(), "runner_b".to_string()];

        let hash_a = hash.clone();
        let runners_a = runners.clone();
        let thread_a = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "runner_a".into(),
                &runners_a,
                hash_a,
                port,
                "run-a-20260415_120000".into(),
                "test-run".into(),
                false,
            )
            .unwrap();

            let log_subdir = coord.discover().unwrap();
            let hosts = coord.peer_hosts();
            coord.ready_barrier("v1").unwrap();
            let results = coord.done_barrier("v1", "success", 0).unwrap();
            (log_subdir, results, hosts)
        });

        let hash_b = hash;
        let runners_b = runners;
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "runner_b".into(),
                &runners_b,
                hash_b,
                port,
                "run-b-20260415_120001".into(),
                "test-run".into(),
                false,
            )
            .unwrap();

            let log_subdir = coord.discover().unwrap();
            let hosts = coord.peer_hosts();
            coord.ready_barrier("v1").unwrap();
            let results = coord.done_barrier("v1", "success", 0).unwrap();
            (log_subdir, results, hosts)
        });

        let (log_subdir_a, results_a, hosts_a) = thread_a.join().unwrap();
        let (log_subdir_b, results_b, hosts_b) = thread_b.join().unwrap();

        // Both runners must agree on the leader's (runner_a) log subfolder.
        assert_eq!(log_subdir_a, "run-a-20260415_120000");
        assert_eq!(log_subdir_b, "run-a-20260415_120000");

        assert_eq!(results_a.len(), 2);
        assert_eq!(results_b.len(), 2);
        assert_eq!(results_a.get("runner_a"), Some(&("success".to_string(), 0)));
        assert_eq!(results_a.get("runner_b"), Some(&("success".to_string(), 0)));

        // Peer host capture: both runners must have entries for both names,
        // and since both ran on the same machine, every host must be 127.0.0.1.
        assert_eq!(hosts_a.len(), 2, "runner_a should have both peer entries");
        assert_eq!(hosts_b.len(), 2, "runner_b should have both peer entries");
        assert_eq!(hosts_a.get("runner_a"), Some(&"127.0.0.1".to_string()));
        assert_eq!(hosts_a.get("runner_b"), Some(&"127.0.0.1".to_string()));
        assert_eq!(hosts_b.get("runner_a"), Some(&"127.0.0.1".to_string()));
        assert_eq!(hosts_b.get("runner_b"), Some(&"127.0.0.1".to_string()));
    }

    #[test]
    fn config_hash_mismatch_detected() {
        let _guard = multicast_test_lock();
        let port = next_test_port();
        let runners = vec!["a".to_string(), "b".to_string()];

        let runners_a = runners.clone();
        let thread_a = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "a".into(),
                &runners_a,
                "hash_AAAA".into(),
                port,
                "run-20260415_120000".into(),
                "test-run".into(),
                false,
            )
            .unwrap();
            coord.discover()
        });

        let runners_b = runners;
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "b".into(),
                &runners_b,
                "hash_BBBB".into(),
                port,
                "run-20260415_120001".into(),
                "test-run".into(),
                false,
            )
            .unwrap();
            coord.discover()
        });

        let result_a = thread_a.join().unwrap();
        let result_b = thread_b.join().unwrap();

        let any_mismatch = result_a.is_err() || result_b.is_err();
        assert!(any_mismatch, "expected config hash mismatch to be detected");

        if let Err(e) = &result_a {
            assert!(e.to_string().contains("config hash mismatch"));
        }
        if let Err(e) = &result_b {
            assert!(e.to_string().contains("config hash mismatch"));
        }
    }

    #[test]
    fn stale_ready_from_different_run_is_ignored() {
        use std::sync::{Arc, Barrier};

        let _guard = multicast_test_lock();
        let port = next_test_port();
        let runners = vec!["runner_a".to_string(), "runner_b".to_string()];

        // runner_a binds on port + 0.
        let runner_a_port = port;

        // Use a barrier to synchronize: the thread creates the Coordinator
        // (binding the socket), then signals so we can inject the stale
        // message before calling ready_barrier.
        let sync = Arc::new(Barrier::new(2));
        let sync_clone = Arc::clone(&sync);

        let runners_for_a = runners.clone();
        let barrier_handle = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "runner_a".into(),
                &runners_for_a,
                "hash".into(),
                port,
                "log-subdir".into(),
                "new-run".into(),
                false,
            )
            .unwrap();

            // Signal that the socket is bound and ready to receive.
            sync_clone.wait();

            coord.ready_barrier("v1")
        });

        // Wait until the Coordinator's socket is bound.
        sync.wait();

        // Phase 1: Send a stale Ready from runner_b with old run ID.
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let stale_msg = Message::Ready {
            name: "runner_b".into(),
            variant: "v1".into(),
            run: "old-run".into(),
        };
        sender
            .send_to(&stale_msg.to_bytes(), format!("127.0.0.1:{runner_a_port}"))
            .unwrap();

        // Phase 2: Wait long enough that the barrier would have completed
        // if the stale message was incorrectly accepted.
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !barrier_handle.is_finished(),
            "barrier should NOT have completed from stale message with different run ID"
        );

        // Phase 3: Send the correct Ready to unblock the barrier.
        let correct_msg = Message::Ready {
            name: "runner_b".into(),
            variant: "v1".into(),
            run: "new-run".into(),
        };
        sender
            .send_to(
                &correct_msg.to_bytes(),
                format!("127.0.0.1:{runner_a_port}"),
            )
            .unwrap();

        // The barrier should now complete within a reasonable time.
        let result = barrier_handle.join().unwrap();
        assert!(result.is_ok(), "barrier should succeed after correct Ready");
    }

    #[test]
    fn barrier_linger_prevents_slow_peer_hang() {
        // Verify that the linger period in ready_barrier and done_barrier
        // allows a slow peer to complete even when the fast peer finishes
        // the barrier first. Without linger, the fast peer would stop
        // broadcasting and the slow peer would hang forever.
        let _guard = multicast_test_lock();
        let port = next_test_port();
        let hash = "lingerhash".to_string();
        let runners = vec!["a".to_string(), "b".to_string()];

        let hash_a = hash.clone();
        let runners_a = runners.clone();
        // Runner "b" starts immediately; runner "a" is delayed so "b" will
        // see all peers first. The linger on "b" must keep it broadcasting
        // long enough for the delayed "a" to also complete.
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "b".into(),
                &runners_a,
                hash_a,
                port,
                "log-sub".into(),
                "linger-run".into(),
                false,
            )
            .unwrap();

            coord.ready_barrier("v1").unwrap();
            coord.done_barrier("v1", "success", 0).unwrap();
        });

        let hash_b = hash;
        let runners_b = runners;
        let thread_a = std::thread::spawn(move || {
            // Delay so "b" enters and potentially completes the barrier first.
            std::thread::sleep(Duration::from_millis(800));

            let coord = Coordinator::new(
                "a".into(),
                &runners_b,
                hash_b,
                port,
                "log-sub".into(),
                "linger-run".into(),
                false,
            )
            .unwrap();

            coord.ready_barrier("v1").unwrap();
            coord.done_barrier("v1", "success", 0).unwrap();
        });

        // Use a timeout to detect hangs: both threads must finish within 10 seconds.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);

        let result_b = thread_b.join();
        assert!(
            std::time::Instant::now() < deadline,
            "runner b hung past the 10-second deadline"
        );
        result_b.expect("runner b thread panicked");

        let result_a = thread_a.join();
        assert!(
            std::time::Instant::now() < deadline,
            "runner a hung past the 10-second deadline"
        );
        result_a.expect("runner a thread panicked");
    }

    #[test]
    fn resume_flag_mismatch_aborts_discovery() {
        // Resume is an all-or-nothing property: a runner with --resume must
        // refuse to coordinate with a peer that does not have --resume.
        // Use runner names unique to this test (rfm_a/rfm_b) so we don't
        // share names with other tests that also exchange Discover messages
        // on the same multicast group.
        let _guard = multicast_test_lock();
        let port = next_test_port();
        let runners = vec!["rfm_a".to_string(), "rfm_b".to_string()];
        let hash = "rfm_hash_matching".to_string();

        let runners_a = runners.clone();
        let hash_a = hash.clone();
        let thread_a = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "rfm_a".into(),
                &runners_a,
                hash_a,
                port,
                "rfm-sub".into(),
                "rfm-run".into(),
                true, // resume
            )
            .unwrap();
            coord.discover()
        });

        let runners_b = runners;
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "rfm_b".into(),
                &runners_b,
                hash,
                port,
                "rfm-sub".into(),
                "rfm-run".into(),
                false, // fresh
            )
            .unwrap();
            coord.discover()
        });

        let result_a = thread_a.join().unwrap();
        let result_b = thread_b.join().unwrap();
        let any_mismatch = result_a.is_err() || result_b.is_err();
        assert!(any_mismatch, "expected resume-flag mismatch to abort");
        if let Err(e) = &result_a {
            assert!(
                e.to_string().contains("resume-flag mismatch"),
                "expected resume-flag mismatch error in a, got: {e}"
            );
        }
        if let Err(e) = &result_b {
            assert!(
                e.to_string().contains("resume-flag mismatch"),
                "expected resume-flag mismatch error in b, got: {e}"
            );
        }
    }

    #[test]
    fn single_runner_resume_manifest_exchange_is_local_only() {
        let coord = Coordinator::new(
            "local".into(),
            &["local".to_string()],
            "h".into(),
            0,
            "sub".into(),
            "run01".into(),
            true,
        )
        .unwrap();
        let local_manifest = vec!["v1".to_string(), "v2".to_string()];
        let all = coord
            .exchange_resume_manifest(local_manifest.clone())
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all.get("local"), Some(&local_manifest));
    }

    #[test]
    fn two_runner_resume_manifest_exchange() {
        // End-to-end: two runners on localhost exchange manifests and each
        // ends up with both peers' lists.
        let _guard = multicast_test_lock();
        let port = next_test_port();
        let runners = vec!["ra".to_string(), "rb".to_string()];
        let hash = "rmhash".to_string();

        let runners_a = runners.clone();
        let hash_a = hash.clone();
        let thread_a = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "ra".into(),
                &runners_a,
                hash_a,
                port,
                "sub".into(),
                "run01".into(),
                true,
            )
            .unwrap();
            coord.discover().unwrap();
            coord.exchange_resume_manifest(vec!["v1".into(), "v2".into()])
        });

        let runners_b = runners;
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "rb".into(),
                &runners_b,
                hash,
                port,
                "sub".into(),
                "run01".into(),
                true,
            )
            .unwrap();
            coord.discover().unwrap();
            coord.exchange_resume_manifest(vec!["v2".into(), "v3".into()])
        });

        let res_a = thread_a.join().unwrap().unwrap();
        let res_b = thread_b.join().unwrap().unwrap();

        assert_eq!(res_a.len(), 2);
        assert_eq!(res_b.len(), 2);
        assert_eq!(
            res_a.get("ra"),
            Some(&vec!["v1".to_string(), "v2".to_string()])
        );
        assert_eq!(
            res_a.get("rb"),
            Some(&vec!["v2".to_string(), "v3".to_string()])
        );
        assert_eq!(
            res_b.get("ra"),
            Some(&vec!["v1".to_string(), "v2".to_string()])
        );
        assert_eq!(
            res_b.get("rb"),
            Some(&vec!["v2".to_string(), "v3".to_string()])
        );
    }
}
