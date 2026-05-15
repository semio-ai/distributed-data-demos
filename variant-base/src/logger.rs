use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::types::{Phase, Qos, ThreadingMode};

/// Structured JSONL log writer.
///
/// Produces one JSON object per line conforming to the JSONL log schema
/// defined in `metak-shared/api-contracts/jsonl-log-schema.md`.
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
        Self::format_ts(Utc::now())
    }

    /// Format a caller-supplied `DateTime<Utc>` as RFC 3339 with
    /// nanosecond precision. Used by `log_write_at` so the driver can
    /// capture `write_ts` before the variant's `try_publish` call (T16.2)
    /// and have it serialised through the same code path that produces
    /// every other `ts` field.
    fn format_ts(ts: DateTime<Utc>) -> String {
        ts.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
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

    /// Log a `write` event.
    ///
    /// Captures the timestamp at the moment of this call. Equivalent to
    /// `log_write_at(Utc::now(), ...)`. Most callers in variant-base use
    /// [`Self::log_write_at`] instead so the driver can capture the
    /// timestamp BEFORE the variant's `try_publish` call (T16.2) and
    /// prevent same-host loopback races where a peer's reader thread
    /// observes the bytes before the writer thread reaches this method.
    pub fn log_write(&mut self, seq: u64, path: &str, qos: Qos, bytes: usize) -> Result<()> {
        self.log_write_at(Utc::now(), seq, path, qos, bytes)
    }

    /// Log a `write` event with a caller-supplied timestamp.
    ///
    /// The driver captures `ts` immediately BEFORE calling
    /// `Variant::try_publish` (T16.2) so on same-host benchmarks the
    /// writer's `write_ts` is monotonically before any peer's reader
    /// thread can observe the bytes. See
    /// `metak-shared/api-contracts/jsonl-log-schema.md` for the
    /// contract.
    pub fn log_write_at(
        &mut self,
        ts: DateTime<Utc>,
        seq: u64,
        path: &str,
        qos: Qos,
        bytes: usize,
    ) -> Result<()> {
        let entry = json!({
            "ts": Self::format_ts(ts),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "write",
            "seq": seq,
            "path": path,
            "qos": qos.as_int(),
            "bytes": bytes,
        });
        self.write_line(&entry)
    }

    /// Log a `backpressure_skipped` event.
    ///
    /// Emitted when `Variant::try_publish` returns `Ok(false)` for a
    /// value the driver intended to write this tick. The value is NOT
    /// delivered and NOT retried within the same tick. See
    /// `metak-shared/api-contracts/jsonl-log-schema.md`.
    pub fn log_backpressure_skipped(&mut self, path: &str, qos: Qos) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "backpressure_skipped",
            "path": path,
            "qos": qos.as_int(),
        });
        self.write_line(&entry)
    }

    /// Log a `receive` event.
    pub fn log_receive(
        &mut self,
        writer: &str,
        seq: u64,
        path: &str,
        qos: Qos,
        bytes: usize,
    ) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "receive",
            "writer": writer,
            "seq": seq,
            "path": path,
            "qos": qos.as_int(),
            "bytes": bytes,
        });
        self.write_line(&entry)
    }

    /// Log a `gap_detected` event.
    pub fn log_gap_detected(&mut self, writer: &str, missing_seq: u64) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "gap_detected",
            "writer": writer,
            "missing_seq": missing_seq,
        });
        self.write_line(&entry)
    }

    /// Log a `gap_filled` event.
    pub fn log_gap_filled(&mut self, writer: &str, recovered_seq: u64) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "gap_filled",
            "writer": writer,
            "recovered_seq": recovered_seq,
        });
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

/// Thread-safe shared handle to a [`Logger`].
///
/// Wraps an `Arc<Mutex<Logger>>` so multiple threads can write events
/// concurrently. Designed for variants whose Multi-mode reader threads
/// need to emit `receive` events directly off the driver thread (T14.10).
///
/// The lock is held for the duration of a single `write_line` call --
/// microseconds for one JSONL line in the common case. Contention is
/// expected to be the new bottleneck cliff at extreme symmetric rates;
/// see `variants/websocket/CUSTOM.md` "Threading modes (T14.2 + T14.10)".
///
/// The handle does NOT take ownership of the underlying `Logger`; the
/// driver still owns the original instance and clones a `LoggerHandle`
/// into the variant via [`crate::variant_trait::Variant::attach_logger`].
/// All public methods are intentionally narrow -- only events that may
/// be emitted from a non-driver thread are exposed. Driver-only events
/// (phase, connected, write, eot_sent, ...) stay on the locked path
/// through the original `Logger` handle.
#[derive(Clone)]
pub struct LoggerHandle {
    inner: Arc<Mutex<Logger>>,
}

