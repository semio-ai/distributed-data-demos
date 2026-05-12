//! T14.4 regression: run the Hybrid two-runner fixture on QoS 4 in
//! BOTH threading modes via the runner's `threading_modes` expansion.
//! Each expanded spawn must complete with non-zero writes inside its
//! operate window and non-zero cross-peer receives.
//!
//! QoS 4 exercises the TCP path end-to-end (the multi-mode per-peer
//! TCP reader thread is the new T14.4 codepath). Multi mode at 10 K
//! msg/s symmetric on localhost is expected to deliver substantial
//! cross-receives; Single mode is also TCP-backed and reliable so
//! delivery is similarly substantial.
//!
//! Gated `#[ignore]` so default `cargo test` stays fast. Invoke via:
//!
//! ```text
//! cargo test --release -p variant-hybrid -- --ignored two_runner_threading_modes --nocapture
//! ```

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serialise the test inside this binary against itself if multiple
/// `#[test]` are present. The discovery layer uses fixed multicast
/// addresses; two concurrent two-runner spawns would cross-talk.
static THREADING_MODES_LOCK: Mutex<()> = Mutex::new(());

const FIXTURE: &str = "tests/fixtures/two-runner-hybrid-100x100hz-both-modes.toml";
const RUN_NAME: &str = "hybrid-t144-both-modes";
const SPAWN_BASE: &str = "hybrid-t144";
const RUNNERS: [&str; 2] = ["alice", "bob"];

/// 1 QoS x 2 threading modes x 2 runners (writer+reader) = 4 JSONL
/// files. Each spawn takes ~6 s wall time; ~90 s budget covers both
/// spawns plus barrier overhead with margin.
const TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Default, Clone)]
struct LogContent {
    operate_start_ts: Option<String>,
    eot_sent_ts: Option<String>,
    write_ts: Vec<String>,
    receive_ts: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct LogEntry {
    spawn_name: String,
    runner: String,
    content: LogContent,
}

#[test]
#[ignore]
fn two_runner_threading_modes_qos4_both_modes() {
    let _guard = THREADING_MODES_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let repo_root = repo_root();
    if !binaries_present(&repo_root) {
        eprintln!(
            "[T14.4-hybrid] SKIPPED: prebuilt binaries missing. \
             Build with `cargo build --release -p runner` and \
             `cargo build --release -p variant-hybrid` from {}.",
            repo_root.display()
        );
        return;
    }

    let outcome = run_two_runner_test(&repo_root, FIXTURE, Duration::from_secs(TIMEOUT_SECS));

    assert_eq!(
        outcome.alice_exit, 0,
        "alice runner exited non-zero ({}). Combined stderr:\n{}",
        outcome.alice_exit, outcome.combined_stderr
    );
    assert_eq!(
        outcome.bob_exit, 0,
        "bob runner exited non-zero ({}). Combined stderr:\n{}",
        outcome.bob_exit, outcome.combined_stderr
    );
    assert!(
        !outcome.combined_stderr.to_lowercase().contains("panic"),
        "found `panic` in combined stderr:\n{}",
        outcome.combined_stderr
    );

    let session_dir = locate_session_dir(outcome.tmpdir.path(), RUN_NAME);
    let entries = parse_session_logs(&session_dir, RUN_NAME);

    // 1 qos x 2 modes x 2 runners = 4 JSONL files.
    for mode in ["single", "multi"] {
        let spawn = format!("{SPAWN_BASE}-{mode}");
        for runner in RUNNERS.iter() {
            assert!(
                entries
                    .iter()
                    .any(|e| e.spawn_name == spawn && e.runner == *runner),
                "[T14.4-hybrid] missing JSONL for spawn={spawn} runner={runner} in {}",
                session_dir.display()
            );
        }
    }

    println!(
        "[T14.4-hybrid] wall_time={:.2}s session_dir={}",
        outcome.wall_time.as_secs_f64(),
        session_dir.display()
    );

    for mode in ["single", "multi"] {
        let spawn = format!("{SPAWN_BASE}-{mode}");
        // Both modes are TCP-backed (qos 4 = reliable-tcp); both must
        // deliver non-zero cross-receives in both directions.
        assert_qos_delivery(&entries, &spawn, mode, 4, false);
    }
}

fn assert_qos_delivery(entries: &[LogEntry], spawn: &str, mode: &str, qos: u8, record_only: bool) {
    let mut per_runner: HashMap<String, &LogContent> = HashMap::new();
    for e in entries.iter().filter(|e| e.spawn_name == spawn) {
        per_runner.insert(e.runner.clone(), &e.content);
    }

    for writer in RUNNERS.iter() {
        for reader in RUNNERS.iter() {
            if writer == reader {
                continue;
            }
            let writer_content = per_runner
                .get(*writer)
                .unwrap_or_else(|| panic!("missing log for writer={writer} spawn={spawn}"));
            let reader_content = per_runner
                .get(*reader)
                .unwrap_or_else(|| panic!("missing log for reader={reader} spawn={spawn}"));

            let op_start = writer_content
                .operate_start_ts
                .as_deref()
                .unwrap_or_else(|| {
                    panic!("[T14.4-hybrid] {spawn}: writer {writer} missing operate phase")
                });
            let eot_sent = writer_content.eot_sent_ts.as_deref().unwrap_or_else(|| {
                panic!("[T14.4-hybrid] {spawn}: writer {writer} missing eot_sent")
            });

            let writes_in_window = writer_content
                .write_ts
                .iter()
                .filter(|ts| ts.as_str() >= op_start && ts.as_str() <= eot_sent)
                .count() as u64;
            let receives_in_window = reader_content
                .receive_ts
                .iter()
                .filter(|(ts, w)| {
                    w == *writer && ts.as_str() >= op_start && ts.as_str() <= eot_sent
                })
                .count() as u64;

            let pct = if writes_in_window == 0 {
                0.0
            } else {
                receives_in_window as f64 / writes_in_window as f64
            };
            println!(
                "[T14.4-hybrid] {writer}->{reader} {spawn} (mode={mode},qos={qos}): \
                 {receives_in_window}/{writes_in_window} ({:.2}%)",
                pct * 100.0
            );
            assert!(
                writes_in_window > 0,
                "[T14.4-hybrid] {spawn}: writer {writer} produced zero writes in operate window"
            );
            if record_only {
                // Single mode at qos 1-2: record only. Single mode may
                // show <100% delivery; that's the measurement we
                // wanted to take.
                continue;
            }
            // For Multi mode at any qos, AND for Single mode at qos
            // 3-4 (TCP, reliable), require at least one delivered
            // frame. We don't tighten the threshold here -- that's
            // the analysis layer's job.
            assert!(
                receives_in_window > 0,
                "[T14.4-hybrid] {spawn}: {reader} received zero frames from {writer} in writer's operate window (mode={mode}, qos={qos})"
            );
        }
    }
}

// ---------- shared infra (mirrors two_runner_regression.rs) ----------

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be at <repo>/variants/hybrid")
        .to_path_buf()
}

