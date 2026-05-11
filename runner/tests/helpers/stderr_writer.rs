//! Test helper binary that writes a known string to stderr.
//!
//! Used by `spawn.rs` unit tests to verify per-spawn stderr capture works
//! across clean exit, panic exit, and timeout-kill scenarios.
//!
//! Behavior is selected via the `STDERR_WRITER_MODE` environment variable:
//!
//! - `plain`         -- write "HELLO STDERR" to stderr and exit 0.
//! - `panic`         -- write "BEFORE PANIC" to stderr (flushed), then panic
//!   with "PANIC HERE". Process exits with a non-zero code.
//! - `sleep`         -- write "BEFORE SLEEP" to stderr (flushed), then sleep
//!   forever so the parent must kill the process via its timeout path.
//! - `lines_then_sleep` -- write a few labelled stderr lines (so the runner
//!   has something to tail) and then sleep forever. Used by the T-impl.9
//!   timeout-with-stderr integration test.
//! - `lines_then_fail` -- write a few labelled stderr lines and exit with
//!   code 1. Used by the T-impl.9 failed-with-stderr integration test.
//! - `silent_fail`   -- write nothing to stderr and exit with code 1. Used by
//!   the T-impl.9 failed-with-empty-stderr integration test.
//!
//! Any other (or missing) value falls through with no output and exit 0.

use std::io::Write;

fn main() {
    let mode = std::env::var("STDERR_WRITER_MODE").unwrap_or_default();
    match mode.as_str() {
        "plain" => {
            eprintln!("HELLO STDERR");
        }
        "panic" => {
            eprintln!("BEFORE PANIC");
            // Force a flush before the panic so the line is on disk even if
            // the runtime buffers stderr differently during the unwind path.
            let _ = std::io::stderr().flush();
            panic!("PANIC HERE");
        }
        "sleep" => {
            eprintln!("BEFORE SLEEP");
            let _ = std::io::stderr().flush();
            std::thread::sleep(std::time::Duration::from_secs(999));
        }
        "lines_then_sleep" => {
            eprintln!("STDERR-LINE-1");
            eprintln!("STDERR-LINE-2");
            eprintln!("STDERR-LINE-3");
            let _ = std::io::stderr().flush();
            std::thread::sleep(std::time::Duration::from_secs(999));
        }
        "lines_then_fail" => {
            eprintln!("FAIL-LINE-1");
            eprintln!("FAIL-LINE-2");
            let _ = std::io::stderr().flush();
            std::process::exit(1);
        }
        "silent_fail" => {
            // No stderr output at all -- exercises the "(stderr capture is
            // empty -- child likely killed before writing any output)"
            // branch in the runner's failure-handling block.
            std::process::exit(1);
        }
        _ => {}
    }
}
