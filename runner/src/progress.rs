//! Runner-side ingest for variant stdout progress events (T15.2, epic E15).
//!
//! The variant (`variant-base` T15.1) emits one JSON line per configured
//! interval to its stdout. The schema is documented in
//! `metak-shared/api-contracts/variant-cli.md` "E15 additions":
//!
//! ```text
//! {"event":"progress","ts":"<RFC3339-ns>","phase":"<phase>","sent":<u64>,
//!  "received":<u64>,"eot_sent":<bool>,"eot_received":<bool>}
//! ```
//!
//! This module owns:
//!
//! - [`LocalProgressTracker`] -- mutable per-spawn state the runner
//!   maintains from the parsed events. The two `last_*_change_ts`
//!   fields are what T15.4's idle detector consumes.
//! - [`TrackerHandle`] -- the `Arc<Mutex<…>>` shared between the reader
//!   thread and whoever wants a snapshot. The snapshot API is
//!   intentionally a `Clone` of the underlying struct, not a borrow,
//!   so callers do not have to worry about holding the mutex across
//!   their own logic.
//! - [`spawn_stdout_reader`] -- the dedicated reader thread that consumes
//!   `child.stdout` line by line, parses each line as a
//!   `#[serde(tag = "event")]` enum, and updates the tracker. Unknown
//!   `event` discriminants and malformed JSON are warned-and-ignored
//!   (T15.2 acceptance criterion); only an underlying IO error or EOF
//!   ends the reader.
//!
//! The reader writes nothing to disk: the JSONL log on the variant side
//! remains the analysis source of truth. This stream is for live runner
//! control only (T15.3 broadcasts snapshots; T15.4 will drive the
//! termination state machine).

use std::io::{BufRead, BufReader, Read};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use serde::Deserialize;

/// Snapshot of per-spawn progress as observed by the runner from the
/// child's stdout.
///
/// The struct is `Clone` so the snapshot API can hand out an owned copy
/// without forcing callers to hold the tracker mutex.
#[derive(Debug, Clone)]
pub struct LocalProgressTracker {
    /// Effective spawn name (e.g. `dummy-qos2`). Matches the
    /// `--variant` CLI arg the runner injected when spawning the child.
    pub spawn_name: String,
    /// Most recent `phase` value observed on a progress event.
    /// `"unknown"` until the first event arrives.
    pub phase: String,
    /// Latest `sent` counter (monotonic per-spawn aggregate).
    pub sent: u64,
    /// Latest `received` counter (monotonic per-spawn aggregate).
    pub received: u64,
    /// Sticky: latest `eot_sent` flag value.
    pub eot_sent: bool,
    /// Sticky: latest `eot_received` flag value.
    pub eot_received: bool,
    /// Wall-clock time at which the most recent progress event was
    /// parsed (regardless of whether any counter advanced).
    pub last_progress_ts: SystemTime,
    /// Wall-clock time at which `sent` last *advanced*. Stays stable
    /// across events where `sent` did not change. Used by T15.4 idle
    /// detection.
    pub last_sent_change_ts: SystemTime,
    /// Wall-clock time at which `received` last *advanced*. Same
    /// semantics as `last_sent_change_ts`.
    pub last_received_change_ts: SystemTime,
}

impl LocalProgressTracker {
    /// Create a fresh tracker for the given spawn name. Counters start
    /// at zero, phase is `"unknown"`, and every timestamp is the
    /// current `SystemTime::now()` so T15.4 has a meaningful epoch to
    /// measure idle windows from even before the first event lands.
    pub fn new(spawn_name: impl Into<String>) -> Self {
        let now = SystemTime::now();
        Self {
            spawn_name: spawn_name.into(),
            phase: "unknown".to_string(),
            sent: 0,
            received: 0,
            eot_sent: false,
            eot_received: false,
            last_progress_ts: now,
            last_sent_change_ts: now,
            last_received_change_ts: now,
        }
    }

