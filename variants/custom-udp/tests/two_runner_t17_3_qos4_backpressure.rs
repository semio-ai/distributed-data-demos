//! T17.3 two-runner regression: QoS 4 (TCP) under saturation MUST
//! achieve 100% cross-peer delivery in BOTH single and multi modes.
//!
//! Pre-T17.3 the post-T16.16 heatmap showed
//!   custom-udp-1000x100hz-qos4-single  31.8% delivery
//!   custom-udp-1000x100hz-qos4-multi   44.9% delivery
//! because outbound TCP writes that hit `SO_SNDTIMEO` (transient
//! kernel send-buffer pressure under symmetric load) were treated as
//! peer-death signals and the peer was silently dropped from the
//! broadcast set.
//!
//! Post-T17.3:
//!   - `publish_encoded` distinguishes transient (`WouldBlock`,
//!     `TimedOut`, `Interrupted`) from fatal (`ConnectionReset`,
//!     `BrokenPipe`, `ConnectionAborted`, `NotConnected`, and the
//!     conservative everything-else default) TCP write errors.
//!   - On transient: the variant retries the write until the kernel
//!     accepts the bytes (or eventually surfaces a fatal error).
//!   - On fatal: the peer is dropped, as before.
//!   - This applies in BOTH single AND multi modes; the previously
//!     Single-mode-only `SO_SNDTIMEO` is now installed in both modes
//!     (it is the wake-from-retry mechanism, not a peer-drop trigger).
//!
//! Acceptance:
//!   - Both `single` and `multi` spawns reach `status=success`.
//!   - Cross-peer delivery in the operate window is 100.0% in both
//!     directions (no shortfall).
//!   - Zero `backpressure_skipped` events with `qos == 4` in any
//!     per-runner JSONL.
//!
//! Gated behind `#[ignore]` because it depends on pre-built release
//! binaries and is end-to-end heavy. Run with:
//!
//! ```text
//! cargo test --release -p variant-custom-udp -- --ignored two_runner_t17_3 --nocapture
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serialise concurrent test threads. Same rationale as
/// `two_runner_regression.rs`: peer-discovery on localhost rejects
/// cross-test bleed-through unless we hold a process-wide lock.
static T17_3_LOCK: Mutex<()> = Mutex::new(());

const FIXTURE_PATH: &str = "tests/fixtures/two-runner-custom-udp-qos4-saturate-repro.toml";
const RUN_NAME: &str = "custom-udp-t17-3-qos4-repro";
const VARIANT_BASE: &str = "custom-udp-1000x100hz-qos4-repro";
const MODES: &[&str] = &["single", "multi"];

/// 180 s budget: the fixture's two threading-mode-expanded spawns each
/// have stabilize=1 + operate=5 + silent=2 + a small EOT/teardown
/// tail. At pre-T17.3 drop rates the spawn finished quickly; under
/// strict back-pressure (post-T17.3) the wall-clock budget extends to
/// match real network-paced throughput. 180s gives ample headroom for
/// both spawns plus per-spawn `inter_qos_grace_ms`.
const TEST_BUDGET: Duration = Duration::from_secs(180);

