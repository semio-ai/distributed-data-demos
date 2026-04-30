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

/// Configuration for the hybrid variant parsed from CLI extra args.
pub struct HybridConfig {
    pub multicast_group: SocketAddrV4,
    pub tcp_base_port: u16,
    pub bind_addr: Ipv4Addr,
    pub peers: Vec<String>,
}

impl HybridConfig {
    /// Parse variant-specific arguments from the extra CLI args.
    ///
    /// Expected format: pairs of `--key value` in any order.
    pub fn from_extra_args(extra: &[String]) -> Result<Self> {
        let mut multicast_group: SocketAddrV4 = "239.0.0.1:9000"
            .parse()
            .expect("valid default multicast address");
        let mut tcp_base_port: u16 = 19900;
        let mut bind_addr: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);
        let mut peers: Vec<String> = Vec::new();

        let mut i = 0;
        while i < extra.len() {
            match extra[i].as_str() {
                "--multicast-group" => {
                    i += 1;
                    let val = extra.get(i).context("--multicast-group requires a value")?;
                    multicast_group = val
                        .parse()
                        .with_context(|| format!("invalid multicast group: {}", val))?;
                }
                "--tcp-base-port" => {
                    i += 1;
                    let val = extra.get(i).context("--tcp-base-port requires a value")?;
                    tcp_base_port = val
                        .parse()
                        .with_context(|| format!("invalid tcp-base-port: {}", val))?;
                }
                "--bind-addr" => {
                    i += 1;
                    let val = extra.get(i).context("--bind-addr requires a value")?;
                    bind_addr = val
                        .parse()
                        .with_context(|| format!("invalid bind-addr: {}", val))?;
                }
                "--peers" => {
                    i += 1;
                    let val = extra.get(i).context("--peers requires a value")?;
                    peers = val
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                other => {
                    anyhow::bail!("unknown variant-specific argument: {}", other);
                }
            }
            i += 1;
        }

        Ok(Self {
            multicast_group,
            tcp_base_port,
            bind_addr,
            peers,
        })
    }
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
    /// Create a new HybridVariant from the runner name and parsed config.
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

        // Set up TCP for QoS 3-4.
        let tcp_listen_addr =
            SocketAddr::new(self.config.bind_addr.into(), self.config.tcp_base_port);

        let tcp_listen_addr = SocketAddr::new(
            self.config.bind_addr.into(),
            self.config.tcp_base_port + if self.runner == "bob" { 1 } else { 0 },
        );

        let mut tcp =
            TcpTransport::new(tcp_listen_addr).context("failed to set up TCP transport")?;

        // Connect to each configured peer.
        for peer_str in &self.config.peers {
            let mut addr: SocketAddr = peer_str
                .parse()
                .with_context(|| format!("invalid peer address: {}", peer_str))?;

            // Apply the inverse logic: if we are Alice, we look for Bob at port + 1.
            // If we are Bob, we look for Alice at the base port (+ 0).
            if self.runner == "alice" {
                addr.set_port(addr.port() + 1);
            }

            tcp.connect_to_peer(addr)
                .with_context(|| format!("failed to connect to TCP peer {}", addr))?;
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

    #[test]
    fn parse_default_config() {
        let config = HybridConfig::from_extra_args(&[]).unwrap();
        assert_eq!(
            config.multicast_group,
            "239.0.0.1:9000".parse::<SocketAddrV4>().unwrap()
        );
        assert_eq!(config.tcp_base_port, 19900);
        assert_eq!(config.bind_addr, Ipv4Addr::new(0, 0, 0, 0));
        assert!(config.peers.is_empty());
    }

    #[test]
    fn parse_custom_config() {
        let extra: Vec<String> = vec![
            "--multicast-group",
            "239.1.2.3:8000",
            "--tcp-base-port",
            "20000",
            "--bind-addr",
            "127.0.0.1",
            "--peers",
            "127.0.0.1:20001,127.0.0.1:20002",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let config = HybridConfig::from_extra_args(&extra).unwrap();
        assert_eq!(
            config.multicast_group,
            "239.1.2.3:8000".parse::<SocketAddrV4>().unwrap()
        );
        assert_eq!(config.tcp_base_port, 20000);
        assert_eq!(config.bind_addr, Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(config.peers, vec!["127.0.0.1:20001", "127.0.0.1:20002"]);
    }

    #[test]
    fn parse_unknown_arg_rejected() {
        let extra: Vec<String> = vec!["--unknown".to_string(), "value".to_string()];
        assert!(HybridConfig::from_extra_args(&extra).is_err());
    }

    #[test]
    fn qos2_stale_discard() {
        let config = HybridConfig::from_extra_args(&[]).unwrap();
        let mut v = HybridVariant::new("self", config);

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
        let config = HybridConfig::from_extra_args(&[]).unwrap();
        let v = HybridVariant::new("r", config);
        assert_eq!(v.name(), "hybrid");
    }
}
