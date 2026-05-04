//! End-to-end two-runner regression tests for the hybrid variant.
//!
//! These tests are gated `#[ignore]` so the default `cargo test` run stays
//! fast. Invoke them explicitly via:
//!
//! ```text
//! cargo test --release -p variant-hybrid -- --ignored two_runner_regression --nocapture
//! ```
//!
//! Each test:
//!
//! 1. Locates the prebuilt `runner` and `variant-hybrid` release binaries.
//!    If either is missing the test is skipped with a clear message.
//! 2. Allocates a `tempfile::TempDir`, copies the source fixture into it,
//!    rewrites only `log_dir = "./logs"` -> `log_dir = "<tmpdir>"`, and
//!    writes the result to `<tmpdir>/config.toml`.
//! 3. Spawns two `runner` children (`alice` and `bob`) from CWD = repo root,
//!    waits for both with a generous wall-clock budget, and parses the
//!    per-spawn JSONL files to assert cross-peer delivery scoped to the
//!    operate window `[phase=operate.ts, eot_sent.ts]` per the EOT
//!    protocol contract (`metak-shared/api-contracts/eot-protocol.md`).
//!
//! Failure regressions targeted:
//!
//! - `two_runner_regression_correctness_sweep` -- exercises all four QoS
//!   levels at modest rate (100 Hz x 10 vps x 3 s). Asserts at-least
//!   99% delivery on every QoS level within the writer's operate window.
//! - `two_runner_regression_highrate_no_cascade` -- exercises the T10.1
//!   WSAEWOULDBLOCK / cascading-peer-drop fix at 100 Hz x 1000 vps
//!   (100K msg/s) for 5 s. Asserts at-least 95% delivery on UDP path
//!   (qos 1-2) and at-least 99% delivery on TCP path (qos 3-4) within
//!   the operate window. The regression target is "no cascade", which
//!   manifests as a non-zero runner exit code if it triggers.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Both regression tests drive two runner subprocesses that perform
/// peer discovery on localhost via runner-level multicast. If cargo
/// runs the two `#[ignore]` tests concurrently, alice from test A
/// hears alice from test B (different rewritten config -> different
/// hash) and the runner aborts with `config hash mismatch`. This
/// mutex serialises the two tests inside the same test binary so the
/// spec command `cargo test --release -p variant-hybrid --
/// --ignored two_runner_regression --nocapture` works without
/// `--test-threads=1`.
static REGRESSION_LOCK: Mutex<()> = Mutex::new(());

const FIXTURE_HYBRID_ONLY: &str = "tests/fixtures/two-runner-hybrid-only.toml";
const FIXTURE_HYBRID_HIGHRATE: &str = "tests/fixtures/two-runner-hybrid-highrate.toml";

const RUN_NAME_CORRECTNESS: &str = "hybrid-t93-validation";
const RUN_NAME_HIGHRATE: &str = "hybrid-t101-highrate-validation";

const SPAWN_BASE_CORRECTNESS: &str = "hybrid-t93";
const SPAWN_BASE_HIGHRATE: &str = "hybrid-t101-highrate";

const RUNNER_NAMES: [&str; 2] = ["alice", "bob"];

/// Maximum time to wait for both runner children to exit. The correctness
/// sweep takes ~30 s in normal conditions; the high-rate test takes ~50 s.
const TIMEOUT_CORRECTNESS_SECS: u64 = 90;
const TIMEOUT_HIGHRATE_SECS: u64 = 180;