fn runner_binary(repo_root: &Path) -> PathBuf {
    repo_root.join("target/release/runner.exe")
}

fn variant_binary(repo_root: &Path) -> PathBuf {
    repo_root.join("target/release/variant-hybrid.exe")
}

fn binaries_present(repo_root: &Path) -> bool {
    runner_binary(repo_root).exists() && variant_binary(repo_root).exists()
}

struct TestOutcome {
    alice_exit: i32,
    bob_exit: i32,
    combined_stderr: String,
    tmpdir: tempfile::TempDir,
    wall_time: Duration,
}

fn run_two_runner_test(repo_root: &Path, fixture_rel: &str, timeout: Duration) -> TestOutcome {
    let fixture_path = repo_root.join("variants/hybrid").join(fixture_rel);
    let fixture_text = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", fixture_path.display()));

    let tmpdir = tempfile::tempdir().expect("failed to create tempdir");
    let log_dir_str = tmpdir
        .path()
        .to_str()
        .expect("tmpdir path must be UTF-8")
        .replace('\\', "/");

    let needle = "log_dir = \"./logs\"";
    assert!(
        fixture_text.contains(needle),
        "fixture {} did not contain the expected `log_dir = \"./logs\"` line",
        fixture_path.display()
    );
    let rewritten = fixture_text.replacen(needle, &format!("log_dir = \"{}\"", log_dir_str), 1);

    let config_path = tmpdir.path().join("config.toml");
    std::fs::write(&config_path, &rewritten).expect("write rewritten config");

    let runner_bin = runner_binary(repo_root);
    let mut alice = spawn_runner(repo_root, &runner_bin, "alice", &config_path);
    let mut bob = spawn_runner(repo_root, &runner_bin, "bob", &config_path);

    let started = Instant::now();
    let deadline = started + timeout;
    let (alice_exit, alice_stderr) = wait_with_deadline(&mut alice, "alice", deadline);
    let (bob_exit, bob_stderr) = wait_with_deadline(&mut bob, "bob", deadline);
    let wall_time = started.elapsed();

    let combined_stderr =
        format!("----- alice stderr -----\n{alice_stderr}\n----- bob stderr -----\n{bob_stderr}");

    TestOutcome {
        alice_exit,
        bob_exit,
        combined_stderr,
        tmpdir,
        wall_time,
    }
}

