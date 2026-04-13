//! Integration test: single-process loopback over real Zenoh transport.
//!
//! The variant publishes and subscribes to itself, verifying that the full
//! connect -> publish -> poll_receive -> disconnect lifecycle works end-to-end
//! through the protocol driver.

#[test]
fn loopback_full_protocol() {
    let log_dir = tempfile::tempdir().unwrap();
    let log_dir_path = log_dir.path().to_str().unwrap().replace('\\', "/");

    // Build the binary path.
    let binary = env!("CARGO_BIN_EXE_variant-zenoh");

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
            "2",
            "--qos",
            "1",
            "--log-dir",
            &log_dir_path,
            "--launch-ts",
            "2026-04-12T14:00:00.000000000Z",
            "--variant",
            "zenoh",
            "--runner",
            "test-runner",
            "--run",
            "run01",
            "--",
            "--zenoh-mode",
            "peer",
        ])
        .status()
        .expect("failed to spawn variant-zenoh");

    assert!(status.success(), "variant-zenoh exited with: {}", status);

    // Verify the JSONL log file was created and contains expected events.
    let log_file = log_dir.path().join("zenoh-test-runner-run01.jsonl");
    assert!(log_file.exists(), "expected log file at {:?}", log_file);

    let contents = std::fs::read_to_string(&log_file).unwrap();
    let lines: Vec<&str> = contents.lines().collect();

    // Should have at least: phase(connect), connected, phase(stabilize),
    // phase(operate), some writes, some receives, phase(silent).
    assert!(
        lines.len() > 10,
        "expected more than 10 log lines, got {}",
        lines.len()
    );

    // Check that we have phase events.
    let has_connect_phase = lines
        .iter()
        .any(|l| l.contains("\"event\":\"phase\"") && l.contains("\"phase\":\"connect\""));
    let has_operate_phase = lines
        .iter()
        .any(|l| l.contains("\"event\":\"phase\"") && l.contains("\"phase\":\"operate\""));
    let has_silent_phase = lines
        .iter()
        .any(|l| l.contains("\"event\":\"phase\"") && l.contains("\"phase\":\"silent\""));

    assert!(has_connect_phase, "missing connect phase event");
    assert!(has_operate_phase, "missing operate phase event");
    assert!(has_silent_phase, "missing silent phase event");

    // Check that we have write events.
    let write_count = lines
        .iter()
        .filter(|l| l.contains("\"event\":\"write\""))
        .count();
    assert!(
        write_count > 0,
        "expected at least one write event, got {}",
        write_count
    );

    // Check that we have receive events (loopback: variant receives its own writes).
    let receive_count = lines
        .iter()
        .filter(|l| l.contains("\"event\":\"receive\""))
        .count();
    assert!(
        receive_count > 0,
        "expected at least one receive event, got {}",
        receive_count
    );

    // Verify receive events reference the correct writer.
    let has_correct_writer = lines
        .iter()
        .any(|l| l.contains("\"event\":\"receive\"") && l.contains("\"writer\":\"test-runner\""));
    assert!(
        has_correct_writer,
        "receive events should reference writer 'test-runner'"
    );
}
