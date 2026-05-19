use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::Utc;
use serde_json::json;

use crate::compact::CompactBuffers;
use crate::types::{Phase, Qos, ThreadingMode};

/// Structured JSONL log writer.
///
/// Produces one JSON object per line conforming to the JSONL log schema
/// defined in `metak-shared/api-contracts/jsonl-log-schema.md`.
///
/// Post-T19.10: the JSONL stream carries **lifecycle events only**
/// (`phase`, `connected`, `eot_*`, `resource`). Per-event observations
/// (`write`, `receive`, `backpressure_skipped`, `gap_*`) live in the
/// sibling `<variant>-<runner>-<run>.compact.parquet` file written
/// during the digest phase.
pub struct Logger {
    writer: BufWriter<File>,
    variant: String,
    runner: String,
    run: String,
    path: PathBuf,
}

impl Logger {
    /// Create a new Logger.
    ///
    /// Creates the output file `<variant>-<runner>-<run>.jsonl` in `log_dir`.
    pub fn new(log_dir: &str, variant: &str, runner: &str, run: &str) -> Result<Self> {
        let dir = Path::new(log_dir);
        fs::create_dir_all(dir)?;

        let filename = format!("{}-{}-{}.jsonl", variant, runner, run);
        let path = dir.join(&filename);
        let file = File::create(&path)?;
        let writer = BufWriter::new(file);

        Ok(Self {
            writer,
            variant: variant.to_string(),
            runner: runner.to_string(),
            run: run.to_string(),
            path,
        })
    }

    /// Return the path to the log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Generate the current timestamp in RFC 3339 with nanosecond precision.
    fn now_ts() -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
    }

    /// Write a JSON line to the log file.
    fn write_line(&mut self, value: &serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.writer, value)?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    /// Log a `connected` event.
    ///
    /// Per E14 the event also records the threading mode the spawn ran
    /// in (`threading_mode`) and the OS-level recv buffer size the
    /// runner asked the variant to use (`recv_buffer_kb`). Both are
    /// recorded for offline reproducibility -- the analysis tool keys
    /// metrics on them in T11.5. See
    /// `metak-shared/api-contracts/jsonl-log-schema.md`.
    pub fn log_connected(
        &mut self,
        launch_ts: &str,
        elapsed_ms: f64,
        threading_mode: ThreadingMode,
        recv_buffer_kb: u32,
    ) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "connected",
            "launch_ts": launch_ts,
            "elapsed_ms": elapsed_ms,
            "threading_mode": threading_mode.as_str(),
            "recv_buffer_kb": recv_buffer_kb,
        });
        self.write_line(&entry)
    }

    /// Log a `phase` event.
    pub fn log_phase(&mut self, phase: Phase, profile: Option<&str>) -> Result<()> {
        let mut entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "phase",
            "phase": phase.as_str(),
        });
        if let Some(p) = profile {
            entry
                .as_object_mut()
                .unwrap()
                .insert("profile".to_string(), json!(p));
        }
        self.write_line(&entry)
    }

    /// Log an `eot_sent` event.
    pub fn log_eot_sent(&mut self, eot_id: u64) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "eot_sent",
            "eot_id": eot_id,
        });
        self.write_line(&entry)
    }

    /// Log an `eot_received` event.
    pub fn log_eot_received(&mut self, writer: &str, eot_id: u64) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "eot_received",
            "writer": writer,
            "eot_id": eot_id,
        });
        self.write_line(&entry)
    }

    /// Log an `eot_timeout` event.
    pub fn log_eot_timeout(&mut self, missing: &[String], wait_ms: u64) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "eot_timeout",
            "missing": missing,
            "wait_ms": wait_ms,
        });
        self.write_line(&entry)
    }

    /// Log a `resource` event.
    pub fn log_resource(&mut self, cpu_percent: f64, memory_mb: f64) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "resource",
            "cpu_percent": cpu_percent,
            "memory_mb": memory_mb,
        });
        self.write_line(&entry)
    }

    /// Force-flush the writer.
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

