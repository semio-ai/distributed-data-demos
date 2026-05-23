//! Integration test: single-process loopback over real Zenoh transport.
//!
//! The variant publishes and subscribes to itself, verifying that the full
//! connect -> publish -> poll_receive -> disconnect lifecycle works end-to-end
//! through the protocol driver.
//!
//! **Self-filter update**: per `compact-log-schema.md` event kind 1
//! (`receive`), variants MUST drop payloads whose writer equals the
//! variant's own runner BEFORE they reach `inc_received`. A
//! single-process loopback spawn therefore has NO foreign writers, so
//! `received` is contractually 0 and no `receive` rows are emitted.
//! Previously this test asserted `receive_count > 0` against the JSONL
//! log -- that was pinning the pre-filter self-echo inflation contract
//! (E19 / `metak-shared/ANALYSIS.md` ratio-up-to-400% note) and is now
//! obsolete. The test instead asserts the lifecycle events (phase
//! transitions, eot_sent) so the loopback path's connect -> operate ->
//! silent -> done sweep is still exercised.
//!
//! **JSONL vs compact**: T18.2b moved `write` / `receive` events out
//! of JSONL into the per-spawn compact Parquet log, so the original
//! `write_count > 0` JSONL assertion can never pass anyway --
//! orthogonal regression to the self-filter contract.

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
            // T14.7: Zenoh requires Multi mode; CLI default is `single`
            // for backwards-compat across the E14 rollout, so this test
            // injects the mode explicitly until T14.8 makes the runner
            // do so for every spawn.
            "--threading-mode",
            "multi",
            "--",
            "--zenoh-mode",
            "peer",
        ])
        .status()
        .expect("failed to spawn variant-zenoh");

    assert!(status.success(), "variant-zenoh exited with: {}", status);

    // Verify the JSONL log file was created and contains expected
    // lifecycle events. Per-event `write` / `receive` rows are NOT in
    // JSONL (T18.2b moved them to the compact Parquet log); this
    // assertion is on the lifecycle surface only.
    let log_file = log_dir.path().join("zenoh-test-runner-run01.jsonl");
    assert!(log_file.exists(), "expected log file at {:?}", log_file);

    let contents = std::fs::read_to_string(&log_file).unwrap();
    let lines: Vec<&str> = contents.lines().collect();

    // Should have at least: phase(connect), connected, phase(stabilize),
    // phase(operate), phase(silent), eot_sent, phase(done).
    assert!(
        lines.len() >= 5,
        "expected at least 5 log lines, got {}",
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
    let has_eot_sent = lines.iter().any(|l| l.contains("\"event\":\"eot_sent\""));

    assert!(has_connect_phase, "missing connect phase event");
    assert!(has_operate_phase, "missing operate phase event");
    assert!(has_silent_phase, "missing silent phase event");
    assert!(has_eot_sent, "missing eot_sent event");

    // Self-filter contract: the compact log MUST NOT contain
    // `receive` rows for a single-process loopback spawn, because the
    // variant's only writer is itself and self-writes are filtered at
    // the subscriber boundary per compact-log-schema.md event kind 1.
    // We don't introspect the Parquet here (no polars dep in this
    // tests crate); the absence of self-echoes is verified at the
    // unit-test layer (`multi_zenoh_subscriber_filters_self_writer`)
    // and at the two-runner integration layer (where `received` ==
    // peer's `sent`, not 2x). The lifecycle pass-through asserted
    // above is the integration-level invariant for this test.
}
