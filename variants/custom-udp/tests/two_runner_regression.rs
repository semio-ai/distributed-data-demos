//! Two-runner-on-localhost regression test for the custom-udp variant.
//!
//! Two test functions:
//!
//! - `two_runner_regression_qos4_no_panic` (T10.6a) -- guards the T10.4
//!   fix (TCP framing panic at QoS 4 when a peer produces an `total_len`
//!   outside `HEADER_FIXED_SIZE..=max_buffer_size`) and asserts >=99%
//!   cross-peer delivery in the post-EOT operate window
//!   (T12.7-custom-udp).
//! - `two_runner_regression_qos1_no_loss` (T12.7-custom-udp) -- exercises
//!   the UDP EOT path landed in T12.3 and asserts >=99% cross-peer
//!   delivery for QoS 1 in the operate window.
//!
//! Operate-window scoping (per `metak-shared/api-contracts/eot-protocol.md`):
//!
//! - `operate_start_ts` = `ts` of the writer's first `phase` event with
//!   `phase = "operate"`.
//! - `eot_sent_ts` = `ts` of the writer's `eot_sent` event.
//! - Writer denominator: `write` events on the writer with
//!   `ts in [operate_start_ts, eot_sent_ts]`.
//! - Cross-peer numerator: `receive` events on the receiver with
//!   `writer = <peer>` and `ts in [operate_start_ts, eot_sent_ts]` (the
//!   WRITER's window).
//!
//! Gated behind `#[ignore]` because they depend on pre-built release
//! binaries and are end-to-end heavy. Run with:
//!
//! ```text
//! cargo test --release -p variant-custom-udp -- --ignored two_runner_regression --nocapture
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Both test fns drive two runner subprocesses that perform peer
/// discovery on localhost. If two cargo test threads run them
/// concurrently each "alice" sees the other test's "bob" during
/// discovery, the runner rejects the config-hash mismatch, and the
/// spawn fails before reaching operate. This mutex forces in-binary
/// serialisation so the spec command
/// `cargo test --release -p variant-custom-udp -- --ignored two_runner_regression --nocapture`
/// works without `--test-threads=1`.
static REGRESSION_LOCK: Mutex<()> = Mutex::new(());

const QOS4_FIXTURE_PATH: &str = "tests/fixtures/two-runner-custom-udp-qos4.toml";
const QOS4_RUN_NAME: &str = "custom-udp-t104-validation";
const QOS4_SPAWN_NAME: &str = "custom-udp-10x1000hz";
const QOS4_LEVEL: u8 = 4;

const QOS1_FIXTURE_PATH: &str = "tests/fixtures/two-runner-custom-udp-qos1-eot.toml";
const QOS1_RUN_NAME: &str = "custom-udp-t123-eot-udp";
const QOS1_SPAWN_NAME: &str = "custom-udp-eot-qos1";
const QOS1_LEVEL: u8 = 1;

const DELIVERY_THRESHOLD: f64 = 0.99;
const TEST_BUDGET: Duration = Duration::from_secs(120);

#[test]
#[ignore]
fn two_runner_regression_qos4_no_panic() {
    run_two_runner_case(TwoRunnerCase {
        tag: "T12.7-custom-udp/qos4",
        fixture_rel: QOS4_FIXTURE_PATH,
        run_name: QOS4_RUN_NAME,
        spawn_name: QOS4_SPAWN_NAME,
        qos: QOS4_LEVEL,
        threshold: DELIVERY_THRESHOLD,
    });
}

#[test]
#[ignore]
fn two_runner_regression_qos1_no_loss() {
    run_two_runner_case(TwoRunnerCase {
        tag: "T12.7-custom-udp/qos1",
        fixture_rel: QOS1_FIXTURE_PATH,
        run_name: QOS1_RUN_NAME,
        spawn_name: QOS1_SPAWN_NAME,
        qos: QOS1_LEVEL,
        threshold: DELIVERY_THRESHOLD,
    });
}

/// Inputs for one two-runner regression invocation.
struct TwoRunnerCase {
    tag: &'static str,
    fixture_rel: &'static str,
    run_name: &'static str,
    spawn_name: &'static str,
    qos: u8,
    threshold: f64,
}

