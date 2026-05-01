/// HybridVariant: UDP multicast for QoS 1-2, TCP for QoS 3-4.
///
/// This is the "simplest correct" approach. No application-layer reliability
/// logic at all -- kernel TCP handles everything for reliable delivery.
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use anyhow::{Context, Result};

use variant_base::types::{Qos, ReceivedUpdate};
use variant_base::Variant;

use crate::protocol;
use crate::tcp::TcpTransport;
use crate::udp::UdpTransport;

/// Receive buffer size for UDP datagrams.
const UDP_RECV_BUF_SIZE: usize = 65536;

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
}

/// Hybrid UDP/TCP variant implementing the Variant trait.
pub struct HybridVariant {
    runner: String,
    config: HybridConfig,
    udp: Option<UdpTransport>,
    tcp: Option<TcpTransport>,
    /// Track highest sequence number per (writer, path) for QoS 2 stale discard.
    latest_seq: HashMap<(String, String), u64>,
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
        // Check UDP first.
        if let Some(udp) = &self.udp {
            let mut buf = [0u8; UDP_RECV_BUF_SIZE];
            if let Some(n) = udp.try_recv(&mut buf)? {
                let update = protocol::decode(&buf[..n])?;
                // QoS 2: discard stale values.
                if update.qos == Qos::LatestValue
                    && self.is_stale_qos2(&update.writer, &update.path, update.seq)
                {
                    // Stale -- skip and try again.
                    return self.poll_receive();
                }
                return Ok(Some(update));
            }
        }

        // Check TCP.
        if let Some(tcp) = &mut self.tcp {
            if let Some(msg) = tcp.try_recv()? {
                let update = protocol::decode(&msg)?;
                return Ok(Some(update));
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
}
