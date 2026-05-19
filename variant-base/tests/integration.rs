use std::io::BufRead;
use std::process::Command;

use tempfile::TempDir;

use variant_base::cli::{CliArgs, DEFAULT_RECV_BUFFER_KB};
use variant_base::driver::run_protocol;
use variant_base::dummy::VariantDummy;
use variant_base::types::{Qos, ThreadingMode};
use variant_base::workload::{create_workload_with_params, WorkloadParams};

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
        // Tests that exercise the soft / hard ceiling thresholds
        // build their own args from scratch.
        digest_mem_soft_mb: variant_base::cli::DEFAULT_DIGEST_MEM_SOFT_MB,
        digest_mem_hard_mb: variant_base::cli::DEFAULT_DIGEST_MEM_HARD_MB,
        // E19 / T19.3: the integration-test default exercises
        // `scalar-flood`, which ignores all workload-shape args.
        // Tests that need to exercise `block-flood` / `mixed-types`
        // build their own args (see the dedicated E19 tests below).
        blob_size: None,
        mixed_scalars_min: None,
        mixed_scalars_max: None,
        mixed_arrays_min: None,
        mixed_arrays_max: None,
        mixed_dict_split_max: None,
        workload_seed: None,
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
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

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

    // Post-T19.10: the JSONL stream carries lifecycle events only --
    // no per-event rows.
    for line in &lines {
        let event = line["event"].as_str().unwrap();
        assert!(
            !matches!(
                event,
                "write" | "receive" | "backpressure_skipped" | "gap_detected" | "gap_filled"
            ),
            "post-T19.10: per-event '{event}' must not appear in JSONL"
        );
    }

    // Collect event types in order.
    let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();

    // Phase events must appear in order: connect, stabilize, operate, silent, digest.
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

    // Resource events should exist (at least one during the operate phase).
    let resource_count = events.iter().filter(|&&e| e == "resource").count();
    assert!(
        resource_count > 0,
        "should have at least one resource event"
    );

    // -- Compact-Parquet inspection (per-event observations live here
    //    exclusively post-T19.10) --
    let parquet_path =
        std::path::Path::new(log_dir).join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
    let rows: Vec<_> = reader
        .get_row_iter(None)
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();

    // Collect write seqs (column 2 = `seq`, column 1 = `kind`).
    let mut write_seqs: Vec<i64> = rows
        .iter()
        .filter(|r| r.get_int(1).unwrap() == EventKind::Write as i32)
        .map(|r| r.get_long(2).unwrap())
        .collect();
    assert!(
        !write_seqs.is_empty(),
        "compact parquet should have at least one write row"
    );
    // Driver hands seqs in monotonic order on each push; sanity check.
    let original = write_seqs.clone();
    write_seqs.sort_unstable();
    assert_eq!(
        original, write_seqs,
        "write seq numbers should be monotonically increasing in append order"
    );
    for window in write_seqs.windows(2) {
        assert!(
            window[1] > window[0],
            "write seq numbers should be strictly increasing: {} -> {}",
            window[0],
            window[1]
        );
    }

    // Receive rows: should exist for each write (dummy echoes).
    let receive_count = rows
        .iter()
        .filter(|r| r.get_int(1).unwrap() == EventKind::Receive as i32)
        .count();
    assert_eq!(
        receive_count,
        write_seqs.len(),
        "every write should have a matching receive (dummy echoes)"
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
/// verify the expected lifecycle JSONL sequence + per-event
/// compact-Parquet rows are produced for both (T14.1 integration
/// acceptance, updated for T19.10).
#[test]
fn test_variant_dummy_runs_in_both_threading_modes() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

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

        // The dummy echoes every publish; compact-Parquet must have a
        // write and a matching receive row for each op.
        let parquet_path =
            std::path::Path::new(log_dir).join("dummy-test-runner-run01.compact.parquet");
        let reader =
            SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
        let kinds: Vec<i32> = reader
            .get_row_iter(None)
            .unwrap()
            .flatten()
            .map(|r| r.get_int(1).unwrap())
            .collect();
        let writes = kinds
            .iter()
            .filter(|&&k| k == EventKind::Write as i32)
            .count();
        let receives = kinds
            .iter()
            .filter(|&&k| k == EventKind::Receive as i32)
            .count();
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
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

    for qos in 1u8..=4u8 {
        let dir = TempDir::new().unwrap();
        let log_dir = dir.path().to_str().unwrap();
        let mut args = test_args(log_dir);
        args.qos = qos;

        let mut dummy = VariantDummy::new(&args.runner);
        run_protocol(&mut dummy, &args)
            .unwrap_or_else(|e| panic!("protocol completes at QoS {qos}: {e}"));

        // Per-event observations are compact-Parquet only post-T19.10.
        let parquet_path =
            std::path::Path::new(log_dir).join("dummy-test-runner-run01.compact.parquet");
        let reader =
            SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
        let rows: Vec<_> = reader
            .get_row_iter(None)
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        let mut writes = 0usize;
        let mut receives = 0usize;
        let mut skipped = 0usize;
        for row in &rows {
            let kind = row.get_int(1).unwrap();
            if kind == EventKind::Write as i32 {
                writes += 1;
                // Column 5 = qos.
                assert_eq!(
                    row.get_int(5).unwrap() as u8,
                    qos,
                    "QoS {qos}: write row's qos column must match requested level"
                );
            } else if kind == EventKind::Receive as i32 {
                receives += 1;
            } else if kind == EventKind::BackpressureSkipped as i32 {
                skipped += 1;
            }
        }

        assert!(writes > 0, "QoS {qos}: expected at least one write");
        assert_eq!(
            writes, receives,
            "QoS {qos}: writes should match receives (dummy echoes)"
        );
        assert_eq!(
            skipped, 0,
            "QoS {qos}: VariantDummy never reports backpressure"
        );
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
/// alongside the lifecycle JSONL, and the file must contain per-event
/// rows for every observation the run produced. The schema must match
/// the documented 13-column layout.
#[test]
fn test_compact_parquet_is_written_alongside_jsonl() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let args = test_args(log_dir);

    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("protocol should complete");

    // The lifecycle JSONL file must exist (post-T19.10 it carries only
    // phase / connected / eot_* / resource lines).
    let jsonl_path = dir.path().join("dummy-test-runner-run01.jsonl");
    assert!(jsonl_path.exists(), "lifecycle JSONL file must be written");

    // The compact parquet file MUST exist alongside the JSONL.
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

    // VariantDummy echoes every write -> at least one row per write.
    assert!(meta.file_metadata().num_rows() > 0);
}

/// T19.10 acceptance: post-cleanup, per-event JSONL lines (`write`,
/// `receive`, `backpressure_skipped`, `gap_*`) MUST NOT appear in the
/// JSONL stream regardless of any flag. Lifecycle events (`phase`,
/// `connected`, `eot_sent`, `resource`) MUST still be present, and the
/// compact Parquet file MUST contain the full per-event row set.
#[test]
fn test_per_event_rows_are_compact_parquet_only_post_t19_10() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let args = test_args(log_dir);

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

    // Per-event rows must be absent under any configuration.
    for event in [
        "write",
        "receive",
        "backpressure_skipped",
        "gap_detected",
        "gap_filled",
    ] {
        assert!(
            !events.contains(&event),
            "event '{event}' must NOT appear in JSONL post-T19.10"
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

    // The compact Parquet file must contain the per-event rows.
    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
    assert!(
        reader.metadata().file_metadata().num_rows() > 0,
        "compact Parquet file must accumulate rows post-T19.10"
    );
}

/// T18.2b acceptance: every lifecycle event the JSONL stream emits
/// MUST also appear as a row in the compact `compact_events` table.
///
/// Cross-check that the compact parquet contains a row for each
/// lifecycle kind the analyzer's pipeline depends on (`phase`,
/// `connected`, `eot_sent`, `resource`). Post-T19.10 the JSONL stream
/// carries lifecycle events only; per-event observations live
/// exclusively in compact-Parquet.
#[test]
fn test_compact_parquet_contains_lifecycle_events_mirrored_from_jsonl() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let args = test_args(log_dir);
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

// (T19.10) The previous `test_compact_parquet_at_least_10x_smaller_than_jsonl`
// acceptance test was removed: it relied on the dual-emission path
// (`--legacy-jsonl-events` ON) to produce both files in the same spawn
// for an on-disk size comparison. With per-event JSONL deleted, the
// JSONL stream now contains only lifecycle events and the size-ratio
// metric is no longer meaningful. The compact-Parquet file remains
// the sole per-event log; its size win is no longer measured by a
// JSONL baseline.

// ---------------------------------------------------------------------------
// E19 / T19.2: workload-shape integration tests
//
// These tests use the workload factory + compact buffer pipeline
// directly (bypassing `run_protocol`). They validate the end-to-end
// emission path: WriteOp generation -> compact-Parquet row
// materialisation. Post-T19.10 there is no JSONL byproduct on the
// per-event path.
// ---------------------------------------------------------------------------

/// E19 / T19.2: block-flood compact-buffer emission.
///
/// Construct a `BlockFlood` workload via `create_workload_with_params`,
/// generate one tick of WriteOps, push each into a fresh
/// `CompactBuffers`, then read the column data back and confirm every
/// write row carries `leaf_count = 100, shape_idx = 1 (array)`.
#[test]
fn test_block_flood_emits_array_shape_through_compact_buffer() {
    use std::sync::{Arc, Mutex};
    use variant_base::compact::CompactBuffers;

    let params = WorkloadParams {
        variant: "dummy".to_string(),
        run: "r1".to_string(),
        blob_size: Some(100),
        ..WorkloadParams::default()
    };
    let mut wl = create_workload_with_params("block-flood", &params).unwrap();
    let ops = wl.generate(1000);
    assert_eq!(ops.len(), 10, "1000 / 100 = 10 WriteOps");

    let buffers = Arc::new(Mutex::new(CompactBuffers::new()));
    let ts = chrono::Utc::now();
    let mut seq = 0u64;
    for op in &ops {
        seq += 1;
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

    // Compact buffer columns: all leaf_count=Some(100), shape_idx=Some(1).
    let buf = buffers.lock().unwrap();
    assert_eq!(buf.len(), 10);
    for i in 0..buf.len() {
        assert_eq!(buf.leaf_count[i], Some(100));
        assert_eq!(buf.shape_idx[i], Some(1));
    }
}

/// E19 / T19.2: mixed-types compact-buffer emission.
///
/// Generate one tick of mixed-types WriteOps, push each through the
/// compact pipeline, then validate the row count, leaf_count total,
/// and that shapes include at least scalar and one non-scalar variant.
#[test]
fn test_mixed_types_emits_heterogeneous_shapes_through_compact_buffer() {
    use std::sync::{Arc, Mutex};
    use variant_base::compact::CompactBuffers;

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

    let buffers = Arc::new(Mutex::new(CompactBuffers::new()));
    let ts = chrono::Utc::now();
    let mut seq = 0u64;
    for op in &ops {
        seq += 1;
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

    let buf = buffers.lock().unwrap();
    assert_eq!(buf.len(), ops.len());

    // Sum of leaf_count across all rows must equal 1000.
    let sum_leaf_count: u32 = buf
        .leaf_count
        .iter()
        .map(|v| v.expect("write rows carry leaf_count"))
        .sum();
    assert_eq!(sum_leaf_count, 1000);

    // Shape diversity: at least one scalar (shape_idx == 0) AND at
    // least one non-scalar (shape_idx > 0). The intern dictionary uses
    // 0 = scalar, 1 = array, 2 = struct.
    let shape_indices: std::collections::HashSet<u8> = buf
        .shape_idx
        .iter()
        .map(|v| v.expect("write rows carry shape_idx"))
        .collect();
    assert!(
        shape_indices.contains(&0),
        "mixed-types must produce at least one scalar (shape_idx=0); got {shape_indices:?}"
    );
    assert!(
        shape_indices.iter().any(|&i| i > 0),
        "mixed-types must produce at least one non-scalar shape; got {shape_indices:?}"
    );
}

/// E19 / T19.3: a `block-flood` spawn that omits `--blob-size`
/// transparently defaults to `--blob-size 100` (per the locked spec),
/// so a `values_per_tick` divisible by 100 must complete successfully
/// without an explicit `--blob-size`. The previous T19.2-era test
/// (`*_through_driver_errors_until_t19_3_lands`) asserted that omitting
/// `--blob-size` produces an Err; T19.3's blob-size default flipped
/// that contract, so this test now asserts the positive path.
#[test]
fn test_block_flood_through_driver_defaults_blob_size_to_100() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.workload = "block-flood".to_string();
    // vpt=100 with the default blob_size=100 produces exactly one
    // block-shaped WriteOp per tick, the smallest valid block-flood
    // workload that exercises the default.
    args.values_per_tick = 100;
    // Leave args.blob_size = None to exercise the default.
    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("block-flood with default blob_size must complete");

    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
    let mut write_rows = 0usize;
    for row in reader.get_row_iter(None).unwrap().flatten() {
        if row.get_int(1).unwrap() == EventKind::Write as i32 {
            // Column 11 = leaf_count, column 12 = shape_idx (1 = array).
            assert_eq!(row.get_int(11).unwrap(), 100, "block-flood leaf_count");
            assert_eq!(row.get_int(12).unwrap(), 1, "block-flood shape_idx=array");
            write_rows += 1;
        }
    }
    assert!(
        write_rows > 0,
        "block-flood produces write rows under the default blob_size"
    );
}

/// E19 / T19.3: a `mixed-types` spawn that omits any of the five
/// required `--mixed-*` args is rejected at startup with a descriptive
/// Err naming the missing argument. Inversion of the T19.2-era
/// `*_through_driver_errors_until_t19_3_lands` test; the error
/// message is now owned by the driver's `validate_and_build_workload_params`
/// rather than the workload factory.
#[test]
fn test_mixed_types_without_required_args_is_rejected_at_startup() {
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
        msg.contains("mixed-types requires") && msg.contains("--mixed-scalars-min"),
        "error must mention the missing mixed-types arg; got: {msg}"
    );
    // Rejection happens before any phase event lands.
    let log_path = dir.path().join("dummy-test-runner-run01.jsonl");
    assert!(
        !log_path.exists(),
        "rejection must happen before logger creates the JSONL file"
    );
}

/// E19 / T19.3 acceptance: a full `block-flood vpt=1000 blob_size=100`
/// spawn completes through `run_protocol` and emits compact-Parquet
/// write rows carrying `leaf_count = 100, shape_idx = 1 (array)`.
/// Pairs the "block-flood validation passes" assertion with the
/// smoke-test requirement from the task spec.
#[test]
fn test_block_flood_through_driver_with_explicit_blob_size_completes() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.workload = "block-flood".to_string();
    args.values_per_tick = 1000;
    args.blob_size = Some(100);
    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("block-flood vpt=1000 blob_size=100 must complete");

    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
    let mut write_rows = 0usize;
    for row in reader.get_row_iter(None).unwrap().flatten() {
        if row.get_int(1).unwrap() == EventKind::Write as i32 {
            assert_eq!(row.get_int(11).unwrap(), 100, "block-flood leaf_count");
            assert_eq!(row.get_int(12).unwrap(), 1, "block-flood shape_idx=array");
            write_rows += 1;
        }
    }
    assert!(write_rows > 0, "block-flood produces write rows");
}

/// E19 / T19.3 acceptance: a full `mixed-types` spawn with sensible
/// defaults completes through `run_protocol` and emits compact-Parquet
/// write rows whose leaf_count values sum to a positive multiple of
/// `values_per_tick` and whose shape_idx values include at least two
/// of the three categories (scalar=0, array=1, struct=2).
#[test]
fn test_mixed_types_through_driver_with_sensible_defaults_completes() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use variant_base::compact::EventKind;

    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.workload = "mixed-types".to_string();
    args.values_per_tick = 100;
    args.mixed_scalars_min = Some(5);
    args.mixed_scalars_max = Some(20);
    args.mixed_arrays_min = Some(5);
    args.mixed_arrays_max = Some(40);
    args.mixed_dict_split_max = Some(4);
    args.workload_seed = Some(0xCAFE_BABE);

    let mut dummy = VariantDummy::new(&args.runner);
    run_protocol(&mut dummy, &args).expect("mixed-types with sensible defaults must complete");

    let parquet_path = dir.path().join("dummy-test-runner-run01.compact.parquet");
    let reader = SerializedFileReader::new(std::fs::File::open(&parquet_path).unwrap()).unwrap();
    let mut total_leaves: i64 = 0;
    let mut shapes: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let mut write_rows = 0usize;
    for row in reader.get_row_iter(None).unwrap().flatten() {
        if row.get_int(1).unwrap() == EventKind::Write as i32 {
            total_leaves += row.get_int(11).unwrap() as i64;
            shapes.insert(row.get_int(12).unwrap());
            write_rows += 1;
        }
    }
    assert!(write_rows > 0, "mixed-types produces write rows");

    // Sum of leaf_count must be a positive multiple of vpt.
    assert!(total_leaves > 0);
    assert_eq!(
        total_leaves % (args.values_per_tick as i64),
        0,
        "total leaves ({total_leaves}) must be a multiple of vpt ({})",
        args.values_per_tick
    );

    // Shapes diversity: at least two distinct shape indices appear.
    assert!(
        shapes.len() >= 2,
        "mixed-types must produce more than one shape; got {shapes:?}"
    );
}

