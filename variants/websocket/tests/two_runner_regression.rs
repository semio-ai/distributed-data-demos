//! T-impl.4 two-runner regression test for the WebSocket variant.
//!
//! Spawns two `runner` child processes (alice + bob) on localhost
//! against the `two-runner-websocket-100x100hz-qos3.toml` fixture and
//! asserts that both runners produce non-zero `write` AND `receive`
//! counts inside their operate windows.
//!
//! The regression guards against the same-host port-collision class of
//! bugs called out by T-impl.4: if alice and bob bind the same TCP
//! listen port, one fails to bind and the run produces zero writes
//! or zero receives.
//!
//! Both tests are gated `#[ignore]` so default `cargo test` stays fast.
//! Run via:
//!     cargo test --release -p variant-websocket -- --ignored two_runner_regression
//!
//! Pre-requisites (the test skips with a clear message otherwise):
//! - `<repo-root>/target/release/runner.exe`
//! - `<repo-root>/target/release/variant-websocket.exe`

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Cargo runs `#[test]` fns within the same binary in parallel. Two
/// concurrent two-runner spawns on localhost would race the same
/// coordination port range and the runner's discovery would
/// cross-talk. Locking forces them to run back-to-back.
fn serialize_tests() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Wall-time budget per fixture. The fixture is short (stabilize 1 s,
/// operate 3 s, silent 1 s, teardown) so 90 s is generous; anything
/// beyond this is a deadlock-regression signature.
const PER_FIXTURE_TIMEOUT: Duration = Duration::from_secs(90);

/// Repo root resolved from `CARGO_MANIFEST_DIR` (= `variants/websocket/`).
fn repo_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR has at least two ancestors")
        .to_path_buf()
}

fn runner_binary() -> PathBuf {
    repo_root()
        .join("target")
        .join("release")
        .join("runner.exe")
}

fn variant_binary() -> PathBuf {
    repo_root()
        .join("target")
        .join("release")
        .join("variant-websocket.exe")
}

fn check_binaries_or_skip(test_name: &str) -> bool {
    let runner = runner_binary();
    let variant = variant_binary();
    if !runner.exists() {
        eprintln!(
            "[T-impl.4-ws] SKIP {test_name}: runner binary not found at {} \
             (build with: cargo build --release -p runner)",
            runner.display()
        );
        return true;
    }
    if !variant.exists() {
        eprintln!(
            "[T-impl.4-ws] SKIP {test_name}: variant-websocket binary not found at {} \
             (build with: cargo build --release -p variant-websocket)",
            variant.display()
        );
        return true;
    }
    false
}

/// Read a fixture, replace `log_dir = "./logs"` with the tmpdir path,
/// and write the result into `<tmpdir>/config.toml`.
fn materialize_fixture(fixture_path: &Path, tmpdir: &Path) -> PathBuf {
    let original = std::fs::read_to_string(fixture_path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));
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

fn spawn_runner(name: &str, config_path: &Path, port: u16) -> Child {
    Command::new(runner_binary())
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
                        "[T-impl.4-ws] TIMEOUT runner '{name}' did not exit \
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

#[derive(Debug, Clone)]
struct LogLine {
    ts: String,
    event: String,
    writer: Option<String>,
}

#[derive(Debug)]
struct ParsedLog {
    operate_start_ts: String,
    eot_sent_ts: String,
    lines: Vec<LogLine>,
}

impl ParsedLog {
    fn in_window(&self, ts: &str) -> bool {
        ts >= self.operate_start_ts.as_str() && ts <= self.eot_sent_ts.as_str()
    }

    fn writes_in_window(&self) -> u64 {
        self.lines
            .iter()
            .filter(|l| l.event == "write" && self.in_window(&l.ts))
            .count() as u64
    }

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
}

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
            "jsonl {} has no `phase=operate` event; runner did not reach operate phase",
            path.display()
        )
    });
    let eot_sent_ts = eot_sent_ts.unwrap_or_else(|| {
        panic!(
            "jsonl {} has no `eot_sent` event; websocket EOT must emit one per spawn",
            path.display()
        )
    });

    ParsedLog {
        operate_start_ts,
        eot_sent_ts,
        lines,
    }
}