/// Shared compact-buffer sink used by [`LoggerHandle::record_receive`]
/// (T18.3a) so per-event observations emitted from non-driver threads
/// land in the same `CompactBuffers` instance the driver later
/// serialises to Parquet during the digest phase.
///
/// Wrapped in `Arc<Mutex<...>>` so the driver's `EventSink` and any
/// number of reader threads can push into the same column buffers
/// without racing. Reader-thread writes acquire this mutex briefly
/// (one push per receive event); the driver's digest phase runs after
/// `stop_reader_threads` has joined every thread so there is no
/// contention by the time the Parquet writer reads the buffers.
pub type CompactSink = Arc<Mutex<CompactBuffers>>;

/// Thread-safe shared handle to a [`Logger`] (plus the optional shared
/// compact buffer introduced by T18.3a).
///
/// Wraps an `Arc<Mutex<Logger>>` so multiple threads can interact with
/// the lifecycle JSONL stream and the shared compact buffers
/// concurrently. Designed for variants whose Multi-mode reader threads
/// need to record `receive` events directly off the driver thread
/// (T14.10).
///
/// The handle does NOT take ownership of the underlying `Logger`; the
/// driver still owns the original instance and clones a `LoggerHandle`
/// into the variant via [`crate::variant_trait::Variant::attach_logger`].
/// All public methods are intentionally narrow -- only events that may
/// be emitted from a non-driver thread are exposed. Driver-only
/// lifecycle events (phase, connected, eot_sent, ...) stay on the
/// locked path through the original `Logger` handle.
///
/// ## T18.3a + T19.10: compact-buffer attachment
///
/// Per-event observations land exclusively in the per-spawn
/// `CompactBuffers`. The driver constructs a shared [`CompactSink`]
/// (`Arc<Mutex<CompactBuffers>>`) alongside the logger, wires it into
/// the handle via [`LoggerHandle::attach_compact_sink`], and shares
/// the same `Arc` with its own `EventSink`. Reader threads then call
/// [`LoggerHandle::record_receive`] which mirrors what the driver's
/// `EventSink::record_receive` does on the driver thread -- a single
/// push into the compact buffer with no JSONL byproduct.
///
/// The compact-sink attachment is optional so existing unit tests that
/// construct a `LoggerHandle` directly via [`LoggerHandle::new`] keep
/// working without modification. When the sink is `None`,
/// `record_receive` silently drops the row (lifecycle-only mode for
/// fixtures).
#[derive(Clone)]
pub struct LoggerHandle {
    inner: Arc<Mutex<Logger>>,
    /// Shared compact-buffer sink, populated by
    /// [`LoggerHandle::attach_compact_sink`]. `None` for handles built
    /// by tests that never set up a compact sink.
    compact: Option<CompactSink>,
}

impl LoggerHandle {
    /// Wrap an owned `Logger` for cross-thread use. The driver retains
    /// its own clone of the `Arc` so the original can keep emitting
    /// driver-side events while reader threads use additional clones.
    ///
    /// The compact-sink attachment defaults to `None`. Callers that
    /// want compact-buffer mirroring invoke [`Self::attach_compact_sink`]
    /// before sharing the handle across threads.
    pub fn new(logger: Logger) -> Self {
        Self {
            inner: Arc::new(Mutex::new(logger)),
            compact: None,
        }
    }

    /// Borrow the inner `Arc<Mutex<Logger>>` -- the driver uses this to
    /// reach driver-only lifecycle event methods (log_phase,
    /// log_connected, log_eot_sent, ...) without exposing them on the
    /// cross-thread handle surface.
    pub fn inner(&self) -> &Arc<Mutex<Logger>> {
        &self.inner
    }

