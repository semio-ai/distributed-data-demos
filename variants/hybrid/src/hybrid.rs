/// HybridVariant: UDP multicast for QoS 1-2, TCP for QoS 3-4.
///
/// This is the "simplest correct" approach. No application-layer reliability
/// logic at all -- kernel TCP handles everything for reliable delivery.
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use anyhow::{Context, Result};

use variant_base::types::{Qos, ReceivedUpdate, ThreadingMode};
use variant_base::Variant;

use crate::protocol::{self, Frame};

/// Internal record of an observed peer EOT marker (T15.8 historical).
///
/// The on-wire EOT exchange was retired in T15.8. The variant still
/// decodes EOT control frames so pre-T15.8 peers don't surface as
/// parser errors, but they no longer reach the driver.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct PeerEot {
    writer: String,
    eot_id: u64,
}
use crate::reader::{self, HubDataMessage, HubLifecycleMessage, ReaderHub};
use crate::tcp::TcpTransport;
use crate::udp::UdpTransport;

/// Receive buffer size for UDP datagrams.
const UDP_RECV_BUF_SIZE: usize = 65536;

/// Outcome of a single `try_recv_*` poll.
enum RecvOutcome {
    /// A data update is ready for the caller.
    Data(ReceivedUpdate),
    /// A non-data frame (stale QoS-2 duplicate or EOT marker) was
    /// dispatched internally; the caller should keep polling so the
    /// downstream data isn't masked.
    Consumed,
    /// The socket had nothing to read.
    Empty,
}

/// Configuration for the hybrid variant.
///
/// Built by `main::run` from the parsed CLI args (`--multicast-group`,
/// `--tcp-base-port`, `--peers`, `--runner`, `--qos`). The variant itself does
/// not need to know about runner identity or QoS strides; all derivation is
/// done in `main` and the resulting concrete addresses are passed in here.
pub struct HybridConfig {
    /// UDP multicast group:port. Same value on every runner; no stride.
    pub multicast_group: SocketAddrV4,
    /// Local interface address to bind UDP/TCP sockets on. Always
    /// `0.0.0.0` for now.
    pub bind_addr: Ipv4Addr,
    /// Local TCP listen address (per-runner / per-qos derived port).
    pub tcp_listen_addr: SocketAddr,
    /// Concrete TCP endpoints to dial (excludes self).
    pub tcp_peers: Vec<SocketAddr>,
    /// Active QoS for this spawn. Informational only post-T15.8 (EOT
    /// over the data path was retired together with the control TCP
    /// channel).
    #[allow(dead_code)]
    pub qos: Qos,
    /// `--recv-buffer-kb` from the runner-injected CLI (T14.1).
    /// Applied to the UDP socket via `SO_RCVBUF` and to each TCP
    /// peer's underlying socket on connect / accept.
    pub recv_buffer_kb: u32,
}

/// Hybrid UDP/TCP variant implementing the Variant trait.
pub struct HybridVariant {
    runner: String,
    config: HybridConfig,
    udp: Option<UdpTransport>,
    tcp: Option<TcpTransport>,
    /// Track highest sequence number per (writer, path) for QoS 2 stale discard.
    latest_seq: HashMap<(String, String), u64>,
    /// (writer, eot_id) pairs already observed. Source of truth for the
    /// variant's EOT dedup; the driver applies a defensive dedup-by-writer
    /// pass on its side too (per the EOT contract).
    seen_eots: HashSet<(String, u64)>,
    /// EOTs observed since the last `poll_peer_eots` call. Drained on every
    /// call.
    pending_eots: VecDeque<PeerEot>,
    /// Threading mode chosen by the driver at `connect` time. Stashed
    /// so `start_reader_threads` and `poll_receive` can branch on it.
    threading_mode: ThreadingMode,
    /// Reader-thread hub. `Some` in Multi mode after
    /// `start_reader_threads`; `None` in Single mode.
    reader_hub: Option<ReaderHub>,
}

