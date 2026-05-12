//! T14.19 two-runner regression: high-rate symmetric TCP qos4 in Single
//! mode must NOT deadlock. Delivery may be near zero ("log everything with
//! bad latency" per user intent), but the spawn MUST exit with
//! `status=success` -- which is the bar the T14.17 classifier uses for
//! "completed".
//!
//! Pre-T14.19, both runners wedged inside `write_all` on QoS 4 outbound
//! TCP because neither side could call `poll_receive` between writes to
//! drain the peer's recv buffer. Post-T14.19, `SO_SNDTIMEO` on outbound
//! TCP turns the wedge into a typed `TimedOut` error -> the peer is
//! dropped from the broadcast set -> the publish loop exits naturally ->
//! EOT phase is reached over the T14.18 control side-channel -> both
//! sides exit cleanly.
//!
//! Gated behind `#[ignore]` because it depends on pre-built release
//! binaries and is end-to-end heavy. Run with:
//!
//! ```text
//! cargo test --release -p variant-custom-udp -- --ignored two_runner_t14_19 --nocapture
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serialise concurrent test threads. Same rationale as
/// `two_runner_regression.rs`: peer-discovery on localhost rejects
/// cross-test bleed-through unless we hold a process-wide lock.
static T14_19_LOCK: Mutex<()> = Mutex::new(());

const FIXTURE_PATH: &str = "tests/fixtures/two-runner-custom-udp-t14-19-stress.toml";
const RUN_NAME: &str = "custom-udp-t14-19";
const SPAWN_NAME: &str = "custom-udp-t14-19-stress-single";

/// 60 s budget: with `default_timeout_secs = 30` per fixture, even the
/// worst-case (one side wedged the full 30 s before its SO_SNDTIMEO
/// kicks in for the last peer) finishes inside the timeout. We give
/// double to cover stderr drain + cleanup.
const TEST_BUDGET: Duration = Duration::from_secs(60);