/// Drive the full localhost two-runner flow: read fixture, substitute
/// log_dir, spawn alice + bob, wait, then validate operate-window
/// delivery >= threshold in both directions.
fn run_two_runner_case(case: TwoRunnerCase) {
    // Serialise concurrent test threads. See REGRESSION_LOCK for why.
    // PoisonError is mapped to the inner guard so a panic in a previous
    // case does not cascade into a misleading lock-poisoned panic in
    // the next.
    let _guard = REGRESSION_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let TwoRunnerCase {
        tag,
        fixture_rel,
        run_name,
        spawn_name,
        qos,
        threshold,
    } = case;

    let repo_root: PathBuf = repo_root();
    let runner_bin: PathBuf = repo_root
        .join("runner")
        .join("target")
        .join("release")
        .join("runner.exe");
    let variant_bin: PathBuf = repo_root
        .join("variants")
        .join("custom-udp")
        .join("target")
        .join("release")
        .join("variant-custom-udp.exe");

    if !runner_bin.exists() {
        eprintln!(
            "[{tag}] SKIP: runner binary missing at {}. \
             Build it with `cargo build --release -p runner` from the repo root.",
            runner_bin.display()
        );
        return;
    }
    if !variant_bin.exists() {
        eprintln!(
            "[{tag}] SKIP: variant-custom-udp binary missing at {}. \
             Build it with `cargo build --release -p variant-custom-udp` from the repo root.",
            variant_bin.display()
        );
        return;
    }

    let tmp: tempfile::TempDir = tempfile::tempdir().expect("failed to create tempdir");
    let tmp_path: &Path = tmp.path();
    let tmp_str: String = tmp_path
        .to_str()
        .expect("tempdir path is not valid UTF-8")
        .replace('\\', "/");

    // Read the fixture and substitute the log_dir line. The fixture is
    // off-limits per task spec; we only edit the in-memory copy and write
    // it to <tmpdir>/config.toml.
    let fixture_abs: PathBuf = repo_root.join("variants/custom-udp").join(fixture_rel);
    let fixture_text: String = std::fs::read_to_string(&fixture_abs)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", fixture_abs.display()));
    assert!(
        fixture_text.contains("log_dir = \"./logs\""),
        "[{tag}] fixture {} does not contain expected `log_dir = \"./logs\"` line",
        fixture_abs.display()
    );
    let patched_text: String =
        fixture_text.replace("log_dir = \"./logs\"", &format!("log_dir = \"{tmp_str}\""));

    let config_path: PathBuf = tmp_path.join("config.toml");
    std::fs::write(&config_path, &patched_text)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", config_path.display()));

    // Spawn alice and bob. CWD = repo root so the fixture's relative
    // `binary = "variants/custom-udp/target/release/variant-custom-udp.exe"`
    // resolves correctly.
    let test_start: Instant = Instant::now();
    let mut alice: Child = spawn_runner(&runner_bin, &repo_root, "alice", &config_path);
    let mut bob: Child = spawn_runner(&runner_bin, &repo_root, "bob", &config_path);

    // Wait for both with a 120 s budget. Hard-kill on timeout.
    let alice_outcome: ProcessOutcome = wait_with_budget(&mut alice, TEST_BUDGET);
    let alice_elapsed: Duration = test_start.elapsed();
    let remaining: Duration = TEST_BUDGET.saturating_sub(alice_elapsed);
    let bob_outcome: ProcessOutcome = wait_with_budget(&mut bob, remaining);
    let wall_time: Duration = test_start.elapsed();

    // Always finalize stdout/stderr capture, even on timeout, so the
    // failure message is informative.
    let alice_capture: Capture = alice_outcome.capture;
    let bob_capture: Capture = bob_outcome.capture;

    if !alice_outcome.exited {
        let _ = alice.kill();
        panic!(
            "[{tag}] alice timed out after {:?}; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
            wall_time, alice_capture.stdout, alice_capture.stderr
        );
    }
    if !bob_outcome.exited {
        let _ = bob.kill();
        panic!(
            "[{tag}] bob timed out after {:?}; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
            wall_time, alice_capture.stdout, bob_capture.stderr
        );
    }

    let alice_status = alice_outcome.status.expect("alice exit status missing");
    let bob_status = bob_outcome.status.expect("bob exit status missing");

    assert!(
        alice_status.success(),
        "[{tag}] alice exited non-zero ({:?}); stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
        alice_status.code(),
        alice_capture.stdout,
        alice_capture.stderr
    );
    assert!(
        bob_status.success(),
        "[{tag}] bob exited non-zero ({:?}); stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
        bob_status.code(),
        bob_capture.stdout,
        bob_capture.stderr
    );

    // Stderr must NOT contain panic (case-insensitive). The clean-shutdown
    // message `[custom-udp] TCP framing: dropping peer ...` IS allowed --
    // its presence proves the T10.4 regression-prone code path was
    // exercised at qos 4 (and is harmless if it ever appears at qos 1).
    let combined_stderr: String = format!("{}\n{}", alice_capture.stderr, bob_capture.stderr);
    let combined_stderr_lc: String = combined_stderr.to_lowercase();
    assert!(
        !combined_stderr_lc.contains("panic"),
        "[{tag}] combined stderr contains 'panic'; stderr=<<<\n{combined_stderr}>>>"
    );

    // Locate the session subfolder. The runner creates
    // `<tmpdir>/<run-name>-<launch-ts>/`.
    let session_dir: PathBuf = find_session_dir(tmp_path, run_name).unwrap_or_else(|| {
        panic!(
            "[{tag}] no session subfolder matching `{run_name}-*` found under {} \
             after spawn. Tempdir entries: {:?}",
            tmp_path.display(),
            list_dir(tmp_path)
        )
    });

    // Locate the two expected JSONL files.
    let alice_log: PathBuf = session_dir.join(format!("{spawn_name}-alice-{run_name}.jsonl"));
    let bob_log: PathBuf = session_dir.join(format!("{spawn_name}-bob-{run_name}.jsonl"));
    assert!(
        alice_log.exists(),
        "[{tag}] expected alice JSONL not found: {}",
        alice_log.display()
    );
    assert!(
        bob_log.exists(),
        "[{tag}] expected bob JSONL not found: {}",
        bob_log.display()
    );

    // Confirm exactly the two variant JSONL files we expect (one per
    // runner). The runner also writes clock-sync sibling JSONL files
    // into the same session subfolder; we filter to the per-variant
    // log file pattern `<spawn-name>-<runner>-<run>.jsonl`.
    let variant_jsonl_files: Vec<PathBuf> = list_jsonl(&session_dir)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.starts_with(&format!("{spawn_name}-")))
        })
        .collect();
    assert_eq!(
        variant_jsonl_files.len(),
        2,
        "[{tag}] expected exactly 2 variant JSONL files matching `{spawn_name}-*` \
         in {}; found {:?}",
        session_dir.display(),
        variant_jsonl_files
    );

    // Per-writer operate-window scoping. The window is
    // [operate_start_ts, eot_sent_ts] for that WRITER's own log; the
    // receiver's `receive` events are filtered against the WRITER's
    // window per the analysis-tool semantics in eot-protocol.md.
    let alice_window: OperateWindow = parse_operate_window(&alice_log, tag, "alice");
    let bob_window: OperateWindow = parse_operate_window(&bob_log, tag, "bob");

    let alice_writes_in_window: u64 = count_writes_in_window(&alice_log, &alice_window);
    let bob_writes_in_window: u64 = count_writes_in_window(&bob_log, &bob_window);

    // bob's receives whose writer == alice with ts in alice's window.
    let bob_recv_from_alice: u64 =
        count_cross_peer_receives_in_window(&bob_log, "alice", &alice_window);
    // alice's receives whose writer == bob with ts in bob's window.
    let alice_recv_from_bob: u64 =
        count_cross_peer_receives_in_window(&alice_log, "bob", &bob_window);

    let alice_to_bob_ratio: f64 = if alice_writes_in_window == 0 {
        0.0
    } else {
        bob_recv_from_alice as f64 / alice_writes_in_window as f64
    };
    let bob_to_alice_ratio: f64 = if bob_writes_in_window == 0 {
        0.0
    } else {
        alice_recv_from_bob as f64 / bob_writes_in_window as f64
    };

    println!(
        "[{tag}] alice -> bob qos{}: {}/{} ({:.2}%) in [op_start..eot_sent] {}",
        qos,
        bob_recv_from_alice,
        alice_writes_in_window,
        alice_to_bob_ratio * 100.0,
        if alice_to_bob_ratio >= threshold {
            "OK"
        } else {
            "FAIL"
        }
    );
    println!(
        "[{tag}] bob -> alice qos{}: {}/{} ({:.2}%) in [op_start..eot_sent] {}",
        qos,
        alice_recv_from_bob,
        bob_writes_in_window,
        bob_to_alice_ratio * 100.0,
        if bob_to_alice_ratio >= threshold {
            "OK"
        } else {
            "FAIL"
        }
    );
    println!("[{tag}] wall-time: {:?}", wall_time);

    assert!(
        alice_writes_in_window > 0,
        "[{tag}] alice produced zero writes in operate window -- spawn never reached operate phase \
         (op_start={}, eot_sent={})",
        alice_window.operate_start_ts,
        alice_window.eot_sent_ts
    );
    assert!(
        bob_writes_in_window > 0,
        "[{tag}] bob produced zero writes in operate window -- spawn never reached operate phase \
         (op_start={}, eot_sent={})",
        bob_window.operate_start_ts,
        bob_window.eot_sent_ts
    );
    assert!(
        alice_to_bob_ratio >= threshold,
        "[{tag}] alice -> bob delivery {:.4} below threshold {:.2}; \
         received={}, written_in_window={}",
        alice_to_bob_ratio,
        threshold,
        bob_recv_from_alice,
        alice_writes_in_window
    );
    assert!(
        bob_to_alice_ratio >= threshold,
        "[{tag}] bob -> alice delivery {:.4} below threshold {:.2}; \
         received={}, written_in_window={}",
        bob_to_alice_ratio,
        threshold,
        alice_recv_from_bob,
        bob_writes_in_window
    );
}

