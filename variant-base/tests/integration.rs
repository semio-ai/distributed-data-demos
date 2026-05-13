use std::io::BufRead;
use std::process::Command;

use tempfile::TempDir;

use variant_base::cli::{CliArgs, DEFAULT_RECV_BUFFER_KB};
use variant_base::driver::run_protocol;
use variant_base::dummy::VariantDummy;
use variant_base::types::ThreadingMode;

/// Helper to build the canonical CLI arg list for spawning the
/// `variant-dummy` binary in tests. `progress_stdout_interval_ms` is
/// the only knob the smoke tests exercise; everything else is fixed.
fn dummy_binary_args(
    log_dir: &str,
    launch_ts: &str,
    runner: &str,
    progress_stdout_interval_ms: u32,
    operate_secs: &str,
) -> Vec<String> {
    vec![
        "--tick-rate-hz".to_string(),
        "100".to_string(),
        "--stabilize-secs".to_string(),
        "0".to_string(),
        "--operate-secs".to_string(),
        operate_secs.to_string(),
        "--silent-secs".to_string(),
        "0".to_string(),
        "--eot-timeout-secs".to_string(),
        "1".to_string(),
        "--workload".to_string(),
        "scalar-flood".to_string(),
        "--values-per-tick".to_string(),
        "5".to_string(),
        "--qos".to_string(),
        "1".to_string(),
        "--log-dir".to_string(),
        log_dir.to_string(),
        "--launch-ts".to_string(),
        launch_ts.to_string(),
        "--variant".to_string(),
        "dummy".to_string(),
        "--runner".to_string(),
        runner.to_string(),
        "--run".to_string(),
        "run-bin".to_string(),
        "--threading-mode".to_string(),
        "single".to_string(),
        "--progress-stdout-interval-ms".to_string(),
        progress_stdout_interval_ms.to_string(),
        "--peers".to_string(),
        format!("{runner}=127.0.0.1"),
    ]
}

/// Build CLI args for a short test run.
fn test_args(log_dir: &str) -> CliArgs {
    test_args_with_mode(log_dir, ThreadingMode::Single)
}

fn test_args_with_mode(log_dir: &str, threading_mode: ThreadingMode) -> CliArgs {
    CliArgs {
        tick_rate_hz: 10,
        stabilize_secs: 0,
        operate_secs: 1,
        silent_secs: 0,
        eot_timeout_secs: Some(1),
        workload: "scalar-flood".to_string(),
        values_per_tick: 5,
        qos: 1,
        log_dir: log_dir.to_string(),
        launch_ts: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.9fZ")
            .to_string(),
        variant: "dummy".to_string(),
        runner: "test-runner".to_string(),
        run: "run01".to_string(),
        threading_mode,
        recv_buffer_kb: DEFAULT_RECV_BUFFER_KB,
        // Disable stdout progress in the in-process integration test
        // path -- the smoke test below covers the enabled path via the
        // child-binary spawn.
        progress_stdout_interval_ms: 0,
        // Disable variant-side idle detection (T15.5) for these
        // protocol-shape tests so the operate phase is purely time-bounded.
        operate_idle_secs: 0,
        // Single-runner self-loopback peers list -> empty expected set
        // -> EOT phase terminates immediately with no `eot_timeout`.
        extra: vec!["--peers".to_string(), "test-runner=127.0.0.1".to_string()],
    }
}

/// Read all JSONL lines from the log file.
fn read_log(log_dir: &str) -> Vec<serde_json::Value> {
    let path = std::path::Path::new(log_dir).join("dummy-test-runner-run01.jsonl");
    let file = std::fs::File::open(&path).expect("log file should exist");
    let reader = std::io::BufReader::new(file);
    reader
        .lines()
        .map(|line| {
            let line = line.expect("should read line");
            serde_json::from_str(&line).expect("each line should be valid JSON")
        })
        .collect()
}