fn spawn_runner(
    repo_root: &Path,
    runner_bin: &Path,
    runner_name: &str,
    config_path: &Path,
) -> Child {
    Command::new(runner_bin)
        .arg("--name")
        .arg(runner_name)
        .arg("--config")
        .arg(config_path)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn runner {runner_name}: {e}"))
}

fn wait_with_deadline(child: &mut Child, label: &str, deadline: Instant) -> (i32, String) {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = drain_stderr(child);
                let code = status.code().unwrap_or(-1);
                return (code, stderr);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stderr = drain_stderr(child);
                    panic!("runner '{label}' did not exit before deadline. Stderr:\n{stderr}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error polling runner '{label}': {e}"),
        }
    }
}

fn drain_stderr(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut buf);
    }
    if let Some(mut s) = child.stdout.take() {
        let mut sbuf = String::new();
        let _ = s.read_to_string(&mut sbuf);
        if !sbuf.is_empty() {
            buf.push_str("\n----- stdout -----\n");
            buf.push_str(&sbuf);
        }
    }
    buf
}

fn locate_session_dir(tmpdir: &Path, run_name: &str) -> PathBuf {
    let entries = std::fs::read_dir(tmpdir)
        .unwrap_or_else(|e| panic!("read tmpdir {}: {e}", tmpdir.display()));
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if name.starts_with(&format!("{run_name}-")) {
            candidates.push(path);
        }
    }
    assert_eq!(
        candidates.len(),
        1,
        "expected exactly one session subfolder under {} starting with `{run_name}-`, found {:?}",
        tmpdir.display(),
        candidates
    );
    candidates.pop().unwrap()
}

fn parse_session_logs(session_dir: &Path, run_name: &str) -> Vec<LogEntry> {
    let suffix = format!("-{run_name}.jsonl");
    let entries = std::fs::read_dir(session_dir)
        .unwrap_or_else(|e| panic!("read session dir {}: {e}", session_dir.display()));
    let mut out: Vec<LogEntry> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if !name.ends_with(&suffix) {
            continue;
        }
        let stem_no_run = &name[..name.len() - suffix.len()];
        if stem_no_run.contains("-clock-sync") {
            continue;
        }
        let mut matched: Option<(String, String)> = None;
        for runner in RUNNERS.iter() {
            let runner_suffix = format!("-{runner}");
            if let Some(spawn_stem) = stem_no_run.strip_suffix(&runner_suffix) {
                matched = Some((spawn_stem.to_owned(), (*runner).to_owned()));
                break;
            }
        }
        let (spawn_name, runner) = match matched {
            Some(v) => v,
            None => continue,
        };
        let content = parse_jsonl(&path);
        out.push(LogEntry {
            spawn_name,
            runner,
            content,
        });
    }
    out
}

fn parse_jsonl(path: &Path) -> LogContent {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read jsonl {}: {e}", path.display()));
    let mut content = LogContent::default();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("malformed JSONL at {}:{}: {e}", path.display(), i + 1));
        let event = value
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let ts = value
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        if ts.is_empty() {
            continue;
        }
        match event {
            "phase" => {
                let phase = value
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if phase == "operate" && content.operate_start_ts.is_none() {
                    content.operate_start_ts = Some(ts);
                }
            }
            "eot_sent" => {
                if content.eot_sent_ts.is_none() {
                    content.eot_sent_ts = Some(ts);
                }
            }
            "write" => {
                content.write_ts.push(ts);
            }
            "receive" => {
                let writer = value
                    .get("writer")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                if !writer.is_empty() {
                    content.receive_ts.push((ts, writer));
                }
            }
            _ => {}
        }
    }
    content
}
