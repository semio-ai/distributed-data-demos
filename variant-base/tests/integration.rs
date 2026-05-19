use std::io::BufRead;
use std::process::Command;

use tempfile::TempDir;

use variant_base::cli::{CliArgs, DEFAULT_RECV_BUFFER_KB};
use variant_base::driver::run_protocol;
use variant_base::dummy::VariantDummy;
use variant_base::types::{Qos, ThreadingMode};
use variant_base::workload::{create_workload_with_params, WorkloadParams, WriteShape};

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
        // Default disable idle detection in the existing smoke tests so
        // their pre-T15.5 expectations (eot phase event, on-wire
        // eot_sent shape) still hold. Tests that exercise the T15.5
        // idle path build args explicitly.
        "--operate-idle-secs".to_string(),
        "0".to_string(),
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
        // Disable the watchdog (T15.11) in these short integration
        // tests -- no internal-stall risk and we want zero extra
        // background threads.
        watchdog_secs: 0,
        // T18.2 / E18: defaults match the CLI defaults so existing
        // smoke tests do not need to think about the new flags.
        // Tests that exercise the compact-only path or the soft /
        // hard ceiling thresholds build their own args from
        // scratch.
        digest_mem_soft_mb: variant_base::cli::DEFAULT_DIGEST_MEM_SOFT_MB,
        digest_mem_hard_mb: variant_base::cli::DEFAULT_DIGEST_MEM_HARD_MB,
        // Keep the legacy per-event JSONL stream ON in the
        // integration tests that pre-date T18.2 -- their assertions
        // explicitly count `write` and `receive` lines.
        legacy_jsonl_events: true,
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

    // After T15.8 the EOT phase is removed; after T18.2 the digest
    // phase is appended -- five phase events: connect, stabilize,
    // operate, silent, digest.
    assert_eq!(phase_events.len(), 5, "should have 5 phase events");
    assert_eq!(phase_events[0].0, "connect");
    assert_eq!(phase_events[1].0, "stabilize");
    assert_eq!(phase_events[2].0, "operate");
    assert_eq!(
        phase_events[2].1,
        Some("scalar-flood"),
        "operate phase should include workload profile"
    );
    assert_eq!(phase_events[3].0, "silent");
    assert_eq!(phase_events[4].0, "digest");

    // The `eot_sent` JSONL marker is still emitted exactly once between
    // operate and silent. No on-wire byproducts (`eot_timeout`,
    // `eot_received`).
    let eot_sent_count = events.iter().filter(|&&e| e == "eot_sent").count();
    assert_eq!(eot_sent_count, 1, "should have exactly one eot_sent event");
    let eot_timeout_count = events.iter().filter(|&&e| e == "eot_timeout").count();
    assert_eq!(
        eot_timeout_count, 0,
        "post-T15.8 driver never emits eot_timeout"
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
            vec!["connect", "stabilize", "operate", "silent", "digest"],
            "phase order must be canonical in {mode} mode (T15.8: no eot phase; T18.2: digest appended)"
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
            "post-T15.8 driver never emits eot_timeout in {mode} mode"
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

/// T15.5: spawn `variant-dummy` with `--operate-idle-secs 1
/// --operate-secs 30 --tick-rate-hz 0` so the variant publishes nothing
/// during operate. With both counters flat from the start, idle
/// detection must fire within ~1.5 s and the operate phase must end
/// well before the 30 s operate_secs budget. Captured JSONL must
/// contain exactly one `eot_sent` event, no `phase=eot` event, and a
/// clean `silent` phase transition.
#[test]
fn test_variant_dummy_idle_detection_short_circuits_operate() {
    // Strategy: set tick_rate_hz=1 and operate-secs=30 to put the
    // operate loop on a one-second cadence, then force values_per_tick
    // very small. The dummy still echoes whatever it publishes, so
    // both counters advance together on every tick. To genuinely idle
    // we use the existing dummy as-is and rely on the LAST publish
    // having drained the queue: after that tick the queue stays empty
    // for the rest of operate. But because the dummy publishes again
    // each tick, this test relies on the special case of
    // tick_rate_hz=1 with values_per_tick=0. Setting values_per_tick=0
    // means the workload generator produces no ops, so try_publish is
    // never called and `sent` stays at 0. poll_receive then always
    // returns None and `received` also stays at 0. This is the
    // cleanest integration scenario for the T15.5 path.
    let binary = env!("CARGO_BIN_EXE_variant-dummy");
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let launch_ts = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string();

    // Hand-build the arg list so we can override values_per_tick to 0
    // and set --operate-idle-secs to 1 without going through the
    // helper (which fixes those values).
    let args = vec![
        "--tick-rate-hz".to_string(),
        "10".to_string(),
        "--stabilize-secs".to_string(),
        "0".to_string(),
        "--operate-secs".to_string(),
        "30".to_string(),
        "--silent-secs".to_string(),
        "0".to_string(),
        "--workload".to_string(),
        "scalar-flood".to_string(),
        // Zero values per tick -> workload generates no ops -> sent
        // counter never advances. The dummy's queue stays empty so
        // received never advances either. Both counters idle from t=0.
        "--values-per-tick".to_string(),
        "0".to_string(),
        "--qos".to_string(),
        "1".to_string(),
        "--log-dir".to_string(),
        log_dir.to_string(),
        "--launch-ts".to_string(),
        launch_ts.clone(),
        "--variant".to_string(),
        "dummy".to_string(),
        "--runner".to_string(),
        "idle-test".to_string(),
        "--run".to_string(),
        "run-idle".to_string(),
        "--threading-mode".to_string(),
        "single".to_string(),
        "--progress-stdout-interval-ms".to_string(),
        "200".to_string(),
        "--operate-idle-secs".to_string(),
        "1".to_string(),
        "--peers".to_string(),
        "idle-test=127.0.0.1".to_string(),
    ];

    let start = std::time::Instant::now();
    let output = Command::new(binary)
        .args(&args)
        .output()
        .expect("variant-dummy binary should run");
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "variant-dummy should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Idle threshold of 1 s + tick + emitter sleep + process overhead.
    // 5 s is generous slack for CI on Windows.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "idle detection should short-circuit the 30 s operate phase; got {elapsed:?}"
    );

    // JSONL log shape on the idle path.
    let log_path = dir.path().join("dummy-idle-test-run-idle.jsonl");
    assert!(log_path.exists(), "JSONL log file should be created");
    let file = std::fs::File::open(&log_path).unwrap();
    let reader = std::io::BufReader::new(file);
    let lines: Vec<serde_json::Value> = reader
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(&l.unwrap()).unwrap())
        .collect();

    // Phase order: connect, stabilize, operate, silent (NO eot).
    let phases: Vec<&str> = lines
        .iter()
        .filter(|l| l["event"] == "phase")
        .map(|l| l["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        phases,
        vec!["connect", "stabilize", "operate", "silent", "digest"],
        "idle path skips eot, then digest is appended (T18.2), got {phases:?}"
    );

    // Exactly one `eot_sent` event.
    let eot_sent: Vec<&serde_json::Value> =
        lines.iter().filter(|l| l["event"] == "eot_sent").collect();
    assert_eq!(
        eot_sent.len(),
        1,
        "idle path must emit exactly one eot_sent, got {}",
        eot_sent.len()
    );

    // No on-wire EOT byproducts.
    let eot_timeout_count = lines.iter().filter(|l| l["event"] == "eot_timeout").count();
    assert_eq!(eot_timeout_count, 0, "idle path must not emit eot_timeout");

    // Stdout progress: at least one line with eot_sent:true should be
    // observable after the idle transition.
    let stdout = String::from_utf8(output.stdout).expect("stdout must be valid UTF-8");
    let progress_lines: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect();
    let has_eot_sent_true = progress_lines
        .iter()
        .any(|v| v["eot_sent"].as_bool() == Some(true));
    assert!(
        has_eot_sent_true,
        "expected at least one progress line with eot_sent=true after idle transition; got progress lines:\n{stdout}"
    );
}