    /// Fold a parsed progress event into the tracker.
    ///
    /// The `now` parameter is injected (rather than reading
    /// `SystemTime::now()` here) so unit tests can drive determinstic
    /// timestamps through the merge logic. Production callers pass
    /// `SystemTime::now()`.
    pub fn apply_progress(&mut self, ev: &ProgressEvent, now: SystemTime) {
        self.phase = ev.phase.clone();
        if ev.sent > self.sent {
            self.sent = ev.sent;
            self.last_sent_change_ts = now;
        }
        if ev.received > self.received {
            self.received = ev.received;
            self.last_received_change_ts = now;
        }
        self.eot_sent = ev.eot_sent;
        self.eot_received = ev.eot_received;
        self.last_progress_ts = now;
    }
}

/// Thread-safe handle to a `LocalProgressTracker` shared between the
/// reader thread (writer) and the spawn loop (snapshot reader).
///
/// `Arc<Mutex<…>>` rather than a more elaborate primitive: the
/// progress cadence is ~1 Hz and the snapshot read in T15.4 happens at
/// a similar rate, so mutex contention is irrelevant compared to the
/// simplicity gained.
#[derive(Debug, Clone)]
pub struct TrackerHandle {
    inner: Arc<Mutex<LocalProgressTracker>>,
}

impl TrackerHandle {
    /// Build a handle wrapping a fresh tracker for the given spawn name.
    pub fn new(spawn_name: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LocalProgressTracker::new(spawn_name))),
        }
    }

    /// Take an owned snapshot of the underlying tracker. Holds the
    /// mutex only for the duration of the clone. Panics only on a
    /// poisoned mutex, which would mean the reader thread itself
    /// panicked -- in that case there is nothing to do but propagate.
    pub fn snapshot(&self) -> LocalProgressTracker {
        self.inner
            .lock()
            .expect("tracker mutex poisoned")
            .clone()
    }

    /// Apply a parsed progress event under the mutex. Public for tests
    /// that drive the tracker directly without spawning a reader.
    pub fn apply_progress(&self, ev: &ProgressEvent, now: SystemTime) {
        self.inner
            .lock()
            .expect("tracker mutex poisoned")
            .apply_progress(ev, now);
    }
}

/// Deserialised view of one stdout line from the variant.
///
/// `#[serde(tag = "event")]` matches the contract: every event has a
/// string discriminant under the `"event"` key. Today the only variant
/// is `"progress"`, but new ones (e.g. `"diagnostic"`) can be added
/// without breaking older runners thanks to the `Other` catch-all.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "event")]
enum StdoutEvent {
    #[serde(rename = "progress")]
    Progress(ProgressEvent),
    /// Forward-compat catch-all. Unknown `event` discriminants
    /// deserialise into this variant and are silently ignored by the
    /// reader. Lets the variant introduce new event types in future
    /// without the runner refusing to parse.
    #[serde(other)]
    Other,
}

/// Strongly-typed view of one `event=progress` line.
///
/// Mirrors the schema in
/// `metak-shared/api-contracts/variant-cli.md` "E15 additions". The
/// `ts` field is kept as a raw string -- the runner only uses its own
/// `SystemTime::now()` for idle detection, so we never have to parse
/// the variant's clock back into a real timestamp.
#[derive(Debug, Clone, Deserialize)]
pub struct ProgressEvent {
    /// RFC 3339 timestamp with nanoseconds, as the variant wrote it.
    /// Captured for completeness / logging; not used by tracker logic.
    #[serde(default)]
    pub ts: String,
    /// One of `"connect"`, `"stabilize"`, `"operate"`, `"eot"`,
    /// `"silent"`, `"done"`. Stored as a string to stay forward
    /// compatible if the variant grows new phases.
    pub phase: String,
    /// Monotonic per-spawn aggregate of successful `try_publish`
    /// outcomes.
    pub sent: u64,
    /// Monotonic per-spawn aggregate of `receive` events drained by
    /// the variant.
    pub received: u64,
    /// Sticky flag: variant has emitted its own `eot_sent` JSONL event.
    pub eot_sent: bool,
    /// Sticky flag: variant has observed every expected peer EOT.
    pub eot_received: bool,
}

