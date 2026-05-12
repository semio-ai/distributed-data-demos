//! T14.13 end-to-end regression: launch two `variant-quic` subprocesses
//! pointed at each other on loopback, run a qos4 burst through the
//! standard variant-base protocol driver, then scan both runners'
//! `receive` events to confirm strictly-ascending per-(writer, receiver)
//! seq order.
//!
//! Marked `#[ignore]` because it spawns real subprocesses and binds UDP
//! ports; opt in with `cargo test --release -p variant-quic -- --ignored`.
//!
//! The pre-T14.13 build of variant-quic opened a fresh unidirectional
//! QUIC stream per qos4 message and `tokio::spawn`-ed the write, which
//! interleaved on the network and produced ~42 K out-of-order receives
//! per direction at the 100 vpt x 100 Hz x 10 s smoke scale (see
//! `metak-orchestrator/STATUS.md` T14.13 audit). This test drives a
//! smaller burst (enough to exercise the receive pipeline without
//! taking a long time) and asserts ZERO out-of-order receives on each
//! direction.

use std::io::{BufRead, BufReader};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Pick an ephemeral free UDP port by binding a throwaway socket and
/// recording the assigned port. Same trick the unit tests use.
fn pick_free_udp_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = s.local_addr().expect("local_addr").port();
    drop(s);
    p
}

/// Parameters bundle for `spawn_variant_quic`. Bundled to keep the
/// function under the clippy `too_many_arguments` threshold and so the
/// call sites read like keyword-args instead of an opaque positional
/// list.
struct SpawnArgs<'a> {
    binary: &'a str,
    runner: &'a str,
    peers: &'a str,
    base_port: u16,
    log_dir: &'a str,
    run: &'a str,
    tick_rate_hz: u32,
    values_per_tick: u32,
    operate_secs: u32,
}

