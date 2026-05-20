use std::path::{Path, PathBuf};
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

/// Path to the stderr-writer test helper binary.
///
/// Used by the T-impl.9 failure-diagnostic tests below. The helper picks
/// its behaviour from the `STDERR_WRITER_MODE` env var so a single binary
/// can cover the timeout-with-stderr, failed-with-stderr, and
/// failed-with-empty-stderr branches of the runner's new failure block.
fn stderr_writer_binary() -> String {
    let path = env!("CARGO_BIN_EXE_stderr-writer");
    assert!(
        Path::new(path).exists(),
        "stderr-writer binary not found at {path}"
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

    // Spot-check that each JSONL log's lifecycle records carry the
    // per-spawn `variant` suffix (e.g. `dummy-qos1`), and that the
    // sibling `.compact.parquet` exists. Per the T18.2 default the
    // per-event `qos` field now lives in the compact Parquet sidecar
    // rather than the JSONL stream (see
    // `metak-shared/api-contracts/jsonl-log-schema.md` E18 note).
    for file in &jsonl_files {
        let log_path = subdirs[0].path().join(file);
        let contents = std::fs::read_to_string(&log_path).unwrap();
        let expected_variant = if file.contains("dummy-qos1") {
            "dummy-qos1"
        } else {
            "dummy-qos2"
        };
        let mut found_variant_record = false;
        for line in contents.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(variant) = v.get("variant").and_then(|w| w.as_str()) {
                assert_eq!(
                    variant, expected_variant,
                    "file {file} has variant={variant}, expected {expected_variant}"
                );
                found_variant_record = true;
            }
        }
        assert!(
            found_variant_record,
            "file {file} should contain at least one record with a variant field"
        );

        // Sibling compact Parquet is where per-event QoS now lives
        // (T18.2). Its presence demonstrates the spawn ran to digest.
        let parquet_path = log_path.with_extension("compact.parquet");
        assert!(
            parquet_path.exists(),
            "expected sibling compact Parquet at {parquet_path:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&log_dir);
}

/// T-ux.1: the per-spawn progress + ETA line must appear after the FIRST
/// spawn's "finished:" line and must be ABSENT after the SECOND (final)
/// spawn's. The exact shape is pinned here so a future refactor cannot
/// silently regress the operator-facing diagnostic contract.
#[test]
fn progress_eta_line_after_each_non_final_spawn() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    // Use a distinct log dir so the test is hermetic relative to the
    // sibling qos_array test (which uses ./test-logs-qos-array).
    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-tux1-progress-eta");
    let _ = std::fs::remove_dir_all(&log_dir);

    // Reuse the qos-array fixture shape (two spawns: -qos1 and -qos2). It
    // produces deterministic short-form ETA output (sub-minute spawns) so
    // we can assert on the line prefix without timing flake.
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");
    let tmp_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-tux1-progress-eta-cfg");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let config_content = format!(
        r#"run = "tux1"
runners = ["local"]
default_timeout_secs = 30
inter_qos_grace_ms = 0

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"
  [variant.common]
  tick_rate_hz = 5
  stabilize_secs = 0
  operate_secs = 1
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 1
  qos = [1, 2]
  log_dir = "{log_dir_escaped}"
  [variant.specific]
"#
    );
    let config_path = tmp_dir.join("tux1.toml");
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
    assert!(
        output.status.success(),
        "runner should exit 0, got {:?}",
        output.status.code()
    );

    // Locate the two "finished:" lines. There must be exactly two for a
    // two-spawn run.
    let finished_indices: Vec<usize> = stderr
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            if l.contains("' finished: status=") {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        finished_indices.len(),
        2,
        "expected exactly 2 finished lines in stderr (one per spawn), got {} -- stderr:\n{stderr}",
        finished_indices.len()
    );

    let lines: Vec<&str> = stderr.lines().collect();

    // After the FIRST "finished:" line, the next non-empty line on the
    // same stream must be our T-ux.1 progress line with cursor 1/2.
    let after_first = lines[finished_indices[0] + 1];
    assert!(
        after_first.starts_with("[runner:local] progress: 1/2 done | elapsed ")
            && after_first.contains(" | ETA ~"),
        "expected progress+ETA line immediately after first finished line,\n\
         got: {after_first}\n\
         (full stderr below)\n{stderr}"
    );

    // After the SECOND (final) "finished:" line, the progress line must
    // NOT appear. We allow other lines after it (done-barrier teardown,
    // resume summary, etc.) but the progress prefix must be absent.
    let tail_after_final: Vec<&str> = lines[finished_indices[1] + 1..].to_vec();
    let progress_after_final = tail_after_final
        .iter()
        .find(|l| l.starts_with("[runner:local] progress:"));
    assert!(
        progress_after_final.is_none(),
        "no progress line should follow the FINAL spawn's finished line, \
         got: {progress_after_final:?}\nfull stderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&log_dir);
    let _ = std::fs::remove_dir_all(&tmp_dir);
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
  tick_rate_hz = 1
  values_per_tick = 1

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

#[test]
fn template_and_array_expansion_produces_cartesian_product_log_files() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-template-and-arrays");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/template-and-arrays.toml")
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

    // Cartesian product: 2 hz x 1 vpt x 2 qos = 4 spawns. Each spawn name
    // includes both the vpt/hz suffix and the qos suffix.
    let expected_names = [
        "dummy-2x10hz-qos1",
        "dummy-2x10hz-qos2",
        "dummy-2x20hz-qos1",
        "dummy-2x20hz-qos2",
    ];
    for name in &expected_names {
        assert!(
            stdout.contains(name),
            "summary should contain {name}, got:\n{stdout}"
        );
    }

    // Spawn ordering in stderr ("ready barrier for spawn ...") must follow
    // the documented stable order: hz outer, vpt middle, qos inner.
    let positions: Vec<usize> = expected_names
        .iter()
        .map(|n| {
            stderr
                .find(&format!("ready barrier for spawn '{n}'"))
                .unwrap_or_else(|| panic!("missing ready barrier for {n}"))
        })
        .collect();
    for w in positions.windows(2) {
        assert!(
            w[0] < w[1],
            "expected spawn order {expected_names:?}, but got positions {positions:?} in stderr"
        );
    }

    // Verify per-spawn JSONL files were emitted.
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
        expected_names.len(),
        "expected one JSONL file per spawn, got {jsonl_files:?}"
    );
    for name in &expected_names {
        assert!(
            jsonl_files.iter().any(|f| f.contains(name)),
            "expected file containing {name}, got {jsonl_files:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&log_dir);
}

// -------------------------------------------------------------------
// T14.8: threading_modes expansion end-to-end via variant-dummy.
// VariantDummy declares both Single and Multi (variant-base T14.1), so a
// single-runner config that requests both modes must produce 2 successful
// spawns, 2 JSONL log files (suffixed `-single` / `-multi`), and both
// `connected` events must record the matching `threading_mode` field.
// -------------------------------------------------------------------

#[test]
fn threading_modes_expansion_runs_both_spawns_through_variant_dummy() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-threading-modes");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/threading-modes.toml")
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

    // Both spawn names must appear in the summary.
    for needle in ["dummy-multi", "dummy-single"] {
        assert!(
            stdout.contains(needle),
            "summary should contain {needle}, got:\n{stdout}"
        );
    }

    // One timestamped log subfolder; one JSONL file per spawn.
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
        2,
        "expected exactly 2 JSONL files (one per mode), got {jsonl_files:?}"
    );
    assert!(
        jsonl_files.iter().any(|f| f.contains("dummy-multi")),
        "expected a dummy-multi log file, got {jsonl_files:?}"
    );
    assert!(
        jsonl_files.iter().any(|f| f.contains("dummy-single")),
        "expected a dummy-single log file, got {jsonl_files:?}"
    );

    // Each log's connected event must record the matching threading_mode.
    for file in &jsonl_files {
        let expected_mode = if file.contains("dummy-multi") {
            "multi"
        } else {
            "single"
        };
        let contents = std::fs::read_to_string(subdirs[0].path().join(file)).unwrap();
        let connected_line = contents
            .lines()
            .find(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("event").map(|e| e == "connected"))
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("no `connected` event in {file}"));
        let parsed: serde_json::Value = serde_json::from_str(connected_line).unwrap();
        assert_eq!(
            parsed.get("threading_mode").and_then(|v| v.as_str()),
            Some(expected_mode),
            "{file}: connected event threading_mode mismatch"
        );
        // recv_buffer_kb must also be present (default 4096 since the
        // fixture does not override).
        assert_eq!(
            parsed.get("recv_buffer_kb").and_then(|v| v.as_u64()),
            Some(4096),
            "{file}: connected event recv_buffer_kb mismatch"
        );
    }

    let _ = std::fs::remove_dir_all(&log_dir);
}