#[test]
fn test_full_protocol_with_dummy() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let args = test_args(log_dir);

    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("protocol should complete successfully");

    let lines = read_log(log_dir);
    assert!(!lines.is_empty(), "log should have entries");

    // Every line must have the common fields.
    for (i, line) in lines.iter().enumerate() {
        assert!(line.get("ts").is_some(), "line {} missing 'ts'", i);
        assert_eq!(line["variant"], "dummy", "line {} variant mismatch", i);
        assert_eq!(line["runner"], "test-runner", "line {} runner mismatch", i);
        assert_eq!(line["run"], "run01", "line {} run mismatch", i);
        assert!(line.get("event").is_some(), "line {} missing 'event'", i);
    }

    // Collect event types in order.
    let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();

    // Phase events must appear in order: connect, stabilize, operate, silent.
    let phase_events: Vec<(&str, Option<&str>)> = lines
        .iter()
        .filter(|l| l["event"] == "phase")
        .map(|l| {
            (
                l["phase"].as_str().unwrap(),
                l.get("profile").and_then(|v| v.as_str()),
            )
        })
        .collect();

    assert_eq!(phase_events.len(), 5, "should have 5 phase events");
    assert_eq!(phase_events[0].0, "connect");
    assert_eq!(phase_events[1].0, "stabilize");
    assert_eq!(phase_events[2].0, "operate");
    assert_eq!(
        phase_events[2].1,
        Some("scalar-flood"),
        "operate phase should include workload profile"
    );
    assert_eq!(phase_events[3].0, "eot");
    assert_eq!(phase_events[4].0, "silent");

    // EOT phase: an `eot_sent` event must appear exactly once. With an
    // empty expected-peer set (single-runner self-loopback) no
    // `eot_timeout` should fire.
    let eot_sent_count = events.iter().filter(|&&e| e == "eot_sent").count();
    assert_eq!(eot_sent_count, 1, "should have exactly one eot_sent event");
    let eot_timeout_count = events.iter().filter(|&&e| e == "eot_timeout").count();
    assert_eq!(
        eot_timeout_count, 0,
        "single-runner self-loopback should not emit eot_timeout"
    );

    // Connected event must exist with launch_ts and elapsed_ms.
    let connected: Vec<&serde_json::Value> =
        lines.iter().filter(|l| l["event"] == "connected").collect();
    assert_eq!(
        connected.len(),
        1,
        "should have exactly one connected event"
    );
    assert!(connected[0].get("launch_ts").is_some());
    assert!(connected[0].get("elapsed_ms").is_some());
    let elapsed = connected[0]["elapsed_ms"].as_f64().unwrap();
    assert!(elapsed >= 0.0, "elapsed_ms should be non-negative");

    // Write events: check monotonic seq numbers.
    let write_seqs: Vec<u64> = lines
        .iter()
        .filter(|l| l["event"] == "write")
        .map(|l| l["seq"].as_u64().unwrap())
        .collect();
    assert!(
        !write_seqs.is_empty(),
        "should have at least one write event"
    );
    for window in write_seqs.windows(2) {
        assert!(
            window[1] > window[0],
            "write seq numbers should be monotonically increasing: {} -> {}",
            window[0],
            window[1]
        );
    }

    // Receive events: should exist for each write (dummy echoes).
    let receive_count = events.iter().filter(|&&e| e == "receive").count();
    assert_eq!(
        receive_count,
        write_seqs.len(),
        "every write should have a matching receive (dummy echoes)"
    );

    // Resource events should exist (at least one during the operate phase).
    let resource_count = events.iter().filter(|&&e| e == "resource").count();
    assert!(
        resource_count > 0,
        "should have at least one resource event"
    );

    // Verify events appear in expected order groups:
    // connect phase -> connected -> stabilize phase -> operate phase -> writes/receives/resources -> silent phase
    let first_phase_idx = events.iter().position(|&e| e == "phase").unwrap();
    let connected_idx = events.iter().position(|&e| e == "connected").unwrap();
    assert!(
        connected_idx > first_phase_idx,
        "connected should come after first phase event"
    );

    let first_write_idx = events.iter().position(|&e| e == "write").unwrap();
    let last_silent_phase_idx = lines
        .iter()
        .rposition(|l| l["event"] == "phase" && l["phase"] == "silent")
        .unwrap();
    assert!(
        first_write_idx < last_silent_phase_idx,
        "writes should occur before silent phase"
    );
}

