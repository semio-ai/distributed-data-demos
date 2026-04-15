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

fn variant_dummy_exists() -> bool {
    Path::new("../variant-base/target/release/variant-dummy.exe").exists()
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