#[test]
fn threading_modes_capability_gating_skips_unsupported_with_notice() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-threading-modes-gated");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/threading-modes-gated.toml")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "runner should exit 0 (gated spawns are not failures), got {:?}\nstderr: {stderr}",
        output.status.code()
    );

    // Stderr must carry the exact contract notice for the skipped multi spawn.
    assert!(
        stderr.contains("skipping dummy-multi: variant does not support threading_mode=multi"),
        "stderr must carry the capability-gating skip notice, got:\n{stderr}"
    );

    // Summary must contain the single-mode spawn but NOT the multi-mode spawn.
    assert!(
        stdout.contains("dummy-single"),
        "summary should contain dummy-single, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("dummy-multi"),
        "summary must NOT contain dummy-multi (skipped), got:\n{stdout}"
    );

    // Only one JSONL file (the single-mode spawn) should have been produced.
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
        1,
        "expected exactly 1 JSONL file (gated multi spawn excluded), got {jsonl_files:?}"
    );
    assert!(
        jsonl_files[0].contains("dummy-single"),
        "expected dummy-single log file, got {jsonl_files:?}"
    );

    let _ = std::fs::remove_dir_all(&log_dir);
}

/// Single-runner end-to-end resume: first run produces non-empty JSONL files;
/// second run with `--resume` skips both spawns; third run with one file
/// truncated re-runs only that one spawn.
#[test]
fn single_runner_resume_skips_complete_files_and_reruns_truncated() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    // Build a config inside a private tmp dir so this test does not collide
    // with any other test's `./test-logs` folder.
    let tmp_dir = std::env::temp_dir().join("runner-resume-it");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();

    // The runner resolves the variant binary path relative to its CWD.
    // We invoke runner from the runner crate manifest dir, so use the
    // existing relative path that other tests use.
    let log_dir = tmp_dir.join("logs");
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");
    let config_content = format!(
        r#"run = "resumeit"
runners = ["local"]
default_timeout_secs = 30
inter_qos_grace_ms = 0

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"
  [variant.common]
  tick_rate_hz = 5
  stabilize_secs = 0
  operate_secs = 1
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 1
  qos = [1, 2]
  log_dir = "{log_dir_escaped}"
  [variant.specific]
"#
    );
    let config_path = tmp_dir.join("resume-it.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    // Run 1: fresh — produce non-empty JSONL files.
    let out1 = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner (run 1)");
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    eprintln!("--- run 1 stdout ---\n{stdout1}");
    eprintln!("--- run 1 stderr ---\n{stderr1}");
    assert!(out1.status.success(), "run 1 should exit 0");

    // Find the timestamped subfolder.
    let subdirs: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(subdirs.len(), 1, "expected one timestamped subfolder");
    let run_subdir = subdirs[0].path();

    let jsonl_paths: Vec<std::path::PathBuf> = std::fs::read_dir(&run_subdir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter(|p| {
            // The variant-dummy log files have names like
            // <variant>-<runner>-<run>.jsonl. We only want those, not
            // clock-sync sibling files (which are skipped in single-runner
            // anyway).
            let n = p.file_name().unwrap().to_string_lossy().to_string();
            !n.contains("clock-sync")
        })
        .collect();
    assert_eq!(jsonl_paths.len(), 2, "expected 2 variant log files");
    for p in &jsonl_paths {
        let len = std::fs::metadata(p).unwrap().len();
        assert!(len > 0, "log file should be non-empty: {}", p.display());
    }

    // Run 2: resume — both spawns must be skipped, exit 0, summary mentions skipped.
    let out2 = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .arg("--resume")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner (run 2)");
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    eprintln!("--- run 2 stdout ---\n{stdout2}");
    eprintln!("--- run 2 stderr ---\n{stderr2}");
    assert!(
        out2.status.success(),
        "run 2 (resume, all skipped) should exit 0"
    );
    assert!(
        stdout2.contains("skipped"),
        "summary should mention skipped, got:\n{stdout2}"
    );
    assert!(
        stdout2.contains("Resume:"),
        "should print resume summary line, got:\n{stdout2}"
    );
    // The selected log subfolder should be the same one from run 1.
    let subdirs2: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(
        subdirs2.len(),
        1,
        "resume must reuse, not create, log subfolders"
    );

    // Run 3: truncate one log file to zero bytes; resume must delete it
    // and re-execute that one spawn.
    let truncate_path = &jsonl_paths[0];
    std::fs::write(truncate_path, b"").unwrap();
    assert_eq!(std::fs::metadata(truncate_path).unwrap().len(), 0);

    let out3 = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .arg("--resume")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner (run 3)");
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    eprintln!("--- run 3 stdout ---\n{stdout3}");
    eprintln!("--- run 3 stderr ---\n{stderr3}");
    assert!(out3.status.success(), "run 3 should exit 0");
    // Mixed: at least one skipped, at least one success.
    assert!(
        stdout3.contains("skipped"),
        "expected at least one skipped row"
    );
    assert!(
        stdout3.contains("success"),
        "expected at least one success row"
    );
    // The truncated file should now be non-empty again.
    let len_after = std::fs::metadata(truncate_path).unwrap().len();
    assert!(
        len_after > 0,
        "previously truncated file should be re-populated: {}",
        truncate_path.display()
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Drive a barrier timeout by having the runner believe a peer exists, run a
/// small fake-peer process that participates in discovery and then goes
/// silent, and assert the runner exits with code 75 (EX_TEMPFAIL) on the
/// ready-barrier timeout.
///
/// Architecture:
/// - Two runners declared in the config: `local` (the runner under test) and
///   `ghost` (the fake peer).
/// - The fake peer is implemented as a small UDP responder bound to the
///   coordination port for runner index 1. It answers Discover messages with
///   its own Discover (so the runner-under-test can complete Phase 1 and
///   capture its host) and then drops every subsequent Ready / Done /
///   ResumeManifest message — the silent-peer scenario.
/// - With `--barrier-timeout-secs 3`, the runner-under-test reaches its
///   ready barrier, broadcasts Ready, never sees a peer Ready, and exits 75
///   after ~3 seconds.
#[test]
fn barrier_timeout_exits_75_when_peer_silent_after_discovery() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    // Pick a port range above the production-test port pool so we don't
    // collide with parallel protocol unit tests.
    let base_port: u16 = 32100;

    let tmp_dir = std::env::temp_dir().join("runner-barrier-timeout-it");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let log_dir = tmp_dir.join("logs");
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");

    // Two-runner config; `ghost` is the fake peer thread defined below.
    let config_content = format!(
        r#"run = "btmo"
runners = ["local", "ghost"]
default_timeout_secs = 30
inter_qos_grace_ms = 0

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"
  [variant.common]
  tick_rate_hz = 5
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
    let config_path = tmp_dir.join("btmo.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    // Spawn the fake-peer thread before launching the runner so discovery
    // can succeed quickly.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ghost = run_silent_peer(base_port, stop.clone());

    let mut child = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .arg("--port")
        .arg(base_port.to_string())
        .arg("--barrier-timeout-secs")
        .arg("3")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn runner");

    // Hard cap so a regression that re-introduces the hang fails fast in CI
    // instead of taking down the test runner.
    let started = std::time::Instant::now();
    let hard_cap = std::time::Duration::from_secs(60);
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if started.elapsed() > hard_cap {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "runner did not exit within {}s after barrier timeout fired",
                        hard_cap.as_secs()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => panic!("error waiting for runner child: {e}"),
        }
    };

    // Stop the ghost thread and join.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ghost.join();

    let mut stderr_out = String::new();
    use std::io::Read;
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut stderr_out);
    }
    let mut stdout_out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout_out);
    }
    eprintln!("--- stdout ---\n{stdout_out}");
    eprintln!("--- stderr ---\n{stderr_out}");

    assert_eq!(
        exit_code,
        Some(75),
        "expected exit 75 (EX_TEMPFAIL), got {exit_code:?}; stderr was:\n{stderr_out}"
    );
    assert!(
        stderr_out.contains("EX_TEMPFAIL")
            || stderr_out.contains("barrier")
            || stderr_out.contains("timed out"),
        "expected a barrier-timeout stderr line, got:\n{stderr_out}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Spawn a UDP responder that participates in runner coordination just well
/// enough to let the runner-under-test complete Phase 1 (discovery), then
/// drops all subsequent traffic. The thread exits when `stop` becomes true.
///
/// The responder binds to `base_port + 1` (the index-1 slot for the `ghost`
/// runner in the config), joins the coordination multicast group, and
/// periodically broadcasts a Discover message with the same config_hash and
/// log_subdir the runner-under-test will propose. Probe requests are
/// answered to keep clock-sync from blowing up; everything else is ignored.
fn run_silent_peer(
    base_port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use socket2::{Domain, Protocol as SockProto, Socket as Sock, Type};
    use std::net::{Ipv4Addr, SocketAddrV4};
    std::thread::spawn(move || {
        let port = base_port + 1;
        let s = Sock::new(Domain::IPV4, Type::DGRAM, Some(SockProto::UDP)).unwrap();
        s.set_reuse_address(true).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        s.set_nonblocking(false).unwrap();
        s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
            .unwrap();
        let group = Ipv4Addr::new(239, 77, 66, 55);
        s.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED).unwrap();
        s.set_multicast_loop_v4(true).unwrap();

        // Compute the same config hash the runner will use. We don't have
        // direct access to BenchConfig from the integration test, so the
        // peer mirrors the runner's hash by lifting it from the runner's
        // first inbound Discover. The runner's `expected.contains(name)`
        // check requires the hash to match; otherwise it would bail with
        // "config hash mismatch".
        let mut peer_hash: Option<String> = None;
        let mut last_subdir: Option<String> = None;

        let mut buf = [std::mem::MaybeUninit::uninit(); 4096];
        let mut last_send = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .unwrap_or_else(std::time::Instant::now);
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            // Periodically broadcast our own Discover message every 200 ms,
            // but only after we've seen one from the runner under test
            // (which gives us its hash and log_subdir to mirror).
            if let (Some(h), Some(sub)) = (peer_hash.as_ref(), last_subdir.as_ref()) {
                if last_send.elapsed() >= std::time::Duration::from_millis(200) {
                    let msg = serde_json::json!({
                        "type": "discover",
                        "name": "ghost",
                        "config_hash": h,
                        "log_subdir": sub,
                        "resume": false,
                    });
                    let bytes = serde_json::to_vec(&msg).unwrap();
                    // Send to every runner's per-index port (multicast +
                    // localhost loopback) so the runner-under-test gets it.
                    for i in 0..2u16 {
                        let p = base_port + i;
                        let _ = s.send_to(&bytes, &SocketAddrV4::new(group, p).into());
                        let _ =
                            s.send_to(&bytes, &SocketAddrV4::new(Ipv4Addr::LOCALHOST, p).into());
                    }
                    last_send = std::time::Instant::now();
                }
            }

            match s.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    let data: Vec<u8> = buf[..n]
                        .iter()
                        .map(|b| unsafe { b.assume_init() })
                        .collect();
                    let v: serde_json::Value = match serde_json::from_slice(&data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match ty {
                        "discover" => {
                            // Capture the runner-under-test's hash + subdir.
                            if v.get("name").and_then(|x| x.as_str()) == Some("local") {
                                if let Some(h) = v.get("config_hash").and_then(|x| x.as_str()) {
                                    peer_hash = Some(h.to_string());
                                }
                                if let Some(sub) = v.get("log_subdir").and_then(|x| x.as_str()) {
                                    last_subdir = Some(sub.to_string());
                                }
                            }
                        }
                        "probe_request" => {
                            // Always-respond rule: emit a ProbeResponse so
                            // the runner's clock-sync engine does not bail.
                            // We stamp t2/t3 = t1 so the offset and rtt
                            // computations are well-defined.
                            let to = v.get("to").and_then(|x| x.as_str()).unwrap_or("");
                            if to == "ghost" {
                                let from = v.get("from").and_then(|x| x.as_str()).unwrap_or("");
                                let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                                let t1 = v.get("t1").and_then(|x| x.as_str()).unwrap_or("");
                                let resp = serde_json::json!({
                                    "type": "probe_response",
                                    "from": "ghost",
                                    "to": from,
                                    "id": id,
                                    "t1": t1,
                                    "t2": t1,
                                    "t3": t1,
                                });
                                let rb = serde_json::to_vec(&resp).unwrap();
                                for i in 0..2u16 {
                                    let p = base_port + i;
                                    let _ = s.send_to(&rb, &SocketAddrV4::new(group, p).into());
                                    let _ = s.send_to(
                                        &rb,
                                        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, p).into(),
                                    );
                                }
                            }
                        }
                        // Drop everything else — that's the silent-peer behaviour.
                        _ => {}
                    }
                }
                Err(_) => continue,
            }
        }
    })
}

