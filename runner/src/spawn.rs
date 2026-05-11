use anyhow::{bail, Context, Result};
use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Outcome of a child process execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutcome {
    /// Process exited with code 0.
    Success,
    /// Process exited with a non-zero code.
    Failed(i32),
    /// Process was killed after exceeding the timeout.
    Timeout,
}

impl ChildOutcome {
    /// Return a status string for reporting.
    pub fn status_str(&self) -> &'static str {
        match self {
            ChildOutcome::Success => "success",
            ChildOutcome::Failed(_) => "failed",
            ChildOutcome::Timeout => "timeout",
        }
    }

    /// Return the exit code (0 for success, -1 for timeout).
    pub fn exit_code(&self) -> i32 {
        match self {
            ChildOutcome::Success => 0,
            ChildOutcome::Failed(code) => *code,
            ChildOutcome::Timeout => -1,
        }
    }
}

/// Spawn a variant binary and monitor it until exit or timeout.
///
/// The caller is responsible for recording `launch_ts` immediately before
/// calling this function and passing it as part of the args.
/// If the child does not exit within `timeout`, it is killed.
///
/// If `stderr_path` is `Some`, the child's stderr is redirected to a file at
/// that path. The file is created (truncating any prior content) **before**
/// the child is spawned, so the file exists even if the child is killed
/// mid-write for a timeout. The OS writes child stderr directly to the file
/// (no intermediate thread, no deadlock risk) and flushes on child exit/kill.
///
/// If `stderr_path` is `None`, the child inherits the parent's stderr.
///
/// The runner's own stderr (panics, FATAL lines, etc.) is never redirected
/// by this function — only the child's stderr is routed to the file.
pub fn spawn_and_monitor(
    binary: &str,
    args: &[String],
    timeout: Duration,
    stderr_path: Option<&Path>,
) -> Result<ChildOutcome> {
    // Validate binary path exists.
    if !Path::new(binary).exists() {
        bail!("binary not found: {}", binary);
    }

    let mut cmd = Command::new(binary);
    cmd.args(args);

    // Open the stderr capture file BEFORE spawning the child. This guarantees
    // the file exists on disk even if the child is killed during a timeout —
    // nothing the child does can prevent file creation. Create-or-truncate
    // semantics ensure a `--resume` re-spawn of the same (variant, runner)
    // overwrites the previous attempt cleanly rather than appending to it.
    if let Some(path) = stderr_path {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating stderr log dir {}", parent.display()))?;
            }
        }
        let file = File::create(path)
            .with_context(|| format!("creating stderr log file {}", path.display()))?;
        cmd.stderr(Stdio::from(file));
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn: {binary}"))?;

    let start = Instant::now();
    let poll_interval = Duration::from_millis(100);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let outcome = match status.code() {
                    Some(0) => ChildOutcome::Success,
                    Some(code) => ChildOutcome::Failed(code),
                    None => ChildOutcome::Failed(-1),
                };
                return Ok(outcome);
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    // Timeout -- kill the child process.
                    let _ = child.kill();
                    // Wait for the process to actually terminate.
                    let _ = child.wait();
                    return Ok(ChildOutcome::Timeout);
                }
                std::thread::sleep(poll_interval);
            }
            Err(e) => {
                bail!("error waiting for child process: {e}");
            }
        }
    }
}

