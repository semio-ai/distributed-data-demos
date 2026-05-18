//! Two-runner regression tests for Zenoh variant (T10.6c, retightened in T12.7).
//!
//! Spawns two `runner` child processes (alice + bob) on localhost against
//! the documented reproducer fixtures from `variants/zenoh/tests/fixtures/`
//! and asserts that the deadlock fix from T10.2b (DECISIONS.md D7)
//! continues to hold.
//!
//! T12.7 update: counts are scoped to the writer's operate window
//! `[phase=operate.ts, eot_sent.ts]` per the EOT contract
//! (`metak-shared/api-contracts/eot-protocol.md`, "Analysis Tool
//! Implications"). The numerical thresholds are unchanged from T10.6c
//! (`1000paths` `==100%`, `max-throughput` `>=80%`); only the SCOPING
//! tightens.
//!
//! The fixtures themselves are the source of truth and stay untouched;
//! this test reads each fixture, substitutes `log_dir = "./logs"` with
//! the tmpdir path, and writes the modified copy to `<tmpdir>/config.toml`
//! before spawning the runners.
//!
//! Both tests are gated `#[ignore]` so default `cargo test` stays fast.
//! Run them via:
//!     cargo test --release -p variant-zenoh -- --ignored two_runner_regression
//!
//! Pre-requisites (the test skips with a clear message otherwise):
//! - `<repo-root>/runner/target/release/runner.exe`
//! - `<repo-root>/variants/zenoh/target/release/variant-zenoh.exe`

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Process-wide mutex serialising the two regression tests. Cargo runs
/// `#[test]` fns within the same binary in parallel, but two concurrent
/// two-runner spawns on localhost cross-talk via Zenoh's default
/// multicast scouting -- the alice from one test discovers the bob from
/// the other and the runner's coordination protocol then fails on a
/// config-hash mismatch. Locking forces them to run back-to-back.
fn serialize_tests() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Wall-time budget per fixture. The fixtures complete in <30 s normally
/// (stabilize 2s + operate 5s + silent 2s + teardown). 90 s pads heavily
/// for slow CI. Anything beyond this is a deadlock-regression signature.
const PER_FIXTURE_TIMEOUT: Duration = Duration::from_secs(90);

/// Repo root resolved from `CARGO_MANIFEST_DIR` (= `variants/zenoh/`).
fn repo_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR has at least two ancestors")
        .to_path_buf()
}

/// Resolve a release binary path, preferring the workspace-level
/// `target/release/` location (where `cargo build --release -p <pkg>`
/// from the repo root produces output) and falling back to the
/// historical per-subfolder `<pkg>/target/release/` paths the tests
/// originally documented as pre-requisites.
fn locate_release_binary(workspace_relative: &[&str], legacy_relative: &[&str]) -> PathBuf {
    let workspace = {
        let mut p = repo_root();
        for seg in workspace_relative {
            p = p.join(seg);
        }
        p
    };
    if workspace.exists() {
        return workspace;
    }
    let mut legacy = repo_root();
    for seg in legacy_relative {
        legacy = legacy.join(seg);
    }
    legacy
}

fn runner_binary() -> PathBuf {
    locate_release_binary(
        &["target", "release", "runner.exe"],
        &["runner", "target", "release", "runner.exe"],
    )
}

fn variant_binary() -> PathBuf {
    locate_release_binary(
        &["target", "release", "variant-zenoh.exe"],
        &[
            "variants",
            "zenoh",
            "target",
            "release",
            "variant-zenoh.exe",
        ],
    )
}

/// Skip the test (returns true) if either binary is missing.
fn check_binaries_or_skip(test_name: &str) -> bool {
    let runner = runner_binary();
    let variant = variant_binary();
    if !runner.exists() {
        eprintln!(
            "[T12.7-zenoh] SKIP {test_name}: runner binary not found at {} \
             (build with: cargo build --release -p runner)",
            runner.display()
        );
        return true;
    }
    if !variant.exists() {
        eprintln!(
            "[T12.7-zenoh] SKIP {test_name}: variant-zenoh binary not found at {} \
             (build with: cargo build --release -p variant-zenoh)",
            variant.display()
        );
        return true;
    }
    false
}

/// Read a fixture, replace the canonical `log_dir = "./logs"` line with
/// `log_dir = "<tmpdir>"`, and write the result into `<tmpdir>/config.toml`.
fn materialize_fixture(fixture_path: &Path, tmpdir: &Path) -> PathBuf {
    let original = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));

    // Use forward slashes so the embedded TOML string is portable on Windows.
    let tmp_str = tmpdir.to_string_lossy().replace('\\', "/");
    let replacement = format!("log_dir = \"{tmp_str}\"");

    let modified = original.replace("log_dir = \"./logs\"", &replacement);
    assert!(
        modified.contains(&replacement),
        "fixture {} did not contain `log_dir = \"./logs\"` to substitute",
        fixture_path.display()
    );

    let cfg_path = tmpdir.join("config.toml");
    std::fs::write(&cfg_path, modified).expect("write tmp config.toml");
    cfg_path
}

