/// Integration tests for the hybrid variant.
///
/// These test real network I/O using loopback addresses.
use std::net::SocketAddrV4;
use std::time::Duration;

/// Test UDP multicast loopback: send a message and receive it back.
#[test]
fn udp_multicast_loopback() {
    // Use a unique multicast port to avoid conflicts with other tests.
    let multicast_addr: SocketAddrV4 = "239.0.0.1:19801".parse().unwrap();

    // Test via the binary subprocess approach since we cannot import private
    // modules from a binary crate. This tests the actual binary end-to-end.
    let binary = env!("CARGO_BIN_EXE_variant-hybrid");
    let tmp = tempfile::tempdir().unwrap();

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
            "1",
            "--log-dir",
            tmp.path().to_str().unwrap(),
            "--launch-ts",
            "2026-04-13T00:00:00.000000000Z",
            "--variant",
            "hybrid",
            "--runner",
            "test-a",
            "--run",
            "run-integ",
            "--",
            "--multicast-group",
            &multicast_addr.to_string(),
        ])
        .spawn()
        .expect("failed to spawn variant-hybrid");

    // Wait with a timeout.
    let status = wait_with_timeout(&mut child, Duration::from_secs(15));
    assert!(
        status.is_some(),
        "variant-hybrid timed out during UDP loopback test"
    );
    let status = status.unwrap();
    assert!(
        status.success(),
        "variant-hybrid exited with non-zero status: {:?}",
        status.code()
    );

    // Check that the log file was created and has content.
    let log_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert!(
        !log_files.is_empty(),
        "no JSONL log file created in {:?}",
        tmp.path()
    );
}

/// Test TCP loopback: connect to self and send/receive QoS 4 messages.
#[test]
fn tcp_self_connect() {
    let binary = env!("CARGO_BIN_EXE_variant-hybrid");
    let tmp = tempfile::tempdir().unwrap();

    // Use a high port to avoid conflicts.
    let tcp_port = "19802";

    // For TCP self-connect, we set --peers to our own listener address.
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
            "4",
            "--log-dir",
            tmp.path().to_str().unwrap(),
            "--launch-ts",
            "2026-04-13T00:00:00.000000000Z",
            "--variant",
            "hybrid",
            "--runner",
            "test-b",
            "--run",
            "run-integ-tcp",
            "--",
            "--tcp-base-port",
            tcp_port,
            "--peers",
            &format!("127.0.0.1:{}", tcp_port),
        ])
        .spawn()
        .expect("failed to spawn variant-hybrid");

    let status = wait_with_timeout(&mut child, Duration::from_secs(15));
    assert!(
        status.is_some(),
        "variant-hybrid timed out during TCP self-connect test"
    );
    let status = status.unwrap();
    assert!(
        status.success(),
        "variant-hybrid exited with non-zero status: {:?}",
        status.code()
    );

    let log_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert!(
        !log_files.is_empty(),
        "no JSONL log file created in {:?}",
        tmp.path()
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
