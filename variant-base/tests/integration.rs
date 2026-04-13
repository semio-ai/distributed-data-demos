use std::io::BufRead;
use std::process::Command;

use tempfile::TempDir;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;
use variant_base::dummy::VariantDummy;

/// Build CLI args for a short test run.
fn test_args(log_dir: &str) -> CliArgs {
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
        extra: vec![],
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

    assert_eq!(phase_events.len(), 4, "should have 4 phase events");
    assert_eq!(phase_events[0].0, "connect");
    assert_eq!(phase_events[1].0, "stabilize");
    assert_eq!(phase_events[2].0, "operate");
    assert_eq!(
        phase_events[2].1,
        Some("scalar-flood"),
        "operate phase should include workload profile"
    );
    assert_eq!(phase_events[3].0, "silent");

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