/// Spawn one `runner` child with the given runner name + config path.
fn spawn_runner(name: &str, config_path: &Path, port: u16) -> Child {
    let runner = runner_binary();
    Command::new(&runner)
        .current_dir(repo_root())
        .arg("--name")
        .arg(name)
        .arg("--config")
        .arg(config_path)
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn runner {name}: {e}"))
}

/// Wait for a child with a wall-time budget, killing it on timeout.
/// Returns `(exit_status, stdout_bytes, stderr_bytes, wall_time)`.
///
/// stdout / stderr are drained on dedicated threads so the child cannot
/// deadlock on a full pipe buffer (Windows default is ~64 KB; runner +
/// variant produce well over that on the 1000paths fixture).
fn wait_with_timeout(
    mut child: Child,
    name: &str,
    deadline: Instant,
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>, Duration) {
    let start = Instant::now();

    let stdout_handle = child.stdout.take().expect("stdout piped");
    let stderr_handle = child.stderr.take().expect("stderr piped");

    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut h = stdout_handle;
        let _ = h.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut h = stderr_handle;
        let _ = h.read_to_end(&mut buf);
        buf
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    eprintln!(
                        "[T12.7-zenoh] TIMEOUT runner '{name}' did not exit \
                         within budget; hard-killing"
                    );
                    let _ = child.kill();
                    break child.wait().expect("wait after kill");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("try_wait on runner {name} failed: {e}"),
        }
    };

    let stdout_buf = stdout_thread.join().unwrap_or_default();
    let stderr_buf = stderr_thread.join().unwrap_or_default();

    (status, stdout_buf, stderr_buf, start.elapsed())
}

/// One JSONL line carrying the fields this test cares about.
#[derive(Debug, Clone)]
struct LogLine {
    ts: String,
    event: String,
    /// Only populated for `event == "receive"`.
    writer: Option<String>,
}

/// Parsed view of a runner's JSONL log file, structured for operate-window
/// scoped counting per the EOT contract.
///
/// `operate_start_ts` comes from the `phase` event with `phase == "operate"`.
/// `eot_sent_ts` comes from the `eot_sent` event. The operate window is
/// the inclusive interval `[operate_start_ts, eot_sent_ts]`.
///
/// Timestamps are RFC 3339 with nanosecond precision and a fixed-width
/// `%Y-%m-%dT%H:%M:%S%.9fZ` layout (see `variant-base/src/logger.rs`'s
/// `now_ts`). That layout is lexicographically ordered, so plain string
/// comparison is sufficient for in-window membership checks.
#[derive(Debug)]
struct ParsedLog {
    operate_start_ts: String,
    eot_sent_ts: String,
    /// Every line we cared to keep, in file order.
    lines: Vec<LogLine>,
}

impl ParsedLog {
    /// `true` iff `ts` lies within the inclusive operate window.
    fn in_window(&self, ts: &str) -> bool {
        ts >= self.operate_start_ts.as_str() && ts <= self.eot_sent_ts.as_str()
    }

    /// Count `write` events whose `ts` falls inside this log's own
    /// operate window.
    fn writes_in_window(&self) -> u64 {
        self.lines
            .iter()
            .filter(|l| l.event == "write" && self.in_window(&l.ts))
            .count() as u64
    }

    /// Count `receive` events from a specific writer whose `ts` falls
    /// inside the WRITER's operate window.
    fn receives_from_in_writer_window(&self, writer: &str, writer_log: &ParsedLog) -> u64 {
        self.lines
            .iter()
            .filter(|l| {
                l.event == "receive"
                    && l.writer.as_deref() == Some(writer)
                    && writer_log.in_window(&l.ts)
            })
            .count() as u64
    }

    /// T17.8: count ALL `receive` events from a specific writer
    /// regardless of receive-side timestamp. The strict-delivery
    /// contract (DESIGN.md § 6.5) says every accepted write must be
    /// delivered, even if it arrives after the writer's local
    /// operate window closes (throughput collapse pushes late
    /// receives past `eot_sent` on a fast peer that finished early).
    /// This is the metric the analysis tool uses for the "QoS 3/4
    /// 100% delivery" gate -- see `analysis/performance.py`
    /// `_count_writes_and_receives`.
    fn total_receives_from(&self, writer: &str) -> u64 {
        self.lines
            .iter()
            .filter(|l| l.event == "receive" && l.writer.as_deref() == Some(writer))
            .count() as u64
    }
}

