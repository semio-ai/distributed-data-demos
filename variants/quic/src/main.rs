mod certs;
mod quic;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;

use crate::quic::QuicVariant;

/// Stride applied per runner index when computing per-spawn ports.
const RUNNER_STRIDE: u16 = 1;
/// Stride applied per QoS level (qos-1) when computing per-spawn ports.
const QOS_STRIDE: u16 = 10;

fn main() {
    variant_base::print_build_banner!("quic");
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();

    let base_port = parse_required_extra_arg(&args.extra, "base-port")
        .context("missing required --base-port in variant-specific args")?
        .parse::<u16>()
        .context("invalid --base-port (expected u16)")?;

    let peers_raw = parse_required_extra_arg(&args.extra, "peers")
        .context("missing runner-injected --peers argument")?;

    let peer_map = parse_peers(&peers_raw).context("failed to parse --peers")?;

    let derived = derive_endpoints(&peer_map, &args.runner, base_port, args.qos)
        .context("port derivation failed")?;

    let mut variant = QuicVariant::new(&args.runner, derived.bind_addr, derived.peers);
    run_protocol(&mut variant, &args)?;
    Ok(())
}

/// Result of port derivation: this runner's bind address and the list of
/// peer endpoints to connect to (excluding self).
#[derive(Debug)]
struct DerivedEndpoints {
    bind_addr: SocketAddr,
    peers: Vec<SocketAddr>,
}

/// Parse a `--peers name1=host1,name2=host2,...` value into a sorted-by-name
/// vector of `(name, host)` pairs. Sorting at parse time gives stable
/// indexing across all runners.
fn parse_peers(raw: &str) -> Result<Vec<(String, String)>> {
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

/// Derive the local bind address and list of peer endpoints from the
/// runner-injected `--peers` map, this runner's name, the configured
/// `base_port`, and the per-spawn `qos` level.
///
/// Convention (matches metak-shared/api-contracts/toml-config-schema.md):
///   runner_stride = 1
///   qos_stride    = 10
///   port = base_port + runner_index * runner_stride + (qos - 1) * qos_stride
fn derive_endpoints(
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

    let my_bind_port = base_port
        .checked_add(runner_offset)
        .and_then(|p| p.checked_add(qos_offset))
        .ok_or_else(|| anyhow!("bind port overflow"))?;

    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), my_bind_port);

    let mut peer_endpoints: Vec<SocketAddr> = Vec::new();
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
        peer_endpoints.push(SocketAddr::new(peer_ip, peer_port));
    }

    Ok(DerivedEndpoints {
        bind_addr,
        peers: peer_endpoints,
    })
}

/// Parse a `--key value` pair from the extra CLI arguments.
/// Returns the value if present, `None` otherwise.
fn parse_extra_arg(extra: &[String], key: &str) -> Option<String> {
    let flag = format!("--{key}");
    let mut iter = extra.iter();
    while let Some(arg) = iter.next() {
        if arg == &flag {
            return iter.next().cloned();
        }
    }
    None
}

/// Parse a required `--key value` pair from the extra CLI arguments.
fn parse_required_extra_arg(extra: &[String], key: &str) -> Result<String> {
    parse_extra_arg(extra, key).ok_or_else(|| anyhow!("missing required --{key} argument"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extra_arg_found() {
        let extra: Vec<String> = vec![
            "--base-port".into(),
            "19930".into(),
            "--peers".into(),
            "a=127.0.0.1".into(),
        ];
        assert_eq!(parse_extra_arg(&extra, "base-port"), Some("19930".into()));
        assert_eq!(parse_extra_arg(&extra, "peers"), Some("a=127.0.0.1".into()));
    }

    #[test]
    fn test_parse_extra_arg_not_found() {
        let extra: Vec<String> = vec!["--base-port".into(), "19930".into()];
        assert_eq!(parse_extra_arg(&extra, "peers"), None);
    }

    #[test]
    fn test_parse_extra_arg_empty() {
        let extra: Vec<String> = vec![];
        assert_eq!(parse_extra_arg(&extra, "base-port"), None);
    }

    #[test]
    fn test_parse_peers_single() {
        let parsed = parse_peers("self=127.0.0.1").unwrap();
        assert_eq!(parsed, vec![("self".into(), "127.0.0.1".into())]);
    }

    #[test]
    fn test_parse_peers_sorts_by_name() {
        // Provide intentionally out-of-order input; expect sorted result.
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
    fn test_parse_peers_trims_whitespace() {
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
    fn test_parse_peers_rejects_malformed() {
        assert!(parse_peers("alice").is_err());
        assert!(parse_peers("alice=").is_err());
        assert!(parse_peers("=127.0.0.1").is_err());
        assert!(parse_peers("").is_err());
    }

    #[test]
    fn test_identity_resolution_alice_index_0() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "alice", 19930, 1).unwrap();
        // alice index 0, qos 1: 19930 + 0 + 0 = 19930
        assert_eq!(derived.bind_addr.port(), 19930);
        assert_eq!(derived.peers.len(), 1);
        // bob index 1, qos 1: 19930 + 1 + 0 = 19931
        assert_eq!(derived.peers[0].port(), 19931);
    }

    #[test]
    fn test_identity_resolution_bob_index_1() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "bob", 19930, 1).unwrap();
        assert_eq!(derived.bind_addr.port(), 19931);
        assert_eq!(derived.peers.len(), 1);
        assert_eq!(derived.peers[0].port(), 19930);
    }

    #[test]
    fn test_port_derivation_qos3_runner1() {
        // base 19930, runner_index 1, qos 3 -> 19930 + 1*1 + 2*10 = 19951
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "bob", 19930, 3).unwrap();
        assert_eq!(derived.bind_addr.port(), 19951);
        // alice (index 0) at qos 3: 19930 + 0 + 20 = 19950
        assert_eq!(derived.peers[0].port(), 19950);
    }

    #[test]
    fn test_port_derivation_all_qos_levels_disjoint() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let mut all_ports = std::collections::HashSet::new();
        for runner in ["alice", "bob"] {
            for qos in 1..=4u8 {
                let d = derive_endpoints(&peers, runner, 19930, qos).unwrap();
                assert!(
                    all_ports.insert(d.bind_addr.port()),
                    "duplicate bind port {} for {} qos {}",
                    d.bind_addr.port(),
                    runner,
                    qos
                );
            }
        }
        // 2 runners * 4 qos levels = 8 distinct bind ports
        assert_eq!(all_ports.len(), 8);
    }

    #[test]
    fn test_runner_not_in_peers_errors() {
        let peers = parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
        let err = derive_endpoints(&peers, "carol", 19930, 1).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("carol") && msg.contains("not present"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn test_invalid_qos_errors() {
        let peers = parse_peers("a=127.0.0.1").unwrap();
        assert!(derive_endpoints(&peers, "a", 19930, 0).is_err());
        assert!(derive_endpoints(&peers, "a", 19930, 5).is_err());
    }

    #[test]
    fn test_self_only_no_peers_to_connect() {
        let peers = parse_peers("self=127.0.0.1").unwrap();
        let derived = derive_endpoints(&peers, "self", 19930, 1).unwrap();
        assert_eq!(derived.bind_addr.port(), 19930);
        assert!(derived.peers.is_empty());
    }
}
