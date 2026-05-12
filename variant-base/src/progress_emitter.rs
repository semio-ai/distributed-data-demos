//! Stdout-based progress emission for the variant.
//!
//! Introduced by T15.1 (E15). The variant emits one JSON line per
//! configured interval to stdout so the runner can observe per-spawn
//! state (phase, sent/received counts, EOT flags) without any IPC
//! beyond one-way stdout. The runner-side reader and the cross-runner
//! coordination ride on top of this in T15.2 / T15.3.
//!
//! ## Schema
//!
//! Each line is a single, atomically-written, newline-terminated JSON
//! object of the form:
//!
//! ```text
//! {"event":"progress","ts":"<RFC3339-ns>","phase":"<phase>","sent":<u64>,
//!  "received":<u64>,"eot_sent":<bool>,"eot_received":<bool>}
//! ```
//!
//! Phase strings are `connect`, `stabilize`, `operate`, `eot`, `silent`,
//! or `done`. The first five match [`crate::types::Phase`]; `done` is
//! the terminal value the emitter switches to after the driver finishes
//! tearing the transport down (the variant is about to exit).
//!
//! ## Stdout invariant
//!
//! The progress line is the ONLY stdout output from a variant. Build
//! banners, diagnostic warnings, and any other variant-side text MUST
//! go to stderr (`eprintln!`). This invariant is what makes the
//! runner's `Stdio::piped()` stdout reader (T15.2) able to parse the
//! stream as line-delimited JSON.
//!
//! ## Disabling emission
//!
//! Construct the emitter with `interval_ms = 0`. No background thread
//! is spawned and nothing is ever written to stdout; counters and phase
//! updates from the driver become no-ops on the writer side.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;

use crate::types::Phase;

/// Terminal phase value emitted after the driver has fully torn down
/// the transport. Not part of [`Phase`] because the driver's phase
/// enum is owned by the protocol state machine; `done` is purely an
/// observer-side label.
pub const DONE_PHASE: &str = "done";

/// Snapshot of the emitter's observable state at a point in time.
/// Returned by [`ProgressEmitter::snapshot`] for tests and for the
/// emitter thread itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub phase: String,
    pub sent: u64,
    pub received: u64,
    pub eot_sent: bool,
    pub eot_received: bool,
}

/// Shared, thread-safe state behind a [`ProgressEmitter`].
///
/// All fields are atomics (lock-free) except `phase`, which is a
/// `Mutex<String>` because the value is a short identifier the
/// emitter thread reads once per interval. Mutex contention is
/// negligible at the 1 Hz default cadence.
struct ProgressState {
    phase: Mutex<String>,
    sent: AtomicU64,
    received: AtomicU64,
    eot_sent: AtomicBool,
    eot_received: AtomicBool,
    /// Set to `true` to ask the emitter thread to exit on its next
    /// wake-up. Used by [`ProgressEmitter::stop`] / `Drop`.
    stop: AtomicBool,
    /// Wakes the emitter thread on either timer expiry or shutdown.
    /// The condvar lets `stop` interrupt the inter-tick sleep so
    /// shutdown does not have to wait up to one full interval.
    wake: Condvar,
    /// Companion mutex for `wake`. Holds no payload; the condvar API
    /// requires one.
    wake_lock: Mutex<()>,
}

impl ProgressState {
    fn new(initial_phase: Phase) -> Self {
        Self {
            phase: Mutex::new(initial_phase.as_str().to_string()),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            eot_sent: AtomicBool::new(false),
            eot_received: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
        }
    }

    fn snapshot(&self) -> ProgressSnapshot {
        let phase = self
            .phase
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|p| p.into_inner().clone());
        ProgressSnapshot {
            phase,
            sent: self.sent.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
            eot_sent: self.eot_sent.load(Ordering::Relaxed),
            eot_received: self.eot_received.load(Ordering::Relaxed),
        }
    }
}