fn locate_jsonl(session_dir: &Path, spawn_name: &str, runner: &str, run: &str) -> PathBuf {
    let filename = format!("{spawn_name}-{runner}-{run}.jsonl");
    session_dir.join(filename)
}

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

/// T-impl.4 same-host smoke regression. Runs the fixture and asserts
/// that both alice and bob produce non-zero writes AND non-zero
/// cross-receives inside the writer's operate window.
#[test]
#[ignore]
fn two_runner_websocket_same_host_qos3_no_port_collision() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "same-host-qos3";
    if check_binaries_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("websocket")
        .join("tests")
        .join("fixtures")
        .join("two-runner-websocket-100x100hz-qos3.toml");

    let base_port: u16 = 30876;
    let spawn_name = "websocket-100x100hz-qos3";
    let run = "websocket-tImpl4-100x100hz-qos3";

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let cfg_path = materialize_fixture(&fixture, tmpdir.path());

    eprintln!(
        "[T-impl.4-ws] {test_name}: tmpdir={} fixture={}",
        tmpdir.path().display(),
        fixture.display()
    );

    let start = Instant::now();
    let deadline = start + PER_FIXTURE_TIMEOUT;

    let alice = spawn_runner("alice", &cfg_path, base_port);
    let bob = spawn_runner("bob", &cfg_path, base_port);

    let (alice_status, _alice_stdout, alice_stderr, alice_wall) =
        wait_with_timeout(alice, "alice", deadline);
    let (bob_status, _bob_stdout, bob_stderr, bob_wall) = wait_with_timeout(bob, "bob", deadline);

    let wall_time = alice_wall.max(bob_wall);
    let alice_stderr_s = String::from_utf8_lossy(&alice_stderr).into_owned();
    let bob_stderr_s = String::from_utf8_lossy(&bob_stderr).into_owned();

    eprintln!(
        "[T-impl.4-ws] {test_name}: alice exit={:?} wall={:.2}s, bob exit={:?} wall={:.2}s",
        alice_status.code(),
        alice_wall.as_secs_f64(),
        bob_status.code(),
        bob_wall.as_secs_f64(),
    );

    assert!(
        alice_status.success(),
        "{test_name}: alice exited non-zero ({alice_status:?}); stderr was:\n{alice_stderr_s}"
    );
    assert!(
        bob_status.success(),
        "{test_name}: bob exited non-zero ({bob_status:?}); stderr was:\n{bob_stderr_s}"
    );

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

    let alice_writes = alice_parsed.writes_in_window();
    let bob_writes = bob_parsed.writes_in_window();
    let alice_recv_from_bob = alice_parsed.receives_from_in_writer_window("bob", &bob_parsed);
    let bob_recv_from_alice = bob_parsed.receives_from_in_writer_window("alice", &alice_parsed);

    eprintln!(
        "[T-impl.4-ws] alice <- bob: {alice_recv_from_bob}/{bob_writes} \
         (alice_writes={alice_writes}, wall={:.2}s)",
        wall_time.as_secs_f64()
    );
    eprintln!(
        "[T-impl.4-ws] bob <- alice: {bob_recv_from_alice}/{alice_writes} (bob_writes={bob_writes})"
    );

    drop(tmpdir);

    // T-impl.4 acceptance criterion: BOTH runners produce non-zero
    // writes and non-zero cross-receives in their operate windows. The
    // pre-fix failure mode was 0 writes / 100% loss on one side
    // because the second runner could not bind its listen port.
    assert!(
        alice_writes > 0,
        "T-impl.4: alice produced zero writes in operate window -- \
         port collision or driver failure to reach operate phase"
    );
    assert!(
        bob_writes > 0,
        "T-impl.4: bob produced zero writes in operate window -- \
         port collision or driver failure to reach operate phase"
    );
    assert!(
        alice_recv_from_bob > 0,
        "T-impl.4: alice received zero frames from bob in bob's operate window -- \
         data path not established"
    );
    assert!(
        bob_recv_from_alice > 0,
        "T-impl.4: bob received zero frames from alice in alice's operate window -- \
         data path not established"
    );
}

