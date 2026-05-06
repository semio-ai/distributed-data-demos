//! Integration tests for `variant-webrtc`.
//!
//! These tests invoke the binary as a subprocess (it is a binary crate)
//! and exercise the CLI parsing, port derivation, and runtime startup
//! paths. Full peer-to-peer DataChannel exchange is validated by the
//! cross-machine config under `configs/two-runner-webrtc-all.toml`.

/// Single-process loopback: with `--peers self=127.0.0.1`, this runner
/// has no other peers to connect to (self is excluded by design). The
/// run should exercise CLI parsing, port derivation, runtime startup,
/// the empty-EOT-set fast path, and clean shutdown -- and exit 0.
#[test]
fn binary_loopback_exits_successfully() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-webrtc");

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
            "webrtc",
            "--runner",
            "self",
            "--run",
            "loopback01",
            "--peers",
            "self=127.0.0.1",
            "--",
            "--signaling-base-port",
            "29980",
            "--media-base-port",
            "30000",
        ])
        .status()
        .expect("failed to run variant-webrtc");

    assert!(
        status.success(),
        "variant-webrtc exited with status: {status}"
    );

    let entries: Vec<_> = std::fs::read_dir(tmp_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !entries.is_empty(),
        "expected at least one log file in {log_dir}"
    );
}

/// The variant must fail loudly when `--runner` is not present in
/// `--peers` -- that indicates a runner/contract bug.
#[test]
fn binary_runner_not_in_peers_fails() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-webrtc");

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
            "webrtc",
            "--runner",
            "carol",
            "--run",
            "missing01",
            "--peers",
            "alice=127.0.0.1,bob=127.0.0.1",
            "--",
            "--signaling-base-port",
            "29981",
            "--media-base-port",
            "30001",
        ])
        .output()
        .expect("failed to run variant-webrtc");

    assert!(
        !output.status.success(),
        "expected variant-webrtc to fail when runner is not in --peers"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("carol") && stderr.contains("not present"),
        "expected clear error mentioning the missing runner; stderr was: {stderr}"
    );
}

/// The variant must fail when `--signaling-base-port` is missing.
#[test]
fn binary_missing_signaling_base_port_fails() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-webrtc");

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
            "webrtc",
            "--runner",
            "self",
            "--run",
            "missing02",
            "--peers",
            "self=127.0.0.1",
            "--",
            "--media-base-port",
            "30002",
        ])
        .output()
        .expect("failed to run variant-webrtc");

    assert!(
        !output.status.success(),
        "expected variant-webrtc to fail when --signaling-base-port is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("signaling-base-port"),
        "expected error mentioning signaling-base-port; stderr was: {stderr}"
    );
}

/// The variant must fail when `--media-base-port` is missing.
#[test]
fn binary_missing_media_base_port_fails() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let log_dir = tmp_dir.path().to_str().unwrap();

    let binary = env!("CARGO_BIN_EXE_variant-webrtc");

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
            "webrtc",
            "--runner",
            "self",
            "--run",
            "missing03",
            "--peers",
            "self=127.0.0.1",
            "--",
            "--signaling-base-port",
            "29983",
        ])
        .output()
        .expect("failed to run variant-webrtc");

    assert!(
        !output.status.success(),
        "expected variant-webrtc to fail when --media-base-port is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("media-base-port"),
        "expected error mentioning media-base-port; stderr was: {stderr}"
    );
}
