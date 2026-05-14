//! Internal-stall watchdog (T15.11).
//!
//! A separate OS thread inside the variant binary that monitors the
//! same `sent` / `received` counters the [`crate::progress_emitter`]
//! maintains. When both counters remain flat for
//! `watchdog_secs` consecutive seconds DURING the `operate` phase, the
//! watchdog flushes the JSONL logger and calls
//! `std::process::exit(2)` -- converting an internal stall (driver
//! thread wedged inside a transport library call) from a runner-side
//! kill + truncated JSONL into a clean self-exit + flushed JSONL.
//!
//! ## Why a separate thread?
//!
//! The T15.5 idle detector ([`crate::driver`]) checks for stalls INSIDE
//! the operate loop. It is therefore only effective when the driver
//! thread is cooperative -- it executes only when the loop body
//! advances. If the driver thread is blocked inside e.g. a Zenoh
//! library call that never returns (an observed failure mode under
//! symmetric qos3/qos4 flood), the inline detector never runs. The
//! watchdog runs on its OWN thread and is therefore robust against
//! that wedged-driver shape.
//!
//! ## Exit-code rationale
//!
//! `std::process::exit(2)` is the documented "internal-stall
//! self-exit" code. The choice:
//!
//! - `0` is reserved for clean success.
//! - `1` is what `anyhow`-style top-level error handling already
//!   uses (`eprintln + exit(1)` in `variant_dummy.rs::main`); the
//!   watchdog must be distinguishable from a regular error exit so
//!   the analysis classifier can tell them apart without parsing
//!   stderr to disambiguate.
//! - `2` is the conventional Unix code for "command-line misuse"
//!   in many tools, but the variant binaries do not currently use
//!   it for anything else. Reusing it for the watchdog keeps the
//!   space of meaningful exit codes small and matches the T15.11
//!   spec recommendation.
//!
//! See `variant-base/CUSTOM.md` ("T15.11 internal-stall watchdog")
//! for the contract documentation and `analysis/ANALYSIS.md`
//! ("Timeout classification") for how the analysis pipeline reads
//! the resulting JSONL + stderr shape.
//!
//! ## JSONL-flush requirement
//!
//! `std::process::exit(2)` does NOT run destructors. The variant's
//! [`crate::logger::Logger`] wraps a `BufWriter<File>` which only
//! flushes via its own Drop impl (i.e. only on normal stack unwind).
//! The watchdog therefore MUST call `logger.flush()` explicitly
//! BEFORE exiting; otherwise the JSONL tail would be truncated
//! exactly like the runner-kill case we are trying to eliminate.
//! The injected `on_fire` callback in [`Watchdog::start_with_actions`]
//! lets the driver bind the flush call site (`logger.flush()`)
//! without coupling this module to the logger directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::progress_emitter::{ProgressEmitter, ProgressState};

/// Default self-exit code chosen by the T15.11 watchdog.
///
/// See module docs for the choice rationale. The analysis classifier
/// (`analysis/timeout_classification.py`) keys on this exact value
/// when distinguishing `variant_self_killed_idle` from generic
/// `Failed(_)` outcomes.
pub const WATCHDOG_EXIT_CODE: i32 = 2;

/// Internal poll cadence. The watchdog wakes once per second to
/// re-sample the shared counters and the phase. This is the cadence
/// referenced in the T15.11 spec and matches the granularity at which
/// `watchdog_secs` is meaningfully resolved.
pub const WATCHDOG_TICK: Duration = Duration::from_secs(1);

/// Phase string the watchdog gates on. Other phases have well-defined
/// wallclock budgets driven by config (`stabilize_secs`,
/// `silent_secs`) so a frozen counter there is expected, not a stall.
pub const WATCHDOG_OPERATE_PHASE: &str = "operate";

/// The stderr line emitted right before the watchdog fires. The
/// analysis classifier substring-searches the spawn's stderr capture
/// for this prefix to confirm the `variant_self_killed_idle` outcome.
///
/// Keep this prefix STABLE. Changing it is an analysis-pipeline-
/// breaking change.
pub const WATCHDOG_STDERR_PREFIX: &str = "[variant] watchdog: no progress";