/// T17.2 smoke: VariantDummy must complete the full protocol cleanly at
/// every QoS level (1..=4) under the post-T17.2 driver. The dummy's
/// default `try_publish` always returns `Ok(true)`, so no `backpressure_skipped`
/// rows should ever appear -- regardless of QoS -- and the strict-
/// delivery branch (QoS 3/4) must produce the same write/receive pair
/// shape as QoS 1/2.
#[test]
fn test_variant_dummy_smoke_every_qos_level() {
    for qos in 1u8..=4u8 {
        let dir = TempDir::new().unwrap();
        let log_dir = dir.path().to_str().unwrap();
        let mut args = test_args(log_dir);
        args.qos = qos;

        let mut dummy = VariantDummy::new(&args.runner);
        run_protocol(&mut dummy, &args)
            .unwrap_or_else(|e| panic!("protocol completes at QoS {qos}: {e}"));

        let path = std::path::Path::new(log_dir).join("dummy-test-runner-run01.jsonl");
        let file = std::fs::File::open(&path).expect("log file should exist");
        let reader = std::io::BufReader::new(file);
        let lines: Vec<serde_json::Value> = reader
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect();

        let writes = lines.iter().filter(|l| l["event"] == "write").count();
        let receives = lines.iter().filter(|l| l["event"] == "receive").count();
        let skipped = lines
            .iter()
            .filter(|l| l["event"] == "backpressure_skipped")
            .count();

        assert!(writes > 0, "QoS {qos}: expected at least one write");
        assert_eq!(
            writes, receives,
            "QoS {qos}: writes should match receives (dummy echoes)"
        );
        assert_eq!(
            skipped, 0,
            "QoS {qos}: VariantDummy never reports backpressure"
        );

        // QoS field must round-trip on every write event.
        for w in lines.iter().filter(|l| l["event"] == "write") {
            assert_eq!(
                w["qos"].as_u64().unwrap(),
                u64::from(qos),
                "QoS {qos}: write.qos must match requested level"
            );
        }
    }
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

// ---------------------------------------------------------------------------
// T18.2 / E18: compact-log Parquet output
// ---------------------------------------------------------------------------

/// T18.2 acceptance: an in-process `VariantDummy` run must produce a
/// well-formed `<variant>-<runner>-<run>.compact.parquet` file
/// alongside the legacy JSONL, and the file must contain at least one
/// row per `write` JSONL event observed in the same run. The schema
/// must match the documented seven-column layout.
#[test]
fn test_compact_parquet_is_written_alongside_jsonl() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let args = test_args(log_dir);

    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("protocol should complete");

    // The legacy JSONL must still exist (legacy_jsonl_events is on in
    // this test's args).
    let jsonl_path = dir.path().join("dummy-test-runner-run01.jsonl");
    assert!(
        jsonl_path.exists(),
        "legacy JSONL must be written when legacy_jsonl_events = true"
    );

    // The new compact parquet file MUST exist regardless of the
    // legacy flag.
    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    assert!(
        parquet_path.exists(),
        "compact Parquet file must be written at <log_dir>/<variant>-<runner>-<run>.compact.parquet"
    );

    // Read it back and validate the documented schema + KV metadata.
    let file = std::fs::File::open(&parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let meta = reader.metadata();
    assert_eq!(
        meta.file_metadata().schema_descr().num_columns(),
        13,
        "compact schema must have exactly 13 columns (T18.2b + E19): 7 base \
         (ts_ns, kind, seq, path_idx, peer_idx, qos, bytes) + 4 extras \
         (extra_f32, extra_f32_b, extra_i64, extra_utf8) + 2 E19 \
         (leaf_count, shape_idx)"
    );

    let kv = meta
        .file_metadata()
        .key_value_metadata()
        .expect("KV metadata must be present");
    let lookup: std::collections::HashMap<&str, &str> = kv
        .iter()
        .filter_map(|x| x.value.as_deref().map(|v| (x.key.as_str(), v)))
        .collect();
    assert_eq!(lookup.get("variant"), Some(&"dummy"));
    assert_eq!(lookup.get("runner"), Some(&"test-runner"));
    assert_eq!(lookup.get("run"), Some(&"run01"));
    assert!(lookup.contains_key("paths"));
    assert!(lookup.contains_key("peers"));

    // Cross-check: T18.2b made the compact buffer cover lifecycle
    // events too (`phase`, `connected`, `eot_sent`, `resource`).
    // The compact row count therefore equals the count of EVERY
    // JSONL event line, not only the per-event subset.
    let jsonl_lines: Vec<serde_json::Value> = std::fs::read_to_string(&jsonl_path)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let jsonl_event_rows = jsonl_lines.len() as i64;
    assert_eq!(
        meta.file_metadata().num_rows(),
        jsonl_event_rows,
        "T18.2b: compact row count must equal the number of JSONL event lines \
         (per-event + lifecycle)"
    );
    // VariantDummy echoes every write -> at least one row per write.
    assert!(meta.file_metadata().num_rows() > 0);
}

/// T18.2 acceptance: with `--legacy-jsonl-events` disabled (the new
/// default), per-event JSONL lines (`write`, `receive`,
/// `backpressure_skipped`) MUST NOT appear in the JSONL stream, but
/// the lifecycle events (`phase`, `connected`, `eot_sent`, `resource`)
/// MUST still be present, and the compact Parquet file MUST contain
/// the full per-event row set.
#[test]
fn test_compact_only_mode_suppresses_per_event_jsonl_but_keeps_lifecycle() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.legacy_jsonl_events = false;

    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("protocol should complete");

    let jsonl_path = dir.path().join("dummy-test-runner-run01.jsonl");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&jsonl_path)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();

    // Per-event rows must be absent.
    for event in [
        "write",
        "receive",
        "backpressure_skipped",
        "gap_detected",
        "gap_filled",
    ] {
        assert!(
            !events.contains(&event),
            "event '{event}' must NOT appear in JSONL when legacy_jsonl_events = false"
        );
    }

    // Lifecycle events must still be present.
    let phase_count = events.iter().filter(|e| **e == "phase").count();
    assert!(
        phase_count >= 5,
        "must still have all phase events; got {phase_count}"
    );
    assert!(
        events.contains(&"connected"),
        "must still have connected event"
    );
    assert!(
        events.contains(&"eot_sent"),
        "must still have eot_sent event"
    );

    // The compact Parquet file must still contain the per-event rows.
    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
    assert!(
        reader.metadata().file_metadata().num_rows() > 0,
        "compact-only mode must STILL accumulate rows in the Parquet file"
    );
}