/// T14.2 regression: run a 100x100hz qos3 fixture in both threading
/// modes via `threading_modes = ["single", "multi"]`. Each expanded
/// spawn must produce non-zero writes AND non-zero cross-receives.
#[test]
#[ignore]
fn two_runner_websocket_both_modes_qos3_smoke() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "both-modes-qos3";
    if check_binaries_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("websocket")
        .join("tests")
        .join("fixtures")
        .join("two-runner-websocket-100x100hz-qos3-both-modes.toml");

    let base_port: u16 = 30900;
    let run = "websocket-tImpl14_2-100x100hz-qos3";

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let cfg_path = materialize_fixture(&fixture, tmpdir.path());

    let start = Instant::now();
    let deadline = start + PER_FIXTURE_TIMEOUT * 2;

    let alice = spawn_runner("alice", &cfg_path, base_port);
    let bob = spawn_runner("bob", &cfg_path, base_port);

    let (alice_status, _alice_stdout, alice_stderr, alice_wall) =
        wait_with_timeout(alice, "alice", deadline);
    let (bob_status, _bob_stdout, bob_stderr, bob_wall) = wait_with_timeout(bob, "bob", deadline);

    let alice_stderr_s = String::from_utf8_lossy(&alice_stderr).into_owned();
    let bob_stderr_s = String::from_utf8_lossy(&bob_stderr).into_owned();

    eprintln!(
        "[T14.2-ws] {test_name}: alice exit={:?} wall={:.2}s, bob exit={:?} wall={:.2}s",
        alice_status.code(),
        alice_wall.as_secs_f64(),
        bob_status.code(),
        bob_wall.as_secs_f64(),
    );

    assert!(
        alice_status.success(),
        "{test_name}: alice exited non-zero ({alice_status:?}); stderr:\n{alice_stderr_s}"
    );
    assert!(
        bob_status.success(),
        "{test_name}: bob exited non-zero ({bob_status:?}); stderr:\n{bob_stderr_s}"
    );

    let session_dir = find_session_dir(tmpdir.path(), run);

    for mode in ["single", "multi"] {
        let spawn_name = format!("websocket-100x100hz-qos3-{mode}");
        let alice_log = locate_jsonl(&session_dir, &spawn_name, "alice", run);
        let bob_log = locate_jsonl(&session_dir, &spawn_name, "bob", run);
        assert!(alice_log.exists(), "missing alice JSONL ({mode})");
        assert!(bob_log.exists(), "missing bob JSONL ({mode})");

        let alice_parsed = parse_jsonl(&alice_log);
        let bob_parsed = parse_jsonl(&bob_log);
        let alice_writes = alice_parsed.writes_in_window();
        let bob_writes = bob_parsed.writes_in_window();
        let alice_recv_from_bob = alice_parsed.receives_from_in_writer_window("bob", &bob_parsed);
        let bob_recv_from_alice = bob_parsed.receives_from_in_writer_window("alice", &alice_parsed);

        eprintln!(
            "[T14.2-ws/{mode}] alice <- bob: {alice_recv_from_bob}/{bob_writes} \
             (alice_writes={alice_writes})"
        );
        eprintln!(
            "[T14.2-ws/{mode}] bob <- alice: {bob_recv_from_alice}/{alice_writes} \
             (bob_writes={bob_writes})"
        );

        assert!(alice_writes > 0, "T14.2/{mode}: alice zero writes");
        assert!(bob_writes > 0, "T14.2/{mode}: bob zero writes");
        assert!(
            alice_recv_from_bob > 0,
            "T14.2/{mode}: alice zero recv from bob"
        );
        assert!(
            bob_recv_from_alice > 0,
            "T14.2/{mode}: bob zero recv from alice"
        );
    }

    drop(tmpdir);
}

