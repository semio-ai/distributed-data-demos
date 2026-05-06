use std::path::Path;
use std::process::Command;

/// Path to the runner binary built by cargo.
fn runner_binary() -> String {
    let path = env!("CARGO_BIN_EXE_runner");
    assert!(
        Path::new(path).exists(),
        "runner binary not found at {path}"
    );
    path.to_string()
}

/// Path to the sleeper test helper binary.
fn sleeper_binary() -> String {
    let path = env!("CARGO_BIN_EXE_sleeper");
    assert!(
        Path::new(path).exists(),
        "sleeper binary not found at {path}"
    );
    path.to_string()
}

/// Path to the arg-echo test helper binary.
fn arg_echo_binary() -> String {
    let path = env!("CARGO_BIN_EXE_arg-echo");
    assert!(
        Path::new(path).exists(),
        "arg-echo binary not found at {path}"
    );
    path.to_string()
}

fn variant_dummy_exists() -> bool {
    Path::new("../target/release/variant-dummy.exe").exists()
}

#[test]
fn single_runner_lifecycle() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/single-runner.toml")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "runner should exit 0, got {:?}\nstderr: {stderr}",
        output.status.code()
    );

    assert!(
        stdout.contains("Benchmark run: test01"),
        "should contain run id"
    );
    assert!(stdout.contains("dummy"), "should contain variant name");
    assert!(stdout.contains("success"), "should show success");

    // Verify JSONL log file was produced inside a timestamped subfolder.
    if log_dir.exists() {
        // The runner now creates a <run>-<YYYYMMDD_HHMMSS> subfolder.
        let subdirs: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        assert!(
            !subdirs.is_empty(),
            "expected a timestamped subfolder in {log_dir:?}"
        );

        let jsonl_count: usize = subdirs
            .iter()
            .flat_map(|d| std::fs::read_dir(d.path()).unwrap())
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .count();
        assert!(
            jsonl_count > 0,
            "expected at least one .jsonl file in timestamped subfolder"
        );
    }

    let _ = std::fs::remove_dir_all(&log_dir);
}

#[test]
fn config_validation_rejects_bad_name() {
    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("nonexistent")
        .arg("--config")
        .arg("tests/fixtures/bad-name.toml")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    assert!(
        !output.status.success(),
        "runner should exit non-zero for bad name"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not in the config runners list"),
        "error should mention runner name not in list, got: {stderr}"
    );
}

#[test]
fn multi_variant_sequential_execution() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-multi");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/multi-variant.toml")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "runner should exit 0, got {:?}\nstderr: {stderr}",
        output.status.code()
    );

    assert!(stdout.contains("dummy-a"), "should contain dummy-a");
    assert!(stdout.contains("dummy-b"), "should contain dummy-b");
    assert!(
        stdout.contains("Benchmark run: test02"),
        "should contain run id"
    );

    if log_dir.exists() {
        // The runner now creates a <run>-<YYYYMMDD_HHMMSS> subfolder.
        let subdirs: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        assert!(
            !subdirs.is_empty(),
            "expected a timestamped subfolder in {log_dir:?}"
        );

        let jsonl_count: usize = subdirs
            .iter()
            .flat_map(|d| std::fs::read_dir(d.path()).unwrap())
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .count();
        assert!(
            jsonl_count >= 2,
            "expected at least 2 .jsonl files, got {jsonl_count}"
        );
    }

    let _ = std::fs::remove_dir_all(&log_dir);
}

#[test]
fn qos_array_produces_per_qos_log_files() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-qos-array");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/qos-array.toml")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "runner should exit 0, got {:?}\nstderr: {stderr}",
        output.status.code()
    );

    // Summary should mention both -qos1 and -qos2 spawn names.
    assert!(
        stdout.contains("dummy-qos1"),
        "summary should contain dummy-qos1, got:\n{stdout}"
    );
    assert!(
        stdout.contains("dummy-qos2"),
        "summary should contain dummy-qos2, got:\n{stdout}"
    );

    // Find the JSONL files in the timestamped subfolder.
    assert!(log_dir.exists(), "log dir should exist");
    let subdirs: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(
        subdirs.len(),
        1,
        "expected exactly one timestamped subfolder"
    );

    let jsonl_files: Vec<_> = std::fs::read_dir(subdirs[0].path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert_eq!(
        jsonl_files.len(),
        2,
        "expected 2 JSONL files (one per QoS), got {jsonl_files:?}"
    );

    // Verify naming: each file's basename contains the spawn name suffix.
    let has_qos1 = jsonl_files.iter().any(|f| f.contains("dummy-qos1"));
    let has_qos2 = jsonl_files.iter().any(|f| f.contains("dummy-qos2"));
    assert!(
        has_qos1 && has_qos2,
        "expected both dummy-qos1 and dummy-qos2 log files, got {jsonl_files:?}"
    );

    // Spot-check the qos field inside each log.
    for file in &jsonl_files {
        let contents = std::fs::read_to_string(subdirs[0].path().join(file)).unwrap();
        let expected_qos = if file.contains("dummy-qos1") { 1 } else { 2 };
        // Find any line with a "qos" field; verify it matches expected.
        let mut found_qos_record = false;
        for line in contents.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(qos) = v.get("qos").and_then(|q| q.as_i64()) {
                assert_eq!(
                    qos, expected_qos,
                    "file {file} has qos={qos}, expected {expected_qos}"
                );
                found_qos_record = true;
            }
        }
        assert!(
            found_qos_record,
            "file {file} should contain at least one record with a qos field"
        );
    }

    let _ = std::fs::remove_dir_all(&log_dir);
}