/// Per-(spawn, runner) JSONL log content needed for operate-window scoping.
///
/// Timestamps are kept as their original RFC 3339 string form. The variant's
/// logger emits all timestamps as `%Y-%m-%dT%H:%M:%S%.9fZ` (UTC, fixed-width
/// nanosecond fraction, trailing `Z`), which means lexicographic ordering on
/// the strings matches chronological ordering -- avoiding the need for a
/// `chrono` dev-dependency just to compare two known-shape strings.
#[derive(Debug, Default, Clone)]
struct LogContent {
    /// First `phase` event with `phase: "operate"` -- start of the operate
    /// window for this runner-as-writer.
    operate_start_ts: Option<String>,
    /// `eot_sent` event timestamp -- end of the operate window for this
    /// runner-as-writer.
    eot_sent_ts: Option<String>,
    /// All `write` events as `(ts, _)` pairs. The second tuple slot is reserved
    /// for any future per-write filtering need; today we only count.
    write_ts: Vec<String>,
    /// All `receive` events as `(ts, writer)` pairs.
    receive_ts: Vec<(String, String)>,
}

/// Parsed identifier for a per-spawn-per-runner JSONL log file.
#[derive(Debug, Clone)]
struct LogEntry {
    spawn_name: String,
    runner: String,
    content: LogContent,
}

#[test]
#[ignore]
fn two_runner_regression_correctness_sweep() {
    let _guard = REGRESSION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo_root = repo_root();
    if !binaries_present(&repo_root) {
        eprintln!(
            "[two_runner_regression_correctness_sweep] SKIPPED: prebuilt binaries missing. \
             Build with `cargo build --release -p runner` and \
             `cargo build --release -p variant-hybrid` from {}.",
            repo_root.display()
        );
        return;
    }

    let outcome = run_two_runner_test(
        &repo_root,
        FIXTURE_HYBRID_ONLY,
        Duration::from_secs(TIMEOUT_CORRECTNESS_SECS),
    );

    // Sanity: both runners must exit 0.
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

    // Stderr must not contain `panic` (case-insensitive).
    assert!(
        !outcome.combined_stderr.to_lowercase().contains("panic"),
        "found `panic` in combined stderr (case-insensitive). Combined stderr:\n{}",
        outcome.combined_stderr
    );

    // Locate the session subfolder produced by the runner under tmpdir.
    let session_dir = locate_session_dir(outcome.tmpdir.path(), RUN_NAME_CORRECTNESS);
    let entries = parse_session_logs(&session_dir, RUN_NAME_CORRECTNESS);

    // We expect 4 spawns (qos1..qos4) x 2 runners = 8 JSONL files.
    let expected_spawns: Vec<String> = (1..=4)
        .map(|q| format!("{}-qos{}", SPAWN_BASE_CORRECTNESS, q))
        .collect();

    for spawn in &expected_spawns {
        for runner in RUNNER_NAMES.iter() {
            assert!(
                entries
                    .iter()
                    .any(|e| &e.spawn_name == spawn && e.runner == *runner),
                "missing JSONL for spawn={spawn} runner={runner} in {}",
                session_dir.display()
            );
        }
    }

    // Per-spawn delivery assertions, scoped to the operate window per
    // the EOT contract (`metak-shared/api-contracts/eot-protocol.md`).
    println!(
        "[T12.7-hybrid][correctness_sweep] wall_time={:.2}s session_dir={}",
        outcome.wall_time.as_secs_f64(),
        session_dir.display()
    );
    for qos in 1u8..=4u8 {
        let spawn = format!("{}-qos{}", SPAWN_BASE_CORRECTNESS, qos);
        // T12.7-hybrid thresholds: with operate-window scoping (writes
        // up to `eot_sent.ts`, receives up to the writer's
        // `eot_sent.ts`) every QoS level must deliver >= 99% on
        // localhost in the correctness sweep.
        let threshold: f64 = 0.99;
        assert_qos_delivery(&entries, &spawn, threshold, "correctness_sweep");
    }
}