/// Stdout progress emitter.
///
/// Owned by the protocol driver. The driver pokes the setter methods
/// (`set_phase`, `inc_sent`, `inc_received`, `mark_eot_sent`,
/// `mark_eot_received`) as state evolves; a background thread observes
/// the state and writes one JSON line to stdout every `interval_ms`.
///
/// Construct with `ProgressEmitter::new(interval_ms, initial_phase)`.
/// When `interval_ms == 0` the emitter is **disabled**: no thread is
/// spawned, no stdout writes happen, and the setter methods still
/// maintain in-memory state (for `snapshot` and tests) but the runner
/// observes nothing. This preserves the back-compat behaviour for
/// callers that pass `--progress-stdout-interval-ms 0`.
/// Type alias for the shared, thread-safe writer callback used by the
/// emitter thread and by `stop()`'s final synchronous emission. The
/// alias exists to keep the field type readable and to satisfy
/// `clippy::type_complexity`.
type SharedWriter = Arc<dyn Fn(&str) + Send + Sync>;

pub struct ProgressEmitter {
    state: Arc<ProgressState>,
    interval_ms: u32,
    thread: Option<JoinHandle<()>>,
    /// Shared writer used by both the background thread and `stop()`'s
    /// final synchronous emission. `None` when the emitter is
    /// disabled (`interval_ms == 0`).
    writer: Option<SharedWriter>,
}

impl ProgressEmitter {
    /// Build a new emitter and start the background thread if
    /// `interval_ms > 0`. The thread emits the first line approximately
    /// one interval after construction (it sleeps before its first
    /// write).
    pub fn new(interval_ms: u32, initial_phase: Phase) -> Self {
        Self::new_with_writer(interval_ms, initial_phase, write_to_stdout)
    }

    /// Build a new emitter with an injectable write callback. The
    /// default writer (`write_to_stdout`) prints the line to stdout
    /// and flushes; tests use this entry point with a capturing
    /// `Arc<Mutex<Vec<String>>>` writer to avoid touching the real
    /// stdout stream during unit tests.
    pub fn new_with_writer<W>(interval_ms: u32, initial_phase: Phase, write_line: W) -> Self
    where
        W: Fn(&str) + Send + Sync + 'static,
    {
        let state = Arc::new(ProgressState::new(initial_phase));
        if interval_ms == 0 {
            return Self {
                state,
                interval_ms,
                thread: None,
                writer: None,
            };
        }
        let writer: SharedWriter = Arc::new(write_line);
        let state_for_thread = Arc::clone(&state);
        let interval = Duration::from_millis(u64::from(interval_ms));
        let writer_for_thread = Arc::clone(&writer);
        let handle = std::thread::Builder::new()
            .name("variant-progress".to_string())
            .spawn(move || emit_loop(state_for_thread, interval, writer_for_thread))
            .expect("spawning progress-emitter thread should not fail");
        Self {
            state,
            interval_ms,
            thread: Some(handle),
            writer: Some(writer),
        }
    }

    /// Return whether the emitter actually emits to stdout. False when
    /// `interval_ms == 0`.
    pub fn is_enabled(&self) -> bool {
        self.interval_ms > 0
    }

    /// Update the current phase. Cheap (a short mutex lock); the next
    /// emitter tick will pick it up.
    pub fn set_phase(&self, phase: Phase) {
        if let Ok(mut guard) = self.state.phase.lock() {
            *guard = phase.as_str().to_string();
        }
    }

    /// Transition the emitter to the terminal `done` phase. Called by
    /// the driver right before exiting the protocol so the last
    /// observable progress line carries `phase=done`.
    pub fn set_done(&self) {
        if let Ok(mut guard) = self.state.phase.lock() {
            *guard = DONE_PHASE.to_string();
        }
    }

