//! Shared socket-tuning helpers used by every UDP-based variant.
//!
//! The high-rate same-host fixtures (100 K pkt/s sustained) overflow the
//! kernel-default UDP buffers on Windows (~64 KB SO_RCVBUF) within
//! milliseconds, producing what the JSONL receive logs see as "loss" that
//! is actually kernel-side drop rather than transport behaviour. Bumping
//! both `SO_RCVBUF` and `SO_SNDBUF` to 8 MiB at every UDP socket creation
//! site removes that single biggest source of cross-stack noise so the
//! benchmark measures the transport, not the kernel queue.
//!
//! The actual achieved buffer size may be capped by the OS below the
//! requested value (Windows in particular silently caps; Linux requires
//! `net.core.{r,w}mem_max` to be raised to go beyond the default 208 KB).
//! [`tune_udp_buffers`] queries the achieved size back and emits a single
//! `eprintln!` warning when it lands below 1 MiB, then continues — the
//! caller never errors on a small buffer. Operators just want to know.

use anyhow::{Context, Result};
use socket2::{SockRef, Socket};

/// Target size for both `SO_RCVBUF` and `SO_SNDBUF` on every UDP socket
/// the benchmark creates: 8 MiB. Chosen so that even a 100 K pkt/s flood
/// with ~80-byte payloads gets ~10 s of headroom in the kernel queue,
/// which is more than enough to absorb the bursts our analysis sees on
/// localhost (T-impl.2 / metak-orchestrator/TASKS.md).
pub const TARGET_UDP_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Achieved-size floor below which we emit a one-line warning on stderr.
/// 1 MiB is conservative — every reasonable kernel grants at least this
/// much without privileged tuning. If we see less, something is
/// genuinely wrong and the operator should hear about it.
pub const WARN_BELOW_BYTES: usize = 1024 * 1024;

/// Bump `SO_RCVBUF` and `SO_SNDBUF` on a `socket2::Socket` to
/// [`TARGET_UDP_BUFFER_BYTES`]. Reads the achieved size back and logs
/// (via `eprintln!`) when either ends up below [`WARN_BELOW_BYTES`].
///
/// Returns `Ok(())` regardless of the achieved size unless one of the
/// underlying `setsockopt` / `getsockopt` calls fails outright — in
/// which case the surface is the syscall error rather than the
/// achieved-size check. A small buffer is a soft warning, not an
/// error: the variants must keep running so the rest of the matrix
/// can still produce data.
pub fn tune_udp_buffers(socket: &Socket) -> Result<()> {
    socket
        .set_recv_buffer_size(TARGET_UDP_BUFFER_BYTES)
        .context("set_recv_buffer_size on UDP socket failed")?;
    let achieved_rcv = socket
        .recv_buffer_size()
        .context("recv_buffer_size readback on UDP socket failed")?;
    if achieved_rcv < WARN_BELOW_BYTES {
        eprintln!(
            "[variant-base] warning: SO_RCVBUF achieved only {} bytes, requested {} bytes (8 MiB)",
            achieved_rcv, TARGET_UDP_BUFFER_BYTES,
        );
    }

    socket
        .set_send_buffer_size(TARGET_UDP_BUFFER_BYTES)
        .context("set_send_buffer_size on UDP socket failed")?;
    let achieved_snd = socket
        .send_buffer_size()
        .context("send_buffer_size readback on UDP socket failed")?;
    if achieved_snd < WARN_BELOW_BYTES {
        eprintln!(
            "[variant-base] warning: SO_SNDBUF achieved only {} bytes, requested {} bytes (8 MiB)",
            achieved_snd, TARGET_UDP_BUFFER_BYTES,
        );
    }

    Ok(())
}

/// Convenience overload for callers holding a `std::net::UdpSocket`
/// (or anything else `socket2::SockRef` can borrow from). Internally
/// builds a `SockRef` view of the socket and delegates to
/// [`tune_udp_buffers`]. The view is a borrow — the underlying socket
/// is not consumed.
pub fn tune_udp_buffers_std(socket: &std::net::UdpSocket) -> Result<()> {
    let sock_ref = SockRef::from(socket);
    tune_udp_buffers(&sock_ref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol, SockAddr, Type};
    use std::net::{Ipv4Addr, SocketAddrV4};

    /// Build a freshly-bound UDP socket on an ephemeral port and tune it.
    /// Sanity-check that both directions report at least 1 MiB after the
    /// helper runs — every reasonable kernel grants that much without
    /// privileged tuning, and any value below is the warning case the
    /// helper exists to surface.
    #[test]
    fn tune_udp_buffers_achieves_at_least_1mib() {
        let socket =
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("create UDP socket");
        let bind_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        socket
            .bind(&SockAddr::from(bind_addr))
            .expect("bind UDP socket");

        tune_udp_buffers(&socket).expect("tune_udp_buffers should succeed");

        let achieved_rcv = socket.recv_buffer_size().expect("read SO_RCVBUF");
        let achieved_snd = socket.send_buffer_size().expect("read SO_SNDBUF");

        assert!(
            achieved_rcv >= WARN_BELOW_BYTES,
            "SO_RCVBUF only {} bytes, expected >= {} (1 MiB)",
            achieved_rcv,
            WARN_BELOW_BYTES,
        );
        assert!(
            achieved_snd >= WARN_BELOW_BYTES,
            "SO_SNDBUF only {} bytes, expected >= {} (1 MiB)",
            achieved_snd,
            WARN_BELOW_BYTES,
        );
    }

    /// Verify the `std::net::UdpSocket` overload borrows correctly and
    /// produces the same achieved sizes as the direct `Socket` path.
    #[test]
    fn tune_udp_buffers_std_works_on_std_udp_socket() {
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind std UdpSocket");

        tune_udp_buffers_std(&std_socket).expect("tune_udp_buffers_std should succeed");

        // Read back via SockRef to confirm.
        let sock_ref = SockRef::from(&std_socket);
        let achieved_rcv = sock_ref.recv_buffer_size().expect("read SO_RCVBUF");
        let achieved_snd = sock_ref.send_buffer_size().expect("read SO_SNDBUF");

        assert!(
            achieved_rcv >= WARN_BELOW_BYTES,
            "SO_RCVBUF only {} bytes, expected >= {}",
            achieved_rcv,
            WARN_BELOW_BYTES,
        );
        assert!(
            achieved_snd >= WARN_BELOW_BYTES,
            "SO_SNDBUF only {} bytes, expected >= {}",
            achieved_snd,
            WARN_BELOW_BYTES,
        );
    }
}
