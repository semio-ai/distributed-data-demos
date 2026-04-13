use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use serde_json::json;

use crate::types::{Phase, Qos};

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
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
    }

    /// Write a JSON line to the log file.
    fn write_line(&mut self, value: &serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.writer, value)?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    /// Log a `connected` event.
    pub fn log_connected(&mut self, launch_ts: &str, elapsed_ms: f64) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "event": "connected",
            "launch_ts": launch_ts,
            "elapsed_ms": elapsed_ms,
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
    pub fn log_write(&mut self, seq: u64, path: &str, qos: Qos, bytes: usize) -> Result<()> {
        let entry = json!({
            "ts": Self::now_ts(),
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
            .log_connected("2026-04-12T14:00:00.000000000Z", 123.456)
            .unwrap();
        logger.flush().unwrap();

        let lines = read_lines(&logger);
        let line = &lines[0];
        assert_eq!(line["event"], "connected");
        assert_eq!(line["launch_ts"], "2026-04-12T14:00:00.000000000Z");
        assert_eq!(line["elapsed_ms"], 123.456);
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