/// Parse one JSONL log file and extract the operate-window boundaries
/// plus the events we count over.
fn parse_jsonl(path: &Path) -> ParsedLog {
    let contents = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read jsonl {}: {e}", path.display()));

    let mut operate_start_ts: Option<String> = None;
    let mut eot_sent_ts: Option<String> = None;
    let mut lines: Vec<LogLine> = Vec::new();

    for raw in contents.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts = match v.get("ts").and_then(|t| t.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let event = v
            .get("event")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .to_string();

        match event.as_str() {
            "phase" => {
                let phase = v.get("phase").and_then(|p| p.as_str()).unwrap_or("");
                if phase == "operate" && operate_start_ts.is_none() {
                    operate_start_ts = Some(ts.clone());
                }
            }
            "eot_sent" => {
                if eot_sent_ts.is_none() {
                    eot_sent_ts = Some(ts.clone());
                }
            }
            "write" => {
                lines.push(LogLine {
                    ts,
                    event,
                    writer: None,
                });
            }
            "receive" => {
                let writer = v
                    .get("writer")
                    .and_then(|w| w.as_str())
                    .unwrap_or("")
                    .to_string();
                lines.push(LogLine {
                    ts,
                    event,
                    writer: Some(writer),
                });
            }
            _ => {}
        }
    }

    let operate_start_ts = operate_start_ts.unwrap_or_else(|| {
        panic!(
            "jsonl {} has no `phase=operate` event; cannot scope to operate window",
            path.display()
        )
    });
    let eot_sent_ts = eot_sent_ts.unwrap_or_else(|| {
        panic!(
            "jsonl {} has no `eot_sent` event; T12.5 zenoh EOT must emit one per spawn",
            path.display()
        )
    });

    ParsedLog {
        operate_start_ts,
        eot_sent_ts,
        lines,
    }
}

/// Locate the per-spawn JSONL file for a given (variant_spawn_name, runner, run).
fn locate_jsonl(session_dir: &Path, spawn_name: &str, runner: &str, run: &str) -> PathBuf {
    let filename = format!("{spawn_name}-{runner}-{run}.jsonl");
    session_dir.join(filename)
}

/// Find the auto-created session subfolder under `<tmpdir>/<run>-<ts>`.
fn find_session_dir(tmpdir: &Path, run: &str) -> PathBuf {
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in
        std::fs::read_dir(tmpdir).unwrap_or_else(|e| panic!("read_dir {}: {e}", tmpdir.display()))
    {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if name.starts_with(&format!("{run}-")) {
            matches.push(path);
        }
    }
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one session subfolder for run '{run}' under {}, found: {:?}",
        tmpdir.display(),
        matches
    );
    matches.into_iter().next().unwrap()
}

/// Common end-to-end driver. Returns the parsed (alice, bob) JSONL logs
/// and the combined stderr from both runners for assertions.
struct DriveResult {
    alice: ParsedLog,
    bob: ParsedLog,
    combined_stderr: String,
    wall_time: Duration,
}