#[test]
fn test_variant_dummy_binary_exit_code() {
    // Build the binary path. In test mode, it's in the target/debug directory.
    let binary = env!("CARGO_BIN_EXE_variant-dummy");
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let launch_ts = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string();

    let output = Command::new(binary)
        .args([
            "--tick-rate-hz",
            "10",
            "--stabilize-secs",
            "0",
            "--operate-secs",
            "1",
            "--silent-secs",
            "0",
            "--eot-timeout-secs",
            "1",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "5",
            "--qos",
            "1",
            "--log-dir",
            log_dir,
            "--launch-ts",
            &launch_ts,
            "--variant",
            "dummy",
            "--runner",
            "bin-test",
            "--run",
            "run-bin",
            "--threading-mode",
            "single",
            "--peers",
            "bin-test=127.0.0.1",
        ])
        .output()
        .expect("failed to execute variant-dummy binary");

    assert!(
        output.status.success(),
        "variant-dummy should exit 0, got: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify JSONL file was produced.
    let log_path = dir.path().join("dummy-bin-test-run-bin.jsonl");
    assert!(log_path.exists(), "JSONL log file should be created");

    // Verify it contains valid JSONL.
    let file = std::fs::File::open(&log_path).unwrap();
    let reader = std::io::BufReader::new(file);
    let line_count = reader
        .lines()
        .map(|l| {
            let l = l.unwrap();
            serde_json::from_str::<serde_json::Value>(&l).expect("each line should be valid JSON");
        })
        .count();
    assert!(line_count > 0, "log file should have at least one line");
}

/// Run VariantDummy end-to-end in `single` and `multi` modes and
/// verify the expected JSONL event sequence is produced for both
/// (T14.1 integration acceptance).
#[test]
fn test_variant_dummy_runs_in_both_threading_modes() {
    for mode in [ThreadingMode::Single, ThreadingMode::Multi] {
        let dir = TempDir::new().unwrap();
        let log_dir = dir.path().to_str().unwrap();
        let args = test_args_with_mode(log_dir, mode);

        let mut dummy = VariantDummy::new(&args.runner);
        run_protocol(&mut dummy, &args)
            .unwrap_or_else(|e| panic!("protocol completes in {mode} mode: {e}"));
        // The dummy stored the mode the driver supplied at connect time.
        assert_eq!(
            dummy.connected_mode(),
            Some(mode),
            "dummy should record the driver-supplied threading mode"
        );

        let lines = read_log(log_dir);
        let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();

        // The expected phase / lifecycle event sequence must be present
        // regardless of mode.
        let phases: Vec<&str> = lines
            .iter()
            .filter(|l| l["event"] == "phase")
            .map(|l| l["phase"].as_str().unwrap())
            .collect();
        assert_eq!(
            phases,
            vec!["connect", "stabilize", "operate", "eot", "silent"],
            "phase order must be canonical in {mode} mode"
        );

        // Exactly one `connected` event carrying the mode and the
        // default recv-buffer size.
        let connected: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "connected").collect();
        assert_eq!(
            connected.len(),
            1,
            "exactly one connected event in {mode} mode"
        );
        assert_eq!(
            connected[0]["threading_mode"],
            mode.as_str(),
            "connected event must record the threading_mode for {mode}"
        );
        assert_eq!(
            connected[0]["recv_buffer_kb"], DEFAULT_RECV_BUFFER_KB,
            "connected event must record recv_buffer_kb for {mode}"
        );

        // Exactly one `eot_sent` event (single-runner -> immediate exit).
        let eot_sent = events.iter().filter(|&&e| e == "eot_sent").count();
        assert_eq!(eot_sent, 1, "expected exactly one eot_sent in {mode} mode");
        let eot_timeout = events.iter().filter(|&&e| e == "eot_timeout").count();
        assert_eq!(
            eot_timeout, 0,
            "single-runner self-loopback should not emit eot_timeout in {mode} mode"
        );

        // The dummy echoes every publish; we expect both writes and
        // matching receives during the operate phase.
        let writes = events.iter().filter(|&&e| e == "write").count();
        let receives = events.iter().filter(|&&e| e == "receive").count();
        assert!(writes > 0, "expected at least one write in {mode} mode");
        assert_eq!(
            writes, receives,
            "every write should have a matching receive in {mode} mode (dummy echoes)"
        );
    }
}

