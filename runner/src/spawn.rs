use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
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
pub fn spawn_and_monitor(binary: &str, args: &[String], timeout: Duration) -> Result<ChildOutcome> {
    // Validate binary path exists.
    if !Path::new(binary).exists() {
        bail!("binary not found: {}", binary);
    }

    let mut child = Command::new(binary)
        .args(args)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonexistent_binary_returns_error() {
        let result = spawn_and_monitor("./no-such-binary-xyz", &[], Duration::from_secs(5));
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

        let outcome = spawn_and_monitor(binary, &args, Duration::from_secs(2)).unwrap();
        assert_eq!(outcome, ChildOutcome::Timeout);
    }
}
