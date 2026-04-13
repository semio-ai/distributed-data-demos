/// Integration test: single-process multicast loopback.
///
/// Publishes a message via multicast and verifies we can receive our own
/// message back (multicast loopback must be enabled).
use std::net::{Ipv4Addr, SocketAddrV4};
use std::thread;
use std::time::Duration;

// We need to access the binary's internal modules. Since integration tests
// cannot access `mod` items directly, we test the protocol module via the
// variant-base trait interface by reimplementing the core logic here.
// Instead, we test end-to-end by using raw sockets directly.

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Minimal re-implementation of encode/decode for the integration test,
/// since integration tests cannot access private modules of a binary crate.
mod wire {
    pub fn encode(qos: u8, seq: u64, path: &str, writer: &str, payload: &[u8]) -> Vec<u8> {
        let path_bytes = path.as_bytes();
        let writer_bytes = writer.as_bytes();
        let total_len = 4 + 1 + 8 + 2 + path_bytes.len() + 2 + writer_bytes.len() + payload.len();

        let mut buf = Vec::with_capacity(total_len);
        buf.extend_from_slice(&(total_len as u32).to_be_bytes());
        buf.push(qos);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&(path_bytes.len() as u16).to_be_bytes());
        buf.extend_from_slice(path_bytes);
        buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
        buf.extend_from_slice(writer_bytes);
        buf.extend_from_slice(payload);
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<(u8, u64, String, String, Vec<u8>)> {
        if buf.len() < 17 {
            return None;
        }
        let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if buf.len() < total_len {
            return None;
        }
        let qos = buf[4];
        let seq = u64::from_be_bytes([
            buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11], buf[12],
        ]);
        let path_len = u16::from_be_bytes([buf[13], buf[14]]) as usize;
        let path = std::str::from_utf8(&buf[15..15 + path_len])
            .ok()?
            .to_string();
        let wl_start = 15 + path_len;
        let writer_len = u16::from_be_bytes([buf[wl_start], buf[wl_start + 1]]) as usize;
        let writer = std::str::from_utf8(&buf[wl_start + 2..wl_start + 2 + writer_len])
            .ok()?
            .to_string();
        let payload_start = wl_start + 2 + writer_len;
        let payload = buf[payload_start..total_len].to_vec();
        Some((qos, seq, path, writer, payload))
    }
}

#[test]
fn multicast_loopback_send_receive() {
    let multicast_ip = Ipv4Addr::new(239, 0, 0, 1);
    let multicast_port = 19000; // Use a non-default port to avoid conflicts.
    let multicast_addr = SocketAddrV4::new(multicast_ip, multicast_port);

    // Create and bind socket.
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    socket.set_reuse_address(true).unwrap();
    socket.set_nonblocking(false).unwrap();

    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, multicast_port);
    socket.bind(&SockAddr::from(bind_addr)).unwrap();
    socket
        .join_multicast_v4(&multicast_ip, &Ipv4Addr::UNSPECIFIED)
        .unwrap();
    socket.set_multicast_loop_v4(true).unwrap();

    // Set a read timeout so the test does not hang.
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    let std_socket: std::net::UdpSocket = socket.into();

    // Send a message to the multicast group.
    let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let encoded = wire::encode(1, 42, "/bench/0", "test-runner", &payload);
    let target = std::net::SocketAddr::V4(multicast_addr);
    std_socket.send_to(&encoded, target).unwrap();

    // Give a brief moment for the message to loop back.
    thread::sleep(Duration::from_millis(50));

    // Receive.
    let mut recv_buf = vec![0u8; 65536];
    let (n, _addr) = std_socket.recv_from(&mut recv_buf).unwrap();

    let (qos, seq, path, writer, recv_payload) = wire::decode(&recv_buf[..n]).unwrap();
    assert_eq!(qos, 1);
    assert_eq!(seq, 42);
    assert_eq!(path, "/bench/0");
    assert_eq!(writer, "test-runner");
    assert_eq!(recv_payload, payload);

    // Leave multicast group.
    let raw: Socket = std_socket.into();
    let _ = raw.leave_multicast_v4(&multicast_ip, &Ipv4Addr::UNSPECIFIED);
}
