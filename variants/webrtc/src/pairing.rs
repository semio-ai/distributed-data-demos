//! Symmetric peer pairing and port derivation for the WebRTC variant.
//!
//! Uses the same conventions as the QUIC variant and the WebSocket variant:
//!
//!   runner_stride = 1
//!   qos_stride    = 10
//!   port = base_port + runner_index * runner_stride + (qos - 1) * qos_stride
//!
//! Two ports are derived per spawn: a TCP signaling port (per-pair signaling
//! socket carrying SDP offer/answer + trickle ICE) and a UDP media port
//! (the host candidate that ICE will gather and use for the DTLS/SCTP
//! flow).
//!
//! Pairing role for the signaling channel is decided by sorted-name
//! comparison: the lower-sorted-name runner connects (and sends the SDP
//! offer); the higher-sorted-name runner accepts (and sends the SDP
//! answer).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{anyhow, Context, Result};

/// Stride applied per runner index when computing per-spawn ports.
pub const RUNNER_STRIDE: u16 = 1;
/// Stride applied per QoS level (qos-1) when computing per-spawn ports.
pub const QOS_STRIDE: u16 = 10;

/// Pairing role for a single peer relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRole {
    /// This runner connects to the peer over TCP signaling and sends the
    /// SDP offer (lower-sorted-name).
    Initiator,
    /// This runner accepts on TCP signaling and replies with the SDP
    /// answer (higher-sorted-name).
    Responder,
}

/// A peer description after pairing/role resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDesc {
    /// Peer's runner name.
    pub name: String,
    /// Address of the peer's signaling listener (for the initiator's
    /// TCP `connect`) or its expected source (for the responder's
    /// `accept`).
    pub signaling_addr: SocketAddr,
    /// Address of the peer's media listener -- used purely for logging
    /// and ICE diagnostics (the actual ICE flow is driven by SDP +
    /// trickle candidates).
    pub media_addr: SocketAddr,
    /// Role this runner takes for this peer pair.
    pub role: PairRole,
}

