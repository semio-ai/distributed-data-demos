//! T14.9a end-to-end smoke test for the `zenohd` sidecar lifecycle.
//!
//! This test exercises the real binary path: locate `zenohd`, spawn it,
//! wait for the REST plugin to respond, kill it, and verify the port
//! is freed. It is marked `#[ignore]` because:
//!
//! 1. It requires `zenohd` to be installed on the host (`cargo install
//!    zenohd --version 1.9.0`). The variant binary's CI tier does not
//!    install zenohd by default.
//! 2. It binds a real TCP port on `127.0.0.1`. Concurrent runs at the
//!    same port number would conflict; we use a high-numbered port
//!    (20199) outside the standard sidecar range to avoid clashing
//!    with operator-driven manual smoke runs.
//!
//! Run with: `cargo test --release -p variant-zenoh -- --ignored
//! sidecar_smoke`.
//!
//! When `zenohd` is NOT installed the test exits successfully with a
//! diagnostic message instead of failing -- so it works on dev hosts
//! and CI alike without gating builds on a binary install.
//!
//! Implementation note: we exercise the sidecar via the variant
//! binary's `--threading-mode single` entry point, not via direct
//! linkage to the internal `sidecar` module. That keeps the test
//! aligned with what operators actually run and proves the wiring
//! end-to-end (CLI -> connect(Single) -> sidecar spawn -> sidecar
//! kill -> port free).

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Resolve zenohd through the same fallthrough the variant uses
/// (ZENOHD_PATH then PATH). Returns None when neither finds it, so
/// the test can skip cleanly on hosts without zenohd.
fn find_zenohd() -> Option<std::path::PathBuf> {
    if let Some(raw) = std::env::var_os("ZENOHD_PATH") {
        let candidate = std::path::PathBuf::from(raw);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path_env = std::env::var_os("PATH")?;
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
        let bare = dir.join("zenohd");
        if bare.is_file() {
            return Some(bare);
        }
        if cfg!(windows) {
            for ext in &exts {
                let with_ext = dir.join(format!("zenohd{ext}"));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

/// Probe the REST plugin's admin space. Returns true if any HTTP
/// response is received within `timeout`.
fn rest_responds(port: u16, timeout: Duration) -> bool {
    use std::io::{Read, Write};
    let addr = match format!("127.0.0.1:{port}").parse::<std::net::SocketAddr>() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(mut stream) =
            std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200))
        {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            let req = format!(
                "GET /@/router/local HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(req.as_bytes()).is_ok() {
                let mut buf = [0u8; 16];
                if let Ok(n) = stream.read(&mut buf) {
                    if n >= 5 && &buf[..5] == b"HTTP/" {
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Returns true if the TCP port is no longer accepting connections
/// (a small grace window for the OS to fully release it after the
/// listening process exits).
fn port_freed(port: u16, timeout: Duration) -> bool {
    let addr = format!("127.0.0.1:{port}")
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
            Ok(_) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return true,
        }
    }
    false
}

#[test]
#[ignore]
fn sidecar_lifecycle_smoke() {
    // Skip-gracefully gate. Print a clear message so the operator
    // running `cargo test -- --ignored` knows why it short-circuited.
    if find_zenohd().is_none() {
        println!(
            "[T14.9a smoke] zenohd not installed (not on PATH and ZENOHD_PATH unset). \
             Install via `cargo install zenohd --version 1.9.0` to exercise this test. \
             Skipping cleanly."
        );
        return;
    }

    // Use a high port outside the canonical sidecar range (default
    // 20100 + index). 20199 is unlikely to collide.
    let base_port = 20199u16;

    let binary = env!("CARGO_BIN_EXE_variant-zenoh");

    // Launch the variant in Single mode against a tempdir log-dir.
    // We don't care about the log contents -- the variant will fail
    // when publish() short-circuits with "T14.9b not implemented",
    // but the sidecar should still have spawned. We avoid that by
    // using `operate_secs = 0` -- the driver still goes through
    // connect/disconnect, which is what this test exercises.
    let log_dir = tempfile::tempdir().unwrap();
    let log_dir_path = log_dir.path().to_str().unwrap().replace('\\', "/");

    let mut child = Command::new(binary)
        .args([
            "--tick-rate-hz",
            "10",
            "--stabilize-secs",
            "0",
            "--operate-secs",
            "1",
            "--silent-secs",
            "0",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "1",
            "--qos",
            "1",
            "--log-dir",
            &log_dir_path,
            "--launch-ts",
            "2026-05-14T00:00:00.000000000Z",
            "--variant",
            "zenoh",
            "--runner",
            "smoke-runner",
            "--run",
            "smoke-run",
            "--threading-mode",
            "single",
            "--",
            "--zenoh-sidecar-base-port",
            &base_port.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn variant-zenoh");

    // Give the variant a moment to spawn its sidecar.
    std::thread::sleep(Duration::from_millis(800));

    // The REST plugin should be live for as long as the variant is
    // running. The variant itself will likely exit non-zero because
    // publish() refuses (T14.9b not implemented), but the timing
    // window before that exit is when we probe.
    //
    // To bound the probe, give the variant up to 4 s to be in the
    // "sidecar-up, before-publish-fails" window. In practice the
    // probe succeeds within ~200 ms.
    let saw_rest = rest_responds(base_port, Duration::from_secs(4));

    // Either way, let the variant finish so we can verify cleanup.
    let _ = child.wait();

    assert!(
        saw_rest,
        "T14.9a smoke: REST plugin on 127.0.0.1:{base_port} never responded; \
         did the sidecar spawn? Check variant-zenoh stderr for diagnostics."
    );

    // The variant has exited -- on Windows the Job Object closing
    // should kill the sidecar; on Linux pre-exec PR_SET_PDEATHSIG
    // should signal it; on either path the explicit kill in
    // Sidecar::stop runs first. Give the OS up to 2 s to release
    // the port.
    assert!(
        port_freed(base_port, Duration::from_secs(2)),
        "T14.9a smoke: port {base_port} still bound after variant exited -- \
         per-platform cleanup may have leaked a sidecar."
    );
}