    /// Wire a shared [`CompactSink`] into this handle (T18.3a).
    ///
    /// The driver calls this on the handle BEFORE cloning it into
    /// variants via `Variant::attach_logger`, passing the same
    /// `Arc<Mutex<CompactBuffers>>` it shares with its own `EventSink`.
    /// Every reader thread that clones this handle therefore writes
    /// into the same column buffers the digest phase serialises.
    ///
    /// Idempotent: calling twice replaces the previously-wired sink.
    /// Tests typically skip this step and rely on the `None` fallback
    /// (`record_receive` then silently drops the row).
    ///
    /// The trailing `_legacy_jsonl` parameter is retained as a
    /// back-compat shim for in-tree test code in concrete variants
    /// that called the pre-T19.10 two-argument form. It is ignored —
    /// per-event JSONL emission is gone, so the flag has no effect.
    /// New code may pass `false` (or any value) for that argument;
    /// the parameter may be removed in a later cleanup pass.
    pub fn attach_compact_sink(&mut self, sink: CompactSink, _legacy_jsonl: bool) {
        self.compact = Some(sink);
    }

    /// Inspect the attached compact sink, if any (test helper).
    #[doc(hidden)]
    pub fn compact_sink(&self) -> Option<&CompactSink> {
        self.compact.as_ref()
    }

    /// Record a `receive` event into the shared compact buffer.
    ///
    /// This is the cross-thread analogue of the driver's
    /// `EventSink::record_receive` -- the public method websocket
    /// reader threads (and the T17.5 Single-mode drain helper) call so
    /// receives never bypass the compact `EventBuffer` the digest
    /// phase serialises. Post-T19.10 there is no JSONL byproduct on
    /// this path: per-event observations live exclusively in
    /// compact-Parquet.
    ///
    /// When no compact sink is attached the row is silently dropped --
    /// the back-compat path for unit tests that construct a bare
    /// `LoggerHandle::new(...)` without compact-buffer mirroring.
    pub fn record_receive(
        &self,
        writer: &str,
        seq: u64,
        path: &str,
        qos: Qos,
        bytes: usize,
    ) -> Result<()> {
        if let Some(sink) = &self.compact {
            let ts_ns = Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let mut buf = sink
                .lock()
                .map_err(|_| anyhow::anyhow!("LoggerHandle compact-sink mutex poisoned"))?;
            buf.push_receive(ts_ns, writer, seq, path, qos.as_int(), bytes as u32)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use tempfile::TempDir;

    fn create_test_logger() -> (Logger, TempDir) {
        let dir = TempDir::new().unwrap();
        let logger = Logger::new(
            dir.path().to_str().unwrap(),
            "test-variant",
            "runner-a",
            "run01",
        )
        .unwrap();
        (logger, dir)
    }

    fn read_lines(logger: &Logger) -> Vec<serde_json::Value> {
        let file = File::open(logger.path()).unwrap();
        let reader = std::io::BufReader::new(file);
        reader
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn test_file_naming() {
        let (logger, _dir) = create_test_logger();
        let name = logger.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "test-variant-runner-a-run01.jsonl");
    }

    #[test]
    fn test_common_fields_present() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_phase(Phase::Connect, None).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.get("ts").is_some());
        assert_eq!(line["variant"], "test-variant");
        assert_eq!(line["runner"], "runner-a");
        assert_eq!(line["run"], "run01");
        assert_eq!(line["event"], "phase");
    }

