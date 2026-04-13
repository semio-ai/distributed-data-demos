use crate::message::Message;
use anyhow::{bail, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

const BROADCAST_INTERVAL: Duration = Duration::from_millis(500);
const RECV_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_MSG_SIZE: usize = 4096;

/// Coordinator manages the UDP coordination protocol for runner synchronization.
pub struct Coordinator {
    /// This runner's name.
    name: String,
    /// All expected runner names.
    expected: HashSet<String>,
    /// Config hash for verification.
    config_hash: String,
    /// UDP socket (None in single-runner mode since no network I/O is needed).
    socket: Option<Socket>,
    /// Broadcast address.
    broadcast_addr: SocketAddr,
    /// Whether this is single-runner mode.
    single_runner: bool,
}

impl Coordinator {
    /// Create a new coordinator.
    ///
    /// In single-runner mode (only this runner in the expected set), no socket
    /// is created and all protocol methods return immediately.
    pub fn new(name: String, runners: &[String], config_hash: String, port: u16) -> Result<Self> {
        let expected: HashSet<String> = runners.iter().cloned().collect();
        let single_runner = runners.len() == 1 && runners[0] == name;
        let broadcast_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, port));

        let socket = if single_runner {
            None
        } else {
            Some(create_broadcast_socket(port)?)
        };

        Ok(Coordinator {
            name,
            expected,
            config_hash,
            socket,
            broadcast_addr,
            single_runner,
        })
    }

    /// Run the discovery phase.
    ///
    /// Broadcasts Discover messages until all expected runners have been seen
    /// with matching config hashes. In single-runner mode, returns immediately.
    pub fn discover(&self) -> Result<()> {
        if self.single_runner {
            return Ok(());
        }

        let socket = self.socket.as_ref().unwrap();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(self.name.clone());

        let msg = Message::Discover {
            name: self.name.clone(),
            config_hash: self.config_hash.clone(),
        };

        loop {
            self.send(socket, &msg)?;

            let deadline = std::time::Instant::now() + BROADCAST_INTERVAL;
            while std::time::Instant::now() < deadline {
                if let Some(Message::Discover { name, config_hash }) = self.recv(socket) {
                    if self.expected.contains(&name) {
                        if config_hash != self.config_hash {
                            bail!(
                                "config hash mismatch from runner '{}': expected {}, got {}",
                                name,
                                &self.config_hash[..8],
                                &config_hash[..config_hash.len().min(8)]
                            );
                        }
                        seen.insert(name);
                    }
                }
            }

            if seen == self.expected {
                return Ok(());
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
        };

        loop {
            self.send(socket, &msg)?;

            let deadline = std::time::Instant::now() + BROADCAST_INTERVAL;
            while std::time::Instant::now() < deadline {
                if let Some(Message::Ready { name, variant }) = self.recv(socket) {
                    if variant == variant_name && self.expected.contains(&name) {
                        seen.insert(name);
                    }
                }
            }

            if seen == self.expected {
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
                    status: s,
                    exit_code: c,
                }) = self.recv(socket)
                {
                    if variant == variant_name && self.expected.contains(&name) {
                        results.insert(name, (s, c));
                    }
                }
            }

            if results.len() == self.expected.len() {
                return Ok(results);
            }
        }
    }

    /// Send a message via UDP broadcast.
    fn send(&self, socket: &Socket, msg: &Message) -> Result<()> {
        let data = msg.to_bytes();
        socket
            .send_to(&data, &self.broadcast_addr.into())
            .map_err(|e| anyhow::anyhow!("UDP send failed: {e}"))?;
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

/// Create a UDP broadcast socket bound to the given port.
fn create_broadcast_socket(port: u16) -> Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(RECV_TIMEOUT))?;
    socket.set_nonblocking(false)?;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

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
        )
        .unwrap();
        assert!(coord.single_runner);
        coord.discover().unwrap();
    }

    #[test]
    fn single_runner_ready_barrier_is_immediate() {
        let coord =
            Coordinator::new("local".into(), &["local".to_string()], "somehash".into(), 0).unwrap();
        coord.ready_barrier("test-variant").unwrap();
    }

    #[test]
    fn single_runner_done_barrier_returns_own_result() {
        let coord =
            Coordinator::new("local".into(), &["local".to_string()], "somehash".into(), 0).unwrap();
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
            let coord = Coordinator::new("runner_a".into(), &runners_a, hash_a, port).unwrap();

            coord.discover().unwrap();
            coord.ready_barrier("v1").unwrap();
            coord.done_barrier("v1", "success", 0).unwrap()
        });

        let hash_b = hash;
        let runners_b = runners;
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new("runner_b".into(), &runners_b, hash_b, port).unwrap();

            coord.discover().unwrap();
            coord.ready_barrier("v1").unwrap();
            coord.done_barrier("v1", "success", 0).unwrap()
        });

        let results_a = thread_a.join().unwrap();
        let results_b = thread_b.join().unwrap();

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
            let coord = Coordinator::new("a".into(), &runners_a, "hash_AAAA".into(), port).unwrap();
            coord.discover()
        });

        let runners_b = runners;
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new("b".into(), &runners_b, "hash_BBBB".into(), port).unwrap();
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
}
