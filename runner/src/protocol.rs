use crate::clock_sync::{respond_to_probe, ClockSyncEngine};
use crate::local_addrs::canonical_peer_host;
use crate::message::Message;
use anyhow::{bail, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BROADCAST_INTERVAL: Duration = Duration::from_millis(500);
const RECV_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_MSG_SIZE: usize = 4096;

/// Error returned when a coordination barrier (ready / done / resume manifest)
/// fails to reach quorum within its configured timeout.
///
/// Discovery is intentionally NOT subject to this timeout — a stuck discovery
/// is a config / firewall problem, not a transient one, so retrying it via the
/// auto-resume wrapper would just spin. The timeout applies to the in-progress
/// barriers that follow Phase 1 (and Phase 1.25 in resume mode), where a hang
/// indicates a peer that crashed mid-run and is the case `--resume` exists to
/// recover from.
///
/// When this error reaches `main`, the runner exits with code 75
/// (`EX_TEMPFAIL` from `<sysexits.h>`) so the wrapper script can detect the
/// transient-failure case and re-launch with `--resume` appended. Any other
/// non-zero exit (panic, config error, variant failure) propagates as-is and
/// stops the wrapper loop.
#[derive(Debug, Clone)]
pub struct BarrierTimeoutError {
    /// Which barrier hit the timeout (e.g. `"ready"`, `"done"`,
    /// `"resume_manifest"`). Used in the human-readable stderr line.
    pub kind: &'static str,
    /// Effective spawn name (or `""` for the resume-manifest barrier, which
    /// has no per-variant identity).
    pub variant: String,
    /// Duration the barrier waited before giving up.
    pub elapsed: Duration,
    /// Names of peers we are still waiting on. Empty in single-runner mode
    /// (which never times out) — populated only when at least one expected
    /// peer never reported.
    pub missing_peers: Vec<String>,
}

impl fmt::Display for BarrierTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.variant.is_empty() {
            write!(
                f,
                "barrier '{}' timed out after {:.1}s waiting for peer(s): {:?}",
                self.kind,
                self.elapsed.as_secs_f64(),
                self.missing_peers
            )
        } else {
            write!(
                f,
                "barrier '{}' for variant '{}' timed out after {:.1}s waiting for peer(s): {:?}",
                self.kind,
                self.variant,
                self.elapsed.as_secs_f64(),
                self.missing_peers
            )
        }
    }
}

impl std::error::Error for BarrierTimeoutError {}

/// Process-wide verbose-tracing toggle for the coordination protocol.
/// Enabled via the `--verbose-coord` CLI flag in `main.rs`.
///
/// When `true`, `ready_barrier`, `done_barrier`, and `exchange_resume_manifest`
/// emit one stderr line per inbound coordination message, recording whether
/// it was accepted or rejected and why (wrong variant, wrong run, wrong type
/// for the current barrier, unexpected name). Off by default — used to
/// diagnose mid-run hangs at barrier transitions (see T-coord.1).
///
/// Reads are `Relaxed` because tracing is best-effort observability.
static VERBOSE_COORD: AtomicBool = AtomicBool::new(false);

/// Enable verbose coordination tracing process-wide. Idempotent.
pub fn set_verbose_coord(on: bool) {
    VERBOSE_COORD.store(on, Ordering::Relaxed);
}