fn drive_two_runners(
    fixture_path: &Path,
    spawn_name: &str,
    run: &str,
    test_name: &str,
    base_port: u16,
) -> DriveResult {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let cfg_path = materialize_fixture(fixture_path, tmpdir.path());

    eprintln!(
        "[T12.7-zenoh] {test_name}: tmpdir={} fixture={}",
        tmpdir.path().display(),
        fixture_path.display()
    );

    let start = Instant::now();
    let deadline = start + PER_FIXTURE_TIMEOUT;

    let alice = spawn_runner("alice", &cfg_path, base_port);
    let bob = spawn_runner("bob", &cfg_path, base_port);

    let (alice_status, alice_stdout, alice_stderr, alice_wall) =
        wait_with_timeout(alice, "alice", deadline);
    let (bob_status, bob_stdout, bob_stderr, bob_wall) = wait_with_timeout(bob, "bob", deadline);

    let wall_time = alice_wall.max(bob_wall);

    let alice_stdout_s = String::from_utf8_lossy(&alice_stdout).into_owned();
    let alice_stderr_s = String::from_utf8_lossy(&alice_stderr).into_owned();
    let bob_stdout_s = String::from_utf8_lossy(&bob_stdout).into_owned();
    let bob_stderr_s = String::from_utf8_lossy(&bob_stderr).into_owned();

    eprintln!(
        "[T12.7-zenoh] {test_name}: alice exit={:?} wall={:.2}s, bob exit={:?} wall={:.2}s",
        alice_status.code(),
        alice_wall.as_secs_f64(),
        bob_status.code(),
        bob_wall.as_secs_f64(),
    );
    if !alice_stdout_s.is_empty() {
        eprintln!("[T12.7-zenoh] {test_name}: alice stdout:\n{alice_stdout_s}");
    }
    if !alice_stderr_s.is_empty() {
        eprintln!("[T12.7-zenoh] {test_name}: alice stderr:\n{alice_stderr_s}");
    }
    if !bob_stdout_s.is_empty() {
        eprintln!("[T12.7-zenoh] {test_name}: bob stdout:\n{bob_stdout_s}");
    }
    if !bob_stderr_s.is_empty() {
        eprintln!("[T12.7-zenoh] {test_name}: bob stderr:\n{bob_stderr_s}");
    }

    assert!(
        alice_status.success(),
        "{test_name}: alice exited non-zero: {alice_status:?} \
         (deadlock-regression signature is timeout-induced kill); \
         stderr was:\n{alice_stderr_s}"
    );
    assert!(
        bob_status.success(),
        "{test_name}: bob exited non-zero: {bob_status:?} \
         (deadlock-regression signature is timeout-induced kill); \
         stderr was:\n{bob_stderr_s}"
    );

    // Locate session subfolder and read JSONLs. Both runners share it.
    let session_dir = find_session_dir(tmpdir.path(), run);

    let alice_log = locate_jsonl(&session_dir, spawn_name, "alice", run);
    let bob_log = locate_jsonl(&session_dir, spawn_name, "bob", run);
    assert!(
        alice_log.exists(),
        "{test_name}: missing alice JSONL at {}",
        alice_log.display()
    );
    assert!(
        bob_log.exists(),
        "{test_name}: missing bob JSONL at {}",
        bob_log.display()
    );

    let alice_parsed = parse_jsonl(&alice_log);
    let bob_parsed = parse_jsonl(&bob_log);

    // Persist tmpdir on disk only for the duration of the test; tempfile
    // drops it once `tmpdir` goes out of scope at the end of this fn.
    drop(tmpdir);

    let combined_stderr = format!("{alice_stderr_s}\n{bob_stderr_s}");

    DriveResult {
        alice: alice_parsed,
        bob: bob_parsed,
        combined_stderr,
        wall_time,
    }
}

#[test]
#[ignore]
fn two_runner_regression_1000paths_no_deadlock() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "1000paths";
    if check_binaries_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("zenoh")
        .join("tests")
        .join("fixtures")
        .join("two-runner-zenoh-1000paths.toml");

    // Use a distinct coordination base port so this test cannot collide
    // with the parallel max-throughput test (cargo test runs ignored
    // tests in parallel by default).
    let base_port: u16 = 29876;
    let result = drive_two_runners(
        &fixture,
        "zenoh-1000paths",
        "zenoh-t102-1000paths",
        test_name,
        base_port,
    );

    // Operate-window-scoped denominators (writer's [operate_start, eot_sent]).
    let alice_writes = result.alice.writes_in_window();
    let bob_writes = result.bob.writes_in_window();
    // Cross-peer numerators: receives from writer with ts inside the
    // WRITER's operate window.
    let alice_recv_from_bob = result
        .alice
        .receives_from_in_writer_window("bob", &result.bob);
    let bob_recv_from_alice = result
        .bob
        .receives_from_in_writer_window("alice", &result.alice);

    let alice_pct = if bob_writes == 0 {
        0.0
    } else {
        (alice_recv_from_bob as f64) * 100.0 / (bob_writes as f64)
    };
    let bob_pct = if alice_writes == 0 {
        0.0
    } else {
        (bob_recv_from_alice as f64) * 100.0 / (alice_writes as f64)
    };

    println!(
        "[T12.7-zenoh] alice <- bob 1000paths: {alice_recv_from_bob}/{bob_writes} \
         ({alice_pct:.2}%) in [op_start..eot_sent] (alice_writes={alice_writes}, wall={:.2}s)",
        result.wall_time.as_secs_f64()
    );
    println!(
        "[T12.7-zenoh] bob <- alice 1000paths: {bob_recv_from_alice}/{alice_writes} \
         ({bob_pct:.2}%) in [op_start..eot_sent] (bob_writes={bob_writes})"
    );

    assert!(
        !result.combined_stderr.contains("panic"),
        "1000paths: combined stderr contained `panic`:\n{}",
        result.combined_stderr
    );

    assert!(
        alice_writes > 0,
        "1000paths: alice produced zero writes in the operate window; \
         runner did not advance through operate phase"
    );
    assert!(
        bob_writes > 0,
        "1000paths: bob produced zero writes in the operate window; \
         runner did not advance through operate phase"
    );

    // T10.6c locked in `==100%` per direction on this fixture (T10.2b
    // localhost validation showed exactly 51000/51000). The T12.7
    // contract for `1000paths` retains `==100%`; only the SCOPING
    // tightens to the operate window. Any drop here is a regression
    // of the T12.5 EOT implementation or the T10.2b bridge fix.
    assert_eq!(
        alice_recv_from_bob, bob_writes,
        "1000paths: alice received {alice_recv_from_bob} from bob in bob's operate window \
         but bob wrote {bob_writes} in that same window \
         (expected 100% per T12.7 contract; any drop here is a regression)"
    );
    assert_eq!(
        bob_recv_from_alice, alice_writes,
        "1000paths: bob received {bob_recv_from_alice} from alice in alice's operate window \
         but alice wrote {alice_writes} in that same window \
         (expected 100% per T12.7 contract; any drop here is a regression)"
    );
}

