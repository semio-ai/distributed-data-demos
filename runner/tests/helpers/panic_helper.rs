//! Test helper binary that installs the runner's panic hook and then
//! triggers a controlled panic so the integration test can assert the
//! hook's stderr line shape AND the process aborts (non-zero exit).
//!
//! The hook source is shared with the runner crate via `#[path = ...]`
//! so this helper exercises the EXACT same code the production
//! `runner.exe` runs. Duplicating the logic here would defeat the
//! purpose of the regression test.
//!
//! Modes (selected via the first CLI argument):
//!
//! - `main` (default): panic on the main thread.
//! - `thread`: spawn a worker thread whose name is `worker`, panic
//!   inside it, return from main without joining. The panic hook is
//!   process-wide so the hook runs from the worker thread, prints the
//!   PANIC line with `'worker'`, and aborts the WHOLE process — which
//!   is the behaviour we want to pin for background-thread panics
//!   inside the runner's coordinator threads.

#[path = "../../src/panic_hook.rs"]
mod panic_hook;

fn main() {
    let runner_name = std::env::var("PANIC_HELPER_RUNNER_NAME").unwrap_or_else(|_| "test".into());
    panic_hook::install_panic_hook(runner_name);
    let mode = std::env::args().nth(1).unwrap_or_else(|| "main".into());
    match mode.as_str() {
        "main" => {
            panic!("intentional main-thread panic");
        }
        "thread" => {
            let handle = std::thread::Builder::new()
                .name("worker".into())
                .spawn(|| {
                    panic!("intentional worker-thread panic");
                })
                .expect("spawn worker thread");
            // The hook aborts the process from the worker thread, so
            // the join never returns. We sleep briefly to be sure the
            // worker has time to enter the hook.
            std::thread::sleep(std::time::Duration::from_secs(5));
            // If we ever reach here the hook failed to abort.
            let _ = handle.join();
            eprintln!("ERROR: process did not abort after worker panic");
            std::process::exit(99);
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(2);
        }
    }
}