/// Build the per-spawn stderr capture file path.
///
/// The capture file lives at `<log_subdir>/<effective_name>-<runner_name>-stderr.txt`
/// where `<log_subdir>` is the absolute path of the variant's log directory.
/// The file is truncated on every spawn (see [`spawn_and_monitor`]) so a
/// `--resume` re-spawn overwrites the previous attempt cleanly.
pub fn stderr_capture_path(
    log_subdir: &Path,
    effective_name: &str,
    runner_name: &str,
) -> std::path::PathBuf {
    log_subdir.join(format!("{effective_name}-{runner_name}-stderr.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve the path to a sibling test-helper binary built by Cargo.
    ///
    /// Each `[[bin]]` declared in `runner/Cargo.toml` (e.g. `sleeper`,
    /// `arg-echo`, `stderr-writer`) is compiled to the same `target/<profile>/`
    /// directory as the unit-test binary. We locate it relative to
    /// `current_exe()` so the lookup works in both `--release` and debug builds.
    fn helper_binary(name: &str) -> std::path::PathBuf {
        let mut path = std::env::current_exe().expect("current_exe");
        // current_exe() points at .../target/<profile>/deps/<test>-<hash>(.exe)
        path.pop(); // drop the test binary name
        if path.file_name().and_then(|s| s.to_str()) == Some("deps") {
            path.pop(); // drop "deps"
        }
        let exe = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        path.push(exe);
        path
    }

    #[test]
    fn nonexistent_binary_returns_error() {
        let result = spawn_and_monitor("./no-such-binary-xyz", &[], Duration::from_secs(5), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("binary not found"));
    }

    #[test]
    fn child_outcome_status_str() {
        assert_eq!(ChildOutcome::Success.status_str(), "success");
        assert_eq!(ChildOutcome::Failed(1).status_str(), "failed");
        assert_eq!(ChildOutcome::Timeout.status_str(), "timeout");
    }

    #[test]
    fn child_outcome_exit_code() {
        assert_eq!(ChildOutcome::Success.exit_code(), 0);
        assert_eq!(ChildOutcome::Failed(42).exit_code(), 42);
        assert_eq!(ChildOutcome::Timeout.exit_code(), -1);
    }

    #[test]
    fn stderr_capture_path_uses_effective_and_runner_names() {
        let p = stderr_capture_path(Path::new("/tmp/logs/run01"), "v-qos2", "alice");
        assert_eq!(
            p,
            std::path::PathBuf::from("/tmp/logs/run01/v-qos2-alice-stderr.txt")
        );
    }

    /// Test timeout handling by spawning a process that sleeps longer than timeout.
    /// Uses `ping -n 999 127.0.0.1` on Windows (long-running) with a 2s timeout.
    #[test]
    fn timeout_kills_child() {
        let binary = if cfg!(windows) {
            "C:\\Windows\\System32\\ping.exe"
        } else {
            "/bin/sleep"
        };

        if !Path::new(binary).exists() {
            eprintln!("skipping timeout test: {binary} not found");
            return;
        }

        let args: Vec<String> = if cfg!(windows) {
            vec!["-n".into(), "999".into(), "127.0.0.1".into()]
        } else {
            vec!["999".into()]
        };

        let outcome = spawn_and_monitor(binary, &args, Duration::from_secs(2), None).unwrap();
        assert_eq!(outcome, ChildOutcome::Timeout);
    }

    /// Spawn the `stderr-writer` helper in `plain` mode and assert the capture
    /// file contains the line it printed to stderr.
    #[test]
    fn captures_child_stderr_on_clean_exit() {
        let binary = helper_binary("stderr-writer");
        if !binary.exists() {
            eprintln!(
                "skipping captures_child_stderr_on_clean_exit: helper not built at {}",
                binary.display()
            );
            return;
        }

        let tmp = tempdir_unique("stderr-clean");
        let path = stderr_capture_path(&tmp, "v", "alice");

        let mut cmd = Command::new(&binary);
        cmd.env("STDERR_WRITER_MODE", "plain");
        cmd.env_remove("RUST_BACKTRACE");
        let outcome = spawn_with_env(&mut cmd, Duration::from_secs(10), Some(&path)).unwrap();
        assert_eq!(outcome, ChildOutcome::Success);

        let content = std::fs::read_to_string(&path).expect("read stderr file");
        assert!(
            content.contains("HELLO STDERR"),
            "expected 'HELLO STDERR' in capture file, got: {content:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Spawn the `stderr-writer` helper in `panic` mode and assert the capture
    /// file contains BOTH the pre-panic line and the panic message.
    #[test]
    fn captures_child_stderr_on_panic() {
        let binary = helper_binary("stderr-writer");
        if !binary.exists() {
            eprintln!(
                "skipping captures_child_stderr_on_panic: helper not built at {}",
                binary.display()
            );
            return;
        }

        let tmp = tempdir_unique("stderr-panic");
        let path = stderr_capture_path(&tmp, "v", "alice");

        let mut cmd = Command::new(&binary);
        cmd.env("STDERR_WRITER_MODE", "panic");
        cmd.env_remove("RUST_BACKTRACE");
        let outcome = spawn_with_env(&mut cmd, Duration::from_secs(10), Some(&path)).unwrap();
        // A Rust panic exits with a non-zero code (typically 101).
        assert!(
            matches!(outcome, ChildOutcome::Failed(_)),
            "expected Failed(_) for panicking child, got {outcome:?}"
        );

        let content = std::fs::read_to_string(&path).expect("read stderr file");
        assert!(
            content.contains("BEFORE PANIC"),
            "expected 'BEFORE PANIC' in capture file, got: {content:?}"
        );
        assert!(
            content.contains("PANIC HERE"),
            "expected 'PANIC HERE' in capture file, got: {content:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Spawn the `stderr-writer` helper in `sleep` mode (prints then sleeps
    /// forever); the runner kills it via the timeout path. Assert the capture
    /// file still contains what was printed before the kill.
    #[test]
    fn captures_child_stderr_when_killed_by_timeout() {
        let binary = helper_binary("stderr-writer");
        if !binary.exists() {
            eprintln!(
                "skipping captures_child_stderr_when_killed_by_timeout: helper not built at {}",
                binary.display()
            );
            return;
        }

        let tmp = tempdir_unique("stderr-timeout");
        let path = stderr_capture_path(&tmp, "v", "alice");

        let mut cmd = Command::new(&binary);
        cmd.env("STDERR_WRITER_MODE", "sleep");
        cmd.env_remove("RUST_BACKTRACE");
        let outcome = spawn_with_env(&mut cmd, Duration::from_secs(2), Some(&path)).unwrap();
        assert_eq!(outcome, ChildOutcome::Timeout);

        let content = std::fs::read_to_string(&path).expect("read stderr file");
        assert!(
            content.contains("BEFORE SLEEP"),
            "expected 'BEFORE SLEEP' in capture file, got: {content:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Build a unique temp directory under the system temp root so concurrent
    /// test runs don't clobber each other's stderr capture files.
    fn tempdir_unique(prefix: &str) -> std::path::PathBuf {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("runner-spawn-{prefix}-{pid}-{ns}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Variant of `spawn_and_monitor` for tests that need to set per-spawn
    /// environment variables on the child. Mirrors the real function's
    /// stderr-capture semantics so tests exercise the same code path.
    fn spawn_with_env(
        cmd: &mut Command,
        timeout: Duration,
        stderr_path: Option<&Path>,
    ) -> Result<ChildOutcome> {
        if let Some(path) = stderr_path {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating stderr log dir {}", parent.display()))?;
                }
            }
            let file = File::create(path)
                .with_context(|| format!("creating stderr log file {}", path.display()))?;
            cmd.stderr(Stdio::from(file));
        }

        let mut child = cmd.spawn().with_context(|| "spawn child")?;
        let start = Instant::now();
        let poll_interval = Duration::from_millis(100);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let outcome = match status.code() {
                        Some(0) => ChildOutcome::Success,
                        Some(code) => ChildOutcome::Failed(code),
                        None => ChildOutcome::Failed(-1),
                    };
                    return Ok(outcome);
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(ChildOutcome::Timeout);
                    }
                    std::thread::sleep(poll_interval);
                }
                Err(e) => bail!("error waiting for child process: {e}"),
            }
        }
    }
}