/// T18.2b acceptance: every lifecycle event the JSONL stream emits
/// MUST also appear as a row in the compact `compact_events` table.
///
/// Run VariantDummy with `--legacy-jsonl-events` OFF so the JSONL
/// stream contains ONLY lifecycle events; cross-check that the
/// compact parquet contains a row for each lifecycle kind the
/// analyzer's existing pipeline depends on (`phase`, `connected`,
/// `eot_sent`, `resource`). After T18.4 lands, the analyzer can
/// drop the JSONL dependency entirely.
#[test]
fn test_compact_parquet_contains_lifecycle_events_when_jsonl_off() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.legacy_jsonl_events = false;
    // Operate long enough that the resource sampler (every 100 ms)
    // produces at least a couple of samples -- the existing
    // test_args sets operate_secs = 1, so at least ~10 samples are
    // expected.

    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("protocol should complete");

    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();

    // Collect kind column values across all rows.
    let kinds: Vec<i32> = reader
        .get_row_iter(None)
        .unwrap()
        .filter_map(|r| r.ok())
        .filter_map(|r| r.get_int(1).ok())
        .collect();

    // Count rows per kind so we can assert presence rather than
    // exact counts (resource sample timing is workload-sensitive).
    let count_kind = |k: EventKind| kinds.iter().filter(|&&v| v == k as i32).count();

    // Phase events: connect, stabilize, operate, silent, digest = 5
    // exact rows (matches the JSONL phase-sequence assertion in
    // `test_full_protocol_with_dummy`).
    assert_eq!(
        count_kind(EventKind::Phase),
        5,
        "compact parquet must contain one row per phase transition \
         (connect, stabilize, operate, silent, digest)"
    );

    // Connected event: exactly one row per spawn.
    assert_eq!(
        count_kind(EventKind::Connected),
        1,
        "compact parquet must contain one row per connect"
    );

    // EotSent: exactly one row.
    assert_eq!(
        count_kind(EventKind::EotSent),
        1,
        "compact parquet must contain one eot_sent row"
    );

    // Resource: variable count, but at least one (operate runs for
    // 1 s, sampler fires every 100 ms).
    assert!(
        count_kind(EventKind::Resource) >= 1,
        "compact parquet must contain at least one resource row, got {}",
        count_kind(EventKind::Resource)
    );

    // EotReceived / EotTimeout: VariantDummy in single-runner
    // self-loopback never emits these. Their absence is the
    // contract.
    assert_eq!(
        count_kind(EventKind::EotReceived),
        0,
        "VariantDummy self-loopback should not produce eot_received"
    );
    assert_eq!(
        count_kind(EventKind::EotTimeout),
        0,
        "VariantDummy self-loopback should not produce eot_timeout"
    );

    // ClockSync: reserved for E8; the driver does not yet emit it.
    assert_eq!(
        count_kind(EventKind::ClockSync),
        0,
        "clock_sync is reserved for E8; should be absent until then"
    );
}