    #[test]
    fn test_connected_event() {
        let (mut logger, _dir) = create_test_logger();
        logger
            .log_connected(
                "2026-04-12T14:00:00.000000000Z",
                123.456,
                ThreadingMode::Single,
                4096,
            )
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "connected");
        assert_eq!(line["launch_ts"], "2026-04-12T14:00:00.000000000Z");
        assert_eq!(line["elapsed_ms"], 123.456);
        assert_eq!(line["threading_mode"], "single");
        assert_eq!(line["recv_buffer_kb"], 4096);
    }

    #[test]
    fn test_connected_event_records_multi_mode() {
        let (mut logger, _dir) = create_test_logger();
        logger
            .log_connected(
                "2026-04-12T14:00:00.000000000Z",
                7.0,
                ThreadingMode::Multi,
                8192,
            )
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["threading_mode"], "multi");
        assert_eq!(line["recv_buffer_kb"], 8192);
    }

    #[test]
    fn test_phase_event_with_profile() {
        let (mut logger, _dir) = create_test_logger();
        logger
            .log_phase(Phase::Operate, Some("scalar-flood"))
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "phase");
        assert_eq!(line["phase"], "operate");
        assert_eq!(line["profile"], "scalar-flood");
    }

    #[test]
    fn test_phase_event_without_profile() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_phase(Phase::Stabilize, None).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "phase");
        assert_eq!(line["phase"], "stabilize");
        assert!(line.get("profile").is_none());
    }

    #[test]
    fn test_eot_sent_event() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_eot_sent(0xDEADBEEF).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "eot_sent");
        assert_eq!(line["eot_id"], 0xDEADBEEF_u64);
    }

    #[test]
    fn test_eot_received_event() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_eot_received("runner-b", 42).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "eot_received");
        assert_eq!(line["writer"], "runner-b");
        assert_eq!(line["eot_id"], 42);
    }

    #[test]
    fn test_eot_timeout_event() {
        let (mut logger, _dir) = create_test_logger();
        let missing = vec!["alice".to_string(), "bob".to_string()];
        logger.log_eot_timeout(&missing, 5000).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "eot_timeout");
        assert_eq!(line["wait_ms"], 5000);
        let arr = line["missing"].as_array().expect("missing should be array");
        let names: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(names, vec!["alice", "bob"]);
    }

    #[test]
    fn test_eot_phase_event() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_phase(Phase::Eot, None).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "phase");
        assert_eq!(line["phase"], "eot");
    }

    #[test]
    fn test_resource_event() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_resource(45.2, 128.5).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "resource");
        assert_eq!(line["cpu_percent"], 45.2);
        assert_eq!(line["memory_mb"], 128.5);
    }

    #[test]
    fn test_ts_rfc3339_nanosecond() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_phase(Phase::Connect, None).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let ts = lines[0]["ts"].as_str().unwrap();
        // Should parse as a valid RFC 3339 timestamp.
        chrono::DateTime::parse_from_rfc3339(ts).expect("ts must be valid RFC 3339");
        // Should have nanosecond precision (9 decimal places).
        assert!(ts.contains('.'), "ts should contain fractional seconds");
        let frac = ts.split('.').nth(1).unwrap();
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        assert_eq!(digits.len(), 9, "ts should have 9 fractional digits");
    }

    // ------ T18.3a / T19.10: LoggerHandle::record_receive tests ------

    fn handle_with_logger(dir: &TempDir) -> LoggerHandle {
        let logger = Logger::new(
            dir.path().to_str().unwrap(),
            "test-variant",
            "runner-a",
            "run01",
        )
        .unwrap();
        LoggerHandle::new(logger)
    }

    #[test]
    fn record_receive_without_compact_sink_silently_drops() {
        // Backwards-compat path: a `LoggerHandle` constructed without
        // `attach_compact_sink` silently drops the row (and never
        // emits a JSONL line post-T19.10).
        let dir = TempDir::new().unwrap();
        let handle = handle_with_logger(&dir);
        let log_path = handle.inner().lock().unwrap().path().to_path_buf();

        handle
            .record_receive("alice", 42, "/bench/0", Qos::ReliableTcp, 64)
            .unwrap();
        handle.inner().lock().unwrap().flush().unwrap();

        let file = File::open(&log_path).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(file)
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert!(
            lines.is_empty(),
            "post-T19.10: no JSONL emission for per-event observations, got {lines:?}"
        );
        // No compact sink attached -> compact_sink() is None.
        assert!(handle.compact_sink().is_none());
    }

    #[test]
    fn record_receive_pushes_into_compact_buffer() {
        // Core invariant: with a compact sink attached, the receive
        // row lands in the shared `CompactBuffers` so the digest phase
        // later serialises it to Parquet.
        let dir = TempDir::new().unwrap();
        let mut handle = handle_with_logger(&dir);
        let sink: CompactSink = Arc::new(Mutex::new(CompactBuffers::new()));
        handle.attach_compact_sink(sink.clone(), false);

        handle
            .record_receive("bob", 7, "/bench/0", Qos::ReliableTcp, 128)
            .unwrap();

        let buf = sink.lock().unwrap();
        assert_eq!(buf.len(), 1, "exactly one compact row");
        assert_eq!(buf.kind, vec![crate::compact::EventKind::Receive as u8]);
        assert_eq!(buf.seq, vec![7]);
        assert_eq!(buf.qos, vec![4]);
        assert_eq!(buf.bytes, vec![128]);
        // Peer name interned at index 0; path at index 0.
        assert_eq!(buf.peer_idx, vec![0]);
        assert_eq!(buf.path_idx, vec![0]);
        assert_eq!(buf.peers.dict(), &["bob".to_string()]);
        assert_eq!(buf.paths.dict(), &["/bench/0".to_string()]);
    }

    #[test]
    fn record_receive_never_emits_jsonl() {
        // T19.10 contract: per-event observations are compact-only.
        // Even with a compact sink attached, `record_receive` MUST NOT
        // emit a JSONL line for the receive — only lifecycle events go
        // to JSONL post-T19.10.
        let dir = TempDir::new().unwrap();
        let mut handle = handle_with_logger(&dir);
        let sink: CompactSink = Arc::new(Mutex::new(CompactBuffers::new()));
        handle.attach_compact_sink(sink.clone(), false);
        let log_path = handle.inner().lock().unwrap().path().to_path_buf();

        handle
            .record_receive("alice", 5, "/bench/0", Qos::ReliableTcp, 16)
            .unwrap();
        handle.inner().lock().unwrap().flush().unwrap();

        assert_eq!(sink.lock().unwrap().len(), 1, "compact row pushed");
        let file = File::open(&log_path).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(file)
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert!(
            lines.is_empty(),
            "post-T19.10: no JSONL line emitted on the receive path, got {lines:?}"
        );
    }

    #[test]
    fn record_receive_clone_shares_compact_sink() {
        // Cloning a `LoggerHandle` (the pattern reader threads use)
        // shares the SAME `CompactSink` `Arc` -- pushes from any clone
        // land in the same buffer.
        let dir = TempDir::new().unwrap();
        let mut handle = handle_with_logger(&dir);
        let sink: CompactSink = Arc::new(Mutex::new(CompactBuffers::new()));
        handle.attach_compact_sink(sink.clone(), false);

        let clone_a = handle.clone();
        let clone_b = handle.clone();
        clone_a
            .record_receive("alice", 1, "/p", Qos::ReliableTcp, 8)
            .unwrap();
        clone_b
            .record_receive("bob", 2, "/p", Qos::ReliableTcp, 8)
            .unwrap();

        let buf = sink.lock().unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.peers.dict(), &["alice".to_string(), "bob".to_string()]);
        // Both rows share the single interned path index.
        assert_eq!(buf.path_idx, vec![0, 0]);
    }

    #[test]
    fn record_receive_concurrent_pushes_all_land() {
        // The promise the audit cares about: under concurrent reader-
        // thread pushes (the websocket Multi-mode reproducer), every
        // receive lands in the compact buffer. Smoke-test by spawning
        // a few threads that each push N rows; assert the final row
        // count is exactly the sum.
        use std::thread;
        let dir = TempDir::new().unwrap();
        let mut handle = handle_with_logger(&dir);
        let sink: CompactSink = Arc::new(Mutex::new(CompactBuffers::new()));
        handle.attach_compact_sink(sink.clone(), false);

        const THREADS: usize = 4;
        const PER_THREAD: u64 = 250;
        let mut joins = Vec::with_capacity(THREADS);
        for t in 0..THREADS {
            let h = handle.clone();
            let writer = format!("peer{t}");
            joins.push(thread::spawn(move || {
                for seq in 0..PER_THREAD {
                    h.record_receive(&writer, seq, "/p", Qos::ReliableTcp, 8)
                        .unwrap();
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
        let buf = sink.lock().unwrap();
        assert_eq!(buf.len(), THREADS * (PER_THREAD as usize));
        // All four peers got interned distinct slots.
        assert_eq!(buf.peers.dict().len(), THREADS);
    }
}
