// Integration tests for variant-quic.
// Since variant-quic is a binary crate, we test via subprocess.

/// Helper: run the variant-quic binary with the new CLI shape (--peers /
/// --runner / --base-port) for a single self-only peer, and verify it
/// exits 0 and produces a log file.
///
/// With the new identity-based peer model, a single-runner variant has no
/// other peers to connect to (self is excluded), so this exercises the
/// binding/lifecycle path but not bidirectional message flow.
#[test]
fn test_binary_loopback_exits_successfully() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-quic");

    // Pick a high base port unlikely to collide.
    let base_port = "19440";

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
            "self",
            "--run",
            "loopback01",
            // Runner-injected --peers (synthesized for the test).
            "--peers",
            "self=127.0.0.1",
            "--",
            "--base-port",
            base_port,
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

/// Verify the variant fails fast and clearly when --runner is not present
/// in --peers (a runner/contract bug).
#[test]
fn test_binary_runner_not_in_peers_fails() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-quic");

    let output = std::process::Command::new(binary)
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
            "carol",
            "--run",
            "missing01",
            "--peers",
            "alice=127.0.0.1,bob=127.0.0.1",
            "--",
            "--base-port",
            "19450",
        ])
        .output()
        .expect("failed to run variant-quic");

    assert!(
        !output.status.success(),
        "expected variant-quic to fail when runner is not in --peers"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("carol") && stderr.contains("not present"),
        "expected clear error mentioning the missing runner; stderr was: {stderr}"
    );
}

/// Verify the variant fails when --base-port is missing from variant-specific args.
#[test]
fn test_binary_missing_base_port_fails() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-quic");

    let output = std::process::Command::new(binary)
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
            "self",
            "--run",
            "missing02",
            "--peers",
            "self=127.0.0.1",
            // No --base-port.
        ])
        .output()
        .expect("failed to run variant-quic");

    assert!(
        !output.status.success(),
        "expected variant-quic to fail when --base-port is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("base-port"),
        "expected error mentioning base-port; stderr was: {stderr}"
    );
}