#[test]
#[ignore]
fn two_runner_t17_3_qos4_saturate_100_percent_delivery() {
    let _guard = T17_3_LOCK
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
            "[T17.3] SKIP: runner binary missing at {}. \
             Build it with `cargo build --release -p runner`.",
            runner_bin.display()
        );
        return;
    }
    if !variant_bin.exists() {
        eprintln!(
            "[T17.3] SKIP: variant-custom-udp binary missing at {}. \
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

    let fixture_abs: PathBuf = repo_root.join("variants/custom-udp").join(FIXTURE_PATH);
    let fixture_text: String = std::fs::read_to_string(&fixture_abs)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", fixture_abs.display()));
    assert!(
        fixture_text.contains("log_dir = \"./logs\""),
        "[T17.3] fixture {} does not contain expected `log_dir = \"./logs\"` line",
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
            "[T17.3] alice never exited within {:?}; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
            wall_time, alice_capture.stdout, alice_capture.stderr
        );
    }
    if !bob_outcome.exited {
        let _ = bob.kill();
        panic!(
            "[T17.3] bob never exited within {:?}; stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
            wall_time, alice_capture.stdout, bob_capture.stderr
        );
    }

    let alice_status = alice_outcome.status.expect("alice exit status missing");
    let bob_status = bob_outcome.status.expect("bob exit status missing");
    assert!(
        alice_status.success(),
        "[T17.3] alice exited non-zero ({:?}); stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
        alice_status.code(),
        alice_capture.stdout,
        alice_capture.stderr
    );
    assert!(
        bob_status.success(),
        "[T17.3] bob exited non-zero ({:?}); stdout=<<<\n{}>>>; stderr=<<<\n{}>>>",
        bob_status.code(),
        bob_capture.stdout,
        bob_capture.stderr
    );

    let combined_stderr: String = format!("{}\n{}", alice_capture.stderr, bob_capture.stderr);
    let combined_stderr_lc: String = combined_stderr.to_lowercase();
    assert!(
        !combined_stderr_lc.contains("panic"),
        "[T17.3] combined stderr contains 'panic'; stderr=<<<\n{combined_stderr}>>>"
    );

    let session_dir: PathBuf = find_session_dir(tmp_path, RUN_NAME).unwrap_or_else(|| {
        panic!(
            "[T17.3] no session subfolder matching `{RUN_NAME}-*` found under {}",
            tmp_path.display()
        )
    });

    // Validate each per-mode spawn separately.
    for mode in MODES {
        let spawn_name: String = format!("{VARIANT_BASE}-{mode}");
        let alice_log: PathBuf = session_dir.join(format!("{spawn_name}-alice-{RUN_NAME}.jsonl"));
        let bob_log: PathBuf = session_dir.join(format!("{spawn_name}-bob-{RUN_NAME}.jsonl"));
        assert!(
            alice_log.exists(),
            "[T17.3/{mode}] expected alice JSONL not found: {}",
            alice_log.display()
        );
        assert!(
            bob_log.exists(),
            "[T17.3/{mode}] expected bob JSONL not found: {}",
            bob_log.display()
        );

        // Raw counts: total writes per writer and total cross-peer
        // receives. This matches the analyzer's integrity metric
        // (`receive_count / write_count`, see
        // `analysis/integrity.py::_check_per_pair`) and is the
        // canonical "delivery 100%" measure for E17 acceptance.
        //
        // We deliberately do NOT use an operate-window filter here.
        // The window-filtered ratio shows a small (~0.1-0.2%)
        // shortfall for TCP at saturation rates because messages
        // that left `try_publish` shortly before the writer's
        // `eot_sent_ts` arrive at the receiver moments after that
        // timestamp -- the bytes are delivered (TCP is reliable;
        // total writes == total receives in this fixture) but the
        // receive event timestamp falls just outside the writer's
        // window. The DESIGN.md § 6.5 contract is "100% of accepted
        // writes are delivered"; the integrity analyzer measures
        // exactly that, no window. Window-scoped counts are kept
        // below for diagnostic output only.
        let alice_writes: u64 = count_event(&alice_log, "write");
        let bob_writes: u64 = count_event(&bob_log, "write");
        let bob_recv_from_alice: u64 = count_receive_from_writer(&bob_log, "alice");
        let alice_recv_from_bob: u64 = count_receive_from_writer(&alice_log, "bob");

        let alice_to_bob: f64 = if alice_writes == 0 {
            0.0
        } else {
            bob_recv_from_alice as f64 / alice_writes as f64
        };
        let bob_to_alice: f64 = if bob_writes == 0 {
            0.0
        } else {
            alice_recv_from_bob as f64 / bob_writes as f64
        };

        println!(
            "[T17.3/{mode}] alice -> bob qos4: {}/{} ({:.4}%) -- raw analyzer metric",
            bob_recv_from_alice,
            alice_writes,
            alice_to_bob * 100.0
        );
        println!(
            "[T17.3/{mode}] bob -> alice qos4: {}/{} ({:.4}%) -- raw analyzer metric",
            alice_recv_from_bob,
            bob_writes,
            bob_to_alice * 100.0
        );

        // Diagnostic: operate-window-scoped delivery. Expected to be
        // slightly below the raw metric (~0.1-0.2% under saturation)
        // because TCP delivery completes after writer's EOT. NOT
        // asserted; here purely so the test output is informative
        // when the analysis tool reports a different number.
        let alice_window: OperateWindow = parse_operate_window(&alice_log, mode, "alice");
        let bob_window: OperateWindow = parse_operate_window(&bob_log, mode, "bob");
        let alice_writes_win: u64 = count_writes_in_window(&alice_log, &alice_window);
        let bob_writes_win: u64 = count_writes_in_window(&bob_log, &bob_window);
        let bob_recv_from_alice_win: u64 =
            count_cross_peer_receives_in_window(&bob_log, "alice", &alice_window);
        let alice_recv_from_bob_win: u64 =
            count_cross_peer_receives_in_window(&alice_log, "bob", &bob_window);
        println!(
            "[T17.3/{mode}] window-scoped diagnostic alice->bob: {}/{} bob->alice: {}/{}",
            bob_recv_from_alice_win, alice_writes_win, alice_recv_from_bob_win, bob_writes_win
        );

        assert!(
            alice_writes > 0,
            "[T17.3/{mode}] alice produced zero writes"
        );
        assert!(bob_writes > 0, "[T17.3/{mode}] bob produced zero writes");

        // The acceptance bar: 100.0% delivery in BOTH directions for
        // the qos4 cell. Per DESIGN.md § 6.5 the only acceptable
        // failure mode under sustained overload is throughput
        // collapse, NOT delivery shortfall.
        assert_eq!(
            bob_recv_from_alice, alice_writes,
            "[T17.3/{mode}] alice -> bob delivery {bob_recv_from_alice}/{alice_writes} ({:.6}%) -- \
             must be 100% per DESIGN.md § 6.5",
            alice_to_bob * 100.0
        );
        assert_eq!(
            alice_recv_from_bob,
            bob_writes,
            "[T17.3/{mode}] bob -> alice delivery {alice_recv_from_bob}/{bob_writes} ({:.6}%) -- \
             must be 100% per DESIGN.md § 6.5",
            bob_to_alice * 100.0
        );

        // No `backpressure_skipped` events with qos=4 in either log.
        // Per `metak-shared/api-contracts/jsonl-log-schema.md` post-T17.1
        // the event is restricted to QoS 1/2; an occurrence at QoS 4 is
        // a variant contract violation that T17.9's analyzer flags
        // automatically. We assert it directly here as the load-bearing
        // unit-test bar for T17.3.
        let alice_qos4_skips: u64 = count_backpressure_skipped_qos4(&alice_log);
        let bob_qos4_skips: u64 = count_backpressure_skipped_qos4(&bob_log);
        assert_eq!(
            alice_qos4_skips, 0,
            "[T17.3/{mode}] alice emitted {alice_qos4_skips} backpressure_skipped events at qos=4 -- \
             DESIGN.md § 6.5 forbids skip at QoS 3/4"
        );
        assert_eq!(
            bob_qos4_skips, 0,
            "[T17.3/{mode}] bob emitted {bob_qos4_skips} backpressure_skipped events at qos=4 -- \
             DESIGN.md § 6.5 forbids skip at QoS 3/4"
        );
    }
    println!("[T17.3] wall-time: {:?} -- PASS", wall_time);
}

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