/// T15.1: spawn `variant-dummy` with `--progress-stdout-interval-ms 200`,
/// capture its stdout, and verify the emitted stream is one well-formed
/// JSON progress event per ~200 ms with the expected phase sequence
/// visible.
#[test]
fn test_variant_dummy_emits_progress_to_stdout() {
    let binary = env!("CARGO_BIN_EXE_variant-dummy");
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let launch_ts = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string();

    // 200 ms interval, 2 s operate -> approximately 10 lines over the
    // operate phase. The dummy's stabilize / silent windows are zero so
    // operate dominates wallclock.
    let args = dummy_binary_args(log_dir, &launch_ts, "stdout-test", 200, "2");

    let output = Command::new(binary)
        .args(&args)
        .output()
        .expect("variant-dummy binary should run");
    assert!(
        output.status.success(),
        "variant-dummy should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout must be valid UTF-8");
    // Every non-empty stdout line must be one of our progress JSON
    // events. There is no other line variant-base writes to stdout.
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !lines.is_empty(),
        "variant-dummy with progress emission should emit at least one line"
    );

    // Lower bound: at 200 ms interval over a 2 s operate phase plus
    // connect/eot phases, we should comfortably see >=5 lines (we err
    // generously low to absorb CI scheduling drift). Upper bound is
    // sanity-only -- absurdly high counts would indicate runaway
    // emission.
    assert!(
        (5..=60).contains(&lines.len()),
        "expected 5..=60 progress lines, got {} for stdout:\n{stdout}",
        lines.len()
    );

    let parsed: Vec<serde_json::Value> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("line {i} did not parse as JSON: {e}\nraw: {l}"))
        })
        .collect();

    // Schema check: every line carries the documented fields.
    for (i, v) in parsed.iter().enumerate() {
        assert_eq!(v["event"], "progress", "line {i} missing event=progress");
        assert!(v["ts"].is_string(), "line {i} missing string ts");
        assert!(v["phase"].is_string(), "line {i} missing string phase");
        assert!(v["sent"].is_u64(), "line {i} sent must be u64");
        assert!(v["received"].is_u64(), "line {i} received must be u64");
        assert!(v["eot_sent"].is_boolean(), "line {i} eot_sent must be bool");
        assert!(
            v["eot_received"].is_boolean(),
            "line {i} eot_received must be bool"
        );
    }

    // Timestamps must be RFC 3339 and monotonically non-decreasing.
    let timestamps: Vec<chrono::DateTime<chrono::FixedOffset>> = parsed
        .iter()
        .map(|v| chrono::DateTime::parse_from_rfc3339(v["ts"].as_str().unwrap()).unwrap())
        .collect();
    for window in timestamps.windows(2) {
        assert!(
            window[1] >= window[0],
            "timestamps must be monotonic non-decreasing"
        );
    }

    // Phase transitions: at minimum operate -> done must appear (the
    // 0-duration stabilize and silent phases mean their progress
    // window is tight and may be missed by the 200 ms emitter, which
    // is expected -- the runner-side state machine treats absence of
    // a phase as just-passed-through, not an error). `operate` and
    // `done` are the load-bearing transitions for T15.1.
    let phases: Vec<&str> = parsed
        .iter()
        .map(|v| v["phase"].as_str().unwrap())
        .collect();
    assert!(
        phases.contains(&"operate"),
        "operate phase missing from progress stream: {phases:?}"
    );
    assert!(
        phases.contains(&"done"),
        "done phase missing from progress stream: {phases:?}"
    );

    // sent / received counters must be monotonic non-decreasing.
    let mut prev_sent = 0u64;
    let mut prev_received = 0u64;
    for v in &parsed {
        let s = v["sent"].as_u64().unwrap();
        let r = v["received"].as_u64().unwrap();
        assert!(s >= prev_sent, "sent must be monotonic: {prev_sent} -> {s}");
        assert!(
            r >= prev_received,
            "received must be monotonic: {prev_received} -> {r}"
        );
        prev_sent = s;
        prev_received = r;
    }
    // At least one line must have advanced both counters (the dummy
    // publishes and echoes during operate, so both grow).
    assert!(
        prev_sent > 0,
        "final sent counter must be > 0 after a 2 s operate phase"
    );
    assert!(
        prev_received > 0,
        "final received counter must be > 0 after a 2 s operate phase"
    );
}

/// T15.1: with `--progress-stdout-interval-ms 0`, the variant must
/// emit ZERO stdout lines (back-compat path).
#[test]
fn test_variant_dummy_progress_stdout_zero_disables_emission() {
    let binary = env!("CARGO_BIN_EXE_variant-dummy");
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let launch_ts = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string();

    let args = dummy_binary_args(log_dir, &launch_ts, "stdout-off", 0, "1");

    let output = Command::new(binary)
        .args(&args)
        .output()
        .expect("variant-dummy binary should run");
    assert!(
        output.status.success(),
        "variant-dummy should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout must be valid UTF-8");
    assert!(
        stdout.trim().is_empty(),
        "--progress-stdout-interval-ms=0 must produce empty stdout, got:\n{stdout}"
    );
}
