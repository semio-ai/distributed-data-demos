/// TCP transport for QoS 3-4 (reliable-ordered, reliable-TCP).
///
/// One TCP connection per peer. Uses length-prefix framing from the protocol
/// module. TCP_NODELAY is set on all connections to minimize latency.
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use anyhow::{Context, Result};

/// A single TCP peer connection with a partial-read buffer for framing.
pub struct TcpPeer {
    pub stream: TcpStream,
    pub addr: SocketAddr,
    /// Buffer for accumulating partial reads.
    read_buf: Vec<u8>,
}

impl TcpPeer {
    fn new(stream: TcpStream, addr: SocketAddr) -> Self {
        Self {
            stream,
            addr,
            read_buf: Vec::new(),
        }
    }

    /// Write a length-prefixed framed message to this peer.
    pub fn send_framed(&mut self, data: &[u8]) -> Result<()> {
        self.stream
            .write_all(data)
            .with_context(|| format!("failed to write to TCP peer {}", self.addr))?;
        Ok(())
    }

    /// Try to read the next complete framed message (non-blocking).
    ///
    /// Returns `None` if no complete message is available yet.
    /// Returns the inner message bytes (without the 4-byte length prefix).
    pub fn try_recv_framed(&mut self) -> Result<Option<Vec<u8>>> {
        // Read whatever is available into the buffer.
        let mut tmp = [0u8; 65536];
        match self.stream.read(&mut tmp) {
            Ok(0) => {
                // Connection closed.
                return Ok(None);
            }
            Ok(n) => {
                self.read_buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No data available right now.
            }
            Err(e) => {
                return Err(anyhow::anyhow!("TCP read error from {}: {}", self.addr, e));
            }
        }

        // Try to extract a complete frame: 4-byte length prefix + payload.
        if self.read_buf.len() < 4 {
            return Ok(None);
        }
        let msg_len = u32::from_be_bytes(self.read_buf[0..4].try_into().unwrap()) as usize;
        let total = 4 + msg_len;
        if self.read_buf.len() < total {
            return Ok(None);
        }

        // Extract the message and shrink the buffer.
        let msg = self.read_buf[4..total].to_vec();
        self.read_buf.drain(..total);

        Ok(Some(msg))
    }
}

/// Manages TCP connections to all peers and the local listener.
pub struct TcpTransport {
    listener: TcpListener,
    /// Outbound connections we initiated to peers.
    outbound: Vec<TcpPeer>,
    /// Inbound connections accepted from peers.
    inbound: Vec<TcpPeer>,
}

impl TcpTransport {
    /// Create a TCP listener on the given address.
    pub fn new(listen_addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(listen_addr)
            .with_context(|| format!("failed to bind TCP listener on {}", listen_addr))?;
        listener
            .set_nonblocking(true)
            .context("failed to set TCP listener non-blocking")?;

        Ok(Self {
            listener,
            outbound: Vec::new(),
            inbound: Vec::new(),
        })
    }

    /// Connect to a peer at the given address. Sets TCP_NODELAY.
    pub fn connect_to_peer(&mut self, addr: SocketAddr) -> Result<()> {
        let stream = TcpStream::connect(addr)
            .with_context(|| format!("failed to connect TCP to peer {}", addr))?;
        stream
            .set_nodelay(true)
            .context("failed to set TCP_NODELAY on outbound")?;
        stream
            .set_nonblocking(true)
            .context("failed to set non-blocking on outbound")?;
        self.outbound.push(TcpPeer::new(stream, addr));
        Ok(())
    }

    /// Accept any pending inbound connections (non-blocking).
    pub fn accept_pending(&mut self) -> Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    stream
                        .set_nodelay(true)
                        .context("failed to set TCP_NODELAY on inbound")?;
                    stream
                        .set_nonblocking(true)
                        .context("failed to set non-blocking on inbound")?;
                    self.inbound.push(TcpPeer::new(stream, addr));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    return Err(anyhow::anyhow!("TCP accept error: {}", e));
                }
            }
        }
        Ok(())
    }

    /// Send a framed message to all outbound peers.
    pub fn broadcast(&mut self, data: &[u8]) -> Result<()> {
        for peer in &mut self.outbound {
            peer.send_framed(data)?;
        }
        Ok(())
    }

    /// Try to receive the next framed message from any peer (inbound or outbound).
    /// Returns `None` if no complete messages are available.
    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        // Accept any new inbound connections first.
        self.accept_pending()?;

        // Check inbound connections.
        for peer in &mut self.inbound {
            if let Some(msg) = peer.try_recv_framed()? {
                return Ok(Some(msg));
            }
        }

        // Check outbound connections (peers may send data back on the same connection).
        for peer in &mut self.outbound {
            if let Some(msg) = peer.try_recv_framed()? {
                return Ok(Some(msg));
            }
        }

        Ok(None)
    }

    /// Close all connections.
    pub fn close(self) -> Result<()> {
        // All streams and the listener are dropped when self goes out of scope.
        drop(self.listener);
        for peer in self.outbound {
            drop(peer.stream);
        }
        for peer in self.inbound {
            drop(peer.stream);
        }
        Ok(())
    }
}