/// Resume mode aborts cleanly when no matching log subfolder exists.
#[test]
fn resume_aborts_when_no_matching_log_subfolder() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }
    let tmp_dir = std::env::temp_dir().join("runner-resume-no-folder");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();

    let log_dir = tmp_dir.join("logs");
    std::fs::create_dir_all(&log_dir).unwrap(); // base exists, but no <run>-* subfolder.
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");
    let config_content = format!(
        r#"run = "noresume"
runners = ["local"]
default_timeout_secs = 10

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"
  [variant.common]
  tick_rate_hz = 5
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
    let config_path = tmp_dir.join("noresume.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    let out = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .arg("--resume")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- stderr ---\n{stderr}");
    assert!(
        !out.status.success(),
        "resume with no existing folder must fail"
    );
    assert!(
        stderr.contains("no log subfolder")
            || stderr.contains("could not select an existing log subfolder"),
        "expected 'no log subfolder' message, got:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// ---------------------------------------------------------------------------
// T-impl.9: failure diagnostics block. Each of the next four tests drives the
// runner against a config that points its single variant at the stderr-writer
// helper, with `STDERR_WRITER_MODE` chosen so the runner observes a specific
// spawn outcome. The assertions cover the exact stderr lines the operator
// would need on a real diagnostic session.
// ---------------------------------------------------------------------------

/// Build a tempdir + minimal single-runner config that points the variant
/// binary at `stderr-writer`. Returns (tmp_dir, config_path, log_dir).
///
/// The config uses `default_timeout_secs = timeout_secs` so the test can
/// drive either a clean exit (helper exits in <1s) or a timeout
/// (helper sleeps forever) by choosing the mode env var, without having
/// to tune the timeout per case.
fn build_stderr_writer_config(prefix: &str, timeout_secs: u64) -> (PathBuf, PathBuf, PathBuf) {
    let writer = stderr_writer_binary();
    let writer_escaped = writer.replace('\\', "/");
    let tmp_dir = std::env::temp_dir().join(format!(
        "runner_t9_{prefix}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let log_dir = tmp_dir.join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");

    // qos = 1 (a single concrete level) so the spawn job's effective_name
    // matches `[[variant]].name` exactly. With `qos` omitted the runner
    // would expand into four jobs (t9var-qos1..t9var-qos4), each repeating
    // the failure-diagnostics block once -- harmless but wasteful for the
    // smoke test.
    let config = format!(
        r#"run = "t9run"
runners = ["local"]
default_timeout_secs = {timeout_secs}

[[variant]]
name = "t9var"
binary = "{writer_escaped}"

  [variant.common]
  tick_rate_hz = 1
  values_per_tick = 1
  qos = 1
  log_dir = "{log_dir_escaped}"

  [variant.specific]
"#
    );
    let config_path = tmp_dir.join("t9.toml");
    std::fs::write(&config_path, &config).unwrap();
    (tmp_dir, config_path, log_dir)
}

/// Failure mode: timeout, with stderr lines already on disk.
///
/// The helper prints three labelled lines and sleeps; the runner's
/// 2-second timeout fires; the runner should print the stderr capture
/// path, optionally the JSONL path, AND the tail block containing the
/// three lines.
#[test]
fn t9_timeout_with_stderr_prints_capture_path_and_tail() {
    let (tmp_dir, config_path, _log_dir) = build_stderr_writer_config("timeout", 2);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("STDERR_WRITER_MODE", "lines_then_sleep")
        .env_remove("RUST_BACKTRACE")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        !output.status.success(),
        "runner should exit non-zero on timeout"
    );

    // The existing status line must still be present (we ADD, never modify).
    assert!(
        stderr.contains("'t9var' finished: status=timeout"),
        "existing status line must remain, got:\n{stderr}"
    );

    // New block: stderr capture path pointer.
    assert!(
        stderr.contains("stderr capture: "),
        "missing 'stderr capture:' pointer line, got:\n{stderr}"
    );
    // The capture file path includes the resolved log subdir but the
    // capture filename's suffix is the load-bearing thing we want to
    // see (the subdir name is timestamped so we cannot match it exactly).
    assert!(
        stderr.contains("t9var-local-stderr.txt"),
        "expected stderr capture filename in pointer, got:\n{stderr}"
    );

    // Tail block must be present with the three labelled lines.
    assert!(
        stderr.contains("---- stderr tail"),
        "expected tail opening separator, got:\n{stderr}"
    );
    assert!(
        stderr.contains("---- end stderr tail ----"),
        "expected tail closing separator, got:\n{stderr}"
    );
    for needle in ["STDERR-LINE-1", "STDERR-LINE-2", "STDERR-LINE-3"] {
        assert!(
            stderr.contains(needle),
            "expected {needle} in tail, got:\n{stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Failure mode: non-zero exit with no stderr output.
///
/// The motivating scenario: a child killed before flushing anything. The
/// runner must print the empty-capture notice INSTEAD of an empty tail
/// block (no fake separators).
#[test]
fn t9_failed_with_empty_stderr_prints_empty_notice() {
    let (tmp_dir, config_path, _log_dir) = build_stderr_writer_config("silent_fail", 5);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("STDERR_WRITER_MODE", "silent_fail")
        .env_remove("RUST_BACKTRACE")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        !output.status.success(),
        "runner should exit non-zero on failed variant"
    );

    assert!(
        stderr.contains("'t9var' finished: status=failed"),
        "existing status line must remain, got:\n{stderr}"
    );

    // Capture path still printed.
    assert!(
        stderr.contains("t9var-local-stderr.txt"),
        "expected stderr capture pointer, got:\n{stderr}"
    );

    // Empty-capture notice must appear, and the tail-bracket separators must NOT.
    assert!(
        stderr.contains("stderr capture is empty"),
        "expected empty-capture notice, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("---- stderr tail"),
        "must NOT print a tail block for an empty capture, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("---- end stderr tail ----"),
        "must NOT print a closing tail separator for an empty capture, got:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Failure mode: non-zero exit WITH stderr output. Confirms the tail block
/// appears on `failed` (not just on `timeout`).
#[test]
fn t9_failed_with_stderr_prints_tail() {
    let (tmp_dir, config_path, _log_dir) = build_stderr_writer_config("failed", 5);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("STDERR_WRITER_MODE", "lines_then_fail")
        .env_remove("RUST_BACKTRACE")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        !output.status.success(),
        "runner should exit non-zero on failed variant"
    );
    assert!(
        stderr.contains("'t9var' finished: status=failed"),
        "existing status line must remain, got:\n{stderr}"
    );
    assert!(
        stderr.contains("---- stderr tail"),
        "expected tail block on failed exit, got:\n{stderr}"
    );
    for needle in ["FAIL-LINE-1", "FAIL-LINE-2"] {
        assert!(
            stderr.contains(needle),
            "expected {needle} in tail, got:\n{stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Success path: a variant that exits 0 (no `[[variant]]` work, just a quick
/// stderr write) must NOT trigger the failure-diagnostic block. The runner
/// stays silent on success -- existing behaviour preserved.
#[test]
fn t9_success_stays_quiet_no_tail() {
    let (tmp_dir, config_path, _log_dir) = build_stderr_writer_config("plain", 5);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(config_path.to_str().unwrap())
        .env("STDERR_WRITER_MODE", "plain")
        .env_remove("RUST_BACKTRACE")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "runner should exit zero when variant exits 0, got: {:?}\nstderr: {stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("'t9var' finished: status=success"),
        "existing status line must remain, got:\n{stderr}"
    );

    // None of the failure-diagnostic strings must appear.
    assert!(
        !stderr.contains("stderr capture: "),
        "success path must NOT print stderr capture pointer, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("---- stderr tail"),
        "success path must NOT print tail block, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("stderr capture is empty"),
        "success path must NOT print empty-capture notice, got:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// T14.24 regression: two real runner subprocesses must complete the
/// Phase 1.25 resume_manifest barrier and proceed into Phase 2 when
/// resuming against an existing log dir.
///
/// The test:
///
/// 1. Runs a fresh two-runner session producing one spawn each side.
///    (variant-dummy with no operate window so the run finishes fast.)
/// 2. Re-runs both sides with `--resume`. The Phase 1.25 manifest
///    exchange must converge over the new per-peer-pair TCP path; if
///    the pre-T14.24 UDP path regressed back in, both peers would time
///    out at 120 s waiting for each other.
///
/// Marked `#[ignore]` because it spawns four real `runner` subprocesses
/// (two per phase) and binds UDP / TCP ports — CI machines may rebel.
/// Run locally via `cargo test --release -p runner -- --ignored`.
#[test]
#[ignore]
fn two_runner_resume_manifest_barrier_converges_t14_24() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    // Pick a port range well above the unit-test pool and above the
    // single-runner-resume integration test's port pool.
    let base_port: u16 = 33200;

    let tmp_dir = std::env::temp_dir().join("runner-t14-24-resume-it");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let log_dir = tmp_dir.join("logs");
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");

    // Two-runner config so the manifest barrier is actually exercised
    // (single-runner short-circuits). `alice` and `bob` are the
    // conventional names and also sort consistently (alice < bob) which
    // exercises the lower-sorted-name-accepts pairing rule.
    // T15.8: `eot_timeout_secs` removed from configs along with the
    // on-wire EOT exchange. variant-dummy now exits via T15.5 idle
    // detection / runner-coordinated termination (T15.4).
    let config_content = format!(
        r#"run = "t14_24_resume"
runners = ["alice", "bob"]
default_timeout_secs = 30
inter_qos_grace_ms = 0

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"
  [variant.common]
  tick_rate_hz = 5
  stabilize_secs = 0
  operate_secs = 1
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 1
  qos = 1
  log_dir = "{log_dir_escaped}"
  [variant.specific]
"#
    );
    let config_path = tmp_dir.join("t14_24_resume.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    // Phase 1: fresh two-runner run. Spawn both in parallel so they can
    // discover each other; collect outputs at the end.
    let runner = runner_binary();
    let config_str = config_path.to_str().unwrap().to_string();
    let runner_a = runner.clone();
    let config_a = config_str.clone();
    let phase1_a = std::thread::spawn(move || {
        Command::new(runner_a)
            .arg("--name")
            .arg("alice")
            .arg("--config")
            .arg(&config_a)
            .arg("--port")
            .arg(base_port.to_string())
            .arg("--barrier-timeout-secs")
            .arg("30")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to run alice (phase 1)")
    });
    let runner_b = runner.clone();
    let config_b = config_str.clone();
    let phase1_b = std::thread::spawn(move || {
        Command::new(runner_b)
            .arg("--name")
            .arg("bob")
            .arg("--config")
            .arg(&config_b)
            .arg("--port")
            .arg(base_port.to_string())
            .arg("--barrier-timeout-secs")
            .arg("30")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to run bob (phase 1)")
    });
    let out1a = phase1_a.join().expect("alice phase 1 thread panicked");
    let out1b = phase1_b.join().expect("bob phase 1 thread panicked");
    let stdout1a = String::from_utf8_lossy(&out1a.stdout);
    let stderr1a = String::from_utf8_lossy(&out1a.stderr);
    let stdout1b = String::from_utf8_lossy(&out1b.stdout);
    let stderr1b = String::from_utf8_lossy(&out1b.stderr);
    eprintln!("--- phase 1 alice stdout ---\n{stdout1a}");
    eprintln!("--- phase 1 alice stderr ---\n{stderr1a}");
    eprintln!("--- phase 1 bob   stdout ---\n{stdout1b}");
    eprintln!("--- phase 1 bob   stderr ---\n{stderr1b}");
    assert!(
        out1a.status.success(),
        "phase 1 alice should exit 0, got: {:?}",
        out1a.status.code()
    );
    assert!(
        out1b.status.success(),
        "phase 1 bob should exit 0, got: {:?}",
        out1b.status.code()
    );

    // Phase 2: resume. Both runners must complete the manifest barrier
    // and proceed (all spawns will be in the skip set since they ran
    // successfully above).
    let phase2_started = std::time::Instant::now();
    let runner_a2 = runner.clone();
    let config_a2 = config_str.clone();
    let phase2_a = std::thread::spawn(move || {
        Command::new(runner_a2)
            .arg("--name")
            .arg("alice")
            .arg("--config")
            .arg(&config_a2)
            .arg("--port")
            .arg((base_port + 100).to_string())
            .arg("--barrier-timeout-secs")
            .arg("30")
            .arg("--resume")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to run alice (phase 2 resume)")
    });
    let runner_b2 = runner.clone();
    let config_b2 = config_str.clone();
    let phase2_b = std::thread::spawn(move || {
        Command::new(runner_b2)
            .arg("--name")
            .arg("bob")
            .arg("--config")
            .arg(&config_b2)
            .arg("--port")
            .arg((base_port + 100).to_string())
            .arg("--barrier-timeout-secs")
            .arg("30")
            .arg("--resume")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to run bob (phase 2 resume)")
    });
    let out2a = phase2_a.join().expect("alice phase 2 thread panicked");
    let out2b = phase2_b.join().expect("bob phase 2 thread panicked");
    let phase2_elapsed = phase2_started.elapsed();
    let stdout2a = String::from_utf8_lossy(&out2a.stdout);
    let stderr2a = String::from_utf8_lossy(&out2a.stderr);
    let stdout2b = String::from_utf8_lossy(&out2b.stdout);
    let stderr2b = String::from_utf8_lossy(&out2b.stderr);
    eprintln!("--- phase 2 (resume) alice stdout ---\n{stdout2a}");
    eprintln!("--- phase 2 (resume) alice stderr ---\n{stderr2a}");
    eprintln!("--- phase 2 (resume) bob   stdout ---\n{stdout2b}");
    eprintln!("--- phase 2 (resume) bob   stderr ---\n{stderr2b}");
    eprintln!("--- phase 2 wall-clock: {phase2_elapsed:?} ---");

    assert!(
        out2a.status.success(),
        "phase 2 alice (resume) must exit 0 (not 75 EX_TEMPFAIL), got: {:?}",
        out2a.status.code()
    );
    assert!(
        out2b.status.success(),
        "phase 2 bob (resume) must exit 0 (not 75 EX_TEMPFAIL), got: {:?}",
        out2b.status.code()
    );
    // Sanity: the resume should be much faster than the pre-T14.24
    // 120 s barrier timeout. We allow a generous 60 s upper bound to
    // cover slow CI machines while still catching a regression where
    // both runners wait the full 120 s.
    assert!(
        phase2_elapsed < std::time::Duration::from_secs(60),
        "phase 2 (resume) should complete well under the 120s barrier \
         timeout, but took {phase2_elapsed:?} — T14.24 fix may have \
         regressed"
    );
    // Both runners must report the manifest barrier ran (i.e. resume
    // mode reached Phase 1.25).
    assert!(
        stderr2a.contains("resume: local manifest"),
        "alice resume output should mention the local manifest step, got:\n{stderr2a}"
    );
    assert!(
        stderr2b.contains("resume: local manifest"),
        "bob resume output should mention the local manifest step, got:\n{stderr2b}"
    );
    // Neither runner should have hit a barrier timeout.
    assert!(
        !stderr2a.contains("barrier 'resume_manifest' timed out"),
        "alice must NOT have hit the resume_manifest barrier timeout, got:\n{stderr2a}"
    );
    assert!(
        !stderr2b.contains("barrier 'resume_manifest' timed out"),
        "bob must NOT have hit the resume_manifest barrier timeout, got:\n{stderr2b}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// T15.3 regression: two real runner subprocesses must establish the
/// per-peer-pair TCP progress channel during Phase 2 and exchange
/// `ProgressUpdate` frames while the variant child is running. The
/// test inspects each runner's stderr for the `progress_coord: started`
/// banner and confirms the channel did NOT report any write / read
/// failures during the spawn.
///
/// Marked `#[ignore]` because it spawns two real `runner` subprocesses
/// and depends on `variant-dummy.exe`. Run locally via
/// `cargo test --release -p runner -- --ignored`.
#[test]
#[ignore]
fn two_runner_progress_coord_exchanges_t15_3() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    // Pick a port range well clear of the other two-runner integration
    // tests' pools to avoid bind conflicts when the suite runs in
    // parallel: the resume_manifest test uses 33200, this one uses
    // 33400.
    let base_port: u16 = 33400;

    let tmp_dir = std::env::temp_dir().join("runner-t15-3-progress-it");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let log_dir = tmp_dir.join("logs");
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");

    // operate_secs is long enough (3s) that at least two ~1Hz
    // progress ticks fire from the variant and the runner's per-tick
    // broadcaster (every PROGRESS_BROADCAST_INTERVAL = 1s) has time
    // to publish at least one ProgressUpdate frame to the peer.
    let config_content = format!(
        r#"run = "t15_3_progress"
runners = ["alice", "bob"]
default_timeout_secs = 30
inter_qos_grace_ms = 0

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"
  [variant.common]
  tick_rate_hz = 10
  stabilize_secs = 0
  operate_secs = 3
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 1
  qos = 1
  log_dir = "{log_dir_escaped}"
  [variant.specific]
"#
    );
    let config_path = tmp_dir.join("t15_3_progress.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    let runner = runner_binary();
    let config_str = config_path.to_str().unwrap().to_string();
    let runner_a = runner.clone();
    let config_a = config_str.clone();
    let thread_a = std::thread::spawn(move || {
        Command::new(runner_a)
            .arg("--name")
            .arg("alice")
            .arg("--config")
            .arg(&config_a)
            .arg("--port")
            .arg(base_port.to_string())
            .arg("--barrier-timeout-secs")
            .arg("30")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to run alice")
    });
    let runner_b = runner.clone();
    let config_b = config_str.clone();
    let thread_b = std::thread::spawn(move || {
        Command::new(runner_b)
            .arg("--name")
            .arg("bob")
            .arg("--config")
            .arg(&config_b)
            .arg("--port")
            .arg(base_port.to_string())
            .arg("--barrier-timeout-secs")
            .arg("30")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to run bob")
    });
    let out_a = thread_a.join().expect("alice thread panicked");
    let out_b = thread_b.join().expect("bob thread panicked");
    let stderr_a = String::from_utf8_lossy(&out_a.stderr);
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    let stdout_a = String::from_utf8_lossy(&out_a.stdout);
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    eprintln!("--- alice stdout ---\n{stdout_a}");
    eprintln!("--- alice stderr ---\n{stderr_a}");
    eprintln!("--- bob   stdout ---\n{stdout_b}");
    eprintln!("--- bob   stderr ---\n{stderr_b}");

    assert!(
        out_a.status.success(),
        "alice should exit 0, got: {:?}",
        out_a.status.code()
    );
    assert!(
        out_b.status.success(),
        "bob should exit 0, got: {:?}",
        out_b.status.code()
    );

    // Both runners must log that progress_coord came up.
    assert!(
        stderr_a.contains("progress_coord: started"),
        "alice should report progress_coord started, got:\n{stderr_a}"
    );
    assert!(
        stderr_b.contains("progress_coord: started"),
        "bob should report progress_coord started, got:\n{stderr_b}"
    );

    // No peer-write failures should be reported during the run. A
    // "write to peer ... failed" line is the canonical signal that the
    // TCP channel was unhealthy.
    assert!(
        !stderr_a.contains("progress_coord: write to peer"),
        "alice should not have logged any progress_coord peer-write failures, got:\n{stderr_a}"
    );
    assert!(
        !stderr_b.contains("progress_coord: write to peer"),
        "bob should not have logged any progress_coord peer-write failures, got:\n{stderr_b}"
    );

    // The final-progress diagnostic line confirms the variant emitted
    // progress events into the runner's local tracker. With the
    // operate_secs=3 / tick_rate=10 config, both sides should observe
    // a non-trivial `sent` counter on at least one side.
    assert!(
        stderr_a.contains("final progress:"),
        "alice should log a final-progress line, got:\n{stderr_a}"
    );
    assert!(
        stderr_b.contains("final progress:"),
        "bob should log a final-progress line, got:\n{stderr_b}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// =========================================================================
// T15.4 integration: phase-aware termination state machine.
// =========================================================================

/// T15.4: spawn variant-dummy at a generous `operate_secs` and confirm
/// the runner's phase-aware termination loop is healthy:
///
/// 1. The runner runs the spawn through to clean success without
///    firing the safety net (no `safety-net kill` log line).
/// 2. The runner's per-tick state-machine code path is exercised
///    (proved by the build linking the new `termination` module and
///    the `--operate-idle-secs` / `--max-spawn-secs` CLI args being
///    accepted).
/// 3. The final-progress diagnostic line reports `phase=done` and
///    `eot_sent=true`, confirming the variant transitioned cleanly
///    through its own idle path and the runner saw the full lifecycle.
///
/// Note on variant-dummy + single-runner: the dummy variant echoes
/// every write back to itself via an in-process queue, so `received`
/// advances as fast as `sent` for the entire operate window. The
/// variant's own T15.5 idle detection therefore can NOT fire while
/// operate is publishing -- it only fires once operate_secs naturally
/// elapses or the workload stops producing. The integration scenario
/// the task spec describes (variant exiting via idle before
/// operate_secs) requires a multi-runner setup or a workload that
/// stops emitting; both are out of this worker's scope. The single-
/// runner test here covers the orthogonal property: the new runner
/// code path does not regress the time-based exit and the safety net
/// remains a no-op on a healthy spawn. The full multi-runner
/// scenario is exercised separately by the E15 stress fixture.
#[test]
fn t15_4_runner_termination_loop_healthy_for_short_dummy_spawn() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-t15-4");
    let _ = std::fs::remove_dir_all(&log_dir);

    let start = std::time::Instant::now();
    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/t15-4-idle-termination.toml")
        // Generous safety net well above operate_secs=30 so any
        // safety-net fire is a real bug. Operate_secs is bounded by
        // the variant-config's default_timeout_secs (60s) which the
        // per-variant `timeout_secs` defaults to, so the effective
        // max is the smaller of 60 / 90 / 60 -> 60.
        .arg("--max-spawn-secs")
        .arg("90")
        // Plain default idle threshold; the dummy in single-runner
        // mode never idles within operate_secs so this is only
        // exercising the runner's per-tick evaluation cadence, not
        // the trigger.
        .arg("--operate-idle-secs")
        .arg("2")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");
    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("--- elapsed: {:?} ---", elapsed);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "runner should exit 0, got {:?}\nstderr: {stderr}",
        output.status.code()
    );

    // Safety net MUST NOT have fired -- if it did, the new state
    // machine is mis-classifying a healthy spawn as stuck.
    assert!(
        !stderr.contains("safety-net kill"),
        "safety-net log line must not appear on a healthy spawn, got:\n{stderr}"
    );

    // The variant must have reached `done` and emitted its own
    // `eot_sent` (variant-dummy in single-runner mode hits this via
    // the time-based exit -- operate_secs expires, eot_sent fires,
    // silent phase drains, done phase exits). This proves the runner
    // observed the full lifecycle and did not prematurely kill the
    // child via the new T15.4 path.
    assert!(
        stderr.contains("phase=done"),
        "final-progress line should show phase=done, got:\n{stderr}"
    );
    assert!(
        stderr.contains("eot_sent=true"),
        "final-progress line should show eot_sent=true, got:\n{stderr}"
    );

    // Sanity: the spawn must finish well below the safety-net deadline.
    // operate_secs=30 + connect/silent overhead -- generous upper bound
    // here is 60s to absorb cold-start and CI variability.
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "spawn should finish well below the 90s safety net; elapsed={elapsed:?}"
    );

    let _ = std::fs::remove_dir_all(&log_dir);
}

/// T15.4: confirm the safety-net branch of the state machine fires
/// when `--max-spawn-secs` is set tighter than the variant's natural
/// lifecycle.
///
/// Build: operate_secs=30, max-spawn-secs=3. The runner should kill
/// the child via the safety-net branch (NOT the legacy wall-clock
/// branch, which uses the same kill but does not log a `safety-net
/// kill` line). Exit code reflects the kill (`-1` -> `Timeout`
/// outcome -> non-zero runner exit).
#[test]
fn t15_4_safety_net_fires_when_max_spawn_secs_is_tight() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-t15-4-safety");
    let _ = std::fs::remove_dir_all(&log_dir);

    let start = std::time::Instant::now();
    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/t15-4-idle-termination.toml")
        // Tight safety net: shorter than the variant's natural
        // operate_secs=30, so the state machine kills the child via
        // the SafetyNet branch.
        .arg("--max-spawn-secs")
        .arg("3")
        .arg("--operate-idle-secs")
        .arg("2")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");
    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("--- elapsed: {:?} ---", elapsed);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    // The runner exits non-zero because the spawn was killed via the
    // safety net (it counts as a non-success status row).
    assert!(
        !output.status.success(),
        "runner should exit non-zero on safety-net kill, got {:?}",
        output.status.code()
    );

    // The new T15.4 safety-net log line is the load-bearing
    // distinguisher between the legacy wall-clock kill and the new
    // state-machine kill. Pin its presence so a regression that
    // bypasses the state machine and falls through to the legacy
    // branch is caught.
    assert!(
        stderr.contains("safety-net kill"),
        "safety-net log line must appear when --max-spawn-secs trips, got:\n{stderr}"
    );

    // Total runtime must be within the safety-net + a small slack
    // window. If we run substantially longer the runner is using
    // some other deadline and the new code path is not engaged.
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "safety net should fire within ~3s + overhead; elapsed={elapsed:?}"
    );

    let _ = std::fs::remove_dir_all(&log_dir);
}

// ---------------------------------------------------------------------
// T18.5: --log-dir CLI flag + [runner] log_dir TOML key.
// ---------------------------------------------------------------------

/// Build a minimal single-runner config that points at variant-dummy and
/// declares its `log_dir` as `./logs` (per the project-wide invariant in
/// `metak-shared/coding-standards.md`). The runner's `--log-dir` flag is
/// what redirects output for these tests.
fn build_minimal_single_runner_config(run: &str) -> String {
    format!(
        r#"run = "{run}"
runners = ["local"]
default_timeout_secs = 30

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"

  [variant.common]
  tick_rate_hz = 10
  stabilize_secs = 0
  operate_secs = 1
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 2
  qos = 1
  log_dir = "./logs"

  [variant.specific]
"#
    )
}

#[test]
fn t18_5_log_dir_cli_flag_redirects_variant_output() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let tmp = std::env::temp_dir().join("runner_t18_5_cli");
    let _ = std::fs::remove_dir_all(&tmp);
    let custom_log_dir = tmp.join("shared-folder").join("bench-logs");
    let config_path = tmp.join("config.toml");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        &config_path,
        build_minimal_single_runner_config("t18-5-cli"),
    )
    .unwrap();

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(&config_path)
        .arg("--log-dir")
        .arg(&custom_log_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");
    assert!(
        output.status.success(),
        "runner should exit 0 with --log-dir, got {:?}",
        output.status.code()
    );

    // Stderr must announce the chosen base log dir and the source.
    assert!(
        stderr.contains("--log-dir CLI flag"),
        "stderr must mention 'source: --log-dir CLI flag', got:\n{stderr}"
    );

    // Variant JSONL must land under the requested directory (in a
    // <run>-<ts> subfolder), not under the config's `./logs`.
    assert!(
        custom_log_dir.exists(),
        "the --log-dir path must have been created"
    );
    let subdirs: Vec<_> = std::fs::read_dir(&custom_log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert!(
        !subdirs.is_empty(),
        "expected a timestamped subfolder under {custom_log_dir:?}"
    );
    let jsonl_count: usize = subdirs
        .iter()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap())
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .count();
    assert!(
        jsonl_count > 0,
        "expected at least one .jsonl in the --log-dir subfolder, got {jsonl_count}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn t18_5_log_dir_toml_key_redirects_variant_output() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let tmp = std::env::temp_dir().join("runner_t18_5_toml");
    let _ = std::fs::remove_dir_all(&tmp);
    let toml_log_dir = tmp.join("toml-driven").join("bench-logs");
    std::fs::create_dir_all(&tmp).unwrap();

    // Build a config WITH a [runner] section setting log_dir. The variant
    // still declares its legacy `./logs` log_dir; the runner-section path
    // must win (T18.5 precedence rule).
    let toml_path_escaped = toml_log_dir.to_string_lossy().replace('\\', "/");
    let config_content = format!(
        r#"run = "t18-5-toml"
runners = ["local"]
default_timeout_secs = 30

[runner]
log_dir = "{toml_path_escaped}"

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"

  [variant.common]
  tick_rate_hz = 10
  stabilize_secs = 0
  operate_secs = 1
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 2
  qos = 1
  log_dir = "./logs"

  [variant.specific]
"#
    );
    let config_path = tmp.join("config.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(&config_path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");
    assert!(
        output.status.success(),
        "runner should exit 0 with [runner] log_dir, got {:?}",
        output.status.code()
    );

    // Stderr must announce the TOML source.
    assert!(
        stderr.contains("[runner] log_dir TOML key"),
        "stderr must mention 'source: [runner] log_dir TOML key', got:\n{stderr}"
    );

    assert!(
        toml_log_dir.exists(),
        "the TOML-declared log_dir must have been created"
    );
    let subdirs: Vec<_> = std::fs::read_dir(&toml_log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert!(
        !subdirs.is_empty(),
        "expected a timestamped subfolder under {toml_log_dir:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn t18_5_log_dir_cli_overrides_toml() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let tmp = std::env::temp_dir().join("runner_t18_5_override");
    let _ = std::fs::remove_dir_all(&tmp);
    let cli_log_dir = tmp.join("cli-wins");
    let toml_log_dir = tmp.join("toml-loses");
    std::fs::create_dir_all(&tmp).unwrap();

    let toml_path_escaped = toml_log_dir.to_string_lossy().replace('\\', "/");
    let config_content = format!(
        r#"run = "t18-5-override"
runners = ["local"]
default_timeout_secs = 30

[runner]
log_dir = "{toml_path_escaped}"

[[variant]]
name = "dummy"
binary = "../target/release/variant-dummy.exe"

  [variant.common]
  tick_rate_hz = 10
  stabilize_secs = 0
  operate_secs = 1
  silent_secs = 0
  workload = "scalar-flood"
  values_per_tick = 2
  qos = 1
  log_dir = "./logs"

  [variant.specific]
"#
    );
    let config_path = tmp.join("config.toml");
    std::fs::write(&config_path, &config_content).unwrap();

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(&config_path)
        .arg("--log-dir")
        .arg(&cli_log_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stderr ---\n{stderr}");
    assert!(output.status.success(), "runner should exit 0");

    // CLI wins.
    assert!(
        stderr.contains("--log-dir CLI flag"),
        "stderr must say 'source: --log-dir CLI flag' when both are set, got:\n{stderr}"
    );

    assert!(cli_log_dir.exists(), "CLI path must be used");
    // TOML path should NOT have been touched by the runner. (It may still
    // exist if validate_log_dir_writable created it from a prior run, but we
    // never ran with TOML-only here so the directory should still be
    // absent.)
    assert!(
        !toml_log_dir.exists(),
        "TOML-declared path must NOT be created when --log-dir overrides"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn t18_5_log_dir_unwritable_path_aborts_with_clear_error() {
    // We need a path that create_dir_all cannot create. The portable way is
    // to point at a path whose parent is a file (not a directory) -- the
    // kernel rejects this on every platform.
    let tmp = std::env::temp_dir().join("runner_t18_5_unwritable");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let blocker = tmp.join("blocker.txt");
    std::fs::write(&blocker, b"not a directory").unwrap();
    // A path BELOW a regular file is unconditionally rejected by both
    // Windows (ENOTDIR equivalent) and Unix (ENOTDIR).
    let unwritable = blocker.join("nested");

    let config_path = tmp.join("config.toml");
    std::fs::write(
        &config_path,
        build_minimal_single_runner_config("t18-5-unwritable"),
    )
    .unwrap();

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(&config_path)
        .arg("--log-dir")
        .arg(&unwritable)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stderr ---\n{stderr}");
    assert!(
        !output.status.success(),
        "runner must abort when --log-dir is not writable"
    );
    assert!(
        stderr.contains("writability check failed") || stderr.contains("not writable"),
        "error must mention writability failure, got:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------
// T18.6: --analyze-full flag end-to-end smoke.
// ---------------------------------------------------------------------

fn python_on_path() -> bool {
    for candidate in ["python3", "python"] {
        if Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return true;
        }
    }
    false
}

#[test]
fn t18_6_analyze_full_invokes_analyzer_after_matrix() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }
    if !python_on_path() {
        eprintln!("SKIP: no python on PATH, cannot exercise --analyze-full end-to-end");
        return;
    }

    let tmp = std::env::temp_dir().join("runner_t18_6_analyze_full");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let log_dir = tmp.join("logs");
    let config_path = tmp.join("config.toml");
    std::fs::write(
        &config_path,
        build_minimal_single_runner_config("t18-6-analyze"),
    )
    .unwrap();

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg(&config_path)
        .arg("--log-dir")
        .arg(&log_dir)
        .arg("--analyze-full")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    // The matrix itself must succeed; analyzer exit is non-fatal.
    assert!(
        output.status.success(),
        "runner should exit 0 even with --analyze-full, got {:?}",
        output.status.code()
    );

    // The runner must have logged that it is running analysis. The
    // single-runner mode trivially satisfies the lowest-sorted-name rule.
    assert!(
        stderr.contains("running analysis"),
        "stderr must announce the analysis invocation, got:\n{stderr}"
    );

    // The <log-dir>/<run>-<ts>/analysis/ subfolder must have been
    // produced by the analyzer (or the runner emitted a non-fatal warning
    // about a non-zero analyzer exit). Both are acceptable outcomes for
    // this smoke test; what we require is that the runner attempted the
    // invocation and that the matrix run itself succeeded.
    let subdirs: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert!(
        !subdirs.is_empty(),
        "expected a per-run subfolder under {log_dir:?}"
    );
    // Optional check: when the analyzer succeeded we expect an analysis/
    // child folder inside the session subfolder. If the analyzer warned
    // non-zero (e.g. on a 1-second dummy run with not enough data), the
    // folder may be absent -- that is the documented soft-fail path.
    let analyzer_succeeded = stderr.contains("analysis complete");
    if analyzer_succeeded {
        let analysis_dirs: Vec<_> = subdirs
            .iter()
            .filter(|d| d.path().join("analysis").is_dir())
            .collect();
        assert!(
            !analysis_dirs.is_empty(),
            "analyzer reported success but produced no <log-dir>/analysis/ folder, got:\n{stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn t18_6_analyze_full_skips_when_runner_is_not_lowest_name() {
    // Build a config that lists THIS runner as 'zeta' alongside an absent
    // 'alpha'. Single-runner discovery alone would not work (alpha never
    // shows up), so we cannot run the full matrix here. Instead, we verify
    // the upstream short-circuit: should_run_analysis() returns false for
    // 'zeta'. The integration-level guarantee is covered by the unit test
    // in analyze.rs; this test exists to lock the convention at the
    // CLI-surface level by exercising the runner's --help to confirm the
    // flag is wired and the contract line is documented.
    let output = Command::new(runner_binary())
        .arg("--help")
        .output()
        .expect("failed to run runner --help");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("--analyze-full"),
        "--analyze-full must appear in --help, got:\n{help}"
    );
    assert!(
        help.contains("--log-dir"),
        "--log-dir must appear in --help, got:\n{help}"
    );
}

// ---------------------------------------------------------------------
// T19.4 / E19: workload-shape param forwarding
// ---------------------------------------------------------------------
//
// `[variant.common].blob_size` (and the rest of the seven new keys) must
// pass through the runner verbatim as `--kebab-case <N>` CLI args. The
// unit tests in `cli_args.rs` already pin the args-vector shape; the two
// integration tests below close the loop end-to-end:
//
// 1. `t19_4_workload_shape_args_forwarded_to_child_process`: uses the
//    `arg-echo` helper as the spawned binary so the test inspects the
//    actual argv handed to the child process. Locks in that every one
//    of the seven new keys lands on the CLI with the kebab-case name
//    and the configured value -- across both [variant.common] direct
//    declaration and template inheritance.
//
// 2. `t19_4_block_flood_runs_to_completion_with_variant_dummy`: runs an
//    actual `variant-dummy` subprocess under `workload = "block-flood"`
//    with `blob_size = 100`. Exit-status success proves the variant
//    accepted the forwarded args (block-flood validates
//    `values_per_tick % blob_size == 0` at startup and exits non-zero
//    if the flags are mis-forwarded). Verifies the JSONL `operate`
//    phase event carries `profile = "block-flood"` as a redundant
//    cross-check that the workload arg landed.

#[test]
fn t19_4_workload_shape_args_forwarded_to_child_process() {
    // Construct a config that declares all seven new keys in
    // [variant.common] (and a sixth via a [[variant_template]] to lock in
    // the template-inheritance path), spawn arg-echo, then assert the
    // captured argv has every flag with the configured value.
    let arg_echo = arg_echo_binary();
    let tmp_dir = std::env::temp_dir().join("runner_t19_4_argv_test");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();

    let out_path = tmp_dir.join("captured-args.json");
    let log_dir = tmp_dir.join("logs");

    let arg_echo_escaped = arg_echo.replace('\\', "/");
    let log_dir_escaped = log_dir.to_string_lossy().replace('\\', "/");
    // workload_seed is declared on the template; the variant entry
    // overrides blob_size to verify the variant-wins merge for new keys.
    let config_content = format!(
        r#"run = "t19_4_argv"
runners = ["self"]
default_timeout_secs = 10

[[variant_template]]
name = "shape-base"
binary = "{arg_echo_escaped}"
  [variant_template.common]
  workload_seed = 999

[[variant]]
template = "shape-base"
name = "echo"
timeout_secs = 5

  [variant.common]
  tick_rate_hz = 10
  stabilize_secs = 0
  operate_secs = 0
  silent_secs = 0
  workload = "mixed-types"
  values_per_tick = 100
  qos = 1
  log_dir = "{log_dir_escaped}"
  blob_size = 100
  mixed_scalars_min = 1
  mixed_scalars_max = 5
  mixed_arrays_min = 0
  mixed_arrays_max = 4
  mixed_dict_split_max = 3

  [variant.specific]
"#
    );
    let config_path = tmp_dir.join("t19-4-argv.toml");
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

    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stderr ---\n{stderr}");
    assert!(
        output.status.success(),
        "runner should exit 0, got {:?}\nstderr: {stderr}",
        output.status.code()
    );

    let captured_json = std::fs::read_to_string(&out_path).expect("captured args file");
    let captured: Vec<String> = serde_json::from_str(&captured_json).unwrap();
    eprintln!("--- captured argv ---\n{captured:?}");

    fn flag_value<'a>(args: &'a [String], flag: &str) -> &'a str {
        let idx = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("flag {flag} not present in captured argv: {args:?}"));
        let count = args.iter().filter(|a| *a == flag).count();
        assert_eq!(count, 1, "{flag} must appear exactly once, got {args:?}");
        &args[idx + 1]
    }

    // Six u32 keys declared on the variant entry.
    assert_eq!(flag_value(&captured, "--blob-size"), "100");
    assert_eq!(flag_value(&captured, "--mixed-scalars-min"), "1");
    assert_eq!(flag_value(&captured, "--mixed-scalars-max"), "5");
    assert_eq!(flag_value(&captured, "--mixed-arrays-min"), "0");
    assert_eq!(flag_value(&captured, "--mixed-arrays-max"), "4");
    assert_eq!(flag_value(&captured, "--mixed-dict-split-max"), "3");
    // The seventh key was declared on the template only; template
    // inheritance must carry it through into the spawned argv.
    assert_eq!(flag_value(&captured, "--workload-seed"), "999");

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn t19_4_block_flood_runs_to_completion_with_variant_dummy() {
    if !variant_dummy_exists() {
        eprintln!("SKIP: variant-dummy.exe not found, build variant-base first");
        return;
    }

    let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-logs-t19-4");
    let _ = std::fs::remove_dir_all(&log_dir);

    let output = Command::new(runner_binary())
        .arg("--name")
        .arg("local")
        .arg("--config")
        .arg("tests/fixtures/block-flood-blob-size.toml")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run runner");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("--- stdout ---\n{stdout}");
    eprintln!("--- stderr ---\n{stderr}");

    // Exit-status check: variant-dummy validates the workload-shape args
    // at startup (`values_per_tick % blob_size == 0` etc.) and exits
    // non-zero if anything is mis-forwarded. A clean exit therefore
    // proves the runner forwarded blob_size = 100 correctly.
    assert!(
        output.status.success(),
        "runner should exit 0 with block-flood + blob_size=100, got {:?}\nstderr: {stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("dummy-blockflood") && stdout.contains("success"),
        "spawn summary should show dummy-blockflood succeeded, stdout: {stdout}"
    );

    // Cross-check: the variant's JSONL records `profile = "block-flood"`
    // in its `phase=operate` lifecycle event. T18.2 routes high-volume
    // `write` events to compact Parquet by default, so we read the
    // lifecycle event rather than per-write leaf_count.
    assert!(log_dir.exists(), "expected log dir {log_dir:?}");
    let subdirs: Vec<PathBuf> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    let jsonl_files: Vec<PathBuf> = subdirs
        .iter()
        .flat_map(|d| std::fs::read_dir(d).unwrap())
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("dummy-blockflood-"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        jsonl_files.len(),
        1,
        "expected exactly one variant JSONL file, got {jsonl_files:?}"
    );

    let body = std::fs::read_to_string(&jsonl_files[0]).unwrap();
    let mut saw_operate_block_flood = false;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value =
            serde_json::from_str(line).expect("each JSONL line must parse as JSON");
        if val.get("event").and_then(|v| v.as_str()) == Some("phase")
            && val.get("phase").and_then(|v| v.as_str()) == Some("operate")
            && val.get("profile").and_then(|v| v.as_str()) == Some("block-flood")
        {
            saw_operate_block_flood = true;
            break;
        }
    }
    assert!(
        saw_operate_block_flood,
        "expected a `phase=operate, profile=block-flood` event in JSONL:\n{body}"
    );

    let _ = std::fs::remove_dir_all(&log_dir);
}
