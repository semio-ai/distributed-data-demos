//! Local network interface address enumeration.
//!
//! Used during peer discovery to detect when a peer's source IP is one of
//! this machine's own interfaces (i.e. same-host loopback). The set is
//! cached on first call because interface enumeration involves syscalls
//! and the answer does not change during a runner's lifetime.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;

static CACHED: OnceLock<HashSet<IpAddr>> = OnceLock::new();

/// Return the set of IP addresses bound to interfaces on this machine.
///
/// Always includes IPv4 `127.0.0.1` and IPv6 `::1` so callers do not need to
/// special-case loopback. The result is cached on first call.
pub fn local_interface_ips() -> &'static HashSet<IpAddr> {
    CACHED.get_or_init(enumerate_local_ips)
}

fn enumerate_local_ips() -> HashSet<IpAddr> {
    let mut set: HashSet<IpAddr> = HashSet::new();

    // Always include loopback in case interface enumeration fails or omits it.
    set.insert(IpAddr::V4(Ipv4Addr::LOCALHOST));
    set.insert(IpAddr::V6(Ipv6Addr::LOCALHOST));

    // Use local-ip-address::list_afinet_netifas to enumerate AF_INET interfaces.
    // Errors here are non-fatal -- we still have loopback in the set.
    match local_ip_address::list_afinet_netifas() {
        Ok(ifaces) => {
            for (_name, ip) in ifaces {
                set.insert(ip);
            }
        }
        Err(e) => {
            eprintln!("[runner] warning: failed to enumerate local interfaces: {e}");
        }
    }

    set
}

/// Classify a peer's observed source IP into the canonical host string used
/// in `--peers`.
///
/// - If `peer_ip` is `127.0.0.1` (or any IP in this machine's local interface
///   set), the peer is on the same host, so return the string `"127.0.0.1"`.
/// - Otherwise, return `peer_ip.to_string()`.
///
/// This collapses Windows multicast loopback quirks (where the source IP can
/// be either the LAN interface or `127.0.0.1`) to a single canonical loopback
/// address that variants can use for inter-process communication.
pub fn canonical_peer_host(peer_ip: IpAddr) -> String {
    if peer_ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || peer_ip == IpAddr::V6(Ipv6Addr::LOCALHOST) {
        return "127.0.0.1".to_string();
    }
    if local_interface_ips().contains(&peer_ip) {
        return "127.0.0.1".to_string();
    }
    peer_ip.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn local_interface_ips_includes_loopback() {
        let ips = local_interface_ips();
        assert!(
            ips.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "expected 127.0.0.1 in local interface set, got {ips:?}"
        );
    }

    #[test]
    fn local_interface_ips_is_non_empty() {
        let ips = local_interface_ips();
        assert!(!ips.is_empty(), "local interface set should be non-empty");
    }

    #[test]
    fn local_interface_ips_is_cached() {
        // Two calls should return the same reference.
        let a = local_interface_ips();
        let b = local_interface_ips();
        assert!(std::ptr::eq(a, b), "local_interface_ips should be cached");
    }

    #[test]
    fn canonical_peer_host_maps_localhost_to_loopback() {
        assert_eq!(
            canonical_peer_host(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "127.0.0.1"
        );
    }

    #[test]
    fn canonical_peer_host_maps_local_interface_to_loopback() {
        // Pick any local interface IP from the cache and verify it collapses.
        // Skip if the only local IP is loopback (unusual but possible on a
        // disconnected machine).
        let ips: Vec<IpAddr> = local_interface_ips()
            .iter()
            .copied()
            .filter(|ip| !ip.is_loopback())
            .collect();
        if ips.is_empty() {
            eprintln!("skipping: no non-loopback interfaces on this machine");
            return;
        }
        for ip in ips {
            assert_eq!(
                canonical_peer_host(ip),
                "127.0.0.1",
                "local interface {ip} should be collapsed to 127.0.0.1"
            );
        }
    }

    #[test]
    fn canonical_peer_host_passes_through_remote_ip() {
        // An obviously-remote IP that is not in the local set should be
        // returned as-is.
        let remote = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 42));
        // Sanity: 203.0.0.0/24 is TEST-NET-3; should never be a local interface.
        assert!(!local_interface_ips().contains(&remote));
        assert_eq!(canonical_peer_host(remote), "203.0.113.42");
    }
}