impl HybridVariant {
    /// Create a new HybridVariant from the runner name and the derived config.
    pub fn new(runner: &str, config: HybridConfig) -> Self {
        Self {
            runner: runner.to_string(),
            config,
            udp: None,
            tcp: None,
            latest_seq: HashMap::new(),
            seen_eots: HashSet::new(),
            pending_eots: VecDeque::new(),
            threading_mode: ThreadingMode::Single,
            reader_hub: None,
        }
    }

    /// Check if a QoS 2 message is stale (seq <= last seen for this writer+path).
    /// If not stale, updates the tracker and returns false.
    fn is_stale_qos2(&mut self, writer: &str, path: &str, seq: u64) -> bool {
        let key = (writer.to_string(), path.to_string());
        match self.latest_seq.get(&key) {
            Some(&last) if seq <= last => true,
            _ => {
                self.latest_seq.insert(key, seq);
                false
            }
        }
    }

    /// Record an observed EOT marker. Idempotent: pushes to the queue only
    /// the first time the `(writer, eot_id)` pair is seen, and only when
    /// the writer is a peer (not this runner -- own EOTs come back through
    /// multicast loopback and would otherwise pollute the driver's `seen`
    /// set, making `seen != expected` permanently true and forcing the
    /// EOT phase to wait for the full timeout).
    fn record_eot(&mut self, writer: String, eot_id: u64) {
        if writer == self.runner {
            return;
        }
        if self.seen_eots.insert((writer.clone(), eot_id)) {
            self.pending_eots.push_back(PeerEot { writer, eot_id });
        }
    }

    /// Poll the UDP socket once for a pending datagram and dispatch it.
    ///
    /// `RecvOutcome::Data` is a non-stale data datagram for the caller.
    /// `RecvOutcome::Consumed` means a frame was dispatched (EOT recorded
    /// internally, or a stale QoS-2 duplicate skipped) but the caller has
    /// nothing new to log this iteration -- it should re-poll.
    /// `RecvOutcome::Empty` means the socket had nothing to read.
    fn try_recv_udp(&mut self) -> Result<RecvOutcome> {
        let udp = match self.udp.as_ref() {
            Some(u) => u,
            None => return Ok(RecvOutcome::Empty),
        };
        let mut buf = [0u8; UDP_RECV_BUF_SIZE];
        let n = match udp.try_recv(&mut buf)? {
            Some(n) => n,
            None => return Ok(RecvOutcome::Empty),
        };
        match protocol::decode_frame(&buf[..n])? {
            Frame::Data(update) => {
                if update.qos == Qos::LatestValue
                    && self.is_stale_qos2(&update.writer, &update.path, update.seq)
                {
                    Ok(RecvOutcome::Consumed)
                } else {
                    Ok(RecvOutcome::Data(update))
                }
            }
            Frame::Eot { writer, eot_id } => {
                self.record_eot(writer, eot_id);
                Ok(RecvOutcome::Consumed)
            }
        }
    }

    /// Poll the TCP transport once for a pending framed message and dispatch it.
    fn try_recv_tcp(&mut self) -> Result<RecvOutcome> {
        let tcp = match self.tcp.as_mut() {
            Some(t) => t,
            None => return Ok(RecvOutcome::Empty),
        };
        let bytes = match tcp.try_recv()? {
            Some(b) => b,
            None => return Ok(RecvOutcome::Empty),
        };
        match protocol::decode_frame(&bytes)? {
            Frame::Data(update) => Ok(RecvOutcome::Data(update)),
            Frame::Eot { writer, eot_id } => {
                self.record_eot(writer, eot_id);
                Ok(RecvOutcome::Consumed)
            }
        }
    }