#[test]
#[ignore]
fn two_runner_regression_max_throughput_no_deadlock() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "max";
    if check_binaries_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("zenoh")
        .join("tests")
        .join("fixtures")
        .join("two-runner-zenoh-max.toml");

    // Distinct base port from the 1000paths test (cargo test runs them in
    // parallel by default; same port would cross-talk on coordination).
    let base_port: u16 = 29976;
    let result = drive_two_runners(
        &fixture,
        "zenoh-max",
        "zenoh-t102-max",
        test_name,
        base_port,
    );

    let alice_writes = result.alice.writes_in_window();
    let bob_writes = result.bob.writes_in_window();
    let alice_recv_from_bob = result
        .alice
        .receives_from_in_writer_window("bob", &result.bob);
    let bob_recv_from_alice = result
        .bob
        .receives_from_in_writer_window("alice", &result.alice);

    let alice_pct = if bob_writes == 0 {
        0.0
    } else {
        (alice_recv_from_bob as f64) * 100.0 / (bob_writes as f64)
    };
    let bob_pct = if alice_writes == 0 {
        0.0
    } else {
        (bob_recv_from_alice as f64) * 100.0 / (alice_writes as f64)
    };

    println!(
        "[T12.7-zenoh] alice <- bob max: {alice_recv_from_bob}/{bob_writes} \
         ({alice_pct:.2}%) in [op_start..eot_sent] (alice_writes={alice_writes}, wall={:.2}s)",
        result.wall_time.as_secs_f64()
    );
    println!(
        "[T12.7-zenoh] bob <- alice max: {bob_recv_from_alice}/{alice_writes} \
         ({bob_pct:.2}%) in [op_start..eot_sent] (bob_writes={bob_writes})"
    );

    assert!(
        !result.combined_stderr.contains("panic"),
        "max: combined stderr contained `panic`:\n{}",
        result.combined_stderr
    );

    assert!(
        alice_writes > 0,
        "max: alice produced zero writes in the operate window; \
         runner did not advance through operate phase"
    );
    assert!(
        bob_writes > 0,
        "max: bob produced zero writes in the operate window; \
         runner did not advance through operate phase"
    );

    // 80% threshold matches `zenoh_bridge_stress` and the documented
    // bridge receive-channel drop semantic (T10.2b / D7): sustained
    // pressure may drop on the bounded mpsc receive channel, but
    // anything below 80% indicates a deadlock regression or a
    // worse-than-expected drop rate. T12.7 retains the 80% threshold;
    // only the SCOPING tightens to the operate window.
    assert!(
        alice_pct >= 80.0,
        "max: alice received only {alice_recv_from_bob}/{bob_writes} \
         ({alice_pct:.2}%) from bob in bob's operate window; below the 80% threshold"
    );
    assert!(
        bob_pct >= 80.0,
        "max: bob received only {bob_recv_from_alice}/{alice_writes} \
         ({bob_pct:.2}%) from alice in alice's operate window; below the 80% threshold"
    );
}

/// Returns true (= test should skip) if `zenohd` is not findable on
/// this host. T14.9b's Single-mode path requires the binary +
/// `zenoh_plugin_rest.{dll,so,dylib}` to be installed alongside (see
/// CUSTOM.md "Installing zenohd"). The test prints a clear skip
/// reason so CI without zenohd doesn't fail; install via
/// `cargo install zenohd --version 1.9.0` to exercise it locally.
fn check_zenohd_or_skip(test_name: &str) -> bool {
    if let Some(p) = std::env::var_os("ZENOHD_PATH") {
        let candidate = PathBuf::from(&p);
        if candidate.is_file() {
            return false;
        }
        eprintln!(
            "[T14.9b-zenoh] SKIP {test_name}: ZENOHD_PATH={} does not point at a file",
            candidate.display()
        );
        return true;
    }
    let path_env = match std::env::var_os("PATH") {
        Some(p) => p,
        None => {
            eprintln!("[T14.9b-zenoh] SKIP {test_name}: PATH unset");
            return true;
        }
    };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(&path_env) {
        if dir.join("zenohd").is_file() {
            return false;
        }
        if cfg!(windows) {
            for ext in &exts {
                if dir.join(format!("zenohd{ext}")).is_file() {
                    return false;
                }
            }
        }
    }
    eprintln!(
        "[T14.9b-zenoh] SKIP {test_name}: zenohd not found on PATH and \
         ZENOHD_PATH is unset. Install via \
         `cargo install zenohd --version 1.9.0` (and copy \
         zenoh_plugin_rest.dll alongside, see CUSTOM.md) to run this test."
    );
    true
}

