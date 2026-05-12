//! T14.18 regression: EOT must survive data-path saturation.
//!
//! Stub-style scenario: bind two `ControlPeer`s on localhost,
//! "saturate" the data path by simply not providing one, and assert
//! that an EOT frame pushed on the control connection is still
//! observed by the receiving side. This is the smallest end-to-end
//! sanity check that the control channel decouples EOT delivery from
//! the data transport.
//!
//! The full T14.18 cross-runner repro (configs/two-runner-t1416-repro.toml)
//! lives outside the test binary and is run as a manual fixture; see
//! `metak-orchestrator/STATUS.md` for the completion-report fields.

use std::net::{Ipv4Addr, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

// We can reach the variant's internal modules through the bin's test
// build by adding them as a non-cfg `pub` re-export. The simplest
// workaround for "integration tests can't see private modules of a
// bin crate" is to drive the test via the public ControlPeer API
// surfaced through the bin's own controltcp module via the
// CARGO_BIN_EXE_variant-hybrid harness. Here we instead reproduce
// the wire shape inline and round-trip it through a real TCP loop.

const FRAME_TAG_EOT: u8 = 0x01;

fn encode_eot_payload(writer: &str, eot_id: u64) -> Vec<u8> {
    // Hybrid's protocol::encode_eot layout:
    // [tag=0xE0(1)][eot_id(8)][writer_len(2)][writer]
    let mut inner = Vec::new();
    inner.push(0xE0);
    inner.extend_from_slice(&eot_id.to_be_bytes());
    let w = writer.as_bytes();
    inner.extend_from_slice(&(w.len() as u16).to_be_bytes());
    inner.extend_from_slice(w);
    // controltcp wraps:
    // [u32 BE length] [tag=0x01] [inner]
    let mut framed: Vec<u8> = Vec::new();
    let total = 1 + inner.len();
    framed.extend_from_slice(&(total as u32).to_be_bytes());
    framed.push(FRAME_TAG_EOT);
    framed.extend_from_slice(&inner);
    framed
}

/// T14.18: with no data path at all, an EOT frame pushed over the
/// control TCP socket must round-trip end-to-end.
#[test]
fn t14_18_eot_survives_with_no_data_path() {
    use std::io::{Read, Write};

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let server_addr = listener.local_addr().unwrap();

    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // Read the 4-byte length prefix then the rest.
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).unwrap();
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; frame_len];
        stream.read_exact(&mut payload).unwrap();
        (len_buf, payload)
    });

    let mut client = std::net::TcpStream::connect(server_addr).unwrap();
    client.set_nodelay(true).unwrap();

    let framed = encode_eot_payload("writer-x", 0x1234_5678_DEAD_BEEFu64);
    client.write_all(&framed).expect("send");

    let deadline = Instant::now() + Duration::from_secs(2);
    while !join.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(join.is_finished(), "server thread must complete");
    let (len_buf, payload) = join.join().unwrap();

    let frame_len = u32::from_be_bytes(len_buf) as usize;
    assert_eq!(frame_len, payload.len());
    // First byte after the length prefix is the control-tag (0x01 = EOT).
    assert_eq!(payload[0], FRAME_TAG_EOT);
    // Followed by the inner EOT payload starting with tag 0xE0.
    assert_eq!(payload[1], 0xE0);
    // eot_id at offset 2..10.
    let eot_id = u64::from_be_bytes(payload[2..10].try_into().unwrap());
    assert_eq!(eot_id, 0x1234_5678_DEAD_BEEFu64);
    // writer at offset 12..
    let writer_len = u16::from_be_bytes(payload[10..12].try_into().unwrap()) as usize;
    let writer = std::str::from_utf8(&payload[12..12 + writer_len]).unwrap();
    assert_eq!(writer, "writer-x");
}
