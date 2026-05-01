/// Integration tests for the custom-udp variant.
///
/// These test the binary end-to-end via subprocess. With the new
/// identity-based peer model (E9 / T9.4b), a single-runner variant has no
/// other peers to connect to (self is excluded), so each test exercises
/// the bind / lifecycle / framing path but not bidirectional message flow.
/// Cross-peer flow is validated manually via the two-runner-on-localhost
/// fixture documented in STATUS.md.
use std::time::Duration;

/// UDP path lifecycle at QoS 1 (best-effort).
#[test]
fn udp_lifecycle_qos1() {
    let multicast_group = "239.0.0.1:19811";
    let tcp_base_port = "19840";

    run_custom_udp_variant(multicast_group, tcp_base_port, 1, "udp-q1");
}

/// UDP path lifecycle at QoS 2 (latest-value).
#[test]
fn udp_lifecycle_qos2() {
    let multicast_group = "239.0.0.1:19812";
    let tcp_base_port = "19841";

    run_custom_udp_variant(multicast_group, tcp_base_port, 2, "udp-q2");
}

/// UDP path lifecycle at QoS 3 (reliable-UDP / NACK).
#[test]
fn udp_lifecycle_qos3() {
    let multicast_group = "239.0.0.1:19813";
    let tcp_base_port = "19842";

    run_custom_udp_variant(multicast_group, tcp_base_port, 3, "udp-q3");
}

/// TCP path lifecycle at QoS 4 (reliable-TCP).
#[test]
fn tcp_lifecycle_qos4() {
    let multicast_group = "239.0.0.1:19814";
    let tcp_base_port = "19843";

    run_custom_udp_variant(multicast_group, tcp_base_port, 4, "tcp-q4");
}

/// Helper: spawn the variant-custom-udp binary with the new CLI shape
/// (--peers self=127.0.0.1 / --runner self / --multicast-group / --buffer-size
/// / --tcp-base-port / --qos N) and verify it exits 0 and produces a JSONL
/// log file.
fn run_custom_udp_variant(multicast_group: &str, tcp_base_port: &str, qos: u8, run_id: &str) {
    let binary = env!("CARGO_BIN_EXE_variant-custom-udp");
    let tmp = tempfile::tempdir().unwrap();
    let qos_str = qos.to_string();

    let mut child = std::process::Command::new(binary)
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
            &qos_str,
            "--log-dir",
            tmp.path().to_str().unwrap(),
            "--launch-ts",
            "2026-04-30T00:00:00.000000000Z",
            "--variant",
            "custom-udp",
            "--runner",
            "self",
            "--run",
            run_id,
            // Runner-injected --peers (synthesized for the test).
            "--peers",
            "self=127.0.0.1",
            "--",
            "--multicast-group",
            multicast_group,
            "--buffer-size",
            "65536",
            "--tcp-base-port",
            tcp_base_port,
        ])
        .spawn()
        .expect("failed to spawn variant-custom-udp");

    let status = wait_with_timeout(&mut child, Duration::from_secs(15));
    assert!(
        status.is_some(),
        "variant-custom-udp timed out (qos {})",
        qos
    );
    let status = status.unwrap();
    assert!(
        status.success(),
        "variant-custom-udp exited with non-zero status (qos {}): {:?}",
        qos,
        status.code()
    );

    let log_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert!(
        !log_files.is_empty(),
        "no JSONL log file created in {:?} for qos {}",
        tmp.path(),
        qos
    );
}

/// Verify the variant fails fast when --runner is not present in --peers
/// (a runner/contract bug).
#[test]
fn runner_not_in_peers_fails() {
    let binary = env!("CARGO_BIN_EXE_variant-custom-udp");
    let tmp = tempfile::tempdir().unwrap();

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
            tmp.path().to_str().unwrap(),
            "--launch-ts",
            "2026-04-30T00:00:00.000000000Z",
            "--variant",
            "custom-udp",
            "--runner",
            "carol",
            "--run",
            "missing01",
            "--peers",
            "alice=127.0.0.1,bob=127.0.0.1",
            "--",
            "--multicast-group",
            "239.0.0.1:19815",
            "--buffer-size",
            "65536",
            "--tcp-base-port",
            "19844",
        ])
        .output()
        .expect("failed to run variant-custom-udp");

    assert!(
        !output.status.success(),
        "expected variant-custom-udp to fail when --runner is not in --peers"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("carol") && stderr.contains("not present"),
        "expected clear error mentioning the missing runner; stderr was: {stderr}"
    );
}

/// Verify the variant fails when --tcp-base-port is missing.
#[test]
fn missing_tcp_base_port_fails() {
    let binary = env!("CARGO_BIN_EXE_variant-custom-udp");
    let tmp = tempfile::tempdir().unwrap();

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
            tmp.path().to_str().unwrap(),
            "--launch-ts",
            "2026-04-30T00:00:00.000000000Z",
            "--variant",
            "custom-udp",
            "--runner",
            "self",
            "--run",
            "missing02",
            "--peers",
            "self=127.0.0.1",
            "--",
            "--multicast-group",
            "239.0.0.1:19816",
            "--buffer-size",
            "65536",
            // No --tcp-base-port.
        ])
        .output()
        .expect("failed to run variant-custom-udp");

    assert!(
        !output.status.success(),
        "expected variant-custom-udp to fail when --tcp-base-port is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tcp-base-port"),
        "expected error mentioning tcp-base-port; stderr was: {stderr}"
    );
}

/// Verify the variant fails when --multicast-group is missing.
#[test]
fn missing_multicast_group_fails() {
    let binary = env!("CARGO_BIN_EXE_variant-custom-udp");
    let tmp = tempfile::tempdir().unwrap();

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
            tmp.path().to_str().unwrap(),
            "--launch-ts",
            "2026-04-30T00:00:00.000000000Z",
            "--variant",
            "custom-udp",
            "--runner",
            "self",
            "--run",
            "missing03",
            "--peers",
            "self=127.0.0.1",
            "--",
            "--buffer-size",
            "65536",
            "--tcp-base-port",
            "19845",
            // No --multicast-group.
        ])
        .output()
        .expect("failed to run variant-custom-udp");

    assert!(
        !output.status.success(),
        "expected variant-custom-udp to fail when --multicast-group is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("multicast-group"),
        "expected error mentioning multicast-group; stderr was: {stderr}"
    );
}

/// Wait for a child process with a timeout. Returns None on timeout.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}