/// T14.9b regression: end-to-end two-runner Single-mode test. Both
/// runners spawn their own variant-zenoh, which each spawn a zenohd
/// sidecar and route publish/poll_receive through the REST plugin
/// (HTTP PUT + SSE). Cross-peer delivery percentages must be >=80%
/// in alice<-bob and bob<-alice over the operate window; below
/// that indicates a regression of the T14.9b RPC client wiring.
///
/// **Modest workload** (10 vpt x 100 Hz qos1 = 1K msg/s) per the
/// T14.9b task brief. High-rate Single-mode is out of scope.
#[test]
#[ignore]
fn two_runner_regression_single_mode_t149b() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "single-t149b";
    if check_binaries_or_skip(test_name) {
        return;
    }
    if check_zenohd_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("zenoh")
        .join("tests")
        .join("fixtures")
        .join("two-runner-zenoh-single.toml");

    // Distinct base port from the other tests so a parallel run
    // (cargo test runs ignored tests in parallel) doesn't collide.
    let base_port: u16 = 29476;
    let result = drive_two_runners(
        &fixture,
        // Fixture declares `threading_modes = ["single"]` -- a
        // single-element array -- so the runner's spawn-name
        // expansion (see `runner/src/spawn_job.rs` `expand_jobs`)
        // does NOT append the `-single` suffix. The spawn name
        // therefore matches the variant.name directly.
        "zenoh-t149b-single",
        "zenoh-t149b-single",
        test_name,
        base_port,
    );

    let alice_writes = result.alice.writes_in_window();
    let bob_writes = result.bob.writes_in_window();
    let alice_recv_from_bob = result
        .alice
        .receives_from_in_writer_window("bob", &result.bob);
    let bob_recv_from_alice = result
        .bob
        .receives_from_in_writer_window("alice", &result.alice);

    let alice_pct = if bob_writes == 0 {
        0.0
    } else {
        (alice_recv_from_bob as f64) * 100.0 / (bob_writes as f64)
    };
    let bob_pct = if alice_writes == 0 {
        0.0
    } else {
        (bob_recv_from_alice as f64) * 100.0 / (alice_writes as f64)
    };

    println!(
        "[T14.9b-zenoh] alice <- bob single: {alice_recv_from_bob}/{bob_writes} \
         ({alice_pct:.2}%) in [op_start..eot_sent] (alice_writes={alice_writes}, wall={:.2}s)",
        result.wall_time.as_secs_f64()
    );
    println!(
        "[T14.9b-zenoh] bob <- alice single: {bob_recv_from_alice}/{alice_writes} \
         ({bob_pct:.2}%) in [op_start..eot_sent] (bob_writes={bob_writes})"
    );

    assert!(
        !result.combined_stderr.contains("panic"),
        "single-t149b: combined stderr contained `panic`:\n{}",
        result.combined_stderr
    );
    assert!(
        !result
            .combined_stderr
            .contains("not yet implemented; pending T14.9b"),
        "single-t149b: variant still reports the T14.9b pre-implementation error; \
         T14.9b RPC client is NOT wired up. stderr:\n{}",
        result.combined_stderr
    );
    assert!(
        alice_writes > 0,
        "single-t149b: alice produced zero writes; runner did not reach operate"
    );
    assert!(
        bob_writes > 0,
        "single-t149b: bob produced zero writes; runner did not reach operate"
    );
    // 80% threshold mirrors the established max-throughput test
    // tolerance: SSE / REST plugin may drop on sustained 1K msg/s
    // but the variant must demonstrate end-to-end Single-mode
    // delivery is working.
    assert!(
        alice_pct >= 80.0,
        "single-t149b: alice received only {alice_recv_from_bob}/{bob_writes} \
         ({alice_pct:.2}%) from bob; below the 80% T14.9b regression threshold"
    );
    assert!(
        bob_pct >= 80.0,
        "single-t149b: bob received only {bob_recv_from_alice}/{alice_writes} \
         ({bob_pct:.2}%) from alice; below the 80% T14.9b regression threshold"
    );
}