/// T18.2 acceptance: the hard memory ceiling aborts the spawn when
/// the running buffer footprint exceeds the threshold. Setting an
/// absurdly low `--digest-mem-hard-mb` (1 MiB) and producing more than
/// 1 MiB of in-memory rows must produce an error from `run_protocol`.
#[test]
fn test_digest_mem_hard_ceiling_aborts_spawn() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    // Crank values_per_tick so the buffers overflow 1 MiB within a
    // sub-second operate phase: 1 MiB / ~32 bytes/row ~= 32 K rows.
    // 100 Hz x 1000 vpt x 1 s = 100K rows -> hard ceiling fires
    // well before operate_secs expires.
    args.tick_rate_hz = 100;
    args.operate_secs = 2;
    args.values_per_tick = 1000;
    args.digest_mem_soft_mb = 1;
    args.digest_mem_hard_mb = 1;

    let mut dummy = VariantDummy::new(&args.runner);
    let result = run_protocol(&mut dummy, &args);
    let err = result.expect_err("run_protocol must abort when hard ceiling is exceeded");
    let msg = format!("{err}");
    assert!(
        msg.contains("compact buffers exceeded hard ceiling"),
        "error message must identify the hard ceiling, got: {msg}"
    );
}

/// T18.2 acceptance: the file-size win. A realistic spawn (5 s
/// scalar-flood at 1000 Hz x 100 vpt = 500 K events) must produce a
/// Parquet file at least 10x smaller than the equivalent JSONL.
///
/// We run with `legacy_jsonl_events = true` to get both files in
/// the same spawn, then compare their on-disk sizes. The acceptance
/// criterion is 10x because conservative -- the target in the epic
/// is 30-50x, and we want to absorb scheduler jitter, the variant
/// dummy's worst-case behaviour, and any future overhead while still
/// catching regressions.
///
/// Smaller-than-realistic operate windows would not exercise the
/// compression payoff because the Parquet footer + KV metadata is a
/// fixed-cost overhead (~1.5 KiB). At 500 K rows the per-row cost
/// dominates and the ratio is stable.
#[test]
fn test_compact_parquet_at_least_10x_smaller_than_jsonl() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.tick_rate_hz = 1000;
    args.operate_secs = 2;
    args.silent_secs = 0;
    args.values_per_tick = 100;
    args.legacy_jsonl_events = true;
    // Disable resource sampling noise to keep the comparison clean.
    args.operate_idle_secs = 0;
    // Bump the ceilings well above what the run produces.
    args.digest_mem_soft_mb = 256;
    args.digest_mem_hard_mb = 512;

    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("protocol must complete");

    let jsonl_path = dir.path().join("dummy-test-runner-run01.jsonl");
    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");

    let jsonl_size = std::fs::metadata(&jsonl_path).unwrap().len();
    let parquet_size = std::fs::metadata(&parquet_path).unwrap().len();

    assert!(jsonl_size > 0);
    assert!(parquet_size > 0);
    let ratio = jsonl_size as f64 / parquet_size as f64;
    eprintln!("compact size win: jsonl={jsonl_size}B parquet={parquet_size}B ratio={ratio:.1}x");
    assert!(
        ratio >= 10.0,
        "T18.2 acceptance: parquet must be at least 10x smaller than JSONL; \
         got jsonl={jsonl_size}B parquet={parquet_size}B ratio={ratio:.1}x"
    );
}