#[test]
fn qos_omitted_produces_four_log_files() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-qos-omitted");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/qos-omitted.toml")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "runner should exit 0, got {:?}\nstderr: {stderr}",
        output.status.code()
    );

    for q in 1..=4 {
        let needle = format!("dummy-qos{q}");
        assert!(
            stdout.contains(&needle),
            "summary should contain {needle}, got:\n{stdout}"
        );
    }

    assert!(log_dir.exists(), "log dir should exist");
    let subdirs: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(
        subdirs.len(),
        1,
        "expected exactly one timestamped subfolder"
    );

    let jsonl_files: Vec<String> = std::fs::read_dir(subdirs[0].path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert_eq!(
        jsonl_files.len(),
        4,
        "expected 4 JSONL files (one per QoS), got {jsonl_files:?}"
    );

    for q in 1..=4 {
        let needle = format!("dummy-qos{q}");
        assert!(
            jsonl_files.iter().any(|f| f.contains(&needle)),
            "expected file containing {needle}, got {jsonl_files:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&log_dir);
}

#[test]
fn single_runner_injects_peers_arg_with_self_loopback() {
    // Use arg-echo as the variant binary so we can inspect the exact CLI args
    // the runner constructed. Verifies the contract: --peers <self>=127.0.0.1
    // appears even in single-runner mode (no actual peers).
    let arg_echo = arg_echo_binary();
    let tmp_dir = std::env::temp_dir().join("runner_peers_inject_test");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();

    let out_path = tmp_dir.join("captured-args.json");
    let log_dir = tmp_dir.join("logs");

    let arg_echo_escaped = arg_echo.replace('\\', "/");
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");
    let config_content = format!(
        r#"run = "peerinj"
runners = ["self"]
default_timeout_secs = 10

[[variant]]
name = "echo"
binary = "{arg_echo_escaped}"
timeout_secs = 5

  [variant.common]
  tick_rate_hz = 1
  stabilize_secs = 0
  operate_secs = 0
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 1
  qos = 1
  log_dir = "{log_dir_escaped}"

  [variant.specific]
"#
    );
    let config_path = tmp_dir.join("peers-inject.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("self")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("ARG_ECHO_OUT", out_path.to_str().unwrap())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");
    assert!(output.status.success(), "runner should exit 0");

    let captured_json = std::fs::read_to_string(&out_path).expect("captured args file");
    let captured: Vec<String> = serde_json::from_str(&captured_json).unwrap();

    // Locate --peers and check the value.
    let peers_idx = captured
        .iter()
        .position(|a| a == "--peers")
        .expect("expected --peers in captured args");
    let peers_val = &captured[peers_idx + 1];
    assert_eq!(
        peers_val, "self=127.0.0.1",
        "expected single-runner --peers self=127.0.0.1, got {peers_val}"
    );

    // Also verify --variant uses the original name (single-QoS, no suffix).
    let variant_idx = captured
        .iter()
        .position(|a| a == "--variant")
        .expect("expected --variant in captured args");
    assert_eq!(captured[variant_idx + 1], "echo");

    // --qos should be the runner-injected concrete level.
    let qos_idx = captured
        .iter()
        .position(|a| a == "--qos")
        .expect("expected --qos in captured args");
    assert_eq!(captured[qos_idx + 1], "1");

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn timeout_handling() {
    // Create a config that points at the sleeper binary (which ignores args and sleeps forever).
    let sleeper = sleeper_binary();
    let tmp_dir = std::env::temp_dir().join("runner_timeout_test");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let config_path = tmp_dir.join("timeout.toml");

    // Use forward-slash path to avoid TOML escape issues.
    let sleeper_escaped = sleeper.replace('\\', "/");

    let config_content = format!(
        r#"run = "timeout-run"
runners = ["local"]
default_timeout_secs = 3

[[variant]]
name = "hangs"
binary = "{sleeper_escaped}"
timeout_secs = 3

  [variant.common]

  [variant.specific]
"#
    );
    std::fs::write(&config_path, &config_content).unwrap();

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    // Runner should exit non-zero because the variant timed out.
    assert!(
        !output.status.success(),
        "runner should exit non-zero on timeout"
    );

    // The summary table should show "timeout" as the status for the "hangs" variant.
    assert!(
        stdout.contains("timeout"),
        "summary should show timeout status, got:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