/// Result of pairing/port derivation: the local listen addresses and the
/// per-peer descriptions.
#[derive(Debug, Clone)]
pub struct DerivedEndpoints {
    /// Local TCP signaling bind address (for the responder side).
    pub signaling_listen: SocketAddr,
    /// Local UDP media bind port (the ICE host candidate port).
    pub media_listen: SocketAddr,
    /// Per-peer descriptions (excluding self).
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

/// Derive the local listen addresses and per-peer descriptions from the
/// runner-injected `--peers` map, this runner's name, the configured
/// signaling and media base ports, and the per-spawn `qos` level.
pub fn derive_endpoints(
    peer_map: &[(String, String)],
    runner: &str,
    signaling_base_port: u16,
    media_base_port: u16,
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

    let my_signaling_port = signaling_base_port
        .checked_add(runner_offset)
        .and_then(|p| p.checked_add(qos_offset))
        .ok_or_else(|| anyhow!("signaling port overflow"))?;
    let my_media_port = media_base_port
        .checked_add(runner_offset)
        .and_then(|p| p.checked_add(qos_offset))
        .ok_or_else(|| anyhow!("media port overflow"))?;

    let signaling_listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), my_signaling_port);
    let media_listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), my_media_port);

    let mut peers: Vec<PeerDesc> = Vec::new();
    for (idx, (name, host)) in peer_map.iter().enumerate() {
        if name == runner {
            continue;
        }
        let peer_runner_offset = (idx as u16)
            .checked_mul(RUNNER_STRIDE)
            .ok_or_else(|| anyhow!("peer runner offset overflow"))?;
        let peer_signaling_port = signaling_base_port
            .checked_add(peer_runner_offset)
            .and_then(|p| p.checked_add(qos_offset))
            .ok_or_else(|| anyhow!("peer signaling port overflow"))?;
        let peer_media_port = media_base_port
            .checked_add(peer_runner_offset)
            .and_then(|p| p.checked_add(qos_offset))
            .ok_or_else(|| anyhow!("peer media port overflow"))?;
        let peer_ip: IpAddr = host
            .parse()
            .with_context(|| format!("invalid peer host IP '{host}' for '{name}'"))?;
        let role = if runner < name.as_str() {
            PairRole::Initiator
        } else {
            PairRole::Responder
        };
        peers.push(PeerDesc {
            name: name.clone(),
            signaling_addr: SocketAddr::new(peer_ip, peer_signaling_port),
            media_addr: SocketAddr::new(peer_ip, peer_media_port),
            role,
        });
    }

    Ok(DerivedEndpoints {
        signaling_listen,
        media_listen,
        peers,
    })
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
    fn alice_initiates_to_bob() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "alice", 19980, 20000, 3).unwrap();
        assert_eq!(derived.signaling_listen.port(), 19980 + 20);
        assert_eq!(derived.media_listen.port(), 20000 + 20);
        assert_eq!(derived.peers.len(), 1);
        let bob = &derived.peers[0];
        assert_eq!(bob.name, "bob");
        assert_eq!(bob.signaling_addr.port(), 19980 + 1 + 20);
        assert_eq!(bob.media_addr.port(), 20000 + 1 + 20);
        assert_eq!(bob.role, PairRole::Initiator);
    }

    #[test]
    fn bob_responds_to_alice() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "bob", 19980, 20000, 4).unwrap();
        assert_eq!(derived.signaling_listen.port(), 19980 + 1 + 30);
        assert_eq!(derived.media_listen.port(), 20000 + 1 + 30);
        assert_eq!(derived.peers.len(), 1);
        let alice = &derived.peers[0];
        assert_eq!(alice.name, "alice");
        assert_eq!(alice.signaling_addr.port(), 19980 + 30);
        assert_eq!(alice.media_addr.port(), 20000 + 30);
        assert_eq!(alice.role, PairRole::Responder);
    }

    #[test]
    fn three_peers_role_decision() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1,carol=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "bob", 19980, 20000, 3).unwrap();
        let by_name: std::collections::HashMap<&str, PairRole> = derived
            .peers
            .iter()
            .map(|p| (p.name.as_str(), p.role))
            .collect();
        assert_eq!(by_name.get("alice"), Some(&PairRole::Responder));
        assert_eq!(by_name.get("carol"), Some(&PairRole::Initiator));
    }

    #[test]
    fn port_derivation_all_qos_levels_disjoint() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let mut sig_ports = std::collections::HashSet::new();
        let mut media_ports = std::collections::HashSet::new();
        for runner in ["alice", "bob"] {
            for qos in 1..=4u8 {
                let d = derive_endpoints(&peers, runner, 19980, 20000, qos).unwrap();
                assert!(
                    sig_ports.insert(d.signaling_listen.port()),
                    "duplicate signaling port"
                );
                assert!(
                    media_ports.insert(d.media_listen.port()),
                    "duplicate media port"
                );
            }
        }
        assert_eq!(sig_ports.len(), 8);
        assert_eq!(media_ports.len(), 8);
    }

    #[test]
    fn runner_not_in_peers_errors() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let err = derive_endpoints(&peers, "carol", 19980, 20000, 3).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("carol") && msg.contains("not present"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn invalid_qos_errors() {
        let peers = parse_peers("a=127.0.0.1").unwrap();
        assert!(derive_endpoints(&peers, "a", 19980, 20000, 0).is_err());
        assert!(derive_endpoints(&peers, "a", 19980, 20000, 5).is_err());
    }

    #[test]
    fn self_only_no_peers_to_connect() {
        let peers = parse_peers("self=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "self", 19980, 20000, 3).unwrap();
        assert_eq!(derived.signaling_listen.port(), 19980 + 20);
        assert_eq!(derived.media_listen.port(), 20000 + 20);
        assert!(derived.peers.is_empty());
    }
}