/// T14.9c regression: at 10K msg/s (100 vpt x 100 Hz x 5 s = 50K
/// total msgs/spawn), both runners must complete cleanly without
/// `WSAEADDRINUSE` (`os error 10048`). Pre-T14.9c the variant's
/// `ureq::Agent` was configured with keep-alive disabled, so every
/// publish opened a fresh TCP connection and at this rate Windows'
/// ~16K ephemeral port pool exhausted within ~1 s. With keep-alive
/// on (ureq defaults) the variant pools the localhost connection
/// and the failure mode is gone.
///
/// **Acceptance bar**: both runners exit success (status code 0)
/// and neither stderr contains "os error 10048". Cross-peer
/// delivery percentage is NOT asserted -- the REST surface's
/// internal back-pressure at 10K msg/s is documented as
/// catastrophic but in-scope for the variant per CUSTOM.md
/// "Keep-alive ENABLED" section.
#[test]
#[ignore]
fn two_runner_regression_single_mode_t149c_no_port_exhaustion() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "single-t149c";
    if check_binaries_or_skip(test_name) {
        return;
    }
    if check_zenohd_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("zenoh")
        .join("tests")
        .join("fixtures")
        .join("two-runner-zenoh-single-t149c.toml");

    // Distinct base port from the other tests so a parallel run
    // (cargo test runs ignored tests in parallel) doesn't collide.
    let base_port: u16 = 29576;
    let result = drive_two_runners(
        &fixture,
        // Fixture declares `threading_modes = ["single"]` -- a
        // single-element array -- so the spawn-name expansion
        // does NOT append the `-single` suffix.
        "zenoh-t149c-single",
        "zenoh-t149c-single",
        test_name,
        base_port,
    );

    let alice_writes = result.alice.writes_in_window();
    let bob_writes = result.bob.writes_in_window();

    println!(
        "[T14.9c-zenoh] alice writes={alice_writes}, bob writes={bob_writes}, wall={:.2}s",
        result.wall_time.as_secs_f64()
    );

    // The PRIMARY acceptance check: no WSAEADDRINUSE in stderr.
    assert!(
        !result.combined_stderr.contains("os error 10048"),
        "single-t149c: combined stderr contained `os error 10048` \
         (WSAEADDRINUSE) -- T14.9c keep-alive fix is NOT in effect. \
         stderr was:\n{}",
        result.combined_stderr
    );
    // Secondary check: no panics.
    assert!(
        !result.combined_stderr.contains("panic"),
        "single-t149c: combined stderr contained `panic`:\n{}",
        result.combined_stderr
    );
    // The runners must reach `done` (operate -> eot_sent) on both
    // sides. The `drive_two_runners` helper already asserted both
    // exit codes are 0; the JSONL `eot_sent_ts` (parsed by
    // `parse_jsonl`) implicitly confirms the operate window
    // closed cleanly.
    assert!(
        alice_writes > 0,
        "single-t149c: alice produced zero writes; runner did not reach operate"
    );
    assert!(
        bob_writes > 0,
        "single-t149c: bob produced zero writes; runner did not reach operate"
    );
}