// ---------------------------------------------------------------------------
// E19 / T19.2: workload-shape integration tests
//
// These tests use the workload factory + logger + compact buffer pipeline
// directly (bypassing `run_protocol`, which cannot yet receive the new
// workload params from the CLI -- that plumbing lands in T19.3). They
// validate the end-to-end emission path: WriteOp generation -> JSONL +
// compact-Parquet row materialisation -> file readback.
// ---------------------------------------------------------------------------

/// E19 / T19.2: block-flood end-to-end emission.
///
/// Construct a `BlockFlood` workload via `create_workload_with_params`,
/// generate one tick of WriteOps, push each into a fresh `Logger` and
/// `CompactBuffers`, then read the resulting JSONL back and confirm
/// every `write` line carries `leaf_count = 100, shape = "array"`. The
/// compact buffer columns must mirror the same metadata.
#[test]
fn test_block_flood_emits_array_shape_through_logger_and_compact() {
    use std::sync::{Arc, Mutex};
    use variant_base::compact::CompactBuffers;
    use variant_base::logger::Logger;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();

    let params = WorkloadParams {
        variant: "dummy".to_string(),
        run: "r1".to_string(),
        blob_size: Some(100),
        ..WorkloadParams::default()
    };
    let mut wl = create_workload_with_params("block-flood", &params).unwrap();
    let ops = wl.generate(1000);
    assert_eq!(ops.len(), 10, "1000 / 100 = 10 WriteOps");

    let mut logger = Logger::new(log_dir, "dummy", "r1", "block-flood-test").unwrap();
    let buffers = Arc::new(Mutex::new(CompactBuffers::new()));

    let ts = chrono::Utc::now();
    let mut seq = 0u64;
    for op in &ops {
        seq += 1;
        // Emit through the same paths the driver uses.
        logger
            .log_write_at(
                ts,
                seq,
                &op.path,
                Qos::BestEffort,
                op.payload.len(),
                op.leaf_count,
                op.shape,
            )
            .unwrap();
        buffers
            .lock()
            .unwrap()
            .push_write(
                ts.timestamp_nanos_opt().unwrap_or(0),
                &op.path,
                Qos::BestEffort.as_int(),
                seq,
                op.payload.len() as u32,
                op.leaf_count,
                op.shape.as_u8(),
            )
            .unwrap();
    }
    logger.flush().unwrap();

    // JSONL: every write line has leaf_count=100 and shape="array".
    let path = dir.path().join("dummy-r1-block-flood-test.jsonl");
    let file = std::fs::File::open(&path).unwrap();
    let lines: Vec<serde_json::Value> = std::io::BufReader::new(file)
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    assert_eq!(lines.len(), 10);
    for (i, v) in lines.iter().enumerate() {
        assert_eq!(v["event"], "write");
        assert_eq!(v["leaf_count"], 100, "line {i} leaf_count");
        assert_eq!(v["shape"], "array", "line {i} shape");
    }

    // Compact buffer columns: all leaf_count=Some(100), shape_idx=Some(1).
    let buf = buffers.lock().unwrap();
    assert_eq!(buf.len(), 10);
    for i in 0..buf.len() {
        assert_eq!(buf.leaf_count[i], Some(100));
        assert_eq!(buf.shape_idx[i], Some(1));
    }
}

