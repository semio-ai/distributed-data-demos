/// UDP multicast transport for QoS 1-2 (best-effort, latest-value).
///
/// Uses `socket2` for multicast group management and non-blocking receive.
///
/// ## Why the socket is non-blocking and `send` retries on `WouldBlock`
///
/// `poll_receive` needs `recv_from` to be non-blocking so the variant's poll
/// loop can interleave UDP and TCP reads without one starving the other.
/// `set_nonblocking(true)` sets the flag for the entire socket — there is no
/// per-direction toggle on a UDP socket — so `send_to` is also non-blocking
/// and can return `WouldBlock` when the kernel send buffer fills.
///
/// On Windows under high multicast load, `WSAEWOULDBLOCK` (10035) is the
/// dominant failure mode for UDP send: the NIC drains slower than user-space
/// pushes packets in. We absorb this transient pressure with a tight retry
/// loop (`yield_now` plus a short wall-clock budget) so the variant doesn't
/// drop messages on the floor at the kernel boundary. We also bump both
/// `SO_RCVBUF` and `SO_SNDBUF` to 8 MiB via
/// [`variant_base::tune_udp_buffers`] to absorb 100 K pkt/s bursts that
/// the ~64 KB Windows-default UDP buffers would otherwise drop kernel-side
/// (T-impl.2).
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Wall-clock budget per `send` call when retrying on `WouldBlock`.
/// Chosen to be small enough not to stall the publish loop noticeably while
/// still absorbing the typical kernel-buffer drain hiccup.
const SEND_RETRY_BUDGET: Duration = Duration::from_millis(1);

/// Trait abstracting the underlying datagram-send call so the retry loop
/// can be unit-tested without a real socket.
pub trait DatagramSend {
    fn send_once(&self, data: &[u8]) -> io::Result<usize>;
}

/// Send `data` over `sender`, retrying on `WouldBlock` for up to
/// `SEND_RETRY_BUDGET`. Yields the thread between attempts to give the
/// kernel a chance to drain the send buffer.
///
/// Returns `Ok(())` when the send eventually succeeds. Returns `Err` if the
/// budget is exhausted while still hitting `WouldBlock` (so the caller
/// surfaces back-pressure rather than silently dropping), or if the send
/// fails with any other error.
pub fn send_with_retry<S: DatagramSend>(sender: &S, data: &[u8]) -> Result<()> {
    let deadline = Instant::now() + SEND_RETRY_BUDGET;
    loop {
        match sender.send_once(data) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "UDP send still WouldBlock after {:?} budget",
                        SEND_RETRY_BUDGET
                    ));
                }
                std::thread::yield_now();
            }
            Err(e) => return Err(anyhow::anyhow!("UDP send error: {}", e)),
        }
    }
}

/// UDP multicast sender/receiver.
pub struct UdpTransport {
    socket: UdpSocket,
    multicast_addr: SocketAddrV4,
}

/// Adapter so the real `UdpSocket`+target-address pair implements the
/// `DatagramSend` trait used by `send_with_retry`.
struct SocketSender<'a> {
    socket: &'a UdpSocket,
    target: SocketAddr,
}

impl DatagramSend for SocketSender<'_> {
    fn send_once(&self, data: &[u8]) -> io::Result<usize> {
        self.socket.send_to(data, self.target)
    }
}

impl UdpTransport {
    /// Create and bind a UDP socket, joining the given multicast group.
    ///
    /// `bind_addr` is the local address to bind (typically `0.0.0.0`).
    /// `multicast_addr` is the multicast group:port (e.g. `239.0.0.1:9000`).
    pub fn new(bind_addr: Ipv4Addr, multicast_addr: SocketAddrV4) -> Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("failed to create UDP socket")?;

        socket
            .set_reuse_address(true)
            .context("failed to set SO_REUSEADDR")?;

        // T-impl.2: bump SO_RCVBUF / SO_SNDBUF to 8 MiB via the shared
        // helper. Replaces the earlier 4 MiB send-only bump — the
        // receive path also needs the buffer headroom on Windows.
        variant_base::tune_udp_buffers(&socket).context("tune UDP buffers")?;

        // Bind to the multicast port on the specified address.
        let bind = SocketAddrV4::new(bind_addr, multicast_addr.port());
        socket
            .bind(&SockAddr::from(bind))
            .with_context(|| format!("failed to bind UDP socket to {}", bind))?;

