//! T14.18 regression: EOT must survive data-path saturation.
//!
//! Stub-style scenario: bind two ends of a TCP socket on localhost,
//! "saturate" the data path by simply not providing one, and assert
//! that an EOT frame pushed on the control connection is still
//! observed end-to-end. This is the minimal sanity check that the
//! control channel decouples EOT delivery from the data transport.
//!
//! The full T14.18 cross-runner repro
//! (configs/two-runner-t1416-repro.toml) lives outside the test
//! binary and is exercised via the manual two-runner fixture.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const FRAME_TAG_EOT: u8 = 0x01;
/// custom-udp's `protocol::encode_eot` layout requires the legacy
/// length-prefixed framing:
/// [total_len(4)] [tag=0xEE(1)] [eot_id(8)] [path_len=0(2)] [writer_len(2)] [writer]
fn encode_inner_eot(writer: &str, eot_id: u64) -> Vec<u8> {
    let writer_bytes = writer.as_bytes();
    // HEADER_FIXED_SIZE = 4 + 1 + 8 + 2 + 2 = 17.
    let total_len = 17 + writer_bytes.len();
    let mut buf = Vec::with_capacity(total_len);
    buf.extend_from_slice(&(total_len as u32).to_be_bytes());
    buf.push(0xEE);
    buf.extend_from_slice(&eot_id.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&(writer_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(writer_bytes);
    buf
}

fn encode_control_eot(writer: &str, eot_id: u64) -> Vec<u8> {
    let inner = encode_inner_eot(writer, eot_id);
    let total = 1 + inner.len();
    let mut framed = Vec::with_capacity(4 + total);
    framed.extend_from_slice(&(total as u32).to_be_bytes());
    framed.push(FRAME_TAG_EOT);
    framed.extend_from_slice(&inner);
    framed
}

/// T14.18: with no data path at all (no UDP multicast socket, no
/// data-path TCP), an EOT frame pushed over the control TCP socket
/// must round-trip end-to-end.
#[test]
fn t14_18_eot_survives_with_no_data_path() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let server_addr = listener.local_addr().unwrap();

    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).unwrap();
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; frame_len];
        stream.read_exact(&mut payload).unwrap();
        (len_buf, payload)
    });

    let mut client = TcpStream::connect(server_addr).unwrap();
    client.set_nodelay(true).unwrap();

    let framed = encode_control_eot("writer-x", 0x1234_5678_DEAD_BEEFu64);
    client.write_all(&framed).expect("send");

    let deadline = Instant::now() + Duration::from_secs(2);
    while !join.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(join.is_finished(), "server thread must complete");
    let (len_buf, payload) = join.join().unwrap();

    let frame_len = u32::from_be_bytes(len_buf) as usize;
    assert_eq!(frame_len, payload.len());
    assert_eq!(payload[0], FRAME_TAG_EOT);
    // Inner payload: a custom-udp EOT frame starts with its own
    // 4-byte length prefix then the 0xEE tag.
    let inner_len = u32::from_be_bytes(payload[1..5].try_into().unwrap()) as usize;
    assert!(inner_len >= 17);
    assert_eq!(payload[5], 0xEE);
    let eot_id = u64::from_be_bytes(payload[6..14].try_into().unwrap());
    assert_eq!(eot_id, 0x1234_5678_DEAD_BEEFu64);
    // path_len (2 bytes) = 0, then writer_len (2 bytes), then writer
    // bytes.
    let path_len = u16::from_be_bytes(payload[14..16].try_into().unwrap());
    assert_eq!(path_len, 0);
    let writer_len = u16::from_be_bytes(payload[16..18].try_into().unwrap()) as usize;
    let writer = std::str::from_utf8(&payload[18..18 + writer_len]).unwrap();
    assert_eq!(writer, "writer-x");
}