/// E19 / T19.2: mixed-types end-to-end emission.
///
/// Generate one tick of mixed-types WriteOps, push each through the
/// logger + compact pipeline, then validate the JSONL contains rows of
/// scalar / array / struct shapes and the leaf_count values still sum
/// to exactly vpt.
#[test]
fn test_mixed_types_emits_heterogeneous_shapes_through_logger() {
    use std::sync::{Arc, Mutex};
    use variant_base::compact::CompactBuffers;
    use variant_base::logger::Logger;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();

    let params = WorkloadParams {
        variant: "dummy".to_string(),
        run: "r1".to_string(),
        mixed_scalars_min: Some(10),
        mixed_scalars_max: Some(50),
        mixed_arrays_min: Some(0),
        mixed_arrays_max: Some(100),
        mixed_dict_split_max: Some(4),
        workload_seed: Some(0xABCD_1234),
        ..WorkloadParams::default()
    };
    let mut wl = create_workload_with_params("mixed-types", &params).unwrap();
    let ops = wl.generate(1000);

    let total_leaves: u32 = ops.iter().map(|o| o.leaf_count).sum();
    assert_eq!(total_leaves, 1000, "sum of leaf_count must equal vpt");

    let mut logger = Logger::new(log_dir, "dummy", "r1", "mixed-types-test").unwrap();
    let buffers = Arc::new(Mutex::new(CompactBuffers::new()));

    let ts = chrono::Utc::now();
    let mut seq = 0u64;
    for op in &ops {
        seq += 1;
        logger
            .log_write_at(
                ts,
                seq,
                &op.path,
                Qos::BestEffort,
                op.payload.len(),
                op.leaf_count,
                op.shape,
            )
            .unwrap();
        buffers
            .lock()
            .unwrap()
            .push_write(
                ts.timestamp_nanos_opt().unwrap_or(0),
                &op.path,
                Qos::BestEffort.as_int(),
                seq,
                op.payload.len() as u32,
                op.leaf_count,
                op.shape.as_u8(),
            )
            .unwrap();
    }
    logger.flush().unwrap();

    // JSONL: the leaf_count values must sum to 1000 and the shape
    // strings must include at least scalar / array / struct.
    let path = dir.path().join("dummy-r1-mixed-types-test.jsonl");
    let file = std::fs::File::open(&path).unwrap();
    let lines: Vec<serde_json::Value> = std::io::BufReader::new(file)
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    let sum_leaf_count: u64 = lines
        .iter()
        .map(|v| v["leaf_count"].as_u64().unwrap())
        .sum();
    assert_eq!(sum_leaf_count, 1000);
    let shapes: std::collections::HashSet<&str> =
        lines.iter().map(|v| v["shape"].as_str().unwrap()).collect();
    assert!(
        shapes.contains("scalar"),
        "mixed-types must produce at least one scalar shape; got {shapes:?}"
    );
    // At least one non-scalar shape too -- under the chosen params
    // the seeded generator emits arrays and/or structs.
    let non_scalar: usize = shapes.iter().filter(|s| **s != "scalar").count();
    assert!(
        non_scalar >= 1,
        "mixed-types must produce at least one non-scalar shape; got {shapes:?}"
    );
}