/// Whether verbose coordination tracing is currently enabled.
fn verbose_coord_enabled() -> bool {
    VERBOSE_COORD.load(Ordering::Relaxed)
}

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
    /// Cached agreed-upon log subfolder, populated by `discover()` just
    /// before it returns (every runner — leader writes its own proposal,
    /// non-leaders write the leader's proposal as observed in its
    /// `Discover`). Used by `maybe_reemit_discover` so that post-discovery
    /// barrier loops can re-broadcast a fully-formed `Discover` message
    /// (with the agreed `log_subdir`) to a peer that joined late and is
    /// still in its own discovery phase. Mutex because barriers run from
    /// `&self`.
    ///
    /// In single-runner mode this is pre-populated with the constructor's
    /// `log_subdir` argument, so the field is always `Some(_)` after `new()`
    /// for that path; in multi-runner mode it stays `None` until
    /// `discover()` succeeds.
    last_log_subdir: Mutex<Option<String>>,
    /// Most-recently-completed `done_barrier` outcome — `(variant_name, run,
    /// status, exit_code)`. Populated at the tail of `done_barrier` just
    /// before returning. Used by `ready_barrier`,
    /// `exchange_resume_manifest`, and the discovery linger to re-emit our
    /// own `Done` in response to a slow peer's stale `Done` request — see
    /// T-coord.1b / DECISIONS.md D9. The cache is bounded to one entry by
    /// design: bob only ever asks for the immediately preceding variant.
    /// Wrapped in a Mutex because `Coordinator` exposes `&self` methods.
    last_completed: Mutex<Option<(String, String, String, i32)>>,
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

        // In single-runner mode `discover()` returns immediately without
        // running the loop, so there is no opportunity to populate
        // `last_log_subdir` from there. Pre-populate it here so the field's
        // post-construction invariant is consistent across modes for any
        // helper that reads it.
        let last_log_subdir = if single_runner {
            Some(log_subdir.clone())
        } else {
            None
        };

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
            last_log_subdir: Mutex::new(last_log_subdir),
            last_completed: Mutex::new(None),
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
    ///
    /// **Discovery is intentionally NOT bounded by a timeout.** A stuck
    /// discovery is a config or firewall problem (mismatched runner names,
    /// blocked UDP multicast, hardware NIC offline) — none of which the
    /// auto-resume wrapper can fix by re-launching. The barrier timeout
    /// applies only to the post-discovery barriers (ready / done /
    /// resume_manifest), where a hang typically means a peer crashed
    /// mid-run and `--resume` is the right recovery.
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
                // The exit condition is satisfied — every expected peer has
                // been observed via *some* message type. But only `Discover`
                // carries `log_subdir`, and the leader's `Discover` may not
                // have arrived yet if the leader has already advanced into a
                // post-discovery barrier (its barrier loops drop our
                // `Discover` and broadcast only `Ready`/`Done`/etc., which
                // is what populated `seen` for us). Without recovery this
                // path used to `.expect("leader log_subdir should be known
                // after discovery")` and panic — see T-coord.3.
                //
                // The fix: post-discovery barrier loops now re-emit
                // `Discover` (with their cached `last_log_subdir`) when
                // they observe an inbound `Discover` from an expected peer
                // — see `maybe_reemit_discover`. So we keep broadcasting
                // our own `Discover` and reading inbound messages, bounded
                // by `LATE_DISCOVER_RECOVERY_BUDGET`, until the leader's
                // `Discover` arrives. Once the fix is in place on both
                // peers this terminates within a single re-broadcast cycle.
                const LATE_DISCOVER_RECOVERY_BUDGET: Duration = Duration::from_secs(30);
                let recovery_deadline = std::time::Instant::now() + LATE_DISCOVER_RECOVERY_BUDGET;
                while leader_log_subdir.is_none() && std::time::Instant::now() < recovery_deadline {
                    self.send(socket, &msg)?;
                    let tick_end = std::time::Instant::now() + BROADCAST_INTERVAL;
                    while std::time::Instant::now() < tick_end {
                        if let Some((received, _src)) = self.recv_from(socket) {
                            match received {
                                Message::Discover {
                                    name,
                                    config_hash,
                                    log_subdir,
                                    resume,
                                } => {
                                    if self.expected.contains(&name)
                                        && config_hash != self.config_hash
                                    {
                                        bail!(
                                            "config hash mismatch from runner '{}': expected {}, got {}",
                                            name,
                                            &self.config_hash[..8],
                                            &config_hash[..config_hash.len().min(8)]
                                        );
                                    }
                                    if self.expected.contains(&name) && resume != self.resume {
                                        bail!(
                                            "resume-flag mismatch from runner '{}': expected {}, got {}",
                                            name,
                                            self.resume,
                                            resume
                                        );
                                    }
                                    if name == *leader && leader_log_subdir.is_none() {
                                        leader_log_subdir = Some(log_subdir);
                                    }
                                }
                                Message::ProbeRequest { from, to, id, t1 } => {
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
                    }
                }

                let agreed = match leader_log_subdir {
                    Some(s) => s,
                    None => bail!(
                        "discovery quorum reached but leader '{}' never re-broadcast its \
                         Discover within {}s — leader may be stuck in a non-cooperating \
                         state (likely an older binary without the T-coord.3 \
                         re-emission fix)",
                        leader,
                        LATE_DISCOVER_RECOVERY_BUDGET.as_secs()
                    ),
                };

                // Linger: keep broadcasting Discover for 2 more seconds so
                // slower peers can complete their discovery phase. Also
                // serves the late-arrival case symmetrically: a peer still
                // missing the leader's `Discover` benefits from us echoing
                // ours during the linger.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Also drain incoming messages during linger to keep
                    // the socket buffer clean.
                    self.drain_and_answer_probe(socket);
                    std::thread::sleep(BROADCAST_INTERVAL);
                }

                // Cache the agreed log subfolder so post-discovery barrier
                // loops can re-emit `Discover` on demand (T-coord.3
                // recovery path for late-joining peers).
                {
                    let mut guard = self.last_log_subdir.lock().unwrap();
                    *guard = Some(agreed.clone());
                }

                return Ok(agreed);
            }
        }
    }

    /// Ready barrier for a specific variant.
    ///
    /// Broadcasts Ready and waits until all runners have signaled ready, or
    /// until `timeout` elapses (whichever comes first). On timeout the call
    /// returns a `BarrierTimeoutError` wrapped in `anyhow::Error`; main
    /// detects it via `Error::downcast_ref` and exits 75.
    ///
    /// In single-runner mode, returns immediately and never times out
    /// (there is no peer to wait for).
    pub fn ready_barrier(&self, variant_name: &str, timeout: Duration) -> Result<()> {
        if self.single_runner {
            return Ok(());
        }

        let socket = self.socket.as_deref().unwrap();
        let started = Instant::now();
        let overall_deadline = started + timeout;
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(self.name.clone());

        let msg = Message::Ready {
            name: self.name.clone(),
            variant: variant_name.to_string(),
            run: self.run.clone(),
        };

        loop {
            self.send(socket, &msg)?;

            let next_tick = std::time::Instant::now() + BROADCAST_INTERVAL;
            let recv_deadline = next_tick.min(overall_deadline);
            while std::time::Instant::now() < recv_deadline {
                match self.recv(socket) {
                    Some(Message::Ready { name, variant, run }) => {
                        let accept = variant == variant_name
                            && run == self.run
                            && self.expected.contains(&name);
                        if verbose_coord_enabled() {
                            eprintln!(
                                "[coord verbose] {}: ready_barrier({variant_name}) recv Ready name={name} variant={variant} run={run} accepted={accept}",
                                self.name
                            );
                        }
                        if accept {
                            seen.insert(name);
                        }
                    }
                    Some(Message::Done {
                        name, variant, run, ..
                    }) => {
                        // T-coord.1b: a slow peer (bob) is still asking for
                        // Done on a variant whose done_barrier we have
                        // already completed and lingered out of. Re-emit
                        // our cached Done if `(variant, run)` matches —
                        // this is gated internally by the cache contents,
                        // so it's a no-op when the cache is empty or the
                        // variant doesn't match.
                        if self.expected.contains(&name) {
                            self.maybe_reemit_stale_done(socket, &variant, &run);
                        }
                        if verbose_coord_enabled() {
                            eprintln!(
                                "[coord verbose] {}: ready_barrier({variant_name}) recv Done name={name} variant={variant} run={run} (ignored — wrong type for this barrier)",
                                self.name
                            );
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
                    Some(Message::Discover { name, .. }) => {
                        // T-coord.3: a late-joining peer is still in its
                        // discovery phase. Re-emit our own Discover so it
                        // can populate its leader_log_subdir.
                        if self.expected.contains(&name) {
                            self.maybe_reemit_discover(socket);
                        }
                    }
                    _ => {}
                }
            }

            if seen == self.expected {
                // Linger: keep broadcasting Ready for 2 more seconds so
                // slower peers can complete their barrier. Linger is bounded
                // and not gated by `timeout` — we already have quorum, so
                // sticking around briefly only helps; it cannot hang.
                let linger_end = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < linger_end {
                    self.send(socket, &msg)?;
                    // Drain incoming messages to keep the socket buffer clean.
                    self.drain_and_answer_probe(socket);
                    std::thread::sleep(BROADCAST_INTERVAL);
                }
                return Ok(());
            }

            if std::time::Instant::now() >= overall_deadline {
                let missing: Vec<String> = self
                    .expected
                    .iter()
                    .filter(|n| !seen.contains(*n))
                    .cloned()
                    .collect();
                return Err(BarrierTimeoutError {
                    kind: "ready",
                    variant: variant_name.to_string(),
                    elapsed: started.elapsed(),
                    missing_peers: missing,
                }
                .into());
            }
        }
    }

    /// Done barrier for a specific variant.
    ///
    /// Broadcasts Done with this runner's outcome and waits until all runners
    /// have reported, or until `timeout` elapses (whichever comes first). On
    /// timeout returns a `BarrierTimeoutError`; main detects it via
    /// `Error::downcast_ref` and exits 75. Returns a map of
    /// `runner_name -> (status, exit_code)` on success.
    ///
    /// In single-runner mode returns immediately with own result and never
    /// times out.
    pub fn done_barrier(
        &self,
        variant_name: &str,
        status: &str,
        exit_code: i32,
        timeout: Duration,
    ) -> Result<HashMap<String, (String, i32)>> {
        let mut results: HashMap<String, (String, i32)> = HashMap::new();
        results.insert(self.name.clone(), (status.to_string(), exit_code));

        if self.single_runner {
            return Ok(results);
        }

        let socket = self.socket.as_deref().unwrap();
        let started = Instant::now();
        let overall_deadline = started + timeout;
        let msg = Message::Done {
            name: self.name.clone(),
            variant: variant_name.to_string(),
            run: self.run.clone(),
            status: status.to_string(),
            exit_code,
        };

        loop {
            self.send(socket, &msg)?;

            let next_tick = std::time::Instant::now() + BROADCAST_INTERVAL;
            let recv_deadline = next_tick.min(overall_deadline);
            while std::time::Instant::now() < recv_deadline {
                match self.recv(socket) {
                    Some(Message::Done {
                        name,
                        variant,
                        run,
                        status: s,
                        exit_code: c,
                    }) => {
                        let accept = variant == variant_name
                            && run == self.run
                            && self.expected.contains(&name);
                        if verbose_coord_enabled() {
                            eprintln!(
                                "[coord verbose] {}: done_barrier({variant_name}) recv Done name={name} variant={variant} run={run} status={s} accepted={accept}",
                                self.name
                            );
                        }
                        if accept {
                            results.insert(name, (s, c));
                        } else if (variant != variant_name || run != self.run)
                            && self.expected.contains(&name)
                        {
                            // T-coord.1b: cross-spawn case — peer is asking
                            // about a previous variant's Done. If our cache
                            // matches, re-emit. This protects the path where
                            // we are the slow peer for spawn N+1 while a peer
                            // is still trying to close spawn N's done_barrier.
                            self.maybe_reemit_stale_done(socket, &variant, &run);
                        }
                    }
                    Some(Message::Ready { name, variant, run }) => {
                        if verbose_coord_enabled() {
                            eprintln!(
                                "[coord verbose] {}: done_barrier({variant_name}) recv Ready name={name} variant={variant} run={run} (ignored — wrong type for this barrier)",
                                self.name
                            );
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
                    Some(Message::Discover { name, .. }) => {
                        // T-coord.3: a late-joining peer is still in its
                        // discovery phase. Re-emit our own Discover so it
                        // can populate its leader_log_subdir.
                        if self.expected.contains(&name) {
                            self.maybe_reemit_discover(socket);
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
                // T-coord.1b: cache this clean done-barrier outcome so
                // post-done coordination phases (`ready_barrier`,
                // `exchange_resume_manifest`) can re-emit our Done in
                // response to a slow peer's stale request after our linger
                // has expired. Single-entry cache by design — bob only ever
                // asks for the immediately preceding variant. Only written
                // on the success path; the timeout-error branch leaves the
                // cache untouched so we never re-emit a Done for a variant
                // whose coordination did not complete cleanly.
                {
                    let mut guard = self.last_completed.lock().unwrap();
                    *guard = Some((
                        variant_name.to_string(),
                        self.run.clone(),
                        status.to_string(),
                        exit_code,
                    ));
                }
                return Ok(results);
            }

            if std::time::Instant::now() >= overall_deadline {
                let missing: Vec<String> = self
                    .expected
                    .iter()
                    .filter(|n| !results.contains_key(*n))
                    .cloned()
                    .collect();
                return Err(BarrierTimeoutError {
                    kind: "done",
                    variant: variant_name.to_string(),
                    elapsed: started.elapsed(),
                    missing_peers: missing,
                }
                .into());
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
        timeout: Duration,
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut all: HashMap<String, Vec<String>> = HashMap::new();
        all.insert(self.name.clone(), local_complete_jobs.clone());

        if self.single_runner {
            return Ok(all);
        }

        let socket = self.socket.as_deref().unwrap();
        let started = Instant::now();
        let overall_deadline = started + timeout;
        let msg = Message::ResumeManifest {
            name: self.name.clone(),
            run: self.run.clone(),
            complete_jobs: local_complete_jobs,
        };

        loop {
            self.send(socket, &msg)?;

            let next_tick = std::time::Instant::now() + BROADCAST_INTERVAL;
            let recv_deadline = next_tick.min(overall_deadline);
            while std::time::Instant::now() < recv_deadline {
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
                    Some(Message::Discover { name, .. }) => {
                        // T-coord.3: a late-joining peer is still in its
                        // discovery phase. Re-emit our own Discover so it
                        // can populate its leader_log_subdir.
                        if self.expected.contains(&name) {
                            self.maybe_reemit_discover(socket);
                        }
                    }
                    Some(Message::Done {
                        name, variant, run, ..
                    }) => {
                        // T-coord.1b: a peer might still be in the
                        // done_barrier of the previous run's last variant
                        // (relevant in resume mode where the manifest phase
                        // immediately precedes Phase 2). Re-emit our cached
                        // Done if it matches; the helper is gated internally
                        // by the cache contents.
                        if self.expected.contains(&name) {
                            self.maybe_reemit_stale_done(socket, &variant, &run);
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

            if std::time::Instant::now() >= overall_deadline {
                let missing: Vec<String> = self
                    .expected
                    .iter()
                    .filter(|n| !all.contains_key(*n))
                    .cloned()
                    .collect();
                return Err(BarrierTimeoutError {
                    kind: "resume_manifest",
                    variant: String::new(),
                    elapsed: started.elapsed(),
                    missing_peers: missing,
                }
                .into());
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

    /// Best-effort re-emission of our own `Discover` for the benefit of a
    /// late-joining peer that is still in its own discovery phase.
    ///
    /// Called from the `Some(Message::Discover { .. })` arm of every
    /// post-discovery barrier loop (`ready_barrier`, `done_barrier`,
    /// `exchange_resume_manifest`). The contract: if our discovery has
    /// completed (so `last_log_subdir` is `Some(_)`), we re-broadcast a
    /// fully-formed `Discover` carrying the agreed `log_subdir`. The slow
    /// peer's `discover()` will then populate its `leader_log_subdir`
    /// (via the leader's re-emission, or via the propagation of leader's
    /// proposal that every other runner already mirrors).
    ///
    /// Errors are intentionally swallowed — we cannot abort the active
    /// barrier on a transient send failure. This is a recovery-only path.
    ///
    /// See T-coord.3 (the discovery panic on late bob) for the bug this
    /// fixes. Mirrors the pattern T-coord.1b uses for stale `Done`.
    fn maybe_reemit_discover(&self, socket: &Socket) {
        let subdir = match self.last_log_subdir.lock().unwrap().clone() {
            Some(s) => s,
            None => return,
        };
        let msg = Message::Discover {
            name: self.name.clone(),
            config_hash: self.config_hash.clone(),
            log_subdir: subdir,
            resume: self.resume,
        };
        let _ = self.send(socket, &msg);
    }

    /// Re-emit our cached `Done` for the most-recently-completed variant if
    /// the inbound `(variant, run)` matches it (T-coord.1b).
    ///
    /// When a fast peer (alice) has already exited `done_barrier` for spawn N
    /// and moved on to a later coordination phase (`ready_barrier`,
    /// `exchange_resume_manifest`, or discovery linger), a slow peer (bob)
    /// arriving at `done_barrier` for spawn N has no peer broadcasting `Done`
    /// for that variant. Without this hook bob would loop forever; the
    /// `barrier_linger_prevents_slow_peer_hang` test only covers the case
    /// where bob shows up inside alice's 2 s linger window. This method
    /// closes the longer gap by giving alice a way to respond to a stale
    /// `Done` request after her linger has already expired.
    ///
    /// Behaviour:
    /// - If `last_completed` is `Some((variant, run, status, exit))` and the
    ///   inbound `(variant, run)` matches, broadcast our own `Done` for that
    ///   variant via `self.send`.
    /// - Otherwise (no cache entry, or older / different variant), do
    ///   nothing — the cache is bounded to a single entry by design and we
    ///   intentionally let stale requests for older spawns time out.
    /// - Errors from `send` are swallowed: this is a best-effort recovery
    ///   hook running inside the hot loop of another barrier; a transient
    ///   send failure must not abort the active barrier.
    fn maybe_reemit_stale_done(&self, socket: &Socket, inbound_variant: &str, inbound_run: &str) {
        let cached = self.last_completed.lock().unwrap().clone();
        if let Some((variant, run, status, exit_code)) = cached {
            if variant == inbound_variant && run == inbound_run {
                let reply = Message::Done {
                    name: self.name.clone(),
                    variant,
                    run,
                    status,
                    exit_code,
                };
                let _ = self.send(socket, &reply);
            }
        }
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
        // Single-runner mode never blocks; even a tiny timeout returns Ok.
        coord
            .ready_barrier("test-variant", Duration::from_millis(1))
            .unwrap();
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
        let results = coord
            .done_barrier("test-variant", "success", 0, Duration::from_millis(1))
            .unwrap();
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
            coord.ready_barrier("v1", Duration::from_secs(30)).unwrap();
            let results = coord
                .done_barrier("v1", "success", 0, Duration::from_secs(30))
                .unwrap();
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
            coord.ready_barrier("v1", Duration::from_secs(30)).unwrap();
            let results = coord
                .done_barrier("v1", "success", 0, Duration::from_secs(30))
                .unwrap();
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

            coord.ready_barrier("v1", Duration::from_secs(30))
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

            coord.ready_barrier("v1", Duration::from_secs(30)).unwrap();
            coord
                .done_barrier("v1", "success", 0, Duration::from_secs(30))
                .unwrap();
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

            coord.ready_barrier("v1", Duration::from_secs(30)).unwrap();
            coord
                .done_barrier("v1", "success", 0, Duration::from_secs(30))
                .unwrap();
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
            .exchange_resume_manifest(local_manifest.clone(), Duration::from_millis(1))
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
            coord.exchange_resume_manifest(vec!["v1".into(), "v2".into()], Duration::from_secs(30))
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
            coord.exchange_resume_manifest(vec!["v2".into(), "v3".into()], Duration::from_secs(30))
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

    /// Helper: build a two-runner Coordinator on a unique port whose peer
    /// will never appear, so any barrier method invoked on it will reach the
    /// timeout path.
    fn coord_with_silent_peer(self_name: &str, peer_name: &str, port: u16) -> Coordinator {
        let runners = vec![self_name.to_string(), peer_name.to_string()];
        Coordinator::new(
            self_name.into(),
            &runners,
            "timeout-hash".into(),
            port,
            "timeout-sub".into(),
            "timeout-run".into(),
            false,
        )
        .unwrap()
    }

    #[test]
    fn ready_barrier_returns_timeout_when_peer_silent() {
        let _guard = multicast_test_lock();
        let port = next_test_port();
        let coord = coord_with_silent_peer("rb_alone", "rb_ghost", port);
        // Skip discovery (we don't want to wait for the ghost). Directly hit
        // the ready barrier with a tight timeout so the test is fast.
        let started = std::time::Instant::now();
        let err = coord
            .ready_barrier("v-tmo", Duration::from_millis(300))
            .expect_err("ready_barrier with silent peer must time out");
        let elapsed = started.elapsed();
        // The timeout must fire within a small fudge of the configured value.
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(5),
            "expected ~300ms timeout, got {elapsed:?}"
        );
        let bt = err
            .downcast_ref::<BarrierTimeoutError>()
            .expect("error must downcast to BarrierTimeoutError");
        assert_eq!(bt.kind, "ready");
        assert_eq!(bt.variant, "v-tmo");
        assert_eq!(bt.missing_peers, vec!["rb_ghost".to_string()]);
    }

    #[test]
    fn done_barrier_returns_timeout_when_peer_silent() {
        let _guard = multicast_test_lock();
        let port = next_test_port();
        let coord = coord_with_silent_peer("db_alone", "db_ghost", port);
        let err = coord
            .done_barrier("v-tmo", "success", 0, Duration::from_millis(300))
            .expect_err("done_barrier with silent peer must time out");
        let bt = err
            .downcast_ref::<BarrierTimeoutError>()
            .expect("error must downcast to BarrierTimeoutError");
        assert_eq!(bt.kind, "done");
        assert_eq!(bt.variant, "v-tmo");
        assert_eq!(bt.missing_peers, vec!["db_ghost".to_string()]);
    }

    #[test]
    fn resume_manifest_returns_timeout_when_peer_silent() {
        let _guard = multicast_test_lock();
        let port = next_test_port();
        let coord = coord_with_silent_peer("rm_alone", "rm_ghost", port);
        let err = coord
            .exchange_resume_manifest(vec!["a".into()], Duration::from_millis(300))
            .expect_err("exchange_resume_manifest with silent peer must time out");
        let bt = err
            .downcast_ref::<BarrierTimeoutError>()
            .expect("error must downcast to BarrierTimeoutError");
        assert_eq!(bt.kind, "resume_manifest");
        assert_eq!(bt.variant, "");
        assert_eq!(bt.missing_peers, vec!["rm_ghost".to_string()]);
    }

    #[test]
    fn barrier_timeout_error_display_mentions_kind_and_variant() {
        let e = BarrierTimeoutError {
            kind: "ready",
            variant: "myvar".into(),
            elapsed: Duration::from_secs(5),
            missing_peers: vec!["b".into()],
        };
        let s = e.to_string();
        assert!(s.contains("ready"), "{s}");
        assert!(s.contains("myvar"), "{s}");
        assert!(s.contains("5"), "{s}");
        assert!(s.contains('b'), "{s}");
    }

    /// Locks in the T-coord.1b fix for the 2026-05-07 mid-run hang.
    ///
    /// Scenario observed in the field: alice finished spawn N's done barrier
    /// (linger included) and moved on to spawn N+1's ready barrier. Bob's
    /// variant ran longer; bob then entered done_barrier for spawn N. Before
    /// the fix, alice was no longer broadcasting `Done` for spawn N AND her
    /// `ready_barrier` silently dropped inbound `Done` messages, leaving bob
    /// hung forever (see `metak-orchestrator/DECISIONS.md` D9 for the
    /// original code-path trace).
    ///
    /// With T-coord.1b in place, alice's `done_barrier` caches the most
    /// recently completed `(variant, run, status, exit_code)` in
    /// `last_completed` just before returning, and `ready_barrier`'s
    /// `Some(Message::Done { .. })` arm calls `maybe_reemit_stale_done` —
    /// re-broadcasting alice's cached Done when bob's stale request matches.
    /// Bob's done_barrier-N loop then receives a fresh Done from alice and
    /// can complete.
    ///
    /// This test exercises that fixed code path without invoking the real
    /// `done_barrier` on bob's side (which would leak a hung thread on
    /// regression). Instead:
    ///
    /// - Alice runs a manual emulation of `ready_barrier(spawn_n_plus_1)`
    ///   that mirrors the **fixed** loop: broadcast `Ready` every 500 ms,
    ///   answer probes, and on inbound `Done` from an expected peer call
    ///   `maybe_reemit_stale_done`. We pre-populate alice's
    ///   `last_completed` cache with the spawn-N-half outcome that bob will
    ///   ask about — in production that write happens at the tail of
    ///   `done_barrier`; the test simulates it directly to keep the
    ///   coordination structure shallow.
    /// - Bob (also manually) sends `Done` for "spawn_n_half" repeatedly
    ///   and waits up to 6 seconds for a matching `Done` back from alice.
    ///   With the fix in place this should arrive within one re-broadcast
    ///   cycle (~500 ms).
    ///
    /// If this test ever fails, the fix has regressed: alice's
    /// post-done-barrier coordination phases are no longer responding to
    /// stale Done requests, and the original mid-run hang is back.
    #[test]
    fn done_barrier_hang_repro_when_peer_already_advanced() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::{Arc, Barrier};

        let _guard = multicast_test_lock();
        let port = next_test_port();
        let runners = vec!["alice".to_string(), "bob".to_string()];
        let hash = "coordhang".to_string();
        let run_id = "coord-hang-run".to_string();

        let setup = Arc::new(Barrier::new(2));
        let setup_a = Arc::clone(&setup);
        let setup_b = Arc::clone(&setup);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_a = Arc::clone(&shutdown);

        // Alice: discover, ready_barrier(spawn_n), done_barrier(spawn_n)
        // (which both complete with bob), then run an in-test loop that
        // emulates the **fixed** `ready_barrier(spawn_n_plus_1)`: send
        // Ready every 500 ms, answer probes, and on inbound Done from an
        // expected peer call `maybe_reemit_stale_done`. We also pre-populate
        // `last_completed` with the spawn-N-half outcome bob will ask about
        // — in production that write happens at the tail of `done_barrier`;
        // the test simulates it directly. Exits when `shutdown` flips.
        let runners_a = runners.clone();
        let hash_a = hash.clone();
        let run_a = run_id.clone();
        let thread_a = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "alice".into(),
                &runners_a,
                hash_a,
                port,
                "log-sub".into(),
                run_a.clone(),
                false,
            )
            .unwrap();
            setup_a.wait();
            coord.discover().unwrap();
            coord
                .ready_barrier("spawn_n", Duration::from_secs(30))
                .unwrap();
            coord
                .done_barrier("spawn_n", "success", 0, Duration::from_secs(30))
                .unwrap();

            // Pretend alice's done_barrier for the prior "spawn_n_half"
            // also completed cleanly: pre-populate the cache that the real
            // `done_barrier` would have written. This is the variant bob
            // will ask about below.
            *coord.last_completed.lock().unwrap() =
                Some(("spawn_n_half".to_string(), run_a, "success".to_string(), 0));

            let socket = coord.socket.as_deref().unwrap();
            let ready_msg = Message::Ready {
                name: "alice".into(),
                variant: "spawn_n_plus_1".into(),
                run: coord.run.clone(),
            };
            let mut last_send = std::time::Instant::now() - BROADCAST_INTERVAL;
            while !shutdown_a.load(AtomicOrdering::Relaxed) {
                if last_send.elapsed() >= BROADCAST_INTERVAL {
                    coord.send(socket, &ready_msg).ok();
                    last_send = std::time::Instant::now();
                }
                match coord.recv(socket) {
                    Some(Message::ProbeRequest { from, to, id, t1 }) => {
                        if to == coord.name {
                            let _ = respond_to_probe(
                                socket,
                                &coord.peer_addrs,
                                &coord.name,
                                &from,
                                id,
                                &t1,
                            );
                        }
                    }
                    Some(Message::Done {
                        name, variant, run, ..
                    }) => {
                        // The fix under test: re-emit cached Done so bob's
                        // stale done_barrier loop can complete.
                        if coord.expected.contains(&name) {
                            coord.maybe_reemit_stale_done(socket, &variant, &run);
                        }
                    }
                    _ => {}
                }
            }
        });

        // Bob: discover, ready_barrier(spawn_n), done_barrier(spawn_n),
        // then a manual emulation of done_barrier("spawn_n_half"):
        // broadcast Done at 500 ms cadence, drain inbound, look for a
        // matching Done from alice. If we observe one within 6 seconds,
        // the bug is "fixed" (the test will fail and prompt the
        // maintainer to invert the assertion). Otherwise, the bug
        // reproduces.
        let runners_b = runners;
        let hash_b = hash;
        let run_b = run_id;
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "bob".into(),
                &runners_b,
                hash_b,
                port,
                "log-sub".into(),
                run_b,
                false,
            )
            .unwrap();
            setup_b.wait();
            coord.discover().unwrap();
            coord
                .ready_barrier("spawn_n", Duration::from_secs(30))
                .unwrap();
            coord
                .done_barrier("spawn_n", "success", 0, Duration::from_secs(30))
                .unwrap();

            // Give alice's post-done loop time to come up.
            std::thread::sleep(Duration::from_millis(300));

            let socket = coord.socket.as_deref().unwrap();
            let done_msg = Message::Done {
                name: "bob".into(),
                variant: "spawn_n_half".into(),
                run: coord.run.clone(),
                status: "success".into(),
                exit_code: 0,
            };

            let deadline = std::time::Instant::now() + Duration::from_secs(6);
            let mut last_send = std::time::Instant::now() - BROADCAST_INTERVAL;
            let mut got_alice_done_for_half = false;
            while std::time::Instant::now() < deadline {
                if last_send.elapsed() >= BROADCAST_INTERVAL {
                    coord.send(socket, &done_msg).ok();
                    last_send = std::time::Instant::now();
                }
                if let Some(Message::Done {
                    name, variant, run, ..
                }) = coord.recv(socket)
                {
                    if name == "alice" && variant == "spawn_n_half" && run == coord.run {
                        got_alice_done_for_half = true;
                        break;
                    }
                }
            }
            got_alice_done_for_half
        });

        let bob_saw_alice_done = thread_b.join().expect("bob thread panicked");
        // Release alice's post-done loop so the socket and multicast
        // membership are dropped before subsequent tests run.
        shutdown.store(true, AtomicOrdering::Relaxed);
        thread_a.join().expect("alice thread panicked");

        assert!(
            bob_saw_alice_done,
            "T-coord.1b regression: bob did NOT receive alice's Done for \
             'spawn_n_half' within 6 s. With the fix in place, alice's \
             parked ready_barrier-like loop should re-emit her cached Done \
             in response to bob's stale request. If this test fails, the \
             `last_completed` cache and/or `maybe_reemit_stale_done` wiring \
             in `ready_barrier` has regressed — see DECISIONS.md D9."
        );
    }

    /// Reproducer for the T-coord.3 discovery panic: a late-joining
    /// non-leader (bob) panicked at the tail of `Coordinator::discover`
    /// because the leader (alice) had already advanced into a Phase 2
    /// barrier and her barrier loops dropped bob's `Discover`.
    ///
    /// Failure path before the fix:
    ///
    /// 1. Bob's `seen` set is filled via a non-Discover message from
    ///    alice (a `Ready` broadcast by alice's parked barrier loop —
    ///    here injected directly so we don't depend on alice's full
    ///    discovery completing first).
    /// 2. Bob's `peer_hosts` gets `alice -> 127.0.0.1` from the source
    ///    of that Ready packet.
    /// 3. Bob's `seen == expected && hosts_known` becomes true. Without
    ///    the fix he hits the `.expect("leader log_subdir should be
    ///    known after discovery")` and panics.
    ///
    /// With the fix (T-coord.3):
    ///
    /// - Alice's parked barrier loop, on receiving bob's `Discover`,
    ///   calls `maybe_reemit_discover` which broadcasts a fully-formed
    ///   `Discover` carrying the agreed `log_subdir`.
    /// - Bob's `discover()` keeps reading messages past the quorum point
    ///   until the leader's `Discover` arrives, bounded by an internal
    ///   30 s recovery budget. Once alice re-emits, bob captures
    ///   `leader_log_subdir` and `discover()` returns
    ///   `Ok(<alice's proposal>)`.
    ///
    /// Wall-clock budget: bob's recovery loop re-broadcasts every 500 ms
    /// and alice's barrier-emulation thread responds promptly, so the
    /// typical end-to-end recovery time is under 5 s. The test caps the
    /// wait at 10 s.
    #[test]
    fn discover_recovers_when_leader_already_in_barrier_t_coord_3() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::Arc;

        let _guard = multicast_test_lock();
        let port = next_test_port();
        let runners = vec!["alice".to_string(), "bob".to_string()];
        let hash = "discover-recover-hash".to_string();
        let run_id = "discover-recover-run".to_string();
        let alice_subdir = "alice-proposal-20260507_180000".to_string();
        let bob_subdir = "bob-proposal-20260507_180001".to_string();

        // Alice's emulator. We do NOT call alice's real discover() /
        // ready_barrier(); instead we construct her Coordinator to bind
        // the alice port and to give us access to `maybe_reemit_discover`
        // and to her configured log_subdir cache. We then pre-populate
        // her `last_log_subdir` cache directly to the agreed value, as
        // her completed `discover()` would have done. Then we run a
        // loop that mirrors `ready_barrier`'s reaction to inbound
        // Discover (the `Some(Message::Discover { .. })` arm calls
        // `maybe_reemit_discover`).
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_a = Arc::clone(&shutdown);
        let runners_a = runners.clone();
        let hash_a = hash.clone();
        let run_a = run_id.clone();
        let alice_subdir_a = alice_subdir.clone();
        let thread_a = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "alice".into(),
                &runners_a,
                hash_a,
                port,
                alice_subdir_a.clone(),
                run_a,
                false,
            )
            .unwrap();
            // Pretend alice's discover() already completed: cache the
            // agreed log_subdir. (Alice is the leader, so her own
            // proposal is the agreed value.)
            *coord.last_log_subdir.lock().unwrap() = Some(alice_subdir_a);

            // Mirror `ready_barrier`'s loop for the parts that matter to
            // this test: broadcast Ready, answer probes, and call
            // `maybe_reemit_discover` on inbound Discover from a peer in
            // `expected`. This is the exact fix code path we want to
            // exercise. The real `ready_barrier` would do the same but
            // has no clean abort path for the test to terminate it.
            let socket = coord.socket.as_deref().unwrap();
            let ready_msg = Message::Ready {
                name: "alice".into(),
                variant: "spawn_n".into(),
                run: coord.run.clone(),
            };
            let mut last_send = std::time::Instant::now() - BROADCAST_INTERVAL;
            while !shutdown_a.load(AtomicOrdering::Relaxed) {
                if last_send.elapsed() >= BROADCAST_INTERVAL {
                    coord.send(socket, &ready_msg).ok();
                    last_send = std::time::Instant::now();
                }
                match coord.recv(socket) {
                    Some(Message::ProbeRequest { from, to, id, t1 }) => {
                        if to == coord.name {
                            let _ = respond_to_probe(
                                socket,
                                &coord.peer_addrs,
                                &coord.name,
                                &from,
                                id,
                                &t1,
                            );
                        }
                    }
                    Some(Message::Discover { name, .. }) => {
                        // The fix under test: re-emit our own Discover so
                        // bob can populate his leader_log_subdir.
                        if coord.expected.contains(&name) {
                            coord.maybe_reemit_discover(socket);
                        }
                    }
                    _ => {}
                }
            }
        });

        // Give alice's emulator a moment to bind its socket and start
        // broadcasting Ready, so bob sees Ready before the recovery
        // window opens.
        std::thread::sleep(Duration::from_millis(200));

        let runners_b = runners;
        let hash_b = hash;
        let run_b = run_id;
        let bob_subdir_b = bob_subdir;
        let bob_started = std::time::Instant::now();
        let thread_b = std::thread::spawn(move || {
            let coord = Coordinator::new(
                "bob".into(),
                &runners_b,
                hash_b,
                port,
                bob_subdir_b,
                run_b,
                false,
            )
            .unwrap();
            coord.discover()
        });

        // Cap the wait at 10 s. The expected time is under 5 s (bob's
        // discovery linger entry plus one re-broadcast cycle once
        // alice's emulator answers his Discover with a re-emitted
        // Discover).
        let bob_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !thread_b.is_finished() && std::time::Instant::now() < bob_deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let bob_finished = thread_b.is_finished();

        // Tear down alice cleanly so subsequent tests can bind ports.
        // Do this BEFORE joining bob, so if bob's thread is still alive
        // alice doesn't keep broadcasting onto a leaked port.
        shutdown.store(true, AtomicOrdering::Relaxed);
        thread_a.join().expect("alice thread panicked");

        if !bob_finished {
            panic!("bob's discover() did not return within 10 s — T-coord.3 fix not in place");
        }
        let bob_result = thread_b.join().expect("bob thread panicked");
        let bob_elapsed = bob_started.elapsed();

        let bob_log_subdir =
            bob_result.expect("bob's discover() must return Ok after T-coord.3 fix");

        // Bob must have adopted alice's proposal as the agreed log
        // subdir, since alice is the leader (runners[0]).
        assert_eq!(
            bob_log_subdir, alice_subdir,
            "bob must adopt alice's (leader's) log_subdir proposal after T-coord.3 fix"
        );

        // Sanity: bob's discovery must complete reasonably fast once
        // alice's parked barrier loop re-emits Discover. We allow up to
        // 10 s but expect well under that.
        assert!(
            bob_elapsed < Duration::from_secs(10),
            "bob's discover() took too long ({bob_elapsed:?}) — T-coord.3 recovery is too slow"
        );
    }
}