    /// Increment the aggregate `sent` counter by one. Lock-free.
    pub fn inc_sent(&self) {
        self.state.sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the aggregate `received` counter by one. Lock-free.
    pub fn inc_received(&self) {
        self.state.received.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark that the variant has emitted its `eot_sent` JSONL event.
    /// The flag is sticky -- it stays `true` for the rest of the spawn.
    pub fn mark_eot_sent(&self) {
        self.state.eot_sent.store(true, Ordering::Relaxed);
    }

    /// Mark that the variant has observed every expected peer EOT.
    /// Sticky once set.
    pub fn mark_eot_received(&self) {
        self.state.eot_received.store(true, Ordering::Relaxed);
    }

    /// Return a consistent snapshot of the current state. Useful for
    /// tests and for the emitter thread itself.
    pub fn snapshot(&self) -> ProgressSnapshot {
        self.state.snapshot()
    }

    /// Signal the background thread to exit and join it. Idempotent.
    /// Called automatically by `Drop`; the driver may call it
    /// explicitly so the thread is joined before the process exits.
    ///
    /// On the enabled path, this also emits ONE final line
    /// synchronously after the thread has joined. The driver calls
    /// `set_done()` immediately before `stop()`; without the final
    /// emission the `done` phase could otherwise be missed by the
    /// runner if `stop()` interrupts a wait between intervals (which
    /// is the common case after a short spawn). The final line
    /// guarantees the runner sees the terminal phase exactly once.
    pub fn stop(&mut self) {
        if let Some(handle) = self.thread.take() {
            self.state.stop.store(true, Ordering::Relaxed);
            // Wake the thread out of its sleep so it can observe the
            // stop flag and exit promptly rather than waiting for the
            // next interval boundary.
            if let Ok(_guard) = self.state.wake_lock.lock() {
                self.state.wake.notify_all();
            }
            let _ = handle.join();
            // Final synchronous emission so the last-known state
            // (e.g. `phase=done`) is observable by the runner.
            if let Some(writer) = self.writer.as_ref() {
                let line = build_progress_line(&self.state.snapshot());
                writer(&line);
            }
        }
    }
}

impl Drop for ProgressEmitter {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Default writer used by `ProgressEmitter::new`. Writes one line to
/// stdout and flushes. Errors are swallowed -- there is no recovery
/// path for a closed stdout in this context, and the runner is
/// expected to be the consumer.
fn write_to_stdout(line: &str) {
    let mut stdout = io::stdout().lock();
    // The line never contains a newline (we built it from a single
    // `serde_json::Value`); append exactly one so the runner reader
    // can split on `'\n'`.
    if writeln!(stdout, "{line}").is_ok() {
        let _ = stdout.flush();
    }
}

/// Build the JSON line for one tick from a state snapshot. Public so
/// tests can validate the schema without going through the emitter
/// thread.
pub fn build_progress_line(snapshot: &ProgressSnapshot) -> String {
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string();
    let value = json!({
        "event": "progress",
        "ts": ts,
        "phase": snapshot.phase,
        "sent": snapshot.sent,
        "received": snapshot.received,
        "eot_sent": snapshot.eot_sent,
        "eot_received": snapshot.eot_received,
    });
    // `serde_json::to_string` produces a single-line JSON document
    // (no embedded newlines for the values we use here -- phase is a
    // short ASCII string, numbers + bools have no whitespace). That
    // guarantee is what makes the runner-side line-delimited parse
    // sound.
    serde_json::to_string(&value).expect("progress JSON serialization should never fail")
}

/// Background-thread loop: wait for `interval`, emit one line, repeat
/// until the stop flag is set. Uses a condvar so shutdown does not
/// have to wait a full interval.
fn emit_loop(state: Arc<ProgressState>, interval: Duration, write_line: SharedWriter) {
    let mut guard = match state.wake_lock.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    while !state.stop.load(Ordering::Relaxed) {
        let result = state.wake.wait_timeout(guard, interval);
        guard = match result {
            Ok((g, _)) => g,
            Err(p) => p.into_inner().0,
        };
        if state.stop.load(Ordering::Relaxed) {
            break;
        }
        let snapshot = state.snapshot();
        let line = build_progress_line(&snapshot);
        write_line(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    /// Build a writer that pushes each line into a shared Vec. Used
    /// instead of stdout so tests can introspect emitted lines without
    /// touching the real process stdout.
    fn capturing_writer() -> (
        Arc<StdMutex<Vec<String>>>,
        impl Fn(&str) + Send + Sync + 'static,
    ) {
        let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let writer = move |line: &str| {
            captured_clone.lock().unwrap().push(line.to_string());
        };
        (captured, writer)
    }

    #[test]
    fn disabled_emitter_never_spawns_thread_or_emits() {
        let (captured, writer) = capturing_writer();
        let emitter = ProgressEmitter::new_with_writer(0, Phase::Connect, writer);
        assert!(!emitter.is_enabled());
        // Even after activity and a sleep larger than any reasonable
        // interval, the disabled emitter must not write anything.
        emitter.inc_sent();
        emitter.inc_received();
        emitter.set_phase(Phase::Operate);
        std::thread::sleep(Duration::from_millis(200));
        let lines = captured.lock().unwrap();
        assert!(
            lines.is_empty(),
            "interval_ms=0 must produce zero emitted lines, got {}",
            lines.len()
        );
    }

    #[test]
    fn line_is_well_formed_json_with_expected_schema() {
        let snap = ProgressSnapshot {
            phase: "operate".to_string(),
            sent: 17,
            received: 9,
            eot_sent: false,
            eot_received: false,
        };
        let line = build_progress_line(&snap);
        let value: serde_json::Value = serde_json::from_str(&line).expect("line must parse");
        assert_eq!(value["event"], "progress");
        assert_eq!(value["phase"], "operate");
        assert_eq!(value["sent"], 17);
        assert_eq!(value["received"], 9);
        assert_eq!(value["eot_sent"], false);
        assert_eq!(value["eot_received"], false);
        let ts = value["ts"].as_str().expect("ts must be a string");
        // RFC 3339 with nanoseconds parses cleanly.
        chrono::DateTime::parse_from_rfc3339(ts).expect("ts must be RFC 3339");
        // The line itself must NOT contain any embedded newline -- the
        // runner reader splits on '\n'.
        assert!(
            !line.contains('\n'),
            "progress line must not contain embedded newlines"
        );
    }

    #[test]
    fn emitter_produces_approximately_one_line_per_interval() {
        let (captured, writer) = capturing_writer();
        // 50 ms interval, observe for ~500 ms -> expect roughly 9-11
        // lines. We allow a generous tolerance because thread
        // scheduling and the condvar wait_timeout granularity can
        // cause modest drift on busy CI hosts.
        let _emitter = ProgressEmitter::new_with_writer(50, Phase::Operate, writer);
        std::thread::sleep(Duration::from_millis(500));
        let lines = captured.lock().unwrap().clone();
        let count = lines.len();
        assert!(
            (7..=15).contains(&count),
            "expected 7..=15 lines over 500 ms at 50 ms interval, got {count}"
        );
        // Every captured line must parse as JSON with the documented
        // shape.
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} parse failed: {e}"));
            assert_eq!(v["event"], "progress");
            assert!(v["ts"].is_string());
            assert!(v["phase"].is_string());
            assert!(v["sent"].is_u64());
            assert!(v["received"].is_u64());
            assert!(v["eot_sent"].is_boolean());
            assert!(v["eot_received"].is_boolean());
        }
    }

    #[test]
    fn counters_increment_monotonically_and_show_up_in_lines() {
        let (captured, writer) = capturing_writer();
        let emitter = ProgressEmitter::new_with_writer(40, Phase::Operate, writer);
        let start = Instant::now();
        // Increment as fast as possible while the emitter ticks; the
        // last line should reflect the final counter values.
        let mut total_sent: u64 = 0;
        let mut total_received: u64 = 0;
        while start.elapsed() < Duration::from_millis(400) {
            emitter.inc_sent();
            total_sent += 1;
            if total_sent.is_multiple_of(3) {
                emitter.inc_received();
                total_received += 1;
            }
            // Avoid spinning the test harness.
            std::thread::sleep(Duration::from_micros(50));
        }
        drop(emitter);
        let lines = captured.lock().unwrap().clone();
        assert!(!lines.is_empty(), "should have emitted at least one line");
        // Parse all lines and check sent/received are monotonic
        // non-decreasing.
        let mut prev_sent = 0u64;
        let mut prev_received = 0u64;
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let s = v["sent"].as_u64().unwrap();
            let r = v["received"].as_u64().unwrap();
            assert!(
                s >= prev_sent,
                "sent must be monotonic non-decreasing: {prev_sent} -> {s}"
            );
            assert!(
                r >= prev_received,
                "received must be monotonic non-decreasing: {prev_received} -> {r}"
            );
            prev_sent = s;
            prev_received = r;
        }
        // The final emitted line should not exceed the totals; and
        // because we increment before each tick boundary, it should
        // be close to (but typically less than or equal to) the
        // totals at the moment of the last tick.
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert!(last["sent"].as_u64().unwrap() <= total_sent);
        assert!(last["received"].as_u64().unwrap() <= total_received);
    }

    #[test]
    fn phase_field_transitions_correctly() {
        let (captured, writer) = capturing_writer();
        let emitter = ProgressEmitter::new_with_writer(40, Phase::Connect, writer);
        // Step through every phase, sleeping long enough each time
        // for the emitter to capture at least one line.
        for phase in [
            Phase::Connect,
            Phase::Stabilize,
            Phase::Operate,
            Phase::Eot,
            Phase::Silent,
        ] {
            emitter.set_phase(phase);
            std::thread::sleep(Duration::from_millis(120));
        }
        emitter.set_done();
        std::thread::sleep(Duration::from_millis(120));
        drop(emitter);

        let lines = captured.lock().unwrap().clone();
        let phases: Vec<String> = lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["phase"].as_str().unwrap().to_string()
            })
            .collect();
        // Every expected phase value must appear at least once in the
        // emitted sequence. Order is preserved (we only set a later
        // phase after the previous emission has had a chance to fire).
        for expected in ["connect", "stabilize", "operate", "eot", "silent", "done"] {
            assert!(
                phases.iter().any(|p| p == expected),
                "missing phase '{expected}' in emitted sequence: {phases:?}"
            );
        }
        // Transition order: each phase's first appearance must be
        // strictly after the prior phase's first appearance.
        let order = ["connect", "stabilize", "operate", "eot", "silent", "done"];
        let mut last_idx: i64 = -1;
        for expected in order {
            let idx = phases.iter().position(|p| p == expected).unwrap() as i64;
            assert!(
                idx > last_idx,
                "phase '{expected}' first appeared at line {idx}, expected after {last_idx}"
            );
            last_idx = idx;
        }
    }