/// Watchdog handle. Dropping the handle signals the worker thread to
/// stop and joins it. Construction via [`Watchdog::start`] is the
/// normal path; [`Watchdog::start_with_actions`] is the injection
/// point unit tests use to substitute fake exit / flush callbacks.
///
/// A disabled watchdog (constructed with `watchdog_secs == 0`) holds
/// no thread. Its [`Drop`] is therefore a cheap no-op.
pub struct Watchdog {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Watchdog {
    /// Start the production watchdog: on fire, flush the supplied
    /// logger via `on_fire`, write the stall diagnostic to stderr,
    /// and call `std::process::exit(WATCHDOG_EXIT_CODE)`.
    ///
    /// Returns a disabled handle (no spawned thread) when
    /// `watchdog_secs == 0`. The caller MUST hold onto the returned
    /// `Watchdog` for the rest of the spawn -- dropping it joins the
    /// monitor thread.
    pub fn start(
        emitter: &ProgressEmitter,
        watchdog_secs: u32,
        mut on_fire: impl FnMut() + Send + 'static,
    ) -> Self {
        Self::start_with_actions(
            emitter.shared_state(),
            watchdog_secs,
            WATCHDOG_TICK,
            move |secs| {
                on_fire();
                eprintln!(
                    "{WATCHDOG_STDERR_PREFIX} in {secs}s during operate phase -- internal stall; self-exiting"
                );
                std::process::exit(WATCHDOG_EXIT_CODE);
            },
        )
    }