#[test]
#[ignore]
fn two_runner_regression_highrate_no_cascade() {
    let _guard = REGRESSION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let repo_root = repo_root();
    if !binaries_present(&repo_root) {
        eprintln!(
            "[two_runner_regression_highrate_no_cascade] SKIPPED: prebuilt binaries missing. \
             Build with `cargo build --release -p runner` and \
             `cargo build --release -p variant-hybrid` from {}.",
            repo_root.display()
        );
        return;
    }

    let outcome = run_two_runner_test(
        &repo_root,
        FIXTURE_HYBRID_HIGHRATE,
        Duration::from_secs(TIMEOUT_HIGHRATE_SECS),
    );

    // Both runners must exit 0. Pre-T10.1 the cascade caused 14/32 spawns
    // to fail at this rate, surfacing as a non-zero exit code.
    assert_eq!(
        outcome.alice_exit, 0,
        "alice runner exited non-zero ({}) -- cascade may have regressed. \
         Combined stderr:\n{}",
        outcome.alice_exit, outcome.combined_stderr
    );
    assert_eq!(
        outcome.bob_exit, 0,
        "bob runner exited non-zero ({}) -- cascade may have regressed. \
         Combined stderr:\n{}",
        outcome.bob_exit, outcome.combined_stderr
    );

    // Stderr must not contain `panic` (case-insensitive). Per-peer
    // fault-tolerance / WouldBlock retry warnings are EXPECTED and do not
    // fail the test -- their presence proves the regression-prone code is
    // exercised.
    assert!(
        !outcome.combined_stderr.to_lowercase().contains("panic"),
        "found `panic` in combined stderr (case-insensitive). Combined stderr:\n{}",
        outcome.combined_stderr
    );

    let session_dir = locate_session_dir(outcome.tmpdir.path(), RUN_NAME_HIGHRATE);
    let entries = parse_session_logs(&session_dir, RUN_NAME_HIGHRATE);

    let expected_spawns: Vec<String> = (1..=4)
        .map(|q| format!("{}-qos{}", SPAWN_BASE_HIGHRATE, q))
        .collect();

    for spawn in &expected_spawns {
        for runner in RUNNER_NAMES.iter() {
            assert!(
                entries
                    .iter()
                    .any(|e| &e.spawn_name == spawn && e.runner == *runner),
                "missing JSONL for spawn={spawn} runner={runner} in {}",
                session_dir.display()
            );
        }
    }

    println!(
        "[T12.7-hybrid][highrate_no_cascade] wall_time={:.2}s session_dir={}",
        outcome.wall_time.as_secs_f64(),
        session_dir.display()
    );
    for qos in 1u8..=4u8 {
        let spawn = format!("{}-qos{}", SPAWN_BASE_HIGHRATE, qos);
        // T12.7-hybrid high-rate thresholds, with operate-window
        // scoping per the EOT contract:
        //
        // - qos 1-2 (UDP at 100K msg/s): >= 95%. The 5% slack vs the
        //   correctness sweep accounts for the documented best-effort
        //   semantics: at 100K msg/s sustained, the multicast send
        //   buffer can transiently overflow even with the bounded
        //   WouldBlock retry, and per-host scheduling jitter on
        //   Windows can cost a small percentage even within the
        //   operate window.
        // - qos 3-4 (TCP at 100K msg/s): >= 99%. TCP is reliable in
        //   transit; with the EOT handshake the operate window now
        //   ends at the writer's `eot_sent.ts` (sent over the same
        //   ordered TCP stream after the last data frame), so any
        //   data that the writer enqueued before EOT must arrive
        //   before EOT does -- modulo a tiny scheduling-overlap window
        //   on the receiver between the last data byte and the EOT
        //   tag, which is bounded to a sub-millisecond fraction of
        //   the 5 s operate phase.
        let threshold: f64 = if qos <= 2 { 0.95 } else { 0.99 };
        assert_qos_delivery(&entries, &spawn, threshold, "highrate_no_cascade");
    }
}

// ---------- helpers ----------

/// Locate the repo root from `CARGO_MANIFEST_DIR` (which points at
/// `<repo>/variants/hybrid`). Two `parent()` hops bring us to the repo root.
fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be at <repo>/variants/hybrid")
        .to_path_buf()
}

fn runner_binary(repo_root: &Path) -> PathBuf {
    repo_root.join("runner/target/release/runner.exe")
}

fn variant_binary(repo_root: &Path) -> PathBuf {
    repo_root.join("variants/hybrid/target/release/variant-hybrid.exe")
}