impl LoggerHandle {
    /// Wrap an owned `Logger` for cross-thread use. The driver retains
    /// its own clone of the `Arc` so the original can keep emitting
    /// driver-side events while reader threads use additional clones.
    pub fn new(logger: Logger) -> Self {
        Self {
            inner: Arc::new(Mutex::new(logger)),
        }
    }

    /// Borrow the inner `Arc<Mutex<Logger>>` -- the driver uses this to
    /// reach driver-only event methods (log_phase, log_write, etc.)
    /// without exposing them on the cross-thread handle surface.
    pub fn inner(&self) -> &Arc<Mutex<Logger>> {
        &self.inner
    }

    /// Emit a `receive` event from any thread.
    ///
    /// Acquires the shared mutex and writes one JSONL line; the lock is
    /// released before this returns. Errors are mapped through the
    /// `anyhow::Result` channel; callers in reader-thread paths typically
    /// log-and-continue on Err since dropping the variant during an
    /// in-flight write is the only realistic source of failure.
    pub fn log_receive(
        &self,
        writer: &str,
        seq: u64,
        path: &str,
        qos: Qos,
        bytes: usize,
    ) -> Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("LoggerHandle mutex poisoned"))?;
        guard.log_receive(writer, seq, path, qos, bytes)
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
    fn test_write_event() {
        let (mut logger, _dir) = create_test_logger();
        logger
            .log_write(42, "/sensors/lidar", Qos::BestEffort, 256)
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "write");
        assert_eq!(line["seq"], 42);
        assert_eq!(line["path"], "/sensors/lidar");
        assert_eq!(line["qos"], 1);
        assert_eq!(line["bytes"], 256);
    }

    #[test]
    fn test_log_write_at_emits_supplied_ts_unchanged() {
        // T16.2: the driver captures `write_ts` BEFORE calling
        // `try_publish` and passes it to `log_write_at`. The emitted
        // `ts` must exactly equal the supplied timestamp (rendered in
        // RFC 3339 with nanosecond precision), not whatever `Utc::now()`
        // returns at log time.
        let (mut logger, _dir) = create_test_logger();
        let supplied = chrono::DateTime::parse_from_rfc3339("2026-05-14T12:34:56.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        logger
            .log_write_at(supplied, 99, "/bench/7", Qos::ReliableTcp, 128)
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "write");
        assert_eq!(line["seq"], 99);
        assert_eq!(line["path"], "/bench/7");
        assert_eq!(line["qos"], 4);
        assert_eq!(line["bytes"], 128);
        assert_eq!(line["ts"], "2026-05-14T12:34:56.123456789Z");
    }

    #[test]
    fn test_log_write_delegates_to_log_write_at() {
        // `log_write` is now a thin wrapper that captures `Utc::now()`
        // and forwards to `log_write_at`. Sanity check that the
        // resulting line has the same shape (all the write fields
        // present) as the explicit-ts variant.
        let (mut logger, _dir) = create_test_logger();
        let before = Utc::now();
        logger.log_write(1, "/path", Qos::BestEffort, 16).unwrap();
        let after = Utc::now();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "write");
        let ts = chrono::DateTime::parse_from_rfc3339(line["ts"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            ts >= before && ts <= after,
            "log_write should capture a ts inside the call window: \
             before={before} ts={ts} after={after}"
        );
    }

    #[test]
    fn test_backpressure_skipped_event() {
        let (mut logger, _dir) = create_test_logger();
        logger
            .log_backpressure_skipped("/sensors/lidar", Qos::ReliableTcp)
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "backpressure_skipped");
        assert_eq!(line["path"], "/sensors/lidar");
        assert_eq!(line["qos"], 4);
        // No `seq` or `bytes` -- the skipped value never got a seq.
        assert!(line.get("seq").is_none());
        assert!(line.get("bytes").is_none());
        // Common fields.
        assert!(line.get("ts").is_some());
        assert_eq!(line["variant"], "test-variant");
        assert_eq!(line["runner"], "runner-a");
        assert_eq!(line["run"], "run01");
    }

    #[test]
    fn test_receive_event() {
        let (mut logger, _dir) = create_test_logger();
        logger
            .log_receive("runner-b", 7, "/bench/0", Qos::ReliableTcp, 128)
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "receive");
        assert_eq!(line["writer"], "runner-b");
        assert_eq!(line["seq"], 7);
        assert_eq!(line["path"], "/bench/0");
        assert_eq!(line["qos"], 4);
        assert_eq!(line["bytes"], 128);
    }

    #[test]
    fn test_gap_detected_event() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_gap_detected("runner-c", 99).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "gap_detected");
        assert_eq!(line["writer"], "runner-c");
        assert_eq!(line["missing_seq"], 99);
    }

    #[test]
    fn test_gap_filled_event() {
        let (mut logger, _dir) = create_test_logger();
        logger.log_gap_filled("runner-c", 99).unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "gap_filled");
        assert_eq!(line["writer"], "runner-c");
        assert_eq!(line["recovered_seq"], 99);
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
}
