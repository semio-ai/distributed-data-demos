// Integration tests for variant-quic.
// Since variant-quic is a binary crate, we test via subprocess.

/// Helper: run the variant-quic binary with loopback args and verify it exits 0.
#[test]
fn test_binary_loopback_exits_successfully() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    // Build the binary path. cargo test sets OUT_DIR but we can use env.
    let binary = env!("CARGO_BIN_EXE_variant-quic");

    let status = std::process::Command::new(binary)
        .args([
            "--tick-rate-hz",
            "10",
            "--stabilize-secs",
            "0",
            "--operate-secs",
            "1",
            "--silent-secs",
            "1",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "1",
            "--qos",
            "1",
            "--log-dir",
            log_dir,
            "--launch-ts",
            "2026-04-12T14:00:00.000000000Z",
            "--variant",
            "quic",
            "--runner",
            "a",
            "--run",
            "run01",
            // No --peers: variant runs with no peers, publishes but nothing to receive.
        ])
        .status()
        .expect("failed to run variant-quic");

    assert!(
        status.success(),
        "variant-quic exited with status: {status}"
    );

    // Verify log file was created.
    let entries: Vec<_> = std::fs::read_dir(tmp_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !entries.is_empty(),
        "expected at least one log file in {log_dir}"
    );
}

/// Test the variant-quic binary with loopback: connect to self, publish, receive.
///
/// This test starts a variant-quic with --peers pointing to its own address.
/// The variant connects to itself, publishes messages, and should receive them back.
#[test]
fn test_binary_self_connect_loopback() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-quic");

    // Bind to a specific port so we can connect to ourselves.
    // Use port 0 for binding -- we need to discover the actual port.
    // Since we cannot easily discover the port from outside, we bind to a fixed port.
    // Pick a high port unlikely to collide.
    let bind_addr = "127.0.0.1:19443";

    let status = std::process::Command::new(binary)
        .args([
            "--tick-rate-hz",
            "10",
            "--stabilize-secs",
            "1",
            "--operate-secs",
            "2",
            "--silent-secs",
            "1",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "1",
            "--qos",
            "3", // Reliable: use streams
            "--log-dir",
            log_dir,
            "--launch-ts",
            "2026-04-12T14:00:00.000000000Z",
            "--variant",
            "quic",
            "--runner",
            "self-test",
            "--run",
            "loopback01",
            "--",
            "--bind-addr",
            bind_addr,
            "--peers",
            bind_addr,
        ])
        .status()
        .expect("failed to run variant-quic");

    assert!(
        status.success(),
        "variant-quic self-connect exited with status: {status}"
    );

    // Read the log file and verify we have both write and receive entries.
    let log_file = std::fs::read_dir(tmp_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .expect("no JSONL log file found");

    let content = std::fs::read_to_string(log_file.path()).unwrap();
    let has_write = content.lines().any(|line| line.contains("\"write\""));
    let has_receive = content.lines().any(|line| line.contains("\"receive\""));

    assert!(has_write, "expected write entries in log");
    // In self-connect mode, we should receive our own messages.
    assert!(
        has_receive,
        "expected receive entries in log for self-connect"
    );
}