fn binaries_present(repo_root: &Path) -> bool {
    runner_binary(repo_root).exists() && variant_binary(repo_root).exists()
}

/// Outcome of running two runner children against a fixture.
struct TestOutcome {
    alice_exit: i32,
    bob_exit: i32,
    combined_stderr: String,
    tmpdir: tempfile::TempDir,
    wall_time: Duration,
}

/// Spawns two runner children against the given fixture and waits for both
/// to exit (or hard-kills on timeout).
fn run_two_runner_test(repo_root: &Path, fixture_rel: &str, timeout: Duration) -> TestOutcome {
    let fixture_path = repo_root.join("variants/hybrid").join(fixture_rel);
    let fixture_text = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", fixture_path.display()));

    let tmpdir = tempfile::tempdir().expect("failed to create tempdir");
    let log_dir_str = tmpdir
        .path()
        .to_str()
        .expect("tmpdir path must be valid UTF-8")
        .replace('\\', "/");

    // Substitute the single `log_dir = "./logs"` line. The fixtures use this
    // exact string verbatim (per coding-standards.md).
    let needle = "log_dir = \"./logs\"";
    assert!(
        fixture_text.contains(needle),
        "fixture {} did not contain the expected `log_dir = \"./logs\"` line",
        fixture_path.display()
    );
    let rewritten = fixture_text.replacen(needle, &format!("log_dir = \"{}\"", log_dir_str), 1);

    let config_path = tmpdir.path().join("config.toml");
    std::fs::write(&config_path, &rewritten).expect("failed to write rewritten config to tmpdir");

    let runner_bin = runner_binary(repo_root);
    let mut alice = spawn_runner(repo_root, &runner_bin, "alice", &config_path);
    let mut bob = spawn_runner(repo_root, &runner_bin, "bob", &config_path);

    // Both children share a single absolute deadline so the second wait
    // doesn't double-count time already spent waiting for the first.
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

/// Spawn a single `runner` child with `--name <runner_name> --config <path>`.
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
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn runner {} with config {}: {e}",
                runner_name,
                config_path.display()
            )
        })
}

/// Wait for `child` to exit, polling `try_wait` so we can enforce a wall-
/// clock deadline.
///
/// On timeout the child is hard-killed and the test fails with a clear
/// message. Stderr is read in full after the child exits (or is killed).
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
                    panic!(
                        "runner '{}' did not exit before deadline. Stderr captured before kill:\n{}",
                        label, stderr
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error polling runner '{label}': {e}"),
        }
    }
}

/// Drain whatever is left on the child's stderr pipe. Safe to call after
/// `try_wait` returns Some, because the kernel buffers any remaining bytes.
fn drain_stderr(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut buf);
    }
    // Also drain stdout in case anything went there; not asserted on but
    // useful for surfacing crashes that print to stdout.
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