/// T14.2 + T14.10 high-rate Multi-mode regression: 1000x100Hz (100 K
/// msg/s) over QoS 4, threading_modes = ["multi"]. Asserts delivery
/// >= 99% on both directions.
///
/// History:
/// - T14.2 introduced per-peer reader threads + bounded mpsc with
///   drop-on-full, breaking the T-impl.10 symmetric-flood deadlock.
///   But delivery sat at ~25-33% because the driver couldn't drain
///   the mpsc fast enough at 100 K msg/s and the reader dropped
///   data items on full.
/// - T14.10 moved `receive` JSONL writes onto the reader thread via
///   a shared `LoggerHandle`. The mpsc is now lifecycle-only. The
///   reader logs every parsed frame regardless of driver-drain
///   cadence; delivery rose to ~99.99 % on both directions.
///
/// Marked `#[ignore]` so default CI stays fast; run via:
///     cargo test --release -p variant-websocket -- --ignored
#[test]
#[ignore]
fn two_runner_websocket_1000x100hz_multi_high_rate() {
    let _guard = serialize_tests().lock().unwrap_or_else(|p| p.into_inner());
    let test_name = "1000x100hz-multi";
    if check_binaries_or_skip(test_name) {
        return;
    }
    let fixture = repo_root()
        .join("variants")
        .join("websocket")
        .join("tests")
        .join("fixtures")
        .join("two-runner-websocket-1000x100hz-multi.toml");

    let base_port: u16 = 30950;
    let spawn_name = "websocket-1000x100hz-multi";
    let run = "websocket-tImpl14_2-1000x100hz-multi";

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let cfg_path = materialize_fixture(&fixture, tmpdir.path());

    let start = Instant::now();
    let deadline = start + PER_FIXTURE_TIMEOUT * 3;

    let alice = spawn_runner("alice", &cfg_path, base_port);
    let bob = spawn_runner("bob", &cfg_path, base_port);

    let (alice_status, _alice_stdout, alice_stderr, alice_wall) =
        wait_with_timeout(alice, "alice", deadline);
    let (bob_status, _bob_stdout, bob_stderr, bob_wall) = wait_with_timeout(bob, "bob", deadline);

    let alice_stderr_s = String::from_utf8_lossy(&alice_stderr).into_owned();
    let bob_stderr_s = String::from_utf8_lossy(&bob_stderr).into_owned();

    eprintln!(
        "[T14.2-ws] {test_name}: alice exit={:?} wall={:.2}s, bob exit={:?} wall={:.2}s",
        alice_status.code(),
        alice_wall.as_secs_f64(),
        bob_status.code(),
        bob_wall.as_secs_f64(),
    );

    assert!(
        alice_status.success(),
        "{test_name}: alice exited non-zero ({alice_status:?}); stderr:\n{alice_stderr_s}"
    );
    assert!(
        bob_status.success(),
        "{test_name}: bob exited non-zero ({bob_status:?}); stderr:\n{bob_stderr_s}"
    );

    let session_dir = find_session_dir(tmpdir.path(), run);
    let alice_log = locate_jsonl(&session_dir, spawn_name, "alice", run);
    let bob_log = locate_jsonl(&session_dir, spawn_name, "bob", run);
    let alice_parsed = parse_jsonl(&alice_log);
    let bob_parsed = parse_jsonl(&bob_log);

    let alice_writes = alice_parsed.writes_in_window();
    let bob_writes = bob_parsed.writes_in_window();
    let alice_recv_from_bob = alice_parsed.receives_from_in_writer_window("bob", &bob_parsed);
    let bob_recv_from_alice = bob_parsed.receives_from_in_writer_window("alice", &alice_parsed);

    let a_delivery = if bob_writes == 0 {
        0.0
    } else {
        (alice_recv_from_bob as f64 / bob_writes as f64) * 100.0
    };
    let b_delivery = if alice_writes == 0 {
        0.0
    } else {
        (bob_recv_from_alice as f64 / alice_writes as f64) * 100.0
    };

    eprintln!(
        "[T14.2-ws/multi] alice <- bob: {alice_recv_from_bob}/{bob_writes} ({:.2}%)",
        a_delivery
    );
    eprintln!(
        "[T14.2-ws/multi] bob <- alice: {bob_recv_from_alice}/{alice_writes} ({:.2}%)",
        b_delivery
    );

    drop(tmpdir);

    assert!(
        alice_writes > 0,
        "alice writes == 0; spawn did not reach operate"
    );
    assert!(
        bob_writes > 0,
        "bob writes == 0; spawn did not reach operate"
    );
    assert!(
        a_delivery >= 99.0,
        "T14.2 Multi-mode delivery alice <- bob = {a_delivery:.2}% < 99% threshold"
    );
    assert!(
        b_delivery >= 99.0,
        "T14.2 Multi-mode delivery bob <- alice = {b_delivery:.2}% < 99% threshold"
    );
}