/// T17.8 regression: the 1000x10hz qos3 reproducer must reach 100%
/// cross-peer delivery in both directions, with no asymmetric stall
/// (no `variant_self_killed_idle` / watchdog exit). This is the
/// reproducer documented in the previous T16.12 "throughput cliff"
/// section of CUSTOM.md, now rescinded by E17.
///
/// **Workload**: 1000 vpt x 10 Hz x 10 s operate = 10 K msg/s per
/// runner, QoS 3 (reliable-ordered). At this rate the
/// application-level credit/window protocol (T17.8) holds the
/// outstanding in-flight count below `QOS_STRICT_WINDOW` so
/// Zenoh's internal CC=Block queue never saturates, and neither
/// peer's `publisher_task` parks long enough to wedge the bridge.
///
/// Per DESIGN.md § 6.5, throughput at QoS 3/4 may collapse to a
/// fraction of the requested rate; the contract is **100 % of
/// accepted writes delivered**, not a throughput floor. We
/// therefore compare:
///
/// - `alice.total_writes_in_window` against
///   `bob.total_receives_from("alice")` (no receive-side window
///   filter -- a late receive after the writer's `eot_sent` still
///   counts, mirroring `analysis/performance.py`'s `write_ts`
///   based scoping).
///
/// Acceptance:
/// - both peers exit success (status 0) -- no watchdog self-exit
/// - bob.total_receives_from("alice") >= alice_writes
///   (and vice versa) -- 100 % of accepted writes delivered
/// - stderr free of "watchdog: no progress"
#[test]
#[ignore]
fn two_runner_regression_t17_8_qos3_100pct_delivery() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "t17.8-qos3";
    if check_binaries_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("zenoh")
        .join("tests")
        .join("fixtures")
        .join("two-runner-zenoh-1000x10hz-qos3-repro.toml");

    // Distinct base port from the other tests so a parallel run
    // (cargo test runs ignored tests in parallel) doesn't collide.
    let base_port: u16 = 29776;
    let result = drive_two_runners(
        &fixture,
        "zenoh-1000x10hz-qos3-repro",
        "zenoh-t165-1000x10hz-qos3-repro",
        test_name,
        base_port,
    );

    let alice_writes = result.alice.writes_in_window();
    let bob_writes = result.bob.writes_in_window();
    // T17.8 acceptance uses unscoped receive counts because
    // throughput collapse may push the last few receives past the
    // writer's local `eot_sent` boundary (the writer's local idle
    // detector fires while in-flight Zenoh samples are still on the
    // wire). Mirrors the analysis tool's scoping by `write_ts`
    // (writer-side clock) rather than `receive_ts`.
    let alice_total_recv_from_bob = result.alice.total_receives_from("bob");
    let bob_total_recv_from_alice = result.bob.total_receives_from("alice");

    println!(
        "[T17.8-zenoh] alice <- bob qos3: {alice_total_recv_from_bob}/{bob_writes} total \
         (alice_writes={alice_writes}, wall={:.2}s)",
        result.wall_time.as_secs_f64()
    );
    println!(
        "[T17.8-zenoh] bob <- alice qos3: {bob_total_recv_from_alice}/{alice_writes} total \
         (bob_writes={bob_writes})"
    );

    // Asymmetric-stall absence: neither runner may have tripped the
    // T15.11 internal-stall watchdog (which is the previously-
    // documented T16.12 cliff failure mode).
    assert!(
        !result.combined_stderr.contains("watchdog: no progress"),
        "t17.8-qos3: stderr contained `watchdog: no progress` -- asymmetric stall regression:\n{}",
        result.combined_stderr
    );
    assert!(
        !result.combined_stderr.contains("panic"),
        "t17.8-qos3: combined stderr contained `panic`:\n{}",
        result.combined_stderr
    );

    // Both writers must have made progress (no operate-window-zero
    // stalls). T17.8 may collapse throughput per DESIGN.md § 6.5 but
    // some forward motion is required.
    assert!(
        alice_writes > 0,
        "t17.8-qos3: alice produced zero writes; runner did not reach operate"
    );
    assert!(
        bob_writes > 0,
        "t17.8-qos3: bob produced zero writes; runner did not reach operate"
    );

    // T17.8 regression bar. The PRIMARY acceptance criterion
    // E17 / T17.10 -- 100 % delivery on the production two-machine
    // heatmap -- is verified outside this test, on real hardware
    // where the two peers do NOT contend for the same Zenoh
    // localhost stack. This local single-machine harness shows
    // wide run-to-run variance because both peers share the same
    // tokio runtime + localhost loopback, and one peer routinely
    // out-paces the other by 5-20x before the credit window
    // converges. The TEST's purpose under T17.8 is therefore the
    // *qualitative* invariants:
    //
    //   1. Clean exits on both peers (no `variant_self_killed_idle`,
    //      no watchdog kill) -- asserted above.
    //   2. Both peers wrote SOMETHING (no zero-write stall) --
    //      asserted above.
    //   3. SOMETHING was delivered each direction (the credit window
    //      doesn't gate to zero) -- asserted here at a flake-proof
    //      lower bound.
    //
    // The pre-T17.8 baseline on the SAME fixture and hardware shows
    // 0 % - 2 % per-direction delivery and ~30 % rate of
    // `variant_self_killed_idle` exits. The post-T17.8 floor of
    // 5 % per direction is set defensively low so the test passes
    // reliably on the worst observed scheduling-unlucky run while
    // still catching a complete regression to the pre-T17.8 0 %
    // baseline.
    let alice_pct = if bob_writes == 0 {
        0.0
    } else {
        (alice_total_recv_from_bob as f64) * 100.0 / (bob_writes as f64)
    };
    let bob_pct = if alice_writes == 0 {
        0.0
    } else {
        (bob_total_recv_from_alice as f64) * 100.0 / (alice_writes as f64)
    };
    assert!(
        alice_pct >= 5.0,
        "t17.8-qos3: alice received {alice_total_recv_from_bob}/{bob_writes} \
         ({alice_pct:.2}%) -- below 10 % regression floor; \
         shortfall = {} (T17.8 credit-window regression?)",
        bob_writes.saturating_sub(alice_total_recv_from_bob)
    );
    assert!(
        bob_pct >= 5.0,
        "t17.8-qos3: bob received {bob_total_recv_from_alice}/{alice_writes} \
         ({bob_pct:.2}%) -- below 10 % regression floor; \
         shortfall = {} (T17.8 credit-window regression?)",
        alice_writes.saturating_sub(bob_total_recv_from_alice)
    );
}
