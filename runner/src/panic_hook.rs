//! Process-wide panic hook that converts any thread panic into a clearly
//! labelled stderr line plus an immediate `process::abort`.
//!
//! ## Motivation
//!
//! On 2026-05-21 an operator reported that two runners launched against
//! `configs/two-runner-all-variants.toml` on the same machine in two
//! terminals (alice + bob) exited silently between the `spawning '...'`
//! and `final progress:` lines for the SECOND spawn of the matrix
//! (`custom-udp-1000x100hz-scalar-qos1-single`). Both terminals returned
//! to the shell prompt with NO panic message, NO `FATAL:` line, NO
//! anyhow `Error:` output, and NO `finished:` line — but the orphaned
//! variant children completed their full lifecycle on their own (Windows
//! does not auto-reap orphaned children) and wrote full JSONL + parquet
//! files. That means the runner process really did vanish; it did not
//! merely hang.
//!
//! The original report covered the runner-side process::exit sites and
//! ruled them out: only `main.rs` has them (one for `BarrierTimeoutError`
//! that prints a FATAL line first, one at end-of-run after the summary
//! table). Neither matches a silent mid-spawn disappearance. The remaining
//! hypotheses all involve some form of "thread panic that the main thread
//! never observes" or "library-level OS exit that bypasses our stderr":
//!
//! - A background thread (progress_coord reader, barrier_coord reader,
//!   stdout-progress reader, clock-sync engine) panicked on a mutex
//!   poison check, an `.expect()` on a network condition, or an
//!   arithmetic overflow. Default Rust behaviour: the thread unwinds
//!   silently to its own panic handler, prints to stderr, the OTHER
//!   threads keep running. But if the main thread then tries to read
//!   data the panicked thread was responsible for producing, or holds
//!   a lock the panicked thread poisoned, the main thread's subsequent
//!   `.expect("mutex poisoned")` becomes ITS OWN panic — and on
//!   release builds with no backtrace and a terminal that scrolled past
//!   the first panic message, the operator may see "shell prompt"
//!   without realising both panics happened.
//! - A library crate inside `local-ip-address`, `socket2`, `clap`, or
//!   `anyhow` could (theoretically) call `process::exit` from a
//!   signal-handling thread.
//! - On Windows specifically, a console scheduler closing stdin in some
//!   buffering modes can deliver `CTRL_CLOSE_EVENT`; the runner does
//!   not install a console control handler, so the default OS handler
//!   exits the process silently.
//!
//! Regardless of which root cause produced the original silent exit,
//! the *correct* defensive posture is identical: **never let a panic
//! disappear**. The hook in this module:
//!
//! 1. Captures the panic payload (`&'static str` or `String`),
//!    the thread name (or `<unnamed>`), and the panic location
//!    (`file:line:col`).
//! 2. Prints a `[runner:<name>] PANIC in thread '<thread>': <payload>`
//!    line to stderr, followed by `[runner:<name>] panic location: ...`.
//! 3. Calls the previously installed panic hook (the default one) so
//!    `RUST_BACKTRACE=1` still produces a backtrace dump as usual.
//! 4. Calls `std::process::abort()` to terminate the WHOLE process
//!    immediately. This is what makes background-thread panics
//!    impossible to miss: instead of "thread dies, main keeps going,
//!    state corrupts, eventually the main thread silently exits", the
//!    first panic kills the process. The operator sees the stderr
//!    line and knows exactly which runner died and where.
//!
//! ## Why abort() and not exit()
//!
//! `process::abort` is preferred over `process::exit(N)` here because
//! abort does not run TLS destructors and does not allow other threads
//! to swallow the signal — it is the canonical "kill the process NOW
//! regardless of state" primitive. Exit codes from abort vary by
//! platform (SIGABRT exit on Unix, `STATUS_FAIL_FAST_EXCEPTION` /
//! 3 on Windows depending on the runtime). The wrapper scripts in
//! `scripts/runner-resume.{sh,ps1}` only retry on exit code 75
//! (`EX_TEMPFAIL`); every abort code is therefore propagated to the
//! operator without an automatic retry, which is the right behaviour
//! for a real panic — `--resume` cannot fix a logic bug.
//!
//! ## Why a panic hook and not `panic = "abort"`
//!
//! Setting `panic = "abort"` in `Cargo.toml` produces a much smaller
//! release binary but loses any per-panic diagnostic line: the runtime
//! simply terminates the process with no stderr output beyond the
//! default abort. A custom hook gives us both the visible `PANIC ...`
//! line AND the abort, at the cost of carrying unwinding tables in
//! the binary (which the runner already does today). Future binary-size
//! pressure might motivate switching to `panic = "abort"` workspace-
//! wide; until then the explicit hook is the more diagnosable choice.

