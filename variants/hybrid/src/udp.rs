/// UDP multicast transport for QoS 1-2 (best-effort, latest-value).
///
/// Uses `socket2` for multicast group management and non-blocking receive.
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// UDP multicast sender/receiver.
pub struct UdpTransport {
    socket: UdpSocket,
    multicast_addr: SocketAddrV4,
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
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.socket
            .send_to(data, SocketAddr::V4(self.multicast_addr))
            .with_context(|| format!("failed to send UDP datagram to {}", self.multicast_addr))?;
        Ok(())
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
}
