use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Maximum number of bytes read from the end of a stderr capture file when
/// building a failure-diagnostic tail. Caps the tail at 64 KiB so a runaway
/// child that filled its stderr with megabytes of output does not OOM the
/// runner during the post-mortem print.
pub const STDERR_TAIL_MAX_BYTES: u64 = 64 * 1024;

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

/// Build the path to the variant's JSONL log file.
///
/// Per `metak-shared/api-contracts/jsonl-log-schema.md` the variant writes
/// its structured log to `<log_subdir>/<effective_name>-<runner_name>-<run>.jsonl`.
/// This helper computes that path from the runner's point of view so the
/// failure-handling block in `main.rs` can print a pointer for the operator.
/// The returned path may or may not exist on disk -- the caller is expected
/// to check before using it.
pub fn jsonl_log_path(
    log_subdir: &Path,
    effective_name: &str,
    runner_name: &str,
    run: &str,
) -> PathBuf {
    log_subdir.join(format!("{effective_name}-{runner_name}-{run}.jsonl"))
}

/// Read the last `n` lines of a stderr capture file for post-mortem display.
///
/// Return semantics:
///
/// - `Ok(None)` when the file does not exist on disk. The runner uses this
///   to silently skip the tail block (no notice, no separators) -- the
///   capture file should always exist by the time we read it, so `None` is
///   a defensive return for the path being wrong rather than an
///   operational signal.
/// - `Ok(Some(s))` where `s` is the empty string `""` when the file exists
///   but has zero bytes. The runner uses this branch to print the
///   "stderr capture is empty" notice that calls out a child killed
///   before flushing anything to disk -- the common timeout-on-Windows
///   pattern that motivated this whole task.
/// - `Ok(Some(s))` with the actual tail content otherwise: the last `n`
///   lines, sourced from at most the last [`STDERR_TAIL_MAX_BYTES`] bytes
///   of the file so a pathologically large capture cannot OOM the runner.
///   The trailing newline (if any) is preserved.
///
/// Non-UTF-8 bytes are sanitised via `String::from_utf8_lossy` so a child
/// that wrote raw binary content cannot crash the runner during the
/// diagnostic print.
pub fn tail_stderr_file(path: &Path, n: usize) -> Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("opening stderr capture {}", path.display()))
        }
    };

    let len = file
        .metadata()
        .with_context(|| format!("stat stderr capture {}", path.display()))?
        .len();

    if len == 0 {
        return Ok(Some(String::new()));
    }

    // Seek to at most STDERR_TAIL_MAX_BYTES from the end. For smaller files
    // we read the whole thing. Reading a fixed window from EOF (rather than
    // streaming line-by-line) keeps the helper allocation-bounded by
    // STDERR_TAIL_MAX_BYTES regardless of input size.
    let read_len = len.min(STDERR_TAIL_MAX_BYTES);
    let start = len - read_len;
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("seeking stderr capture {}", path.display()))?;

    let mut buf = Vec::with_capacity(read_len as usize);
    file.read_to_end(&mut buf)
        .with_context(|| format!("reading stderr capture {}", path.display()))?;

    // Drop any partial line at the start of the window. Only do this when
    // we actually truncated -- otherwise the file's first line would be
    // dropped erroneously.
    if start > 0 {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        } else {
            // The whole window is one giant unterminated line. Keep it as-is
            // so the operator still sees something.
        }
    }

    let s = String::from_utf8_lossy(&buf).into_owned();

    // Now keep only the last `n` lines. We split on '\n' but reassemble
    // with '\n' so any trailing newline on the original file is preserved.
    // A leading empty entry from a leading newline is preserved too; this
    // matches operator expectation that the tail prints "what the file
    // ends with", including blank lines.
    if n == 0 {
        return Ok(Some(String::new()));
    }

    // Split the content keeping line breaks. `str::split_inclusive` yields
    // pieces that each end with '\n' (except possibly the last).
    let lines: Vec<&str> = s.split_inclusive('\n').collect();
    if lines.len() <= n {
        Ok(Some(s))
    } else {
        let tail: String = lines[lines.len() - n..].concat();
        Ok(Some(tail))
    }
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

    #[test]
    fn jsonl_log_path_follows_schema() {
        let p = jsonl_log_path(Path::new("/tmp/logs/run01"), "v-qos2", "alice", "run42");
        assert_eq!(
            p,
            std::path::PathBuf::from("/tmp/logs/run01/v-qos2-alice-run42.jsonl")
        );
    }

    /// File missing -> `Ok(None)` so the caller can silently skip the tail
    /// block. The path is allowed to be wrong; this is a courtesy pointer,
    /// not a guarantee.
    #[test]
    fn tail_stderr_file_missing_returns_none() {
        let tmp = tempdir_unique("tail-missing");
        let missing = tmp.join("no-such-stderr.txt");
        let got = tail_stderr_file(&missing, 20).expect("ok");
        assert_eq!(got, None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Empty file -> `Ok(Some(""))` so the caller can branch to the
    /// "stderr capture is empty -- child likely killed before writing
    /// any output" notice that motivates this whole task.
    #[test]
    fn tail_stderr_file_empty_returns_some_empty() {
        let tmp = tempdir_unique("tail-empty");
        let path = tmp.join("stderr.txt");
        File::create(&path).expect("create");
        let got = tail_stderr_file(&path, 20).expect("ok");
        assert_eq!(got.as_deref(), Some(""));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// File with fewer than N lines -> the whole content is returned
    /// verbatim, including its trailing newline.
    #[test]
    fn tail_stderr_file_fewer_lines_returns_all() {
        let tmp = tempdir_unique("tail-fewer");
        let path = tmp.join("stderr.txt");
        std::fs::write(&path, "line a\nline b\nline c\n").expect("write");
        let got = tail_stderr_file(&path, 20).expect("ok");
        assert_eq!(got.as_deref(), Some("line a\nline b\nline c\n"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// File with more than N lines -> exactly the last N lines.
    #[test]
    fn tail_stderr_file_more_lines_returns_last_n() {
        let tmp = tempdir_unique("tail-more");
        let path = tmp.join("stderr.txt");
        let mut content = String::new();
        for i in 0..50 {
            content.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&path, &content).expect("write");
        let got = tail_stderr_file(&path, 5).expect("ok").unwrap();
        // Last 5 lines: 45..49.
        let mut expected = String::new();
        for i in 45..50 {
            expected.push_str(&format!("line {i}\n"));
        }
        assert_eq!(got, expected);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// File with no trailing newline -> the last partial line is still
    /// returned. Children that crash without flushing a newline shouldn't
    /// be silently dropped from the diagnostic.
    #[test]
    fn tail_stderr_file_no_trailing_newline() {
        let tmp = tempdir_unique("tail-no-nl");
        let path = tmp.join("stderr.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma").expect("write");
        let got = tail_stderr_file(&path, 2).expect("ok").unwrap();
        assert_eq!(got, "beta\ngamma");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Larger than the 64 KiB byte cap -> returns the last <= 64 KiB worth
    /// of lines without panicking. The exact tail length is bounded by the
    /// byte cap, not the line count, so we just assert the returned chunk
    /// is within bounds and ends with the very last line of the file.
    #[test]
    fn tail_stderr_file_huge_file_is_byte_bounded() {
        let tmp = tempdir_unique("tail-huge");
        let path = tmp.join("stderr.txt");
        // Build > 64 KiB of short lines so the byte cap, not the line cap,
        // is the limiting factor.
        let mut content = String::new();
        let mut last_line = String::new();
        for i in 0..20_000 {
            let line = format!("line-{i:06}\n");
            content.push_str(&line);
            last_line = line;
        }
        assert!(content.len() as u64 > STDERR_TAIL_MAX_BYTES);
        std::fs::write(&path, &content).expect("write");

        let got = tail_stderr_file(&path, 100_000).expect("ok").unwrap();
        // Must end with the very last line of the file (no truncation at EOF).
        assert!(
            got.ends_with(&last_line),
            "tail should end with the last line, got tail ending: {:?}",
            &got[got.len().saturating_sub(40)..]
        );
        // Must be within the byte cap.
        assert!(
            got.len() as u64 <= STDERR_TAIL_MAX_BYTES,
            "tail of {} bytes exceeds STDERR_TAIL_MAX_BYTES={}",
            got.len(),
            STDERR_TAIL_MAX_BYTES
        );
        // The first character of the returned tail must be the start of a
        // line (we drop the partial line at the head of the window).
        let first_byte = got.as_bytes().first().copied();
        assert!(
            first_byte.is_some() && first_byte != Some(b'\n'),
            "tail should start at a line boundary, got first byte {first_byte:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Non-UTF-8 bytes must not panic the helper -- the operator gets the
    /// `from_utf8_lossy` replacement output instead of a runner crash.
    #[test]
    fn tail_stderr_file_handles_non_utf8() {
        let tmp = tempdir_unique("tail-binary");
        let path = tmp.join("stderr.txt");
        std::fs::write(&path, b"good\n\xff\xfe bad\n").expect("write");
        let got = tail_stderr_file(&path, 5).expect("ok").unwrap();
        // The lossy replacement char (U+FFFD) is what we expect for the
        // two invalid bytes. Either one or two of them is fine -- we
        // only assert no panic and a non-empty output.
        assert!(got.contains("good"));
        assert!(got.contains("bad"));
    }
}