use std::sync::atomic::{AtomicBool, Ordering};

/// Guard against multiple `install_panic_hook` calls in the same
/// process. The hook is global state; installing it twice would chain
/// the abort-on-panic twice (harmless but messy in stderr). Tests that
/// repeatedly construct a fresh process do not hit this; the guard
/// only matters if production code accidentally calls `main`'s
/// installer twice (e.g. a future refactor that wires the runner into
/// a library re-entry point).
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install the process-wide panic hook that converts any thread panic
/// into a clearly labelled stderr line plus an immediate `process::abort`.
///
/// Idempotent under repeated calls: subsequent calls after the first
/// are silent no-ops (the original hook installed by the first call
/// remains in place; the `runner_name` from the first call is the one
/// that prefixes future panics). In production `main()` only calls this
/// once.
///
/// The `runner_name` is captured by value so the closure owns its own
/// copy; subsequent changes to the caller's `String` cannot affect the
/// hook's prefix.
///
/// **Behaviour on panic**:
/// 1. Prints `[runner:<name>] PANIC in thread '<thread_name>': <payload>` to stderr.
/// 2. Prints the panic location (`at file:line:col`) if available.
/// 3. Calls the previously-installed hook (so the standard backtrace
///    formatting still appears when `RUST_BACKTRACE=1`).
/// 4. Calls `std::process::abort()` to kill the WHOLE process — never
///    just the panicking thread.
pub fn install_panic_hook(runner_name: String) {
    // CAS: only the first caller actually installs.
    if HOOK_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread_name = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let payload: String = if let Some(s) = info.payload().downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        eprintln!("[runner:{runner_name}] PANIC in thread '{thread_name}': {payload}");
        if let Some(loc) = info.location() {
            eprintln!(
                "[runner:{runner_name}] panic location: {}:{}:{}",
                loc.file(),
                loc.line(),
                loc.column()
            );
        }
        // Run the default hook so RUST_BACKTRACE=1 still works.
        prev_hook(info);
        // Hard exit. Never silent; never leaves the main thread alive
        // after a background-thread panic. `abort()` is documented as
        // diverging.
        std::process::abort();
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// We cannot exercise the abort path from inside a `cargo test`
    /// run without killing the test process. The end-to-end behaviour
    /// (panic -> stderr line -> abort) is covered by the
    /// `panic_helper_emits_labeled_stderr_and_aborts` integration test
    /// which spawns a dedicated helper binary.
    ///
    /// This unit test only verifies that calling `install_panic_hook`
    /// is idempotent (second call must NOT chain another abort). The
    /// hook itself is global process state and would interfere with
    /// other unit tests, so we keep the hook OUT of this thread by
    /// only validating the idempotency guard.
    #[test]
    fn install_panic_hook_idempotent_guard_flips_once() {
        // The static is global, so this test relies on running before
        // any production code path inside the unit-test binary calls
        // install. Cargo's unit-test runner does not call main; the
        // only way the guard could already be set is if a sibling unit
        // test had called install_panic_hook. None does (the function
        // is only called from main()). So we can safely flip + flip:
        // the first call wins; the second is a no-op.
        //
        // We do NOT install a real abort-hook here -- doing so would
        // kill the test runner on the next panic. Instead we just
        // assert the guard's behaviour by stubbing the install via a
        // direct CAS.
        let first = HOOK_INSTALLED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        let second = HOOK_INSTALLED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        // Reset for any future test (defensive, even though tests
        // run sequentially in this module).
        HOOK_INSTALLED.store(false, Ordering::SeqCst);
        assert!(first, "first CAS should succeed");
        assert!(!second, "second CAS should be a no-op");
    }
}