    #[test]
    fn eot_flags_propagate_to_emitted_lines() {
        let (captured, writer) = capturing_writer();
        let emitter = ProgressEmitter::new_with_writer(40, Phase::Eot, writer);
        std::thread::sleep(Duration::from_millis(80));
        emitter.mark_eot_sent();
        std::thread::sleep(Duration::from_millis(80));
        emitter.mark_eot_received();
        std::thread::sleep(Duration::from_millis(80));
        drop(emitter);
        let lines = captured.lock().unwrap().clone();
        assert!(!lines.is_empty());
        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // Once the flags flip true, they must stay true for every
        // subsequent line (sticky semantics).
        let mut seen_sent = false;
        let mut seen_received = false;
        for v in &parsed {
            let s = v["eot_sent"].as_bool().unwrap();
            let r = v["eot_received"].as_bool().unwrap();
            if seen_sent {
                assert!(s, "eot_sent must stay true after flipping");
            }
            if seen_received {
                assert!(r, "eot_received must stay true after flipping");
            }
            seen_sent |= s;
            seen_received |= r;
        }
        assert!(seen_sent, "eot_sent must be true in at least one line");
        assert!(
            seen_received,
            "eot_received must be true in at least one line"
        );
    }

    #[test]
    fn stop_is_idempotent_and_joins_thread() {
        let (_captured, writer) = capturing_writer();
        let mut emitter = ProgressEmitter::new_with_writer(20, Phase::Operate, writer);
        std::thread::sleep(Duration::from_millis(40));
        emitter.stop();
        // Second call must not panic or hang.
        emitter.stop();
    }

    #[test]
    fn snapshot_reflects_current_state() {
        let (_captured, writer) = capturing_writer();
        let emitter = ProgressEmitter::new_with_writer(0, Phase::Connect, writer);
        let s0 = emitter.snapshot();
        assert_eq!(s0.phase, "connect");
        assert_eq!(s0.sent, 0);
        assert_eq!(s0.received, 0);
        assert!(!s0.eot_sent);
        assert!(!s0.eot_received);

        emitter.set_phase(Phase::Operate);
        emitter.inc_sent();
        emitter.inc_sent();
        emitter.inc_received();
        emitter.mark_eot_sent();
        let s1 = emitter.snapshot();
        assert_eq!(s1.phase, "operate");
        assert_eq!(s1.sent, 2);
        assert_eq!(s1.received, 1);
        assert!(s1.eot_sent);
        assert!(!s1.eot_received);
    }
}