/// E19 / T19.2 acceptance: the dummy binary integration test the spec
/// names runs `block-flood vpt=1000 blob_size=100` through the variant
/// CLI. Since the CLI plumbing for `--blob-size` is owned by T19.3,
/// this test instead drives `run_protocol` directly against
/// `VariantDummy` with a workload name that the T19.2 driver
/// recognises but cannot construct (no `blob_size` plumbing yet) --
/// and verifies the driver returns a descriptive Err that names the
/// missing argument. This is the load-bearing acceptance check that
/// T19.2 wired the workload factory through the driver correctly;
/// T19.3 will replace this with a positive-path acceptance test once
/// the CLI arg lands.
#[test]
fn test_block_flood_through_driver_errors_until_t19_3_lands() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.workload = "block-flood".to_string();
    let mut dummy = VariantDummy::new(&args.runner);
    let result = run_protocol(&mut dummy, &args);
    let err = match result {
        Err(e) => e,
        Ok(()) => panic!("expected block-flood without --blob-size to Err"),
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("blob-size") || msg.contains("blob_size"),
        "error must name the missing arg; got: {msg}"
    );
}

/// Same for mixed-types: until T19.3 wires the CLI args, the driver
/// must Err with a descriptive message naming the first missing
/// mixed-* parameter.
#[test]
fn test_mixed_types_through_driver_errors_until_t19_3_lands() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.workload = "mixed-types".to_string();
    let mut dummy = VariantDummy::new(&args.runner);
    let result = run_protocol(&mut dummy, &args);
    let err = match result {
        Err(e) => e,
        Ok(()) => panic!("expected mixed-types without --mixed-* args to Err"),
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("mixed-scalars-min")
            || msg.contains("mixed_scalars_min")
            || msg.contains("mixed-types requires"),
        "error must mention the missing mixed-types arg; got: {msg}"
    );
}