    /// Start the watchdog against an explicit shared state with an
    /// injectable fire action. This is the unit-test entry point --
    /// the test passes a callback that records the fire event rather
    /// than terminating the process. The `tick` parameter lets tests
    /// run the watchdog at a much higher cadence than 1 Hz so they
    /// can complete in seconds.
    ///
    /// Production code should call [`Watchdog::start`] instead. The
    /// `state` parameter exposes the crate-private `ProgressState`,
    /// which is why this entry point is `pub(crate)` rather than
    /// fully public.
    pub(crate) fn start_with_actions(
        state: Arc<ProgressState>,
        watchdog_secs: u32,
        tick: Duration,
        mut fire: impl FnMut(u32) + Send + 'static,
    ) -> Self {
        if watchdog_secs == 0 {
            return Self {
                thread: None,
                stop: Arc::new(AtomicBool::new(false)),
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("variant-watchdog".to_string())
            .spawn(move || {
                run_loop(state, watchdog_secs, tick, stop_thread, &mut fire);
            })
            .expect("spawning watchdog thread should not fail");
        Self {
            thread: Some(handle),
            stop,
        }
    }

    /// Whether this handle owns a live monitor thread.
    /// `watchdog_secs == 0` yields a disabled handle and this returns
    /// false.
    pub fn is_enabled(&self) -> bool {
        self.thread.is_some()
    }

    /// Signal the worker to stop and join it. Idempotent. Invoked
    /// automatically from [`Drop`]; the driver may also call it
    /// explicitly so the thread is joined before normal protocol
    /// teardown returns.
    pub fn stop(&mut self) {
        if let Some(handle) = self.thread.take() {
            self.stop.store(true, Ordering::Relaxed);
            // Best-effort join. The worker checks `stop` every tick
            // (1 second by default) so this returns within at most
            // one tick after the call.
            let _ = handle.join();
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Internal monitor loop. Exposed for unit-test injection only --
/// production callers go through [`Watchdog::start`].
///
/// The loop tracks the last observed `(sent, received)` values and
/// the wallclock instant at which either of them last advanced. Each
/// tick:
///
/// - If `stop` is set, the loop exits.
/// - If the current phase is not `operate`, the "last change" instant
///   is refreshed to `now` and the snapshot is captured. A frozen
///   counter outside operate is expected (stabilize / silent have
///   well-defined wallclock budgets) and must not trigger the
///   watchdog. This also means that if the variant returns to
///   operate after a long stabilize, the watchdog starts measuring
///   from the operate-entry moment, not from process start.
/// - Otherwise (phase == operate): if either counter advanced, the
///   bookkeeping is updated. If neither has changed for at least
///   `watchdog_secs` since the last advance, `fire` is invoked with
///   the configured threshold and the loop exits. (Production `fire`
///   calls `std::process::exit` and never returns; the loop's exit
///   path matters only for unit tests that record-and-continue.)
fn run_loop(
    state: Arc<ProgressState>,
    watchdog_secs: u32,
    tick: Duration,
    stop: Arc<AtomicBool>,
    fire: &mut dyn FnMut(u32),
) {
    let threshold = Duration::from_secs(u64::from(watchdog_secs));
    let initial = state.snapshot();
    let mut last_sent = initial.sent;
    let mut last_received = initial.received;
    let mut last_change_at = Instant::now();

    loop {
        std::thread::sleep(tick);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let snap = state.snapshot();
        let now = Instant::now();
        let advanced = snap.sent != last_sent || snap.received != last_received;
        if advanced {
            last_sent = snap.sent;
            last_received = snap.received;
            last_change_at = now;
            continue;
        }
        if snap.phase != WATCHDOG_OPERATE_PHASE {
            // Outside operate phase: refresh the "last change" anchor
            // to now so the threshold timer effectively pauses. The
            // captured counters stay the same (they are also unchanged
            // here by definition); the only meaningful update is the
            // anchor.
            last_change_at = now;
            continue;
        }
        if now.duration_since(last_change_at) >= threshold {
            fire(watchdog_secs);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    use super::*;
    use crate::types::Phase;

    /// Capture-by-reference fire callback for unit tests. Records the
    /// number of times the watchdog fired and the threshold value the
    /// loop passed to it.
    #[allow(clippy::type_complexity)]
    fn capturing_fire() -> (Arc<AtomicU32>, Arc<Mutex<Vec<u32>>>, impl FnMut(u32) + Send) {
        let count: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
        let thresholds: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let count_for_cb = Arc::clone(&count);
        let thresholds_for_cb = Arc::clone(&thresholds);
        let cb = move |secs: u32| {
            count_for_cb.fetch_add(1, Ordering::Relaxed);
            thresholds_for_cb.lock().unwrap().push(secs);
        };
        (count, thresholds, cb)
    }

    /// Helper: produce a fresh ProgressState pre-seeded with `phase`.
    fn fresh_state(phase: Phase) -> Arc<ProgressState> {
        Arc::new(ProgressState::new(phase))
    }

    #[test]
    fn disabled_watchdog_never_spawns_thread() {
        let state = fresh_state(Phase::Operate);
        let (count, _thresholds, fire) = capturing_fire();
        let wd = Watchdog::start_with_actions(state, 0, Duration::from_millis(20), fire);
        assert!(
            !wd.is_enabled(),
            "watchdog_secs=0 must yield a disabled handle"
        );
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "disabled watchdog must never fire"
        );
    }

    #[test]
    fn watchdog_fires_when_both_counters_frozen_in_operate() {
        // operate phase, counters never move -> watchdog should fire
        // within `threshold + 1 tick` of the loop start.
        let state = fresh_state(Phase::Operate);
        let (count, thresholds, fire) = capturing_fire();
        // 2 s threshold, 50 ms tick (so the loop polls 20x/s -- well
        // under the threshold, so we see exactly one fire).
        let _wd =
            Watchdog::start_with_actions(Arc::clone(&state), 2, Duration::from_millis(50), fire);
        // Wait threshold + a generous slack for OS scheduling. The
        // first tick after construction is ~50ms in (the loop sleeps
        // BEFORE its first check), then the threshold accumulates
        // from the construction time.
        std::thread::sleep(Duration::from_millis(3000));
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "watchdog must fire exactly once when both counters frozen"
        );
        assert_eq!(thresholds.lock().unwrap().as_slice(), &[2]);
    }

    #[test]
    fn watchdog_does_not_fire_when_sent_advances() {
        // Operate phase, sent advances each tick -> watchdog must
        // never fire even after several thresholds elapse.
        let state = fresh_state(Phase::Operate);
        let (count, _thresholds, fire) = capturing_fire();
        let _wd =
            Watchdog::start_with_actions(Arc::clone(&state), 1, Duration::from_millis(50), fire);
        // Bump `sent` every 25 ms for 1500 ms -- 3x the threshold.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            state.sent.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "watchdog must not fire while `sent` is advancing"
        );
    }

    #[test]
    fn watchdog_does_not_fire_when_received_advances() {
        // Symmetric to the previous test: only `received` advances.
        let state = fresh_state(Phase::Operate);
        let (count, _thresholds, fire) = capturing_fire();
        let _wd =
            Watchdog::start_with_actions(Arc::clone(&state), 1, Duration::from_millis(50), fire);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            state.received.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "watchdog must not fire while `received` is advancing"
        );
    }

    #[test]
    fn watchdog_does_not_fire_in_stabilize_phase() {
        // Frozen counters but phase=stabilize -- the watchdog must
        // NOT fire because non-operate phases have their own
        // wallclock budgets.
        let state = fresh_state(Phase::Stabilize);
        let (count, _thresholds, fire) = capturing_fire();
        let _wd =
            Watchdog::start_with_actions(Arc::clone(&state), 1, Duration::from_millis(50), fire);
        // 3x the threshold -- if the watchdog were going to fire it
        // would have done so by now.
        std::thread::sleep(Duration::from_millis(3000));
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "watchdog must not fire outside operate phase"
        );
    }

    #[test]
    fn watchdog_does_not_fire_in_silent_phase() {
        // Same shape as the stabilize test, for the silent phase --
        // ensures the gate is "phase == operate" not "phase != some
        // hardcoded subset".
        let state = fresh_state(Phase::Silent);
        let (count, _thresholds, fire) = capturing_fire();
        let _wd =
            Watchdog::start_with_actions(Arc::clone(&state), 1, Duration::from_millis(50), fire);
        std::thread::sleep(Duration::from_millis(3000));
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "watchdog must not fire in silent phase"
        );
    }

    #[test]
    fn watchdog_starts_timing_when_phase_transitions_to_operate() {
        // The variant spends ~600 ms in stabilize with frozen
        // counters, then flips to operate and stays there with
        // frozen counters. The watchdog must time the operate-phase
        // stall starting from the transition moment, so a 1 s
        // threshold should fire ~1 s AFTER the transition (not from
        // construction time).
        let state = fresh_state(Phase::Stabilize);
        let (count, _thresholds, fire) = capturing_fire();
        let _wd =
            Watchdog::start_with_actions(Arc::clone(&state), 1, Duration::from_millis(50), fire);
        // 600 ms of stabilize: the watchdog ticks 12 times but never
        // fires (phase != operate). After this the anchor is refreshed
        // to "now" on every tick.
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(count.load(Ordering::Relaxed), 0);
        // Transition to operate -- the watchdog's next tick captures
        // phase=operate and starts the timer fresh.
        if let Ok(mut g) = state.phase.lock() {
            *g = Phase::Operate.as_str().to_string();
        }
        // Sleep one more threshold + generous slack. The fire must
        // happen.
        std::thread::sleep(Duration::from_millis(1800));
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "watchdog must fire ~1 s after the phase transitions to operate"
        );
    }

    #[test]
    fn stop_is_idempotent() {
        let state = fresh_state(Phase::Operate);
        let (_count, _thresholds, fire) = capturing_fire();
        let mut wd = Watchdog::start_with_actions(state, 10, Duration::from_millis(50), fire);
        wd.stop();
        // Second call must not panic / hang.
        wd.stop();
    }

    #[test]
    fn dropping_disabled_handle_is_noop() {
        // Sanity: constructing-and-dropping a disabled watchdog must
        // not crash, hang, or otherwise misbehave even with no thread
        // to join.
        let state = fresh_state(Phase::Operate);
        let (_count, _thresholds, fire) = capturing_fire();
        let wd = Watchdog::start_with_actions(state, 0, Duration::from_millis(50), fire);
        drop(wd);
    }
}