/// E19 / T19.3: block-flood with `vpt % blob_size != 0` is rejected at
/// startup. The error must name both numbers and the divisibility
/// constraint so operators can fix their config without consulting the
/// contract doc.
#[test]
fn test_block_flood_indivisible_blob_size_is_rejected_at_startup() {
    let dir = TempDir::new().unwrap();
    let log_dir = dir.path().to_str().unwrap();
    let mut args = test_args(log_dir);
    args.workload = "block-flood".to_string();
    args.values_per_tick = 1000;
    args.blob_size = Some(300);
    let mut dummy = VariantDummy::new(&args.runner);
    let err = run_protocol(&mut dummy, &args)
        .expect_err("block-flood vpt=1000 blob_size=300 must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("divisible"),
        "error must explain divisibility; got: {msg}"
    );
    let log_path = dir.path().join("dummy-test-runner-run01.jsonl");
    assert!(
        !log_path.exists(),
        "rejection must happen before logger creates the JSONL file"
    );
}

/// E19 / T19.2: scalar-flood through `run_protocol` writes
/// `leaf_count = 1, shape_idx = 0` on every compact `write` row. This
/// is the "no regression" smoke test guaranteed by the E19 acceptance:
/// existing scalar-flood spawns add the two new fields with their
/// default values and remain otherwise unchanged.
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

// (T19.10) Removed `test_write_shape_string_roundtrip_through_logger`:
// it asserted on per-event JSONL write lines produced by
// `Logger::log_write_at`, which no longer exists. Compact-Parquet
// shape_idx round-tripping is covered by the block-flood /
// mixed-types / scalar-flood Parquet tests above.
