//! Symmetric peer pairing and port derivation for the WebSocket variant.
//!
//! Uses the same conventions as the Hybrid TCP path and the QUIC variant:
//!
//!   runner_stride = 1
//!   qos_stride    = 10
//!   port = base_port + runner_index * runner_stride + (qos - 1) * qos_stride
//!
//! Pairing role is decided by sorted-name comparison: the lower-sorted-name
//! runner is the WebSocket client (it connects), the higher-sorted-name
//! runner is the WebSocket server (it accepts). One connection per pair.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{anyhow, Context, Result};

/// Stride applied per runner index when computing per-spawn ports.
pub const RUNNER_STRIDE: u16 = 1;
/// Stride applied per QoS level (qos-1) when computing per-spawn ports.
pub const QOS_STRIDE: u16 = 10;

/// A peer description after pairing/role resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDesc {
    /// Peer's runner name.
    pub name: String,
    /// Concrete socket address used for either dialing (when this runner
    /// is the client for the pair) or expected from `accept` (informational
    /// when this runner is the server).
    pub addr: SocketAddr,
    /// Role this runner takes for this peer pair.
    pub role: PairRole,
}

/// Pairing role for a single peer relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRole {
    /// This runner connects to the peer (lower-sorted-name).
    Client,
    /// This runner accepts the peer (higher-sorted-name).
    Server,
}

/// Result of pairing/port derivation: the local listen address and the
/// per-peer descriptions.
#[derive(Debug, Clone)]
pub struct DerivedEndpoints {
    pub listen_addr: SocketAddr,
    pub peers: Vec<PeerDesc>,
}