/// Launch one variant-quic subprocess wired up with the new
/// runner-injected CLI shape so that it dials the other runner on
/// loopback. Returns the spawned `Child`; caller is responsible for
/// `wait()`-ing or `kill()`-ing it.
fn spawn_variant_quic(a: &SpawnArgs<'_>) -> Child {
    let binary = a.binary;
    let runner = a.runner;
    let peers = a.peers;
    let base_port = a.base_port;
    let log_dir = a.log_dir;
    let run = a.run;
    let tick_rate_hz = a.tick_rate_hz;
    let values_per_tick = a.values_per_tick;
    let operate_secs = a.operate_secs;
    let child = Command::new(binary)
        .args([
            "--tick-rate-hz",
            &tick_rate_hz.to_string(),
            "--stabilize-secs",
            "1",
            "--operate-secs",
            &operate_secs.to_string(),
            "--silent-secs",
            "1",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            &values_per_tick.to_string(),
            "--qos",
            "4",
            "--log-dir",
            log_dir,
            "--launch-ts",
            "2026-05-11T14:00:00.000000000Z",
            "--variant",
            "quic",
            "--runner",
            runner,
            "--run",
            run,
            "--threading-mode",
            "multi",
            "--peers",
            peers,
            "--",
            "--base-port",
            &base_port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn variant-quic subprocess");
    child
}

#[test]
#[ignore = "spawns subprocesses + binds UDP ports; opt in with --ignored"]
// Clippy's `zombie_processes` lint flags the `try_wait` loop because
// it doesn't trace the kill+wait fallback at the end. Every code path
// out of the function does end with either a successful `try_wait`
// observation or a `kill()`+`wait()` -- the explicit
// `expect("alice/bob exit status missing even after kill+wait fallback")`
// asserts that. Allow the lint locally with rationale.
#[allow(clippy::zombie_processes)]
fn two_runner_t14_13_qos4_ordering() {
    let binary = env!("CARGO_BIN_EXE_variant-quic");
    let tmp = tempfile::tempdir().expect("temp dir");
    let log_dir = tmp.path().to_path_buf();
    let log_dir_str = log_dir.to_str().unwrap().to_string();

    // Two free ports near a common base so the variant's
    // base-port-derivation lands on real free slots. We pick the
    // base_port and just let both runners derive their own bind ports
    // from it via the documented stride (runner_index * 1 + (qos-1) *
    // 10). qos=4 so the per-runner offset is 30 + index.
    //
    // To make absolutely sure both ports are free, we probe two
    // ephemeral ports first then compute a base such that
    // (base + 30) and (base + 31) are both free.
    let port_alice = pick_free_udp_port();
    let port_bob = pick_free_udp_port();
    // base_port + 0 + (4-1)*10 = port_alice
    // base_port + 1 + (4-1)*10 = port_bob
    // The variant requires port_bob = port_alice + 1; if it isn't, pick
    // a clean base from port_alice and probe-verify port_alice+1 is free.
    let _ = port_bob;
    let base_port: u16 = port_alice.saturating_sub(30);
    // Probe-bind the derived port pair to confirm they're free, then
    // drop the probes immediately.
    let alice_port = base_port + 30;
    let bob_port = base_port + 31;
    {
        let _a = UdpSocket::bind(format!("127.0.0.1:{alice_port}")).expect("alice port free");
        let _b = UdpSocket::bind(format!("127.0.0.1:{bob_port}")).expect("bob port free");
    }

    let peers = "alice=127.0.0.1,bob=127.0.0.1";
    let run = "t14_13_qos4";

    // Run at a modest rate -- 50 vpt x 50 Hz x 5 s = 12,500 messages
    // per writer. Plenty to expose any cross-stream interleaving
    // (pre-fix this would produce thousands of out-of-order receives).
    let tick_rate_hz = 50u32;
    let values_per_tick = 50u32;
    let operate_secs = 5u32;

    let mut bob = spawn_variant_quic(&SpawnArgs {
        binary,
        runner: "bob",
        peers,
        base_port,
        log_dir: &log_dir_str,
        run,
        tick_rate_hz,
        values_per_tick,
        operate_secs,
    });
    // Brief head-start so bob's accept loop is armed before alice
    // dials -- matches the unit-test's ordering trick.
    thread::sleep(Duration::from_millis(300));
    let mut alice = spawn_variant_quic(&SpawnArgs {
        binary,
        runner: "alice",
        peers,
        base_port,
        log_dir: &log_dir_str,
        run,
        tick_rate_hz,
        values_per_tick,
        operate_secs,
    });

    // Drain stderr in background threads so the subprocess pipes don't
    // back up. We don't print to test stdout under the default test
    // harness, but we DO collect into Strings for failure diagnostics.
    let alice_stderr = alice.stderr.take().expect("alice stderr");
    let bob_stderr = bob.stderr.take().expect("bob stderr");
    let alice_log = thread::spawn(move || {
        let mut out = String::new();
        let reader = BufReader::new(alice_stderr);
        for line in reader.lines().map_while(Result::ok) {
            out.push_str(&line);
            out.push('\n');
        }
        out
    });
    let bob_log = thread::spawn(move || {
        let mut out = String::new();
        let reader = BufReader::new(bob_stderr);
        for line in reader.lines().map_while(Result::ok) {
            out.push_str(&line);
            out.push('\n');
        }
        out
    });

    // Wall-clock deadline well above the expected variant duration.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut alice_status: Option<std::process::ExitStatus> = None;
    let mut bob_status: Option<std::process::ExitStatus> = None;
    while Instant::now() < deadline && !(alice_status.is_some() && bob_status.is_some()) {
        if alice_status.is_none() {
            if let Some(s) = alice.try_wait().expect("alice wait") {
                alice_status = Some(s);
            }
        }
        if bob_status.is_none() {
            if let Some(s) = bob.try_wait().expect("bob wait") {
                bob_status = Some(s);
            }
        }
        if !(alice_status.is_some() && bob_status.is_some()) {
            thread::sleep(Duration::from_millis(100));
        }
    }
    // Force-reap any process that didn't exit on its own so we don't
    // leave a zombie behind (clippy::zombie_processes); wait() after
    // kill() blocks only briefly because the child is already dead.
    if alice_status.is_none() {
        let _ = alice.kill();
        alice_status = alice.wait().ok();
    }
    if bob_status.is_none() {
        let _ = bob.kill();
        bob_status = bob.wait().ok();
    }
    let alice_stderr_text = alice_log.join().unwrap_or_default();
    let bob_stderr_text = bob_log.join().unwrap_or_default();
    let alice_status =
        alice_status.expect("alice exit status missing even after kill+wait fallback");
    let bob_status = bob_status.expect("bob exit status missing even after kill+wait fallback");
    assert!(
        alice_status.success() && bob_status.success(),
        "subprocesses did not exit cleanly within deadline. \
         alice={alice_status:?} bob={bob_status:?}\nalice_stderr:\n{alice_stderr_text}\nbob_stderr:\n{bob_stderr_text}"
    );

    // Parse the per-runner jsonl logs and assert per-(writer)
    // ascending seq order on each receive event in receive-time
    // order. This mirrors what `analysis/integrity.py` does
    // (`prev_seq` shift + comparison) and is the actual gate the
    // T11.5 report uses.
    let alice_log_path = find_log(&log_dir, "alice");
    let bob_log_path = find_log(&log_dir, "bob");

    let alice_ooo = count_out_of_order_receives(&alice_log_path);
    let bob_ooo = count_out_of_order_receives(&bob_log_path);

    assert_eq!(
        alice_ooo,
        0,
        "alice observed {alice_ooo} out-of-order receives in qos4 stream; \
         T14.13 regression. alice_stderr (tail):\n{}",
        tail(&alice_stderr_text, 60)
    );
    assert_eq!(
        bob_ooo,
        0,
        "bob observed {bob_ooo} out-of-order receives in qos4 stream; \
         T14.13 regression. bob_stderr (tail):\n{}",
        tail(&bob_stderr_text, 60)
    );
}

/// Locate the variant-quic jsonl log file for the given runner inside
/// `log_dir`. The variant uses
/// `<variant>-<runner>-<run>.jsonl` per the runner contract.
fn find_log(log_dir: &std::path::Path, runner: &str) -> PathBuf {
    let entries = std::fs::read_dir(log_dir).expect("read log_dir");
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains(&format!("-{runner}-")) && name.ends_with(".jsonl") {
            return e.path();
        }
    }
    panic!(
        "no jsonl log found for runner {runner} in {}",
        log_dir.display()
    );
}

/// Count `receive` events whose `seq` is strictly less than the
/// previous `receive`'s `seq` in file order, grouped by `writer`.
/// Returns the sum across all writers (matches the analysis tool's
/// per-(writer, receiver) `out_of_order` aggregation summed).
fn count_out_of_order_receives(log_path: &std::path::Path) -> usize {
    let f = std::fs::File::open(log_path).expect("open log");
    let reader = BufReader::new(f);
    // Per-writer running max-prev-seq state -- the analysis tool
    // sorts by receive_ts then takes prev-seq; the log is written in
    // receive order by the driver so file order IS receive order.
    let mut prev: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut ooo = 0usize;
    for line in reader.lines().map_while(Result::ok) {
        // Cheap event-type filter before json parse to keep this hot.
        if !line.contains("\"event\":\"receive\"") {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let writer = match v.get("writer").and_then(|w| w.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let seq = match v.get("seq").and_then(|s| s.as_u64()) {
            Some(s) => s,
            None => continue,
        };
        if let Some(&p) = prev.get(&writer) {
            if seq < p {
                ooo += 1;
            }
        }
        prev.insert(writer, seq);
    }
    ooo
}

fn tail(s: &str, n_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n_lines);
    lines[start..].join("\n")
}