        // Join the multicast group on all interfaces.
        socket
            .join_multicast_v4(multicast_addr.ip(), &bind_addr)
            .with_context(|| format!("failed to join multicast group {}", multicast_addr.ip()))?;

        // Enable multicast loopback so we can receive our own messages (for testing).
        socket
            .set_multicast_loop_v4(true)
            .context("failed to enable multicast loopback")?;

        // Set non-blocking for poll_receive.
        socket
            .set_nonblocking(true)
            .context("failed to set non-blocking mode")?;

        let std_socket: UdpSocket = socket.into();

        Ok(Self {
            socket: std_socket,
            multicast_addr,
        })
    }

    /// Send a datagram to the multicast group.
    ///
    /// Retries on `WouldBlock` for up to `SEND_RETRY_BUDGET` before bailing.
    /// See module-level docs for the rationale.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        let sender = SocketSender {
            socket: &self.socket,
            target: SocketAddr::V4(self.multicast_addr),
        };
        send_with_retry(&sender, data)
            .with_context(|| format!("failed to send UDP datagram to {}", self.multicast_addr))
    }

    /// Single non-blocking `send_to`. Returns:
    /// - `Ok(true)`  — the datagram was accepted by the kernel.
    /// - `Ok(false)` — the kernel send buffer was full (`WouldBlock`).
    /// - `Err(_)`    — any other I/O error.
    ///
    /// Used by `HybridVariant::try_publish` for QoS 1/2 (T-impl.7): the
    /// driver assigns the seq before calling us, and these two QoS
    /// levels tolerate seq gaps by design (best-effort / latest-value),
    /// so a `WouldBlock` here is recorded as `backpressure_skipped`
    /// rather than dropped on the wire.
    pub fn try_send_nonblocking(&self, data: &[u8]) -> Result<bool> {
        let target = SocketAddr::V4(self.multicast_addr);
        match self.socket.send_to(data, target) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(anyhow::anyhow!(
                "UDP try_send to {}: {}",
                self.multicast_addr,
                e
            )),
        }
    }

    /// Try to receive a datagram (non-blocking).
    /// Returns `None` if no data is available.
    pub fn try_recv(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        match self.socket.recv_from(buf) {
            Ok((n, _addr)) => Ok(Some(n)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(anyhow::anyhow!("UDP recv error: {}", e)),
        }
    }

    /// Leave the multicast group and drop the socket.
    pub fn close(self) -> Result<()> {
        // Socket is dropped when self goes out of scope. The OS will leave
        // the multicast group when the socket is closed.
        drop(self.socket);
        Ok(())
    }

    /// Build a `UdpTransport` from an already-configured `UdpSocket`
    /// plus an arbitrary unicast target. Test-only escape hatch used by
    /// the T-impl.7 `try_publish` backpressure tests, which need a
    /// non-multicast send path with a tiny `SO_SNDBUF` to force the
    /// kernel into `WouldBlock`.
    #[cfg(test)]
    pub(crate) fn from_raw_for_test(socket: UdpSocket, target: SocketAddrV4) -> Self {
        Self {
            socket,
            multicast_addr: target,
        }
    }

    /// Apply `SO_RCVBUF = kb * 1024` to the underlying send-side
    /// socket (T14.1 / T14.4). Overrides the implicit 8 MiB target
    /// installed by `tune_udp_buffers` so the user-tunable knob wins.
    pub fn apply_recv_buffer_kb(&self, kb: u32) -> Result<()> {
        let bytes = (kb as usize).saturating_mul(1024);
        let sock = socket2::SockRef::from(&self.socket);
        sock.set_recv_buffer_size(bytes)
            .context("set_recv_buffer_size on UDP send socket")?;
        let achieved = sock
            .recv_buffer_size()
            .context("read SO_RCVBUF on UDP send socket")?;
        if achieved < bytes {
            eprintln!(
                "[variant-hybrid] warning: UDP SO_RCVBUF achieved only {achieved} bytes, requested {bytes} ({kb} KiB)"
            );
        }
        Ok(())
    }

    /// Build a SECOND, dedicated-for-recv UDP socket joined to the
    /// same multicast group. Used by Multi mode so the reader thread
    /// can do truly-blocking `recv_from` while the main socket (held
    /// by `self`) stays non-blocking for `try_send_nonblocking`.
    pub fn make_blocking_recv_socket(
        bind_addr: Ipv4Addr,
        multicast_addr: SocketAddrV4,
        recv_buffer_kb: u32,
        recv_timeout: Duration,
    ) -> Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("create UDP recv socket")?;
        socket
            .set_reuse_address(true)
            .context("set SO_REUSEADDR on UDP recv socket")?;

        let bytes = (recv_buffer_kb as usize).saturating_mul(1024);
        socket
            .set_recv_buffer_size(bytes)
            .context("set_recv_buffer_size on UDP recv socket")?;
        let achieved = socket
            .recv_buffer_size()
            .context("read SO_RCVBUF on UDP recv socket")?;
        if achieved < bytes {
            eprintln!(
                "[variant-hybrid] warning: UDP recv SO_RCVBUF achieved only {achieved} bytes, requested {bytes} ({recv_buffer_kb} KiB)"
            );
        }

        let bind = SocketAddrV4::new(bind_addr, multicast_addr.port());
        socket
            .bind(&SockAddr::from(bind))
            .with_context(|| format!("bind UDP recv socket to {bind}"))?;
        socket
            .join_multicast_v4(multicast_addr.ip(), &bind_addr)
            .with_context(|| {
                format!("UDP recv socket join_multicast_v4 {}", multicast_addr.ip())
            })?;
        socket
            .set_multicast_loop_v4(true)
            .context("UDP recv socket enable multicast loopback")?;
        socket
            .set_read_timeout(Some(recv_timeout))
            .context("UDP recv socket set_read_timeout")?;

        Ok(socket.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Test shim that returns `WouldBlock` for the first N attempts and then
    /// reports a successful send.
    struct FlakySender {
        wouldblock_remaining: Cell<u32>,
        attempts: Cell<u32>,
    }

    impl DatagramSend for FlakySender {
        fn send_once(&self, data: &[u8]) -> io::Result<usize> {
            self.attempts.set(self.attempts.get() + 1);
            let remaining = self.wouldblock_remaining.get();
            if remaining > 0 {
                self.wouldblock_remaining.set(remaining - 1);
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                Ok(data.len())
            }
        }
    }

    /// Always returns `WouldBlock`. Used to verify the retry loop bails when
    /// the budget is exhausted instead of spinning forever.
    struct AlwaysBlockSender {
        attempts: Cell<u32>,
    }

    impl DatagramSend for AlwaysBlockSender {
        fn send_once(&self, _data: &[u8]) -> io::Result<usize> {
            self.attempts.set(self.attempts.get() + 1);
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    #[test]
    fn send_with_retry_recovers_after_one_wouldblock() {
        let sender = FlakySender {
            wouldblock_remaining: Cell::new(1),
            attempts: Cell::new(0),
        };
        let payload = b"hello";

        send_with_retry(&sender, payload).expect("retry path must yield Ok after one WouldBlock");

        // Retry happened (>=2 attempts) and isn't infinite (well below any
        // pathological number).
        let attempts = sender.attempts.get();
        assert!(
            (2..=10_000).contains(&attempts),
            "expected a small finite retry count, got {attempts}"
        );
    }

    #[test]
    fn send_with_retry_bails_after_budget_exhausted() {
        let sender = AlwaysBlockSender {
            attempts: Cell::new(0),
        };
        let err = send_with_retry(&sender, b"x")
            .expect_err("retry loop must surface error when budget is exhausted");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("WouldBlock"),
            "error should mention WouldBlock, got: {msg}"
        );
        // Sanity: the loop made multiple attempts before giving up, but did
        // not spin pathologically.
        let attempts = sender.attempts.get();
        assert!(attempts >= 1, "expected at least one attempt");
    }

    #[test]
    fn send_with_retry_propagates_non_wouldblock_errors() {
        struct BrokenSender;
        impl DatagramSend for BrokenSender {
            fn send_once(&self, _data: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken pipe"))
            }
        }
        let err = send_with_retry(&BrokenSender, b"x")
            .expect_err("non-WouldBlock errors must not be retried");
        assert!(format!("{err:#}").contains("broken pipe"));
    }
}