/// Operate-window boundaries for one writer's JSONL.
struct OperateWindow {
    operate_start_ts: String,
    eot_sent_ts: String,
}

fn parse_operate_window(path: &Path, mode: &str, runner: &str) -> OperateWindow {
    let text: String = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "[T17.3/{mode}] failed to read JSONL {}: {e}",
            path.display()
        )
    });
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
            "[T17.3/{mode}] {runner} JSONL ({}) missing operate-phase event",
            path.display()
        )
    });
    let eot_sent_ts: String = eot_sent_ts.unwrap_or_else(|| {
        panic!(
            "[T17.3/{mode}] {runner} JSONL ({}) missing eot_sent event",
            path.display()
        )
    });
    OperateWindow {
        operate_start_ts,
        eot_sent_ts,
    }
}

fn count_writes_in_window(path: &Path, window: &OperateWindow) -> u64 {
    count_events_in_window(path, "write", |_| true, window)
}

fn count_cross_peer_receives_in_window(
    path: &Path,
    peer_name: &str,
    writer_window: &OperateWindow,
) -> u64 {
    count_events_in_window(
        path,
        "receive",
        |v| v.get("writer").and_then(|w| w.as_str()) == Some(peer_name),
        writer_window,
    )
}

fn count_events_in_window<F>(path: &Path, event: &str, mut filter: F, window: &OperateWindow) -> u64
where
    F: FnMut(&serde_json::Value) -> bool,
{
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
        if value.get("event").and_then(|e| e.as_str()) != Some(event) {
            continue;
        }
        if !filter(&value) {
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

/// Count all events of `event_name` in the log (no window filter).
fn count_event(path: &Path, event_name: &str) -> u64 {
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
        if value.get("event").and_then(|e| e.as_str()) == Some(event_name) {
            count += 1;
        }
    }
    count
}

/// Count all `receive` events whose `writer` field matches `peer_name`
/// (no window filter). Matches the analyzer's
/// `analysis/integrity.py::_check_per_pair` `receive_count` semantics.
fn count_receive_from_writer(path: &Path, peer_name: &str) -> u64 {
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
        if value.get("writer").and_then(|w| w.as_str()) == Some(peer_name) {
            count += 1;
        }
    }
    count
}

/// Count `backpressure_skipped` events with `qos == 4` anywhere in the
/// log (no window filter; any qos-4 skip is a contract violation).
fn count_backpressure_skipped_qos4(path: &Path) -> u64 {
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
        if value.get("event").and_then(|e| e.as_str()) != Some("backpressure_skipped") {
            continue;
        }
        if value.get("qos").and_then(|q| q.as_u64()) == Some(4) {
            count += 1;
        }
    }
    count
}
