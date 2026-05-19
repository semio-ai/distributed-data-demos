use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::compact::CompactBuffers;
use crate::types::{Phase, Qos, ThreadingMode};
use crate::workload::WriteShape;

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
    ///
    /// Back-compat wrapper: defaults `leaf_count = 1, shape = Scalar`
    /// per the E19 contract for legacy callers.
    pub fn log_write(&mut self, seq: u64, path: &str, qos: Qos, bytes: usize) -> Result<()> {
        self.log_write_at(Utc::now(), seq, path, qos, bytes, 1, WriteShape::Scalar)
    }

    /// Log a `write` event with a caller-supplied timestamp.
    ///
    /// The driver captures `ts` immediately BEFORE calling
    /// `Variant::try_publish` (T16.2) so on same-host benchmarks the
    /// writer's `write_ts` is monotonically before any peer's reader
    /// thread can observe the bytes. See
    /// `metak-shared/api-contracts/jsonl-log-schema.md` for the
    /// contract.
    ///
    /// `leaf_count` and `shape` (E19 / T19.2) record the workload-shape
    /// metadata documented in the JSONL schema's E19 additions. Both
    /// are emitted on every `write` event (no legacy-omitting branch);
    /// the analyzer's pre-E19 backfill defaults match the values
    /// scalar-flood / max-throughput emit (`leaf_count = 1, shape =
    /// "scalar"`) so old and new logs share the same schema surface.
    #[allow(clippy::too_many_arguments)]
    pub fn log_write_at(
        &mut self,
        ts: DateTime<Utc>,
        seq: u64,
        path: &str,
        qos: Qos,
        bytes: usize,
        leaf_count: u32,
        shape: WriteShape,
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
            "leaf_count": leaf_count,
            "shape": shape.as_str(),
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
///
/// ## T18.3a: compact-buffer attachment
///
/// In the T18.2 compact-default world every per-event row must land in
/// the per-spawn `CompactBuffers` regardless of which thread emits it.
/// The driver constructs a shared [`CompactSink`] (`Arc<Mutex<CompactBuffers>>`)
/// alongside the logger, wires both into the handle via
/// [`LoggerHandle::attach_compact_sink`], and shares the same `Arc`
/// with its own `EventSink`. Reader threads then call
/// [`LoggerHandle::record_receive`] which mirrors what the driver's
/// `EventSink::record_receive` does on the driver thread -- one push
/// into the compact buffer and (gated on `legacy_jsonl_events`) one
/// JSONL line.
///
/// The compact-sink attachment is optional so existing unit tests that
/// construct a `LoggerHandle` directly via [`LoggerHandle::new`] keep
/// working without modification. When the sink is `None`,
/// `record_receive` falls back to the legacy `log_receive` behaviour
/// (JSONL only) -- equivalent to a pre-T18.2 setup.
#[derive(Clone)]
pub struct LoggerHandle {
    inner: Arc<Mutex<Logger>>,
    /// Shared compact-buffer sink, populated by
    /// [`LoggerHandle::attach_compact_sink`]. `None` for handles built
    /// by tests or for pre-T18.2 callers that never set up a compact
    /// sink.
    compact: Option<CompactSink>,
    /// Whether the legacy JSONL `receive` line should also be written.
    /// Mirrors the driver's `--legacy-jsonl-events` flag so reader-
    /// thread emissions stay consistent with driver-thread emissions
    /// under both T18.2 defaults (`false`) and the legacy-compatible
    /// opt-in (`true`).
    legacy_jsonl: bool,
}

impl LoggerHandle {
    /// Wrap an owned `Logger` for cross-thread use. The driver retains
    /// its own clone of the `Arc` so the original can keep emitting
    /// driver-side events while reader threads use additional clones.
    ///
    /// The compact-sink attachment defaults to `None` and `legacy_jsonl`
    /// to `true`. Callers that want T18.3a compact-buffer mirroring
    /// invoke [`Self::attach_compact_sink`] before sharing the handle
    /// across threads.
    pub fn new(logger: Logger) -> Self {
        Self {
            inner: Arc::new(Mutex::new(logger)),
            compact: None,
            legacy_jsonl: true,
        }
    }

    /// Borrow the inner `Arc<Mutex<Logger>>` -- the driver uses this to
    /// reach driver-only event methods (log_phase, log_write, etc.)
    /// without exposing them on the cross-thread handle surface.
    pub fn inner(&self) -> &Arc<Mutex<Logger>> {
        &self.inner
    }

    /// Wire a shared [`CompactSink`] into this handle (T18.3a).
    ///
    /// The driver calls this on the handle BEFORE cloning it into
    /// variants via `Variant::attach_logger`, passing the same
    /// `Arc<Mutex<CompactBuffers>>` it shares with its own `EventSink`
    /// and the `legacy_jsonl` flag from `CliArgs::legacy_jsonl_events`.
    /// Every reader thread that clones this handle therefore writes
    /// into the same column buffers the digest phase serialises.
    ///
    /// Idempotent: calling twice replaces the previously-wired sink.
    /// Tests typically skip this step and rely on the `None` fallback
    /// (`record_receive` then writes JSONL only).
    pub fn attach_compact_sink(&mut self, sink: CompactSink, legacy_jsonl: bool) {
        self.compact = Some(sink);
        self.legacy_jsonl = legacy_jsonl;
    }

    /// Inspect the attached compact sink, if any (test helper).
    #[doc(hidden)]
    pub fn compact_sink(&self) -> Option<&CompactSink> {
        self.compact.as_ref()
    }

    /// Whether the legacy JSONL `receive` line is enabled for
    /// [`Self::record_receive`].
    #[doc(hidden)]
    pub fn legacy_jsonl_enabled(&self) -> bool {
        self.legacy_jsonl
    }

    /// Emit a `receive` event from any thread (legacy JSONL only).
    ///
    /// Acquires the shared mutex and writes one JSONL line; the lock is
    /// released before this returns. Errors are mapped through the
    /// `anyhow::Result` channel; callers in reader-thread paths typically
    /// log-and-continue on Err since dropping the variant during an
    /// in-flight write is the only realistic source of failure.
    ///
    /// **Prefer [`Self::record_receive`]** in new code: under the T18.2
    /// compact-default writer, `log_receive` writes only the legacy
    /// JSONL line and the receive will be missing from
    /// `*.compact.parquet`. This method remains for back-compat and
    /// for tests that explicitly want the legacy-only behaviour.
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

    /// Record a `receive` event into the shared compact buffer and
    /// (when enabled) the legacy JSONL stream (T18.3a).
    ///
    /// This is the cross-thread analogue of the driver's
    /// `EventSink::record_receive` -- the public method websocket
    /// reader threads (and the T17.5 Single-mode drain helper) call
    /// instead of `log_receive` so receives never bypass the compact
    /// `EventBuffer` that the digest phase serialises.
    ///
    /// **Mutex behaviour**: this acquires the compact-sink mutex
    /// briefly, performs the push, releases it, then acquires the
    /// logger mutex briefly for the optional JSONL line. The two
    /// mutexes are distinct, so concurrent reader threads may have one
    /// holding the compact lock while another holds the logger lock --
    /// this is intentional, not a contention regression. The T14.10
    /// design's "one mutex acquisition per receive" property is no
    /// longer load-bearing: the compact push is microsecond-scale (one
    /// `Vec::push` per column plus an intern-table lookup) and the
    /// JSONL line is microsecond-scale too. Empirically the cliff that
    /// motivated T14.10 (100 K msg/s symmetric on the WS reader) moves
    /// only slightly with the extra lock; the legacy-JSONL-OFF mode
    /// (the T18.2 default) drops the logger lock entirely from this
    /// path, so the effective cost is one lock per receive under the
    /// default flag.
    ///
    /// When no compact sink is attached the row is silently dropped
    /// and the call degenerates to `log_receive` (gated on
    /// `legacy_jsonl`). This keeps unit-test callsites that construct
    /// a bare `LoggerHandle::new(...)` working unmodified.
    pub fn record_receive(
        &self,
        writer: &str,
        seq: u64,
        path: &str,
        qos: Qos,
        bytes: usize,
    ) -> Result<()> {
        // Capture `ts` once so the compact row and the JSONL line
        // share the same timestamp -- analysis code can then
        // cross-correlate the two streams on `(ts, seq, writer)`
        // exactly.
        let ts = Utc::now();
        if let Some(sink) = &self.compact {
            let ts_ns = ts.timestamp_nanos_opt().unwrap_or(0);
            let mut buf = sink
                .lock()
                .map_err(|_| anyhow::anyhow!("LoggerHandle compact-sink mutex poisoned"))?;
            buf.push_receive(ts_ns, writer, seq, path, qos.as_int(), bytes as u32)?;
        }
        if self.legacy_jsonl {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("LoggerHandle mutex poisoned"))?;
            // Use the JSONL `log_receive` shape so the on-disk
            // timestamp string format is unchanged. We pay an extra
            // `Utc::now()` here vs. `ts` above; on the legacy-JSONL
            // path that small drift is harmless (the analyzer keys on
            // (seq, writer) for correlation, not on the textual ts).
            guard.log_receive(writer, seq, path, qos, bytes)?;
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
        // E19: legacy `log_write` defaults to scalar shape.
        assert_eq!(line["leaf_count"], 1);
        assert_eq!(line["shape"], "scalar");
    }

    #[test]
    fn test_write_event_records_e19_leaf_count_and_shape() {
        // E19: block-flood / mixed-types callers emit non-scalar
        // shapes. Both fields must round-trip on the JSONL line.
        let (mut logger, _dir) = create_test_logger();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-05-19T00:00:00.000000000Z")
            .unwrap()
            .with_timezone(&Utc);
        logger
            .log_write_at(
                ts,
                1,
                "/bench/block/0",
                Qos::BestEffort,
                800,
                100,
                WriteShape::Array,
            )
            .unwrap();
        logger
            .log_write_at(
                ts,
                2,
                "/bench/mixed/dict/flat",
                Qos::BestEffort,
                336,
                42,
                WriteShape::Struct,
            )
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        assert_eq!(lines[0]["leaf_count"], 100);
        assert_eq!(lines[0]["shape"], "array");
        assert_eq!(lines[1]["leaf_count"], 42);
        assert_eq!(lines[1]["shape"], "struct");
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
            .log_write_at(
                supplied,
                99,
                "/bench/7",
                Qos::ReliableTcp,
                128,
                1,
                WriteShape::Scalar,
            )
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

    // ------ T18.3a: LoggerHandle::record_receive tests ------

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

    fn read_jsonl_at(path: &Path) -> Vec<serde_json::Value> {
        let file = File::open(path).unwrap();
        let reader = std::io::BufReader::new(file);
        reader
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn t18_3a_record_receive_without_compact_sink_writes_jsonl_only() {
        // Backwards-compat path: a `LoggerHandle` constructed without
        // `attach_compact_sink` (e.g. legacy unit tests) keeps writing
        // the JSONL line and silently skips the compact push.
        let dir = TempDir::new().unwrap();
        let handle = handle_with_logger(&dir);
        let log_path = handle.inner().lock().unwrap().path().to_path_buf();

        handle
            .record_receive("alice", 42, "/bench/0", Qos::ReliableTcp, 64)
            .unwrap();
        handle.inner().lock().unwrap().flush().unwrap();

        let lines = read_jsonl_at(&log_path);
        assert_eq!(lines.len(), 1, "exactly one JSONL line");
        assert_eq!(lines[0]["event"], "receive");
        assert_eq!(lines[0]["writer"], "alice");
        assert_eq!(lines[0]["seq"], 42);
        assert_eq!(lines[0]["path"], "/bench/0");
        assert_eq!(lines[0]["qos"], 4);
        assert_eq!(lines[0]["bytes"], 64);
        // No compact sink attached -> compact_sink() is None.
        assert!(handle.compact_sink().is_none());
    }

    #[test]
    fn t18_3a_record_receive_pushes_into_compact_buffer() {
        // Core T18.3a invariant: with a compact sink attached, the
        // receive row lands in the shared `CompactBuffers` so the
        // digest phase later serialises it to Parquet.
        let dir = TempDir::new().unwrap();
        let mut handle = handle_with_logger(&dir);
        let sink: CompactSink = Arc::new(Mutex::new(CompactBuffers::new()));
        // Default `legacy_jsonl = true` matches the legacy code path
        // exactly; the T18.2-default `false` is tested below.
        handle.attach_compact_sink(sink.clone(), true);

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
    fn t18_3a_record_receive_emits_jsonl_when_legacy_flag_on() {
        let dir = TempDir::new().unwrap();
        let mut handle = handle_with_logger(&dir);
        let sink: CompactSink = Arc::new(Mutex::new(CompactBuffers::new()));
        handle.attach_compact_sink(sink.clone(), true);
        let log_path = handle.inner().lock().unwrap().path().to_path_buf();

        handle
            .record_receive("alice", 1, "/bench/0", Qos::ReliableTcp, 16)
            .unwrap();
        handle.inner().lock().unwrap().flush().unwrap();

        // Compact row pushed.
        assert_eq!(sink.lock().unwrap().len(), 1);
        // JSONL line written.
        let lines = read_jsonl_at(&log_path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["event"], "receive");
        assert_eq!(lines[0]["writer"], "alice");
        assert_eq!(lines[0]["seq"], 1);
    }

    #[test]
    fn t18_3a_record_receive_skips_jsonl_when_legacy_flag_off() {
        // T18.2 default: legacy_jsonl=false. The compact row is
        // pushed (the digest writer is the analyser's only source)
        // and NO JSONL line lands.
        let dir = TempDir::new().unwrap();
        let mut handle = handle_with_logger(&dir);
        let sink: CompactSink = Arc::new(Mutex::new(CompactBuffers::new()));
        handle.attach_compact_sink(sink.clone(), false);
        let log_path = handle.inner().lock().unwrap().path().to_path_buf();

        handle
            .record_receive("alice", 5, "/bench/0", Qos::ReliableTcp, 16)
            .unwrap();
        handle.inner().lock().unwrap().flush().unwrap();

        assert_eq!(
            sink.lock().unwrap().len(),
            1,
            "compact row pushed under legacy_jsonl=false"
        );
        let lines = read_jsonl_at(&log_path);
        assert!(
            lines.is_empty(),
            "no JSONL line under legacy_jsonl=false, got {lines:?}"
        );
    }

    #[test]
    fn t18_3a_record_receive_clone_shares_compact_sink() {
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
    fn t18_3a_record_receive_concurrent_pushes_all_land() {
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