/// Find the runner-created session subfolder under `tmpdir`. The runner
/// writes to `<log_dir>/<run_name>-<launch_ts>/`; we glob for the unique
/// matching directory.
fn locate_session_dir(tmpdir: &Path, run_name: &str) -> PathBuf {
    let entries = std::fs::read_dir(tmpdir)
        .unwrap_or_else(|e| panic!("failed to read tmpdir {}: {e}", tmpdir.display()));
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

/// Parse all `<spawn>-<runner>-<run>.jsonl` files in `session_dir` and
/// return one `LogEntry` per file. Files not matching the expected name
/// pattern (e.g. clock-sync logs) are skipped silently.
fn parse_session_logs(session_dir: &Path, run_name: &str) -> Vec<LogEntry> {
    let suffix = format!("-{run_name}.jsonl");
    let entries = std::fs::read_dir(session_dir)
        .unwrap_or_else(|e| panic!("failed to read session dir {}: {e}", session_dir.display()));
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
        // Skip the runner's clock-sync sibling files: they end in
        // `<runner>-clock-sync-<run>.jsonl` /
        // `<runner>-clock-sync-debug-<run>.jsonl`. The variant log filename
        // schema is `<variant>-<runner>-<run>.jsonl`.
        let stem_no_run = &name[..name.len() - suffix.len()];
        if stem_no_run.contains("-clock-sync") {
            continue;
        }
        // Split `<variant>-<runner>` from the right on the LAST `-<runner>`.
        // Match against the known runner names to avoid ambiguity with
        // `-` characters embedded in variant names like `hybrid-t93-qos1`.
        let mut matched: Option<(String, String)> = None;
        for runner in RUNNER_NAMES.iter() {
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

/// Parse a single JSONL log file and collect:
/// - first `phase=="operate"` event timestamp (operate-window start)
/// - `eot_sent` event timestamp (operate-window end)
/// - all `write` event timestamps
/// - all `receive` event `(ts, writer)` pairs
///
/// The variant emits all timestamps with the same RFC 3339 nanosecond UTC
/// shape (`YYYY-MM-DDTHH:MM:SS.NNNNNNNNNZ`), so string ordering matches
/// chronological ordering -- callers may compare timestamps via plain `<=`.
fn parse_jsonl(path: &Path) -> LogContent {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read jsonl {}: {e}", path.display()));
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

/// Assert cross-peer delivery for a given spawn against `threshold`
/// (fraction in [0.0, 1.0]) using operate-window scoping per the EOT
/// contract. Prints a one-line summary per (writer, reader, qos).
///
/// The window is defined per-writer:
///   `[writer.operate_start_ts, writer.eot_sent_ts]`
/// Both endpoints are inclusive. Counts:
/// - numerator = receiver's `receive` events with `writer == <writer>` and
///   `ts` in the WRITER's window
/// - denominator = writer's `write` events with `ts` in the writer's window
fn assert_qos_delivery(entries: &[LogEntry], spawn: &str, threshold: f64, label: &str) {
    // Pick the per-runner content for this spawn.
    let mut per_runner: HashMap<String, &LogContent> = HashMap::new();
    for e in entries.iter().filter(|e| e.spawn_name == spawn) {
        per_runner.insert(e.runner.clone(), &e.content);
    }

    for writer in RUNNER_NAMES.iter() {
        for reader in RUNNER_NAMES.iter() {
            if writer == reader {
                continue;
            }
            let writer_content = per_runner
                .get(*writer)
                .unwrap_or_else(|| panic!("missing log entry for writer={writer} spawn={spawn}"));
            let reader_content = per_runner
                .get(*reader)
                .unwrap_or_else(|| panic!("missing log entry for reader={reader} spawn={spawn}"));

            let op_start = writer_content
                .operate_start_ts
                .as_deref()
                .unwrap_or_else(|| {
                    panic!(
                        "[{label}] {spawn}: writer {writer} has no `phase=operate` event; \
                         operate-window scoping requires it"
                    )
                });
            let eot_sent = writer_content.eot_sent_ts.as_deref().unwrap_or_else(|| {
                panic!(
                    "[{label}] {spawn}: writer {writer} has no `eot_sent` event; \
                     operate-window scoping requires it (T12.2 should ship EOT)"
                )
            });

            // Writer's writes inside its own operate window.
            let writes_in_window = writer_content
                .write_ts
                .iter()
                .filter(|ts| ts.as_str() >= op_start && ts.as_str() <= eot_sent)
                .count() as u64;

            // Receiver's receives from this writer inside the writer's
            // operate window.
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
                "[T12.7-hybrid][{label}] {writer} -> {reader} {spawn}: {receives_in_window}/{writes_in_window} ({:.2}%) in [op_start..eot_sent]",
                pct * 100.0
            );
            assert!(
                writes_in_window > 0,
                "[{label}] {spawn}: writer {writer} produced zero writes inside its operate window; expected non-zero"
            );
            assert!(
                pct >= threshold,
                "[{label}] {spawn}: {writer}->{reader} delivery {receives_in_window}/{writes_in_window} ({:.2}%) below threshold {:.2}%",
                pct * 100.0,
                threshold * 100.0
            );
        }
    }
}