    /// Multi-mode `poll_receive` (T14.4 + T14.16): drain the reader-
    /// thread mpsc channels.
    ///
    /// T14.16: drain the unbounded lifecycle channel FIRST so EOT
    /// observations are never starved by a saturated data channel.
    /// Lifecycle items are infrequent (O(peers) per spawn) so we
    /// always drain them to empty before touching data. Then drain
    /// the bounded data channel until the first non-stale data
    /// update or the channel is empty / the per-call budget is hit.
    /// Stale QoS-2 duplicates are filtered exactly as in the inline
    /// path.
    fn poll_receive_multi(&mut self) -> Result<Option<ReceivedUpdate>> {
        if self.reader_hub.is_none() {
            return Ok(None);
        }

        // Lifecycle drain first -- never starved.
        loop {
            let recv_result = {
                let hub = self.reader_hub.as_ref().unwrap();
                hub.lifecycle_rx.try_recv()
            };
            match recv_result {
                Ok(HubLifecycleMessage::Eot { writer, eot_id }) => {
                    self.record_eot(writer, eot_id);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // Data drain second, bounded by POLL_BUDGET to keep
        // `poll_receive` responsive.
        const POLL_BUDGET: u32 = 256;
        for _ in 0..POLL_BUDGET {
            let recv_result = {
                let hub = self.reader_hub.as_ref().unwrap();
                hub.rx.try_recv()
            };
            match recv_result {
                Ok(HubDataMessage::Data(update)) => {
                    if update.qos == Qos::LatestValue
                        && self.is_stale_qos2(&update.writer, &update.path, update.seq)
                    {
                        continue;
                    }
                    return Ok(Some(update));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(None),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(None),
            }
        }
        Ok(None)
    }
}

/// Helper: take a single peer's read clone, raise its `SO_RCVTIMEO`
/// to `reader::TCP_READER_TIMEOUT`, keep a shutdown-side clone so
/// `ReaderHub::stop_and_join` can wake the blocked reader, and spawn
/// the per-peer reader thread.
///
/// T14.16: both sender clones (`data_tx`, `lifecycle_tx`) are passed
/// to the spawned thread so it can route Data frames to the bounded
/// data channel and EOT frames to the unbounded lifecycle channel.
fn spawn_tcp_reader_for(
    peer: &mut crate::tcp::TcpPeer,
    label_prefix: &str,
    data_tx: &std::sync::mpsc::SyncSender<HubDataMessage>,
    lifecycle_tx: &std::sync::mpsc::Sender<HubLifecycleMessage>,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    hub: &mut ReaderHub,
) -> Result<()> {
    let read = peer
        .take_read_stream()
        .with_context(|| format!("TCP read stream already taken for {}", peer.addr))?;
    read.set_read_timeout(Some(reader::TCP_READER_TIMEOUT))
        .with_context(|| format!("set TCP read timeout for {}", peer.addr))?;
    let shutdown_handle = read
        .try_clone()
        .with_context(|| format!("clone TCP read for shutdown {}", peer.addr))?;
    hub.tcp_shutdown_handles.push(shutdown_handle);
    let label = format!("{label_prefix}-{}", peer.addr);
    let handle = reader::spawn_tcp_reader(
        read,
        label,
        data_tx.clone(),
        lifecycle_tx.clone(),
        shutdown.clone(),
    );
    hub.handles.push(handle);
    Ok(())
}

impl Variant for HybridVariant {
    fn name(&self) -> &str {
        "hybrid"
    }

    /// Hybrid supports both `Single` and `Multi`. See T14.4 in
    /// `metak-orchestrator/TASKS.md` and CUSTOM.md "Threading modes
    /// (T14.4)". In `Single` mode `poll_receive` is the existing
    /// inline UDP + TCP probe. In `Multi` mode the variant spawns one
    /// UDP recv thread plus one per-peer TCP reader thread and the
    /// driver thread only drains the resulting bounded mpsc.
    fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
        &[ThreadingMode::Single, ThreadingMode::Multi]
    }

    fn connect(&mut self, threading_mode: variant_base::ThreadingMode) -> Result<()> {
        // Stash for `start_reader_threads` and `poll_receive`.
        self.threading_mode = threading_mode;

        // Set up UDP multicast for QoS 1-2.
        let udp = UdpTransport::new(self.config.bind_addr, self.config.multicast_group)
            .context("failed to set up UDP multicast transport")?;
        // T14.1 / T14.4: apply the user-tunable SO_RCVBUF. Overrides
        // the implicit 8 MiB target from `tune_udp_buffers`. In Multi
        // mode the dedicated recv socket (created in
        // `start_reader_threads`) applies its own SO_RCVBUF from the
        // same value.
        udp.apply_recv_buffer_kb(self.config.recv_buffer_kb)
            .context("failed to apply --recv-buffer-kb on UDP socket")?;
        self.udp = Some(udp);

        // Set up TCP listener for QoS 3-4 on the runner-/qos-derived
        // port. T16.3: the transport needs the threading mode so it
        // can install `SO_SNDTIMEO` on Single-mode peer streams (and
        // skip it in Multi mode, where the reader-thread drain
        // breaks the wedge anyway).
        let mut tcp = TcpTransport::new(self.config.tcp_listen_addr, threading_mode)
            .context("failed to set up TCP transport")?;

        // Connect to each peer (excluding self -- already filtered in main).
        // T14.1 / T14.4: apply --recv-buffer-kb on every outbound TCP
        // socket before the read clone is made. `connect_to_peer` now
        // retries on `ConnectionRefused` for a bounded budget so the
        // two-runner startup race past the ready barrier is absorbed.
        for peer_addr in &self.config.tcp_peers {
            tcp.connect_to_peer(*peer_addr, Some(self.config.recv_buffer_kb))
                .with_context(|| format!("failed to connect to TCP peer {}", peer_addr))?;
        }

        self.tcp = Some(tcp);

        Ok(())
    }

    fn start_reader_threads(&mut self, mode: variant_base::ThreadingMode) -> Result<()> {
        if mode != ThreadingMode::Multi {
            return Ok(());
        }
        // Wait briefly for inbound TCP peers to dial in, so each
        // accepted stream gets a reader thread at startup. The
        // driver's stabilize phase gives the other side time to dial.
        let expected_inbound = self.config.tcp_peers.len();
        let recv_kb = Some(self.config.recv_buffer_kb);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        {
            let tcp = self.tcp.as_mut().context("TCP transport not connected")?;
            loop {
                tcp.accept_pending_with_buffer(recv_kb)?;
                if tcp.inbound_peers_mut().len() >= expected_inbound
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        let (mut hub, data_tx, lifecycle_tx) = ReaderHub::new();
        let shutdown = hub.shutdown.clone();

        // UDP recv thread: dedicated blocking recv socket sharing the
        // multicast group with the primary (send) socket. The primary
        // stays non-blocking for `try_send_nonblocking`; this one is
        // blocking with a short `SO_RCVTIMEO` so the reader thread
        // can poll the shutdown flag between attempts.
        let udp_recv = UdpTransport::make_blocking_recv_socket(
            self.config.bind_addr,
            self.config.multicast_group,
            self.config.recv_buffer_kb,
            reader::UDP_READER_TIMEOUT,
        )
        .context("failed to build Multi-mode UDP recv socket")?;
        let udp_shutdown = udp_recv
            .try_clone()
            .context("failed to clone UDP recv socket for shutdown signalling")?;
        hub.udp_shutdown_handle = Some(udp_shutdown);
        let udp_handle = reader::spawn_udp_reader(
            udp_recv,
            data_tx.clone(),
            lifecycle_tx.clone(),
            shutdown.clone(),
        );
        hub.handles.push(udp_handle);

        // Per-peer TCP reader threads.
        {
            let tcp = self.tcp.as_mut().context("TCP transport not connected")?;
            for peer in tcp.outbound_peers_mut() {
                spawn_tcp_reader_for(peer, "out", &data_tx, &lifecycle_tx, &shutdown, &mut hub)?;
            }
            for peer in tcp.inbound_peers_mut() {
                spawn_tcp_reader_for(peer, "in", &data_tx, &lifecycle_tx, &shutdown, &mut hub)?;
            }
        }
        // T15.8: per-peer control reader threads removed.

        // Drop the variant-held senders so the channels correctly
        // report `Disconnected` after every reader thread exits.
        drop(data_tx);
        drop(lifecycle_tx);

        self.reader_hub = Some(hub);
        Ok(())
    }

    fn stop_reader_threads(&mut self) -> Result<()> {
        // T15.8: control reader-thread teardown removed.
        if let Some(hub) = self.reader_hub.take() {
            hub.stop_and_join()
                .context("failed to stop hybrid reader threads")?;
        }
        Ok(())
    }

    fn publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<()> {
        match qos {
            Qos::BestEffort | Qos::LatestValue => {
                // QoS 1-2: UDP multicast.
                let udp = self.udp.as_ref().context("UDP transport not connected")?;
                let data = protocol::encode(qos, seq, path, &self.runner, payload);
                udp.send(&data)?;
            }
            Qos::ReliableUdp | Qos::ReliableTcp => {
                // QoS 3-4: TCP to each peer.
                let tcp = self.tcp.as_mut().context("TCP transport not connected")?;
                let data = protocol::encode_framed(qos, seq, path, &self.runner, payload);
                tcp.broadcast(&data)?;
            }
        }
        Ok(())
    }

    /// T-impl.7: honest backpressure for the driver.
    ///
    /// QoS 1/2 (UDP multicast) do a single non-blocking `send_to`.
    /// `WouldBlock` -> `Ok(false)`; the driver logs
    /// `backpressure_skipped` and moves on. The receiver tolerates the
    /// resulting seq gap (best-effort by definition, latest-value
    /// discards anything older than the newest seq anyway).
    ///
    /// QoS 3/4 (TCP) use the existing blocking `broadcast` -> always
    /// `Ok(true)`. TCP receivers expect contiguous sequences and the
    /// kernel send buffer is the natural pacing mechanism: a full
    /// send buffer makes `write_all` block, which is the back-pressure
    /// signal we want to measure for the benchmark. Returning
    /// `Ok(false)` here would corrupt the per-peer receiver state by
    /// emitting a seq the receiver will never see.
    ///
    /// See `variants/hybrid/CUSTOM.md` "Backpressure semantics (T-impl.7)".
    fn try_publish(&mut self, path: &str, payload: &[u8], qos: Qos, seq: u64) -> Result<bool> {
        match qos {
            Qos::BestEffort | Qos::LatestValue => {
                let udp = self.udp.as_ref().context("UDP transport not connected")?;
                let data = protocol::encode(qos, seq, path, &self.runner, payload);
                udp.try_send_nonblocking(&data)
            }
            Qos::ReliableUdp | Qos::ReliableTcp => {
                let tcp = self.tcp.as_mut().context("TCP transport not connected")?;
                let data = protocol::encode_framed(qos, seq, path, &self.runner, payload);
                tcp.broadcast(&data)?;
                Ok(true)
            }
        }
    }

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        // T14.4: Multi mode pulls from the reader-thread hub.
        if self.threading_mode == ThreadingMode::Multi && self.reader_hub.is_some() {
            return self.poll_receive_multi();
        }

        // T15.8: control-channel poll removed.

        // Single-mode inline polling (existing behaviour).
        const POLL_BUDGET: u32 = 256;
        for _ in 0..POLL_BUDGET {
            let udp_outcome = self.try_recv_udp()?;
            if let RecvOutcome::Data(update) = udp_outcome {
                return Ok(Some(update));
            }

            let tcp_outcome = self.try_recv_tcp()?;
            if let RecvOutcome::Data(update) = tcp_outcome {
                return Ok(Some(update));
            }

            let made_progress = matches!(udp_outcome, RecvOutcome::Consumed)
                || matches!(tcp_outcome, RecvOutcome::Consumed);
            if !made_progress {
                return Ok(None);
            }
        }
        Ok(None)
    }

    fn disconnect(&mut self) -> Result<()> {
        // T15.8: control-channel teardown removed.
        if let Some(udp) = self.udp.take() {
            udp.close()?;
        }
        if let Some(tcp) = self.tcp.take() {
            tcp.close()?;
        }
        self.latest_seq.clear();
        Ok(())
    }

    // T15.8: signal_end_of_test / poll_peer_eots removed from the trait.
    // The on-wire EOT exchange (and the T14.18 control TCP side-channel)
    // is gone; the runner-coordinated termination (T15.4) plus variant-
    // side idle detection (T15.5) drive end-of-operate.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_config() -> HybridConfig {
        HybridConfig {
            multicast_group: "239.0.0.1:9000".parse().unwrap(),
            bind_addr: Ipv4Addr::UNSPECIFIED,
            tcp_listen_addr: "0.0.0.0:0".parse().unwrap(),
            tcp_peers: Vec::new(),
            qos: Qos::BestEffort,
            recv_buffer_kb: 4096,
        }
    }

    #[test]
    fn qos2_stale_discard() {
        let mut v = HybridVariant::new("self", dummy_config());

        // First message with seq=5 is not stale.
        assert!(!v.is_stale_qos2("writer-a", "/path", 5));
        // Same seq is stale.
        assert!(v.is_stale_qos2("writer-a", "/path", 5));
        // Lower seq is stale.
        assert!(v.is_stale_qos2("writer-a", "/path", 3));
        // Higher seq is not stale.
        assert!(!v.is_stale_qos2("writer-a", "/path", 10));
        // Different writer is independent.
        assert!(!v.is_stale_qos2("writer-b", "/path", 1));
        // Different path is independent.
        assert!(!v.is_stale_qos2("writer-a", "/other", 1));
    }

    #[test]
    fn name_returns_hybrid() {
        let v = HybridVariant::new("r", dummy_config());
        assert_eq!(v.name(), "hybrid");
    }

    // T15.8: removed EOT-related tests
    //   - record_eot_dedupes_by_writer_and_id
    //   - record_eot_filters_own_runner
    //   - record_eot_preserves_arrival_order
    //   - udp_retry_and_dedup_via_record_eot
    //   - signal_end_of_test_returns_nonzero_id_no_peers
    //   - signal_end_of_test_dispatches_over_control
    // They exercised the on-wire EOT exchange and the T14.18 control TCP
    // channel, both removed in T15.8.

    // ---- T-impl.7: try_publish backpressure semantics ----

    /// Detect whether the host's UDP loopback path can surface
    /// `WouldBlock` under SO_SNDBUF pressure. Some platforms (notably
    /// some Windows NIC configurations) silently drop datagrams at a
    /// layer below the syscall return without ever reporting
    /// `WouldBlock`, in which case we can't *force* the override into
    /// the `Ok(false)` branch with a real socket — we then settle for
    /// "every `try_publish` call returns `Ok(true)` without erroring".
    fn host_surfaces_udp_wouldblock() -> bool {
        use socket2::{Domain, Protocol as P2, SockAddr, Socket, Type};
        use std::io;
        use std::net::SocketAddrV4;
        let Ok(socket) = Socket::new(Domain::IPV4, Type::DGRAM, Some(P2::UDP)) else {
            return false;
        };
        if socket.set_nonblocking(true).is_err() {
            return false;
        }
        let _ = socket.set_send_buffer_size(1024);
        let bind_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        if socket.bind(&SockAddr::from(bind_addr)).is_err() {
            return false;
        }
        let target = SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1));
        let payload = vec![0u8; 60_000];
        for _ in 0..200_000 {
            match socket.send_to(&payload, &target) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
        false
    }

    /// Build a `HybridVariant` whose UDP transport is replaced with
    /// one bound to a tiny `SO_SNDBUF`, targeting a closed loopback
    /// port. The variant's `connect()` is NOT called; we splice in the
    /// transport directly to keep the test deterministic.
    fn make_pressured_variant(qos: Qos) -> HybridVariant {
        use crate::udp::UdpTransport;
        use socket2::{Domain, Protocol as P2, SockAddr, Socket, Type};
        use std::net::{SocketAddrV4, UdpSocket};

        // Build a real `UdpTransport`, then swap its socket out for a
        // tiny-SNDBUF one targeting a discard port. `UdpTransport`'s
        // fields are `pub(crate)` for the socket and `pub(crate)` for
        // the multicast_addr; both are reachable from this module.
        let raw = Socket::new(Domain::IPV4, Type::DGRAM, Some(P2::UDP)).unwrap();
        let _ = raw.set_reuse_address(true);
        raw.set_nonblocking(true).unwrap();
        let _ = raw.set_send_buffer_size(1024);
        let bind = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        raw.bind(&SockAddr::from(bind)).unwrap();
        let socket: UdpSocket = raw.into();
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);

        let transport = UdpTransport::from_raw_for_test(socket, target);
        let mut cfg = dummy_config();
        cfg.qos = qos;
        cfg.multicast_group = target;
        let mut v = HybridVariant::new("test-runner", cfg);
        v.udp = Some(transport);
        v
    }

    /// QoS 1 (BestEffort): `try_publish` honestly reports backpressure
    /// when the UDP send buffer fills. With a 1 KB SO_SNDBUF and
    /// 60 KB payloads, a tight loop must hit `Ok(false)` quickly OR
    /// the host doesn't surface `WouldBlock` for loopback UDP at all
    /// (in which case the override still must not panic or return
    /// `Err`, which is checked implicitly by the loop completing).
    #[test]
    fn try_publish_qos1_returns_false_under_send_buffer_pressure() {
        let mut v = make_pressured_variant(Qos::BestEffort);
        let payload = vec![0xABu8; 60_000];
        let mut saw_false = false;
        for seq in 0..200_000u64 {
            match v.try_publish("/p", &payload, Qos::BestEffort, seq) {
                Ok(true) => {}
                Ok(false) => {
                    saw_false = true;
                    break;
                }
                Err(e) => panic!("try_publish errored: {e:#}"),
            }
        }
        if !saw_false && host_surfaces_udp_wouldblock() {
            panic!("expected try_publish to return Ok(false) on QoS 1 — host can surface WouldBlock but try_publish did not");
        }
    }

    /// QoS 2 (LatestValue): same shape as QoS 1.
    #[test]
    fn try_publish_qos2_returns_false_under_send_buffer_pressure() {
        let mut v = make_pressured_variant(Qos::LatestValue);
        let payload = vec![0xCDu8; 60_000];
        let mut saw_false = false;
        for seq in 0..200_000u64 {
            match v.try_publish("/p", &payload, Qos::LatestValue, seq) {
                Ok(true) => {}
                Ok(false) => {
                    saw_false = true;
                    break;
                }
                Err(e) => panic!("try_publish errored: {e:#}"),
            }
        }
        if !saw_false && host_surfaces_udp_wouldblock() {
            panic!("expected try_publish to return Ok(false) on QoS 2 — host can surface WouldBlock but try_publish did not");
        }
    }

    /// QoS 3 (ReliableUdp): TCP transport, blocking `broadcast`. With
    /// no peers connected the broadcast is a no-op, but it must still
    /// return `Ok(true)` — TCP receivers expect contiguous seqs and
    /// the driver must not log `backpressure_skipped` for QoS 3/4.
    #[test]
    fn try_publish_qos3_never_reports_backpressure_no_peers() {
        use crate::tcp::TcpTransport;
        let mut cfg = dummy_config();
        cfg.qos = Qos::ReliableUdp;
        let mut v = HybridVariant::new("test-runner", cfg);
        // Splice in a TCP transport with no peers. broadcast() over an
        // empty peer set is a no-op that still returns Ok.
        v.tcp = Some(
            TcpTransport::new(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                ThreadingMode::Single,
            )
            .unwrap(),
        );

        let payload = vec![0u8; 64];
        for seq in 0..100u64 {
            let result = v
                .try_publish("/p", &payload, Qos::ReliableUdp, seq)
                .expect("QoS 3 try_publish must succeed");
            assert!(
                result,
                "QoS 3 must never return Ok(false) — TCP path, contiguous seqs required"
            );
        }
    }

    /// QoS 4 (ReliableTcp): identical contract to QoS 3 — TCP path,
    /// always `Ok(true)`.
    #[test]
    fn try_publish_qos4_never_reports_backpressure_no_peers() {
        use crate::tcp::TcpTransport;
        let mut cfg = dummy_config();
        cfg.qos = Qos::ReliableTcp;
        let mut v = HybridVariant::new("test-runner", cfg);
        v.tcp = Some(
            TcpTransport::new(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                ThreadingMode::Single,
            )
            .unwrap(),
        );

        let payload = vec![0u8; 64];
        for seq in 0..100u64 {
            let result = v
                .try_publish("/p", &payload, Qos::ReliableTcp, seq)
                .expect("QoS 4 try_publish must succeed");
            assert!(
                result,
                "QoS 4 must never return Ok(false) — TCP receivers expect contiguous seqs"
            );
        }
    }

    /// Happy path: when nothing is backpressured, `try_publish`
    /// returns `Ok(true)` on QoS 1. Uses a real loopback multicast
    /// socket (same path the variant uses in production).
    #[test]
    fn try_publish_happy_path_returns_true() {
        let mut cfg = dummy_config();
        cfg.multicast_group = "239.0.0.1:19952".parse().unwrap();
        let mut v = HybridVariant::new("self", cfg);
        if v.connect(variant_base::ThreadingMode::Single).is_err() {
            // CI without multicast: skip silently.
            return;
        }
        let result = v
            .try_publish("/p", b"x", Qos::BestEffort, 0)
            .expect("happy-path try_publish must succeed");
        assert!(result, "expected Ok(true) on idle transport");
        v.disconnect().ok();
    }

    // ---- T14.16: EOT survives reader-channel saturation ----

    /// Build a minimal `ReaderHub` whose data channel has the
    /// requested tiny capacity, and an unbounded lifecycle channel.
    /// Returns the hub plus matching data + lifecycle sender pairs.
    /// Mirrors `ReaderHub::new` but uses a custom data capacity so the
    /// test can saturate it deterministically.
    #[allow(dead_code)]
    fn small_hub(
        data_capacity: usize,
    ) -> (
        ReaderHub,
        std::sync::mpsc::SyncSender<HubDataMessage>,
        std::sync::mpsc::Sender<HubLifecycleMessage>,
    ) {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let (data_tx, data_rx) = std::sync::mpsc::sync_channel(data_capacity);
        let (lifecycle_tx, lifecycle_rx) = std::sync::mpsc::channel();
        let hub = ReaderHub {
            rx: data_rx,
            lifecycle_rx,
            shutdown: Arc::new(AtomicBool::new(false)),
            handles: Vec::new(),
            tcp_shutdown_handles: Vec::new(),
            udp_shutdown_handle: None,
        };
        (hub, data_tx, lifecycle_tx)
    }

    // T15.8: removed reader-hub EOT survival tests
    //   - t14_16_eot_survives_data_channel_saturation
    //   - t14_16_lifecycle_drained_before_data
    // They asserted poll_peer_eots semantics that no longer exist.
}
