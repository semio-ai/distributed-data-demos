//! Integration tests for the WebSocket variant: single-process loopback
//! exercising bind/listen, role-decision logic, and message framing in
//! isolation. Cross-peer flow is validated end-to-end by the project-level
//! two-runner localhost run (T3f.4).

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};

use variant_base::types::Qos;

// Re-export the inner crate modules under their binary path. Cargo
// integration tests can only access the `bin` crate's public surface, so
// we rely on the `Variant` trait surface from `variant-base` and on
// public re-exports via the binary crate. Since this is a binary crate
// without a library target, we instead re-implement the small pieces we
// need by importing the binary's public module path through a helper
// crate-style trick: include the source files directly here.
//
// Simpler approach: keep the scope small enough to test through the
// binary's public surface using its CLI, OR put pure-logic helpers
// behind the `pub` keyword in the binary crate so they're reachable.
//
// Tungstenite's accept/connect is exercised below via a trivial
// loopback handshake, validating that the variant's protocol bytes
// round-trip via `tungstenite` correctly.

#[path = "../src/protocol.rs"]
mod protocol;

#[path = "../src/pairing.rs"]
mod pairing;

use protocol::{decode_frame, encode_data, encode_eot, Frame};

static PORT_COUNTER: AtomicU16 = AtomicU16::new(20100);

fn next_test_port() -> u16 {
    PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Verifies that bind/listen at the derived port works for the single-peer
/// `--peers self=...` case and that there are zero peers to connect to.
#[test]
fn bind_listen_self_peer_only() {
    let peers = pairing::parse_peers("self=127.0.0.1").unwrap();
    let derived = pairing::derive_endpoints(&peers, "self", next_test_port(), 3).unwrap();
    assert!(derived.peers.is_empty());

    let listener = TcpListener::bind(derived.listen_addr).expect("bind succeeds on derived port");
    drop(listener);
}

/// Round-trip a data frame through tungstenite to validate the binary
/// frame body decoder works against real WS framing.
#[test]
fn round_trip_data_frame_through_tungstenite() {
    let port = next_test_port();
    let bind_addr = ("127.0.0.1", port);
    let listener = TcpListener::bind(bind_addr).unwrap();

    // Server thread: accept one connection, perform the upgrade, read one
    // binary message, and verify the bytes decode correctly.
    let server_thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let mut ws = tungstenite::accept(stream).expect("server accept");
        let msg = ws.read().expect("server read");
        match msg {
            tungstenite::Message::Binary(bytes) => match decode_frame(&bytes).unwrap() {
                Frame::Data(u) => {
                    assert_eq!(u.qos, Qos::ReliableTcp);
                    assert_eq!(u.seq, 99);
                    assert_eq!(u.path, "/bench/0");
                    assert_eq!(u.writer, "alice");
                    assert_eq!(u.payload, vec![1u8, 2, 3, 4]);
                }
                other => panic!("expected Frame::Data, got {other:?}"),
            },
            other => panic!("expected Message::Binary, got {other:?}"),
        }
    });

    // Client: open TCP, do the upgrade against ws://.../bench, send a
    // data frame.
    let url = format!("ws://127.0.0.1:{port}/bench");
    let stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    stream.set_nodelay(true).unwrap();
    let (mut ws, _resp) =
        tungstenite::client::client(url.as_str(), stream).expect("client upgrade");
    let frame = encode_data(Qos::ReliableTcp, 99, "/bench/0", "alice", &[1u8, 2, 3, 4]);
    ws.send(tungstenite::Message::Binary(frame))
        .expect("client send");
    ws.flush().ok();

    server_thread.join().expect("server thread panicked");
}

/// Round-trip an EOT frame through tungstenite. Validates the EOT_TAG
/// discriminator survives the WS framing layer.
#[test]
fn round_trip_eot_frame_through_tungstenite() {
    let port = next_test_port();
    let bind_addr = ("127.0.0.1", port);
    let listener = TcpListener::bind(bind_addr).unwrap();

    let server_thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let mut ws = tungstenite::accept(stream).expect("server accept");
        let msg = ws.read().expect("server read");
        match msg {
            tungstenite::Message::Binary(bytes) => match decode_frame(&bytes).unwrap() {
                Frame::Eot { writer, eot_id } => {
                    assert_eq!(writer, "writer-x");
                    assert_eq!(eot_id, 0xCAFE_BABE_DEAD_BEEF);
                }
                other => panic!("expected Frame::Eot, got {other:?}"),
            },
            other => panic!("expected Message::Binary, got {other:?}"),
        }
    });

    let url = format!("ws://127.0.0.1:{port}/bench");
    let stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    stream.set_nodelay(true).unwrap();
    let (mut ws, _resp) =
        tungstenite::client::client(url.as_str(), stream).expect("client upgrade");
    let frame = encode_eot("writer-x", 0xCAFE_BABE_DEAD_BEEF);
    ws.send(tungstenite::Message::Binary(frame)).expect("send");
    ws.flush().ok();

    server_thread.join().expect("server thread panicked");
}

/// Pair derivation: alice connects to bob, bob accepts alice. Verify
/// listen ports diverge so a same-host pair doesn't collide.
#[test]
fn pair_role_assignment_consistent() {
    let peers = pairing::parse_peers("alice=127.0.0.1,bob=127.0.0.1").unwrap();
    let alice = pairing::derive_endpoints(&peers, "alice", next_test_port(), 3).unwrap();
    let bob = pairing::derive_endpoints(&peers, "bob", next_test_port(), 3).unwrap();
    // Different listen ports.
    assert_ne!(alice.listen_addr.port(), bob.listen_addr.port());
    // Roles are mirrored.
    assert_eq!(alice.peers[0].role, pairing::PairRole::Client);
    assert_eq!(bob.peers[0].role, pairing::PairRole::Server);
}
