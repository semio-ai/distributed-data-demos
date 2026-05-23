//! End-to-end regression for the QoS 1 oversize-datagram bug:
//! launch two `variant-quic` subprocesses with the `mixed-types`
//! workload at 1000 vpt, qos=1, threading=multi on loopback. The
//! workload generator produces dict WriteOps with 200-600 array
//! leaves per tick; encoded payloads routinely exceed the QUIC
//! `max_datagram_frame_size` (~1200 B on loopback). Pre-fix:
//! `Connection::send_datagram` returned `SendDatagramError::TooLarge`,
//! the variant bubbled it as `Error: quic send_datagram failed:
//! datagram too large`, and the spawn exited 1 with no parquet
//! written. Post-fix the variant treats `TooLarge` as a skip
//! (mirrors `ConnectionLost`), returns `Ok(false)` so the driver
//! records `backpressure_skipped`, emits exactly one stderr
//! `[quic] note: ...` line per spawn, and the spawn exits 0.
//!
//! Marked `#[ignore]` because it spawns real subprocesses and binds
//! UDP ports; opt in with
//! `cargo test --release -p variant-quic -- --ignored`.

use std::io::{BufRead, BufReader};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Pick an ephemeral free UDP port by binding a throwaway socket and
/// recording the assigned port.
fn pick_free_udp_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = s.local_addr().expect("local_addr").port();
    drop(s);
    p
}

struct SpawnArgs<'a> {
    binary: &'a str,
    runner: &'a str,
    peers: &'a str,
    base_port: u16,
    log_dir: &'a str,
    run: &'a str,
}

/// Launch one variant-quic subprocess running the `mixed-types`
/// workload at qos=1, which guarantees oversize datagrams are
/// generated during the operate phase.
fn spawn_variant_quic_mixed_qos1(a: &SpawnArgs<'_>) -> Child {
    Command::new(a.binary)
        .args([
            "--tick-rate-hz",
            "100",
            "--stabilize-secs",
            "1",
            "--operate-secs",
            "3",
            "--silent-secs",
            "1",
            "--workload",
            "mixed-types",
            "--values-per-tick",
            "1000",
            "--qos",
            "1",
            "--log-dir",
            a.log_dir,
            "--launch-ts",
            "2026-05-22T14:00:00.000000000Z",
            "--variant",
            "quic-1000x100hz-mixed-qos1",
            "--runner",
            a.runner,
            "--run",
            a.run,
            "--threading-mode",
            "multi",
            "--mixed-scalars-min",
            "5",
            "--mixed-scalars-max",
            "20",
            "--mixed-arrays-min",
            "200",
            "--mixed-arrays-max",
            "600",
            "--mixed-dict-split-max",
            "4",
            "--workload-seed",
            "12345",
            "--peers",
            a.peers,
            "--",
            "--base-port",
            &a.base_port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn variant-quic subprocess")
}

/// Find the compact-parquet file for the given runner under `log_dir`.
fn find_compact_parquet(log_dir: &std::path::Path, runner: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(log_dir).ok()?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains(&format!("-{runner}-")) && name.ends_with(".compact.parquet") {
            return Some(e.path());
        }
    }
    None
}

/// Find the jsonl lifecycle log for the given runner under `log_dir`.
fn find_jsonl(log_dir: &std::path::Path, runner: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(log_dir).ok()?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains(&format!("-{runner}-")) && name.ends_with(".jsonl") {
            return Some(e.path());
        }
    }
    None
}