/// Spawn the dedicated reader thread for one child's stdout.
///
/// `stdout` is the `child.stdout.take().unwrap()` from a `Command`
/// configured with `Stdio::piped()`. `tracker` is the handle the spawn
/// loop holds for snapshotting; the reader thread holds the other end.
/// `runner_name` and `spawn_name` are used only for warning messages
/// on malformed lines (so the operator can attribute them).
///
/// The returned `JoinHandle` should be `join()`ed by the spawn loop
/// after the child has exited (the reader exits naturally on EOF when
/// the child closes stdout).
pub fn spawn_stdout_reader<R>(
    stdout: R,
    tracker: TrackerHandle,
    runner_name: String,
    spawn_name: String,
) -> JoinHandle<()>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line_res in reader.lines() {
            let line = match line_res {
                Ok(l) => l,
                Err(e) => {
                    // IO error on the pipe: the child's stdout is
                    // unusable. Log once and exit cleanly so the
                    // spawn loop can still `join()` us.
                    eprintln!(
                        "[runner:{runner_name}] warning: stdout read error for {spawn_name}: {e}"
                    );
                    return;
                }
            };
            if line.is_empty() {
                continue;
            }
            handle_stdout_line(&line, &tracker, &runner_name, &spawn_name, SystemTime::now());
        }
    })
}