#[test]
#[ignore]
fn two_runner_t14_19_qos4_single_completes() {
    let _guard = T14_19_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

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
            "[T14.19] SKIP: runner binary missing at {}. \
             Build it with `cargo build --release -p runner`.",
            runner_bin.display()
        );
        return;
    }
    if !variant_bin.exists() {
        eprintln!(
            "[T14.19] SKIP: variant-custom-udp binary missing at {}. \
             Build it with `cargo build --release -p variant-custom-udp`.",
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

    // Read the fixture and substitute the log_dir line.
    let fixture_abs: PathBuf = repo_root.join("variants/custom-udp").join(FIXTURE_PATH);
    let fixture_text: String = std::fs::read_to_string(&fixture_abs)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", fixture_abs.display()));
    assert!(
        fixture_text.contains("log_dir = \"./logs\""),
        "[T14.19] fixture {} does not contain expected `log_dir = \"./logs\"` line",
        fixture_abs.display()
    );
    let patched_text: String =
        fixture_text.replace("log_dir = \"./logs\"", &format!("log_dir = \"{tmp_str}\""));

    let config_path: PathBuf = tmp_path.join("config.toml");
    std::fs::write(&config_path, &patched_text)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", config_path.display()));

    let test_start: Instant = Instant::now();
    let mut alice: Child = spawn_runner(&runner_bin, &repo_root, "alice", &config_path);
    let mut bob: Child = spawn_runner(&runner_bin, &repo_root, "bob", &config_path);

    let alice_outcome: ProcessOutcome = wait_with_budget(&mut alice, TEST_BUDGET);
    let alice_elapsed: Duration = test_start.elapsed();
    let remaining: Duration = TEST_BUDGET.saturating_sub(alice_elapsed);
    let bob_outcome: ProcessOutcome = wait_with_budget(&mut bob, remaining);
    let wall_time: Duration = test_start.elapsed();

    let alice_capture: Capture = alice_outcome.capture;
    let bob_capture: Capture = bob_outcome.capture;

    if !alice_outcome.exited {
        let _ = alice.kill();
        panic!(
            "[T14.19] alice never exited within {:?} -- the SO_SNDTIMEO \
             fix did NOT unwedge the deadlock; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
            wall_time, alice_capture.stdout, alice_capture.stderr
        );
    }
    if !bob_outcome.exited {
        let _ = bob.kill();
        panic!(
            "[T14.19] bob never exited within {:?} -- the SO_SNDTIMEO \
             fix did NOT unwedge the deadlock; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
            wall_time, bob_capture.stdout, bob_capture.stderr
        );
    }

    let alice_status = alice_outcome.status.expect("alice exit status missing");
    let bob_status = bob_outcome.status.expect("bob exit status missing");

    // The bar: BOTH runners exit cleanly. Delivery percentage is NOT
    // asserted; the user's "log everything with bad latency" intent
    // accepts near-zero delivery as a valid outcome, as long as the
    // spawn doesn't deadlock.
    assert!(
        alice_status.success(),
        "[T14.19] alice exited non-zero ({:?}) in {:?}; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
        alice_status.code(),
        wall_time,
        alice_capture.stdout,
        alice_capture.stderr
    );
    assert!(
        bob_status.success(),
        "[T14.19] bob exited non-zero ({:?}) in {:?}; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
        bob_status.code(),
        wall_time,
        bob_capture.stdout,
        bob_capture.stderr
    );

    let combined_stderr: String = format!("{}\n{}", alice_capture.stderr, bob_capture.stderr);
    let combined_stderr_lc: String = combined_stderr.to_lowercase();
    assert!(
        !combined_stderr_lc.contains("panic"),
        "[T14.19] combined stderr contains 'panic'; stderr=<<<\n{combined_stderr}>>>"
    );

    // Locate the session subfolder and confirm both JSONL files exist
    // with the expected per-mode spawn naming. The `single` suffix
    // comes from runner threading-mode-expansion (T14.8).
    let session_dir: PathBuf = find_session_dir(tmp_path, RUN_NAME).unwrap_or_else(|| {
        panic!(
            "[T14.19] no session subfolder matching `{RUN_NAME}-*` found under {} \
             after spawn",
            tmp_path.display()
        )
    });

    let alice_log: PathBuf = session_dir.join(format!("{SPAWN_NAME}-alice-{RUN_NAME}.jsonl"));
    let bob_log: PathBuf = session_dir.join(format!("{SPAWN_NAME}-bob-{RUN_NAME}.jsonl"));
    assert!(
        alice_log.exists(),
        "[T14.19] expected alice JSONL not found: {}",
        alice_log.display()
    );
    assert!(
        bob_log.exists(),
        "[T14.19] expected bob JSONL not found: {}",
        bob_log.display()
    );

    // Sanity: each side must have reached at least the `eot_sent`
    // event. That's the strongest single-line marker that the operate
    // phase exited cleanly (the prior deadlock blocked BEFORE EOT).
    assert_log_contains_event(&alice_log, "eot_sent");
    assert_log_contains_event(&bob_log, "eot_sent");

    println!("[T14.19] alice+bob both reached eot_sent; wall-time={wall_time:?} -- PASS");
}

/// Find the repository root by walking up from this crate's manifest.
fn repo_root() -> PathBuf {
    let manifest_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("failed to derive repo root from CARGO_MANIFEST_DIR")
}

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

struct ProcessOutcome {
    exited: bool,
    status: Option<std::process::ExitStatus>,
    capture: Capture,
}

struct Capture {
    stdout: String,
    stderr: String,
}

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

fn assert_log_contains_event(path: &Path, event: &str) {
    let text: String = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read JSONL {}: {e}", path.display()));
    let needle: String = format!("\"event\":\"{event}\"");
    assert!(
        text.contains(&needle),
        "[T14.19] expected `{event}` event in {}; file was {} bytes",
        path.display(),
        text.len()
    );
}