/// Find the repository root by walking up from this crate's manifest.
/// `CARGO_MANIFEST_DIR` is `<repo>/variants/custom-udp` for this crate.
fn repo_root() -> PathBuf {
    let manifest_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("failed to derive repo root from CARGO_MANIFEST_DIR")
}

/// Spawn one runner child process with stdout/stderr piped.
fn spawn_runner(runner_bin: &Path, repo_root: &Path, name: &str, config_path: &Path) -> Child {
    Command::new(runner_bin)
        .current_dir(repo_root)
        .args([
            "--name",
            name,
            "--config",
            config_path
                .to_str()
                .expect("config path is not valid UTF-8"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn runner --name {name}: {e}"))
}

/// Result of waiting on a child with a budget.
struct ProcessOutcome {
    exited: bool,
    status: Option<std::process::ExitStatus>,
    capture: Capture,
}

/// Captured stdout/stderr text.
struct Capture {
    stdout: String,
    stderr: String,
}

/// Poll `try_wait` until the child exits or the budget elapses. After exit
/// (or timeout), drain stdout/stderr fully so the buffers cannot deadlock
/// the child during operate.
fn wait_with_budget(child: &mut Child, budget: Duration) -> ProcessOutcome {
    let start: Instant = Instant::now();
    let mut status: Option<std::process::ExitStatus> = None;
    let mut exited: bool = false;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                exited = true;
                break;
            }
            Ok(None) => {
                if start.elapsed() >= budget {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }

    let stdout: String = match child.stdout.take() {
        Some(mut s) => {
            let mut buf: String = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        }
        None => String::new(),
    };
    let stderr: String = match child.stderr.take() {
        Some(mut s) => {
            let mut buf: String = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        }
        None => String::new(),
    };

    ProcessOutcome {
        exited,
        status,
        capture: Capture { stdout, stderr },
    }
}

/// Find the runner-created session subfolder named `<run>-<ts>` directly
/// under `parent`.
fn find_session_dir(parent: &Path, run_name: &str) -> Option<PathBuf> {
    let prefix: String = format!("{run_name}-");
    let entries = std::fs::read_dir(parent).ok()?;
    for entry in entries.flatten() {
        let path: PathBuf = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(stem) = path.file_name().and_then(|n| n.to_str()) {
            if stem.starts_with(&prefix) {
                return Some(path);
            }
        }
    }
    None
}

/// List immediate children of a directory (for diagnostics).
fn list_dir(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(it) => it
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// List all `*.jsonl` files in a directory (non-recursive).
fn list_jsonl(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(it) = std::fs::read_dir(dir) {
        for entry in it.flatten() {
            let path: PathBuf = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Operate-window boundaries for one writer's JSONL.
struct OperateWindow {
    /// `ts` of the first `phase` event with `phase == "operate"` -- the
    /// inclusive lower bound of the operate window.
    operate_start_ts: String,
    /// `ts` of the writer's `eot_sent` event -- the inclusive upper
    /// bound of the operate window. Per `eot-protocol.md`, this is the
    /// authoritative end of writes.
    eot_sent_ts: String,
}

/// Parse the writer's JSONL and extract `[operate_start_ts, eot_sent_ts]`.
/// Panics with a descriptive message if either boundary is missing --
/// these tests REQUIRE the EOT phase landed in T12.3.
fn parse_operate_window(path: &Path, tag: &str, runner: &str) -> OperateWindow {
    let text: String = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("[{tag}] failed to read JSONL {}: {e}", path.display()));
    let mut operate_start_ts: Option<String> = None;
    let mut eot_sent_ts: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event: &str = match value.get("event").and_then(|e| e.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let ts: &str = match value.get("ts").and_then(|t| t.as_str()) {
            Some(s) => s,
            None => continue,
        };
        match event {
            "phase" => {
                if value.get("phase").and_then(|p| p.as_str()) == Some("operate")
                    && operate_start_ts.is_none()
                {
                    operate_start_ts = Some(ts.to_string());
                }
            }
            "eot_sent" => {
                if eot_sent_ts.is_none() {
                    eot_sent_ts = Some(ts.to_string());
                }
            }
            _ => {}
        }
    }
    let operate_start_ts: String = operate_start_ts.unwrap_or_else(|| {
        panic!(
            "[{tag}] {runner} JSONL ({}) missing `phase` event with phase=operate -- \
             spawn never entered operate phase",
            path.display()
        )
    });
    let eot_sent_ts: String = eot_sent_ts.unwrap_or_else(|| {
        panic!(
            "[{tag}] {runner} JSONL ({}) missing `eot_sent` event -- variant did not \
             emit EOT (T12.3 regression?)",
            path.display()
        )
    });
    OperateWindow {
        operate_start_ts,
        eot_sent_ts,
    }
}

/// Count `write` events whose `ts` is within the writer's own operate
/// window. RFC 3339 with nanosecond precision sorts lexicographically,
/// so plain string comparison is the right tool here (per
/// jsonl-log-schema.md, all timestamps share the same fixed-width format
/// and zero-padded UTC `Z` suffix).
fn count_writes_in_window(path: &Path, window: &OperateWindow) -> u64 {
    let text: String = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read JSONL {}: {e}", path.display()));
    let mut count: u64 = 0;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("event").and_then(|e| e.as_str()) != Some("write") {
            continue;
        }
        let ts: &str = match value.get("ts").and_then(|t| t.as_str()) {
            Some(s) => s,
            None => continue,
        };
        if ts >= window.operate_start_ts.as_str() && ts <= window.eot_sent_ts.as_str() {
            count += 1;
        }
    }
    count
}

/// Count `receive` events on this log whose `writer` matches `peer_name`
/// and whose `ts` is within the WRITER's operate window (passed in by
/// the caller). Mirrors the analysis tool's cross-peer scoping.
fn count_cross_peer_receives_in_window(
    path: &Path,
    peer_name: &str,
    writer_window: &OperateWindow,
) -> u64 {
    let text: String = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read JSONL {}: {e}", path.display()));
    let mut count: u64 = 0;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("event").and_then(|e| e.as_str()) != Some("receive") {
            continue;
        }
        if value.get("writer").and_then(|w| w.as_str()) != Some(peer_name) {
            continue;
        }
        let ts: &str = match value.get("ts").and_then(|t| t.as_str()) {
            Some(s) => s,
            None => continue,
        };
        if ts >= writer_window.operate_start_ts.as_str() && ts <= writer_window.eot_sent_ts.as_str()
        {
            count += 1;
        }
    }
    count
}