/// Parse one stdout line and fold it into the tracker. Public for
/// unit tests that drive the parser directly without spawning a real
/// child.
pub fn handle_stdout_line(
    line: &str,
    tracker: &TrackerHandle,
    runner_name: &str,
    spawn_name: &str,
    now: SystemTime,
) {
    match serde_json::from_str::<StdoutEvent>(line) {
        Ok(StdoutEvent::Progress(ev)) => {
            tracker.apply_progress(&ev, now);
        }
        Ok(StdoutEvent::Other) => {
            // Unknown event type. Forward-compat: silently ignore so
            // a newer variant can introduce events without breaking
            // an older runner. No warning here -- the operator would
            // see noise on every event of an unknown but otherwise
            // valid type.
        }
        Err(err) => {
            // Malformed JSON. Warn once and continue so a single bad
            // line cannot kill the reader (the variant might have
            // written a stray banner to stdout despite the
            // "stdout-must-be-clean" invariant; we never want that
            // to abort the spawn).
            eprintln!(
                "[runner:{runner_name}] warning: malformed progress line from {spawn_name}: {err}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_now() -> SystemTime {
        SystemTime::now()
    }

    #[test]
    fn new_tracker_has_zero_counters_and_unknown_phase() {
        let t = LocalProgressTracker::new("sp");
        assert_eq!(t.spawn_name, "sp");
        assert_eq!(t.phase, "unknown");
        assert_eq!(t.sent, 0);
        assert_eq!(t.received, 0);
        assert!(!t.eot_sent);
        assert!(!t.eot_received);
    }

    #[test]
    fn apply_progress_updates_counters_and_phase() {
        let mut t = LocalProgressTracker::new("sp");
        let ev = ProgressEvent {
            ts: "t".into(),
            phase: "operate".into(),
            sent: 5,
            received: 3,
            eot_sent: false,
            eot_received: false,
        };
        t.apply_progress(&ev, ts_now());
        assert_eq!(t.phase, "operate");
        assert_eq!(t.sent, 5);
        assert_eq!(t.received, 3);
    }

    #[test]
    fn last_sent_change_ts_only_bumps_on_real_advance() {
        let mut t = LocalProgressTracker::new("sp");
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let t1 = t0 + std::time::Duration::from_secs(1);
        let t2 = t1 + std::time::Duration::from_secs(1);

        let ev_a = ProgressEvent {
            ts: "a".into(),
            phase: "operate".into(),
            sent: 10,
            received: 10,
            eot_sent: false,
            eot_received: false,
        };
        t.apply_progress(&ev_a, t0);
        assert_eq!(t.last_sent_change_ts, t0);
        assert_eq!(t.last_received_change_ts, t0);

        // Same counters again at t1: timestamps must NOT advance.
        let ev_b = ev_a.clone();
        t.apply_progress(&ev_b, t1);
        assert_eq!(t.last_sent_change_ts, t0, "sent ts must not move when flat");
        assert_eq!(
            t.last_received_change_ts, t0,
            "received ts must not move when flat"
        );
        // But last_progress_ts always tracks the latest event.
        assert_eq!(t.last_progress_ts, t1);

        // Now sent advances but received does not: only sent ts moves.
        let ev_c = ProgressEvent {
            ts: "c".into(),
            phase: "operate".into(),
            sent: 11,
            received: 10,
            eot_sent: false,
            eot_received: false,
        };
        t.apply_progress(&ev_c, t2);
        assert_eq!(t.last_sent_change_ts, t2);
        assert_eq!(t.last_received_change_ts, t0);
    }

    #[test]
    fn handle_stdout_line_parses_progress() {
        let tracker = TrackerHandle::new("sp");
        let line = r#"{"event":"progress","ts":"2026-05-11T00:00:00.000000000Z","phase":"operate","sent":42,"received":7,"eot_sent":false,"eot_received":false}"#;
        handle_stdout_line(line, &tracker, "a", "sp", ts_now());
        let snap = tracker.snapshot();
        assert_eq!(snap.phase, "operate");
        assert_eq!(snap.sent, 42);
        assert_eq!(snap.received, 7);
    }

    #[test]
    fn handle_stdout_line_ignores_unknown_event() {
        let tracker = TrackerHandle::new("sp");
        // Unknown event discriminant -- must not panic, must not
        // touch the tracker.
        let line = r#"{"event":"diagnostic","msg":"hi"}"#;
        handle_stdout_line(line, &tracker, "a", "sp", ts_now());
        let snap = tracker.snapshot();
        assert_eq!(snap.phase, "unknown");
        assert_eq!(snap.sent, 0);
    }

    #[test]
    fn handle_stdout_line_warns_and_continues_on_malformed_json() {
        let tracker = TrackerHandle::new("sp");
        // Not valid JSON at all -- the line must be ignored without
        // poisoning the tracker. We can't easily intercept the
        // eprintln warning from a unit test, so we just verify the
        // tracker is still pristine and a subsequent valid line still
        // updates it.
        handle_stdout_line("not even json {{{", &tracker, "a", "sp", ts_now());
        let snap = tracker.snapshot();
        assert_eq!(snap.sent, 0);

        let good = r#"{"event":"progress","ts":"x","phase":"connect","sent":1,"received":0,"eot_sent":false,"eot_received":false}"#;
        handle_stdout_line(good, &tracker, "a", "sp", ts_now());
        let snap = tracker.snapshot();
        assert_eq!(snap.sent, 1);
        assert_eq!(snap.phase, "connect");
    }

    #[test]
    fn spawn_stdout_reader_drains_and_joins() {
        use std::io::Cursor;
        let buf = b"{\"event\":\"progress\",\"ts\":\"x\",\"phase\":\"operate\",\"sent\":3,\"received\":2,\"eot_sent\":false,\"eot_received\":false}\n\
                   {\"event\":\"progress\",\"ts\":\"y\",\"phase\":\"silent\",\"sent\":5,\"received\":4,\"eot_sent\":true,\"eot_received\":true}\n";
        let tracker = TrackerHandle::new("sp");
        let h = spawn_stdout_reader(
            Cursor::new(buf.to_vec()),
            tracker.clone(),
            "a".into(),
            "sp".into(),
        );
        h.join().expect("reader thread panicked");
        let snap = tracker.snapshot();
        assert_eq!(snap.phase, "silent");
        assert_eq!(snap.sent, 5);
        assert_eq!(snap.received, 4);
        assert!(snap.eot_sent);
        assert!(snap.eot_received);
    }

    #[test]
    fn spawn_stdout_reader_tolerates_malformed_lines_mid_stream() {
        use std::io::Cursor;
        // Three lines: malformed, progress, malformed. The reader must
        // process all three, applying the middle one.
        let buf = b"this is not json\n\
                    {\"event\":\"progress\",\"ts\":\"y\",\"phase\":\"operate\",\"sent\":9,\"received\":8,\"eot_sent\":false,\"eot_received\":false}\n\
                    {oops not json\n";
        let tracker = TrackerHandle::new("sp");
        let h = spawn_stdout_reader(
            Cursor::new(buf.to_vec()),
            tracker.clone(),
            "a".into(),
            "sp".into(),
        );
        h.join().expect("reader thread panicked");
        let snap = tracker.snapshot();
        assert_eq!(snap.sent, 9);
        assert_eq!(snap.received, 8);
        assert_eq!(snap.phase, "operate");
    }
}