#[test]
#[ignore = "spawns subprocesses + binds UDP ports; opt in with --ignored"]
#[allow(clippy::zombie_processes)]
fn two_runner_qos1_oversize_skip_no_crash() {
    let binary = env!("CARGO_BIN_EXE_variant-quic");
    let tmp = tempfile::tempdir().expect("temp dir");
    let log_dir = tmp.path().to_path_buf();
    let log_dir_str = log_dir.to_str().unwrap().to_string();

    // Pick a base_port such that (base + 0) and (base + 1) -- the qos=1
    // derived ports for index 0 (alice) and index 1 (bob) -- are both
    // free on loopback. qos=1 -> qos_offset = 0.
    let probe = pick_free_udp_port();
    let base_port: u16 = probe;
    let alice_port = base_port;
    let bob_port = base_port + 1;
    // Probe-bind to confirm both are free; drop the probes immediately.
    {
        let _a = UdpSocket::bind(format!("127.0.0.1:{alice_port}")).expect("alice port free");
        let _b = UdpSocket::bind(format!("127.0.0.1:{bob_port}")).expect("bob port free");
    }

    let peers = "alice=127.0.0.1,bob=127.0.0.1";
    let run = "qos1_oversize_skip";

    let mut bob = spawn_variant_quic_mixed_qos1(&SpawnArgs {
        binary,
        runner: "bob",
        peers,
        base_port,
        log_dir: &log_dir_str,
        run,
    });
    // Brief head-start so bob's accept loop is armed before alice dials.
    thread::sleep(Duration::from_millis(300));
    let mut alice = spawn_variant_quic_mixed_qos1(&SpawnArgs {
        binary,
        runner: "alice",
        peers,
        base_port,
        log_dir: &log_dir_str,
        run,
    });

    let alice_stderr = alice.stderr.take().expect("alice stderr");
    let bob_stderr = bob.stderr.take().expect("bob stderr");
    let alice_log_thread = thread::spawn(move || {
        let mut out = String::new();
        let reader = BufReader::new(alice_stderr);
        for line in reader.lines().map_while(Result::ok) {
            out.push_str(&line);
            out.push('\n');
        }
        out
    });
    let bob_log_thread = thread::spawn(move || {
        let mut out = String::new();
        let reader = BufReader::new(bob_stderr);
        for line in reader.lines().map_while(Result::ok) {
            out.push_str(&line);
            out.push('\n');
        }
        out
    });

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
    if alice_status.is_none() {
        let _ = alice.kill();
        alice_status = alice.wait().ok();
    }
    if bob_status.is_none() {
        let _ = bob.kill();
        bob_status = bob.wait().ok();
    }
    let alice_stderr_text = alice_log_thread.join().unwrap_or_default();
    let bob_stderr_text = bob_log_thread.join().unwrap_or_default();
    let alice_status =
        alice_status.expect("alice exit status missing even after kill+wait fallback");
    let bob_status = bob_status.expect("bob exit status missing even after kill+wait fallback");

    // Primary regression assertion: BOTH peers must exit 0 (pre-fix
    // they exited 1 with "datagram too large").
    assert!(
        alice_status.success() && bob_status.success(),
        "subprocesses did not exit cleanly within deadline. \
         alice={alice_status:?} bob={bob_status:?}\n\
         alice_stderr (tail):\n{}\nbob_stderr (tail):\n{}",
        tail(&alice_stderr_text, 60),
        tail(&bob_stderr_text, 60),
    );

    // At least one peer must have emitted the `[quic] note:` line --
    // the workload generator at 1000 vpt mixed-types absolutely
    // produces oversize payloads on loopback (typical loopback
    // max_datagram_size is ~1200 B; payloads with 200-600 leaves of
    // serialized data exceed that). It must appear AT MOST ONCE per
    // runner (the warning gate dedups per spawn).
    let alice_notes =
        count_lines_containing(&alice_stderr_text, "[quic] note: QoS 1 datagram payload");
    let bob_notes = count_lines_containing(&bob_stderr_text, "[quic] note: QoS 1 datagram payload");
    assert!(
        alice_notes <= 1,
        "alice emitted {alice_notes} oversize notes (expected at most 1 per spawn). \
         alice_stderr:\n{alice_stderr_text}"
    );
    assert!(
        bob_notes <= 1,
        "bob emitted {bob_notes} oversize notes (expected at most 1 per spawn). \
         bob_stderr:\n{bob_stderr_text}"
    );
    assert!(
        alice_notes + bob_notes >= 1,
        "expected at least one [quic] note: line on stderr from the oversize path, got none. \
         alice_stderr (tail):\n{}\nbob_stderr (tail):\n{}",
        tail(&alice_stderr_text, 60),
        tail(&bob_stderr_text, 60),
    );

    // Both spawns must have written their compact parquet. Pre-fix the
    // crash before digest meant no parquet on disk.
    let alice_parquet =
        find_compact_parquet(&log_dir, "alice").expect("alice compact.parquet missing");
    let bob_parquet = find_compact_parquet(&log_dir, "bob").expect("bob compact.parquet missing");
    assert!(
        std::fs::metadata(&alice_parquet)
            .map(|m| m.len() > 0)
            .unwrap_or(false),
        "alice compact parquet is empty: {}",
        alice_parquet.display()
    );
    assert!(
        std::fs::metadata(&bob_parquet)
            .map(|m| m.len() > 0)
            .unwrap_or(false),
        "bob compact parquet is empty: {}",
        bob_parquet.display()
    );

    // And both spawns must have reached the `silent` phase per their
    // jsonl lifecycle log. The lifecycle JSONL is the canonical
    // post-T18 "phases ran" signal (phase events stay in JSONL,
    // per-event observations move to compact-parquet). Hitting
    // `silent` confirms operate completed without the pre-fix
    // mid-phase crash on "datagram too large".
    let alice_jsonl = find_jsonl(&log_dir, "alice").expect("alice jsonl missing");
    let bob_jsonl = find_jsonl(&log_dir, "bob").expect("bob jsonl missing");
    assert!(
        jsonl_contains_phase(&alice_jsonl, "silent"),
        "alice jsonl never logged phase=silent: {}",
        alice_jsonl.display()
    );
    assert!(
        jsonl_contains_phase(&bob_jsonl, "silent"),
        "bob jsonl never logged phase=silent: {}",
        bob_jsonl.display()
    );
}

fn count_lines_containing(s: &str, needle: &str) -> usize {
    s.lines().filter(|line| line.contains(needle)).count()
}

fn jsonl_contains_phase(path: &std::path::Path, phase: &str) -> bool {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let reader = BufReader::new(f);
    let needle = format!("\"phase\":\"{phase}\"");
    for line in reader.lines().map_while(Result::ok) {
        if line.contains(&needle) {
            return true;
        }
    }
    false
}

fn tail(s: &str, n_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n_lines);
    lines[start..].join("\n")
}
