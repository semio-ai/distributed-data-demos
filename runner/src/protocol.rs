use crate::message::Message;
use anyhow::{bail, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
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
    /// UDP socket (None in single-runner mode since no network I/O is needed).
    socket: Option<Socket>,
    /// Addresses of all peer runners (including self for multicast, excluding
    /// self for unicast fallback). Each runner gets its own port to avoid
    /// Windows same-port delivery issues.
    peer_addrs: Vec<SocketAddr>,
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
            Some(create_coordination_socket(my_port)?)
        };

        Ok(Coordinator {
            name,
            expected,
            runners_order: runners.to_vec(),
            config_hash,
            run,
            proposed_log_subdir: log_subdir,
            socket,
            peer_addrs,
            single_runner,
        })
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

        let socket = self.socket.as_ref().unwrap();
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
        };

        loop {
            self.send(socket, &msg)?;

            let deadline = std::time::Instant::now() + BROADCAST_INTERVAL;
            while std::time::Instant::now() < deadline {
                if let Some(received) = self.recv(socket) {
                    // Accept any message type as proof that a peer exists.
                    // This handles the race where a fast peer has already
                    // moved past discovery and is sending Ready/Done messages.
                    let peer_name = match &received {
                        Message::Discover {
                            name,
                            config_hash,
                            log_subdir,
                        } => {
                            if self.expected.contains(name) && *config_hash != self.config_hash {
                                bail!(
                                    "config hash mismatch from runner '{}': expected {}, got {}",
                                    name,
                                    &self.config_hash[..8],
                                    &config_hash[..config_hash.len().min(8)]
                                );
                            }
                            // Capture the leader's log subfolder proposal.
                            if name == leader && leader_log_subdir.is_none() {
                                leader_log_subdir = Some(log_subdir.clone());
                            }
                            Some(name.clone())
                        }
                        Message::Ready { ref name, .. } => Some(name.clone()),
                        Message::Done { ref name, .. } => Some(name.clone()),
                    };
                    if let Some(name) = peer_name {
                        if self.expected.contains(&name) {
                            seen.insert(name);
                        }
                    }
                }
            }

            if seen == self.expected {
                // Linger: keep broadcasting Discover for 2 more seconds so
                // slower peers can complete their discovery phase.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Also drain incoming messages during linger to keep
                    // the socket buffer clean.
                    let _ = self.recv(socket);
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

        let socket = self.socket.as_ref().unwrap();
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
                if let Some(Message::Ready { name, variant, run }) = self.recv(socket) {
                    if variant == variant_name && run == self.run && self.expected.contains(&name) {
                        seen.insert(name);
                    }
                }
            }

            if seen == self.expected {
                // Linger: keep broadcasting Ready for 2 more seconds so
                // slower peers can complete their barrier.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Drain incoming messages to keep the socket buffer clean.
                    let _ = self.recv(socket);
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

        let socket = self.socket.as_ref().unwrap();
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
                if let Some(Message::Done {
                    name,
                    variant,
                    run,
                    status: s,
                    exit_code: c,
                }) = self.recv(socket)
                {
                    if variant == variant_name && run == self.run && self.expected.contains(&name) {
                        results.insert(name, (s, c));
                    }
                }
            }

            if results.len() == self.expected.len() {
                // Linger: keep broadcasting Done for 2 more seconds so
                // slower peers can complete their barrier.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Drain incoming messages to keep the socket buffer clean.
                    let _ = self.recv(socket);
                    std::thread::sleep(BROADCAST_INTERVAL);
                }
                return Ok(results);
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

    /// Try to receive a message from the socket. Returns None on timeout or
    /// parse failure.
    fn recv(&self, socket: &Socket) -> Option<Message> {
        let mut buf = [std::mem::MaybeUninit::uninit(); MAX_MSG_SIZE];
        match socket.recv(&mut buf) {
            Ok(n) => {
                // SAFETY: socket.recv guarantees the first `n` bytes are initialized.
                let data: Vec<u8> = buf[..n]
                    .iter()
                    .map(|b| unsafe { b.assume_init() })
                    .collect();
                Message::from_bytes(&data)
            }
            Err(_) => None,
        }
    }
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

    /// Allocate a unique port for each test to avoid conflicts when tests run in parallel.
    fn next_test_port() -> u16 {
        static PORT_COUNTER: AtomicU16 = AtomicU16::new(29800);
        PORT_COUNTER.fetch_add(1, Ordering::Relaxed)
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
        )
        .unwrap();
        assert!(coord.single_runner);
        let log_subdir = coord.discover().unwrap();
        assert_eq!(log_subdir, "run01-20260415_120000");
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
        )
        .unwrap();
        let results = coord.done_barrier("test-variant", "success", 0).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results.get("local"), Some(&("success".to_string(), 0)));
    }

    #[test]
    fn two_runner_localhost_coordination() {
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
            )
            .unwrap();

            let log_subdir = coord.discover().unwrap();
            coord.ready_barrier("v1").unwrap();
            let results = coord.done_barrier("v1", "success", 0).unwrap();
            (log_subdir, results)
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
            )
            .unwrap();

            let log_subdir = coord.discover().unwrap();
            coord.ready_barrier("v1").unwrap();
            let results = coord.done_barrier("v1", "success", 0).unwrap();
            (log_subdir, results)
        });

        let (log_subdir_a, results_a) = thread_a.join().unwrap();
        let (log_subdir_b, results_b) = thread_b.join().unwrap();

        // Both runners must agree on the leader's (runner_a) log subfolder.
        assert_eq!(log_subdir_a, "run-a-20260415_120000");
        assert_eq!(log_subdir_b, "run-a-20260415_120000");

        assert_eq!(results_a.len(), 2);
        assert_eq!(results_b.len(), 2);
        assert_eq!(results_a.get("runner_a"), Some(&("success".to_string(), 0)));
        assert_eq!(results_a.get("runner_b"), Some(&("success".to_string(), 0)));
    }

    #[test]
    fn config_hash_mismatch_detected() {
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
}
