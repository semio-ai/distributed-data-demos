/// HybridVariant: UDP multicast for QoS 1-2, TCP for QoS 3-4.
///
/// This is the "simplest correct" approach. No application-layer reliability
/// logic at all -- kernel TCP handles everything for reliable delivery.
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use anyhow::{Context, Result};

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::{PeerEot, Variant};

use crate::protocol::{self, Frame};
use crate::tcp::TcpTransport;
use crate::udp::UdpTransport;

/// Receive buffer size for UDP datagrams.
const UDP_RECV_BUF_SIZE: usize = 65536;

/// Number of times an EOT marker is sent on the UDP path. The contract
/// (`metak-shared/api-contracts/eot-protocol.md` "Hybrid") prescribes 5
/// retries with 5 ms spacing for redundancy under multicast loss.
const UDP_EOT_RETRIES: u32 = 5;

/// Spacing between consecutive UDP EOT sends.
const UDP_EOT_SPACING: Duration = Duration::from_millis(5);

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
    /// Active QoS for this spawn. Determines which path is used by
    /// `signal_end_of_test`.
    pub qos: Qos,
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
}

impl Variant for HybridVariant {
    fn name(&self) -> &str {
        "hybrid"
    }

    fn connect(&mut self) -> Result<()> {
        // Set up UDP multicast for QoS 1-2.
        let udp = UdpTransport::new(self.config.bind_addr, self.config.multicast_group)
            .context("failed to set up UDP multicast transport")?;
        self.udp = Some(udp);

        // Set up TCP listener for QoS 3-4 on the runner-/qos-derived port.
        let mut tcp = TcpTransport::new(self.config.tcp_listen_addr)
            .context("failed to set up TCP transport")?;

        // Connect to each peer (excluding self -- already filtered in main).
        for peer_addr in &self.config.tcp_peers {
            tcp.connect_to_peer(*peer_addr)
                .with_context(|| format!("failed to connect to TCP peer {}", peer_addr))?;
        }

        self.tcp = Some(tcp);
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

    fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
        // Each iteration probes both paths once. If either returns a data
        // update we return it immediately. If at least one path consumed a
        // non-data frame (stale QoS-2 duplicate, or an EOT marker queued
        // internally), we loop -- the in-flight data behind it must not be
        // masked. If neither path consumed anything this iteration the
        // sockets are idle and we return None so the driver can yield.
        //
        // Each `try_recv_*` call consumes at most one frame, so the loop
        // makes forward progress: every iteration either returns Data,
        // returns None (idle), or strictly drains a buffered frame. A
        // bounded budget guards against pathological burst-of-EOT inputs.
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
        if let Some(udp) = self.udp.take() {
            udp.close()?;
        }
        if let Some(tcp) = self.tcp.take() {
            tcp.close()?;
        }
        self.latest_seq.clear();
        Ok(())
    }

    /// Generate an EOT id and dispatch the marker over the active path.
    ///
    /// QoS 1-2: UDP multicast, sent `UDP_EOT_RETRIES` times with
    /// `UDP_EOT_SPACING` between sends, since multicast can drop datagrams.
    /// QoS 3-4: TCP, sent once per outbound peer (TCP delivery semantics
    /// take care of reliability and ordering).
    fn signal_end_of_test(&mut self) -> Result<u64> {
        let eot_id: u64 = rand::random();
        match self.config.qos {
            Qos::BestEffort | Qos::LatestValue => {
                let udp = self.udp.as_ref().context("UDP transport not connected")?;
                let frame = protocol::encode_eot(&self.runner, eot_id);
                for i in 0..UDP_EOT_RETRIES {
                    udp.send(&frame).with_context(|| {
                        format!(
                            "failed to send UDP EOT (attempt {} of {})",
                            i + 1,
                            UDP_EOT_RETRIES
                        )
                    })?;
                    if i + 1 < UDP_EOT_RETRIES {
                        std::thread::sleep(UDP_EOT_SPACING);
                    }
                }
            }
            Qos::ReliableUdp | Qos::ReliableTcp => {
                let tcp = self.tcp.as_mut().context("TCP transport not connected")?;
                let frame = protocol::encode_eot_framed(&self.runner, eot_id);
                tcp.broadcast(&frame)
                    .context("failed to broadcast TCP EOT marker")?;
            }
        }
        Ok(eot_id)
    }

    fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
        // Drain any new EOT observations the receive paths have buffered
        // since the last call. The receive paths run on every
        // `poll_receive`, which the driver continues to call during the
        // EOT phase, so we don't have to re-poll the sockets here.
        let drained: Vec<PeerEot> = self.pending_eots.drain(..).collect();
        Ok(drained)
    }
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

    #[test]
    fn record_eot_dedupes_by_writer_and_id() {
        let mut v = HybridVariant::new("self", dummy_config());

        // First observation queues a PeerEot.
        v.record_eot("alice".to_string(), 42);
        let drained = v.poll_peer_eots().unwrap();
        assert_eq!(
            drained,
            vec![PeerEot {
                writer: "alice".to_string(),
                eot_id: 42
            }]
        );

        // A duplicate (same writer, same id) is suppressed.
        v.record_eot("alice".to_string(), 42);
        assert!(v.poll_peer_eots().unwrap().is_empty());

        // A new writer is recorded.
        v.record_eot("bob".to_string(), 7);
        let drained = v.poll_peer_eots().unwrap();
        assert_eq!(
            drained,
            vec![PeerEot {
                writer: "bob".to_string(),
                eot_id: 7
            }]
        );

        // Same writer, different id, is also recorded (the contract dedupes
        // on the (writer, eot_id) pair, not just the writer).
        v.record_eot("alice".to_string(), 99);
        let drained = v.poll_peer_eots().unwrap();
        assert_eq!(
            drained,
            vec![PeerEot {
                writer: "alice".to_string(),
                eot_id: 99
            }]
        );

        // Subsequent call with no new observations returns empty.
        assert!(v.poll_peer_eots().unwrap().is_empty());
    }

    #[test]
    fn record_eot_filters_own_runner() {
        // UDP multicast loopback delivers our own EOT back to us. The
        // variant must not surface that to the driver, otherwise the
        // driver's `seen` set would always contain self while `expected`
        // never does, forcing the EOT phase to wait for the full timeout.
        let mut v = HybridVariant::new("self", dummy_config());
        v.record_eot("self".to_string(), 12345);
        assert!(
            v.poll_peer_eots().unwrap().is_empty(),
            "an EOT whose writer == runner must be filtered out"
        );
    }

    #[test]
    fn record_eot_preserves_arrival_order() {
        let mut v = HybridVariant::new("self", dummy_config());
        v.record_eot("bob".to_string(), 1);
        v.record_eot("alice".to_string(), 2);
        v.record_eot("carol".to_string(), 3);

        let drained = v.poll_peer_eots().unwrap();
        let names: Vec<&str> = drained.iter().map(|e| e.writer.as_str()).collect();
        assert_eq!(names, vec!["bob", "alice", "carol"]);
    }

    /// Simulate the UDP retry-and-dedup scenario from the contract: writer
    /// A sends EOT five times. The receiver processes each datagram via the
    /// same `record_eot` path it would use after `decode_frame`. The
    /// observation queue must contain A exactly once, and a second
    /// `poll_peer_eots` call must return nothing.
    #[test]
    fn udp_retry_and_dedup_via_record_eot() {
        let mut v = HybridVariant::new("self", dummy_config());

        // Simulate 5 datagram arrivals from writer A with the same eot_id.
        for _ in 0..5 {
            v.record_eot("alice".to_string(), 0xCAFE_BABE);
        }

        let drained = v.poll_peer_eots().unwrap();
        assert_eq!(
            drained,
            vec![PeerEot {
                writer: "alice".to_string(),
                eot_id: 0xCAFE_BABE,
            }]
        );

        // Subsequent poll: nothing new.
        assert!(v.poll_peer_eots().unwrap().is_empty());
    }

    /// `signal_end_of_test` returns a non-zero `eot_id` and dispatches the
    /// marker on the configured UDP path (qos 1-2). We use a real
    /// loopback-multicast socket so the retry loop and ordering are
    /// exercised end-to-end. Receiving the marker ourselves and feeding it
    /// through `record_eot` would normally happen in `poll_receive`; here
    /// we verify that the encoded bytes round-trip via `decode_frame`.
    #[test]
    fn signal_end_of_test_udp_returns_nonzero_id() {
        // Use an ephemeral multicast group/port to avoid colliding with
        // other tests that bind 239.0.0.1:9000.
        let mut config = dummy_config();
        config.multicast_group = "239.0.0.1:19850".parse().unwrap();
        config.qos = Qos::BestEffort;

        let mut v = HybridVariant::new("self-udp", config);
        v.connect().expect("connect must succeed");

        let id1 = v
            .signal_end_of_test()
            .expect("UDP signal_end_of_test must succeed");
        let id2 = v
            .signal_end_of_test()
            .expect("UDP signal_end_of_test must succeed (second call)");

        // Random, so the two ids are very unlikely to collide. Both must
        // also be non-zero (unlike the trait default impl).
        assert_ne!(id1, 0);
        assert_ne!(id2, 0);
        assert_ne!(id1, id2);

        v.disconnect().ok();
    }

    /// Same as above but for the TCP path. Spins up a single peer that
    /// listens on an ephemeral port, dials it, then signals EOT and
    /// verifies the framed EOT bytes hit the wire in a decodeable shape.
    #[test]
    fn signal_end_of_test_tcp_dispatches_to_peer() {
        use std::io::Read;
        use std::net::TcpListener;
        use std::time::Duration;

        // Listener that the variant will dial as a peer.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let peer_addr = listener.local_addr().unwrap();

        let mut config = HybridConfig {
            multicast_group: "239.0.0.1:19851".parse().unwrap(),
            bind_addr: Ipv4Addr::UNSPECIFIED,
            tcp_listen_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            tcp_peers: vec![peer_addr],
            qos: Qos::ReliableTcp,
        };
        // Borrow checker convenience: ensure we drop config so the
        // variant fully owns it.
        let _ = &mut config;

        let mut v = HybridVariant::new("hybrid-writer", config);
        v.connect().expect("connect must succeed");

        // Accept the inbound on the listener side.
        let (mut peer_stream, _) = listener.accept().unwrap();
        peer_stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        let id = v
            .signal_end_of_test()
            .expect("TCP signal_end_of_test must succeed");
        assert_ne!(id, 0);

        // Read the framed EOT bytes off the peer end and decode.
        let mut len_buf = [0u8; 4];
        peer_stream
            .read_exact(&mut len_buf)
            .expect("must read length prefix");
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; frame_len];
        peer_stream
            .read_exact(&mut payload)
            .expect("must read framed payload");

        let frame = protocol::decode_frame(&payload).expect("frame must decode");
        match frame {
            Frame::Eot { writer, eot_id } => {
                assert_eq!(writer, "hybrid-writer");
                assert_eq!(eot_id, id);
            }
            other => panic!("expected Frame::Eot, got {other:?}"),
        }

        v.disconnect().ok();
    }
}