/// Parse a `--peers name1=host1,name2=host2,...` value into a sorted-by-name
/// vector of `(name, host)` pairs.
pub fn parse_peers(raw: &str) -> Result<Vec<(String, String)>> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, host) = part
            .split_once('=')
            .with_context(|| format!("malformed peer entry '{part}', expected name=host"))?;
        let name = name.trim();
        let host = host.trim();
        if name.is_empty() || host.is_empty() {
            anyhow::bail!("malformed peer entry '{part}': empty name or host");
        }
        entries.push((name.to_string(), host.to_string()));
    }
    if entries.is_empty() {
        anyhow::bail!("--peers must contain at least one name=host pair");
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Derive the local listen address and per-peer descriptions from the
/// runner-injected `--peers` map, this runner's name, the configured
/// `base_port`, and the per-spawn `qos` level.
pub fn derive_endpoints(
    peer_map: &[(String, String)],
    runner: &str,
    base_port: u16,
    qos: u8,
) -> Result<DerivedEndpoints> {
    if !(1..=4).contains(&qos) {
        anyhow::bail!("invalid --qos {qos}; expected 1..=4");
    }

    let runner_index = peer_map
        .iter()
        .position(|(name, _)| name == runner)
        .ok_or_else(|| {
            anyhow!(
                "runner '{}' not present in --peers (have: {})",
                runner,
                peer_map
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })?;

    let qos_offset = (qos as u16 - 1)
        .checked_mul(QOS_STRIDE)
        .ok_or_else(|| anyhow!("qos offset overflow"))?;
    let runner_offset = (runner_index as u16)
        .checked_mul(RUNNER_STRIDE)
        .ok_or_else(|| anyhow!("runner offset overflow (too many peers)"))?;

    let my_listen_port = base_port
        .checked_add(runner_offset)
        .and_then(|p| p.checked_add(qos_offset))
        .ok_or_else(|| anyhow!("listen port overflow"))?;
    let listen_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), my_listen_port);

    let mut peers: Vec<PeerDesc> = Vec::new();
    for (idx, (name, host)) in peer_map.iter().enumerate() {
        if name == runner {
            continue;
        }
        let peer_runner_offset = (idx as u16)
            .checked_mul(RUNNER_STRIDE)
            .ok_or_else(|| anyhow!("peer runner offset overflow"))?;
        let peer_port = base_port
            .checked_add(peer_runner_offset)
            .and_then(|p| p.checked_add(qos_offset))
            .ok_or_else(|| anyhow!("peer port overflow"))?;
        let peer_ip: IpAddr = host
            .parse()
            .with_context(|| format!("invalid peer host IP '{host}' for '{name}'"))?;
        let addr = SocketAddr::new(peer_ip, peer_port);
        // Sorted name comparison decides role. Lower sorted name connects.
        let role = if runner < name.as_str() {
            PairRole::Client
        } else {
            PairRole::Server
        };
        peers.push(PeerDesc {
            name: name.clone(),
            addr,
            role,
        });
    }

    Ok(DerivedEndpoints { listen_addr, peers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_peers_single() {
        let parsed = parse_peers("self=127.0.0.1").unwrap();
        assert_eq!(parsed, vec![("self".into(), "127.0.0.1".into())]);
    }

    #[test]
    fn parse_peers_sorts_by_name() {
        let parsed = parse_peers("bob=10.0.0.2,alice=10.0.0.1").unwrap();
        assert_eq!(
            parsed,
            vec![
                ("alice".into(), "10.0.0.1".into()),
                ("bob".into(), "10.0.0.2".into()),
            ]
        );
    }

    #[test]
    fn parse_peers_trims_whitespace() {
        let parsed = parse_peers(" alice = 127.0.0.1 , bob = 127.0.0.1 ").unwrap();
        assert_eq!(
            parsed,
            vec![
                ("alice".into(), "127.0.0.1".into()),
                ("bob".into(), "127.0.0.1".into()),
            ]
        );
    }

    #[test]
    fn parse_peers_rejects_malformed() {
        assert!(parse_peers("alice").is_err());
        assert!(parse_peers("alice=").is_err());
        assert!(parse_peers("=127.0.0.1").is_err());
        assert!(parse_peers("").is_err());
    }

    #[test]
    fn alice_is_client_to_bob() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "alice", 19960, 3).unwrap();
        assert_eq!(derived.listen_addr.port(), 19960 + 20);
        assert_eq!(derived.peers.len(), 1);
        let bob = &derived.peers[0];
        assert_eq!(bob.name, "bob");
        assert_eq!(bob.addr.port(), 19960 + 1 + 20);
        assert_eq!(bob.role, PairRole::Client, "alice < bob => alice connects");
    }

    #[test]
    fn bob_is_server_to_alice() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "bob", 19960, 4).unwrap();
        assert_eq!(derived.listen_addr.port(), 19960 + 1 + 30);
        assert_eq!(derived.peers.len(), 1);
        let alice = &derived.peers[0];
        assert_eq!(alice.name, "alice");
        assert_eq!(alice.addr.port(), 19960 + 30);
        assert_eq!(alice.role, PairRole::Server, "bob > alice => bob accepts");
    }

    #[test]
    fn three_peers_role_decision() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1,carol=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "bob", 19960, 3).unwrap();
        // bob's peers: alice (server, since bob > alice) and carol (client, since bob < carol).
        let by_name: std::collections::HashMap<&str, PairRole> = derived
            .peers
            .iter()
            .map(|p| (p.name.as_str(), p.role))
            .collect();
        assert_eq!(by_name.get("alice"), Some(&PairRole::Server));
        assert_eq!(by_name.get("carol"), Some(&PairRole::Client));
    }

    #[test]
    fn port_derivation_all_qos_levels_disjoint() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let mut all_ports = std::collections::HashSet::new();
        for runner in ["alice", "bob"] {
            for qos in 1..=4u8 {
                let d = derive_endpoints(&peers, runner, 19960, qos).unwrap();
                assert!(
                    all_ports.insert(d.listen_addr.port()),
                    "duplicate listen port {} for {} qos {}",
                    d.listen_addr.port(),
                    runner,
                    qos
                );
            }
        }
        assert_eq!(all_ports.len(), 8);
    }

    #[test]
    fn runner_not_in_peers_errors() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let err = derive_endpoints(&peers, "carol", 19960, 3).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("carol") && msg.contains("not present"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn invalid_qos_errors() {
        let peers = parse_peers("a=127.0.0.1").unwrap();
        assert!(derive_endpoints(&peers, "a", 19960, 0).is_err());
        assert!(derive_endpoints(&peers, "a", 19960, 5).is_err());
    }

    #[test]
    fn self_only_no_peers_to_connect() {
        let peers = parse_peers("self=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "self", 19960, 3).unwrap();
        assert_eq!(derived.listen_addr.port(), 19960 + 20);
        assert!(derived.peers.is_empty());
    }
}