/// E19 / T19.2: scalar-flood end-to-end via `run_protocol` still emits
/// `leaf_count = 1, shape = "scalar"` on every write line. This is the
/// "no regression" smoke test guaranteed by the E19 acceptance: existing
/// scalar-flood spawns add the two new fields with their default values
/// and remain otherwise unchanged.
#[test]
fn test_scalar_flood_through_driver_emits_scalar_leaf_count_and_shape() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let args = test_args(log_dir);
    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("scalar-flood spawn must complete");

    let log_path = dir.path().join("dummy-test-runner-run01.jsonl");
    let file = std::fs::File::open(&log_path).unwrap();
    let lines: Vec<serde_json::Value> = std::io::BufReader::new(file)
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    let writes: Vec<&serde_json::Value> = lines.iter().filter(|l| l["event"] == "write").collect();
    assert!(!writes.is_empty(), "scalar-flood produces write events");
    for (i, w) in writes.iter().enumerate() {
        assert_eq!(w["leaf_count"], 1, "scalar-flood write {i} leaf_count");
        assert_eq!(w["shape"], "scalar", "scalar-flood write {i} shape");
    }
}

/// E19 / T19.2: scalar-flood through `run_protocol` also writes
/// `leaf_count = 1, shape_idx = 0` on every compact `write` row. This
/// pairs with the JSONL assertion above to lock in the
/// no-regression contract end-to-end.
#[test]
fn test_scalar_flood_through_driver_emits_scalar_columns_in_parquet() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let args = test_args(log_dir);
    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("scalar-flood spawn must complete");

    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
    let rows: Vec<_> = reader
        .get_row_iter(None)
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    // Filter to write rows only (kind column index 1, EventKind::Write = 0).
    let mut write_rows = 0usize;
    for row in &rows {
        if row.get_int(1).unwrap() == 0 {
            // leaf_count at col 11, shape_idx at col 12.
            assert_eq!(row.get_int(11).unwrap(), 1, "scalar-flood leaf_count");
            assert_eq!(row.get_int(12).unwrap(), 0, "scalar-flood shape_idx");
            write_rows += 1;
        }
    }
    assert!(write_rows > 0, "expected at least one write row");
}

/// Use the unused `WriteShape` import to silence the rustc dead-code
/// warning -- this asserts the canonical strings round-trip through
/// the Logger's emit path.
#[test]
fn test_write_shape_string_roundtrip_through_logger() {
    use variant_base::logger::Logger;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut logger = Logger::new(log_dir, "v", "r", "shape-roundtrip").unwrap();
    for shape in [WriteShape::Scalar, WriteShape::Array, WriteShape::Struct] {
        logger
            .log_write_at(
                chrono::Utc::now(),
                shape.as_u8() as u64,
                "/p",
                Qos::BestEffort,
                8,
                3,
                shape,
            )
            .unwrap();
    }
    logger.flush().unwrap();
    let path = dir.path().join("v-r-shape-roundtrip.jsonl");
    let file = std::fs::File::open(&path).unwrap();
    let lines: Vec<serde_json::Value> = std::io::BufReader::new(file)
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    assert_eq!(lines[0]["shape"], "scalar");
    assert_eq!(lines[1]["shape"], "array");
    assert_eq!(lines[2]["shape"], "struct");
}
