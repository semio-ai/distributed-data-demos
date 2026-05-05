//! JSONL writer for clock-sync events.
//!
//! Each runner produces a single file `<runner>-clock-sync-<run>.jsonl` in
//! the variant-log subfolder. One JSONL line per (peer, measurement_event).
//! Schema is the `clock_sync` event in
//! `metak-shared/api-contracts/jsonl-log-schema.md`.
//!
//! `peer`, `offset_ms`, and `rtt_ms` are required columnar fields consumed by
//! analysis. `samples`, `min_rtt_ms`, `max_rtt_ms`, and `outlier_rejected`
//! are diagnostic and kept here for human inspection only.
//!
//! A sibling file `<runner>-clock-sync-debug-<run>.jsonl` is also produced.
//! It contains one JSONL line per raw `(t1,t2,t3,t4)` sample and is the
//! primary tool for diagnosing rare clock-sync anomalies (T8.4). It is NOT
//! consumed by analysis; analysis globs only the canonical
//! `*-clock-sync-<run>.jsonl` files.

use crate::clock_sync::{format_ts, OffsetMeasurement, RawSample};
use anyhow::{Context, Result};
use chrono::Utc;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Buffered append-only writer for one runner's clock-sync log.
///
/// The file is opened on first call and held open for the whole run. Each
/// `write` flushes the line so a crash mid-run still leaves a usable file.
///
/// Holds two files: the canonical `<runner>-clock-sync-<run>.jsonl` consumed
/// by analysis, and a sibling `<runner>-clock-sync-debug-<run>.jsonl` with
/// one line per raw sample for post-mortem inspection.
pub struct ClockSyncLogger {
    /// Path of the underlying canonical JSONL file. Currently only consumed
    /// by tests via `path()`; production code holds the writer and never
    /// re-opens it.
    #[allow(dead_code)]
    path: PathBuf,
    /// Path of the sibling per-sample debug JSONL file. Only consumed by
    /// tests via `debug_path()`.
    #[allow(dead_code)]
    debug_path: PathBuf,
    file: File,
    debug_file: File,
    runner: String,
    run: String,
}

impl ClockSyncLogger {
    /// Path of the underlying canonical JSONL file. Mostly useful for tests.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path of the sibling per-sample debug JSONL file. Mostly useful for
    /// tests.
    #[allow(dead_code)]
    pub fn debug_path(&self) -> &Path {
        &self.debug_path
    }
}

/// Open (or create+append to) the clock-sync log file for this runner.
///
/// Two files are created:
/// - `<runner>-clock-sync-<run>.jsonl` — canonical event log consumed by
///   analysis (one line per peer-measurement summary).
/// - `<runner>-clock-sync-debug-<run>.jsonl` — sibling debug log with one
///   line per raw sample. Not consumed by analysis; used to diagnose rare
///   clock-sync anomalies post-mortem.
///
/// The directory must already exist; the runner's main loop creates the
/// per-run subfolder before this is called.
pub fn open_clock_sync_log(log_dir: &Path, runner: &str, run: &str) -> Result<ClockSyncLogger> {
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("creating clock-sync log dir {}", log_dir.display()))?;
    let file_name = format!("{runner}-clock-sync-{run}.jsonl");
    let path = log_dir.join(file_name);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening clock-sync log {}", path.display()))?;
    let debug_name = format!("{runner}-clock-sync-debug-{run}.jsonl");
    let debug_path = log_dir.join(debug_name);
    let debug_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&debug_path)
        .with_context(|| format!("opening clock-sync debug log {}", debug_path.display()))?;
    Ok(ClockSyncLogger {
        path,
        debug_path,
        file,
        debug_file,
        runner: runner.to_string(),
        run: run.to_string(),
    })
}

impl ClockSyncLogger {
    /// Append one `clock_sync` event for a peer measurement. Sets `ts` to
    /// `Utc::now()` at write time. `variant` is `""` for the initial sync
    /// and the variant name for per-variant resyncs.
    ///
    /// Writes the canonical summary line to the primary log if `m` is
    /// `Some`. Always writes one debug line per attempt to the sibling
    /// debug log (T8.5: previously the debug file only got rows on
    /// success, leaving nothing to inspect when a probe storm timed out).
    /// `attempts` may include rows whose `result != ProbeResult::Ok`,
    /// signalling timeouts, filter rejections, or parse errors.
    pub fn write(
        &mut self,
        variant: &str,
        peer: &str,
        m: Option<&OffsetMeasurement>,
        attempts: &[RawSample],
    ) -> Result<()> {
        let ts = format_ts(Utc::now());

        // Canonical summary line: only emitted when we actually have a
        // measurement. Analysis tolerates missing entries (per the contract,
        // it falls back to "no correction" and flags the cross-runner pair
        // as uncorrected).
        if let Some(m) = m {
            let line = serde_json::json!({
                "ts": ts,
                "variant": variant,
                "runner": self.runner,
                "run": self.run,
                "event": "clock_sync",
                // Required columnar fields.
                "peer": peer,
                "offset_ms": m.offset_ms,
                "rtt_ms": m.rtt_ms,
                // Diagnostic-only fields. Analysis ignores these.
                "samples": m.samples,
                "min_rtt_ms": m.min_rtt_ms,
                "max_rtt_ms": m.max_rtt_ms,
                "outlier_rejected": m.outlier_rejected,
            });
            let s = serde_json::to_string(&line)?;
            writeln!(self.file, "{s}")?;
            self.file.flush()?;
        }

        // Per-sample debug trace. One line per probe ATTEMPT (T8.5),
        // including timeouts and filter rejections so an empty cohort still
        // leaves diagnostic rows.
        let outlier_rejected = m.is_some_and(|m| m.outlier_rejected);
        for (i, raw) in attempts.iter().enumerate() {
            // serde_json refuses to serialize NaN. For non-Ok rows we leave
            // the numeric fields out; the `result` field tells the reader
            // why no measurement was produced.
            let mut dline = serde_json::json!({
                "ts": ts,
                "variant": variant,
                "runner": self.runner,
                "run": self.run,
                "event": "clock_sync_sample",
                "peer": peer,
                "sample_index": i,
                "t1_ns": raw.t1_ns,
                "t2_ns": raw.t2_ns,
                "t3_ns": raw.t3_ns,
                "t4_ns": raw.t4_ns,
                "accepted": raw.accepted,
                "outlier_rejected": outlier_rejected,
                "result": raw.result.as_str(),
            });
            if raw.offset_ms.is_finite() {
                dline["offset_ms"] = serde_json::json!(raw.offset_ms);
            }
            if raw.rtt_ms.is_finite() {
                dline["rtt_ms"] = serde_json::json!(raw.rtt_ms);
            }
            let ds = serde_json::to_string(&dline)?;
            writeln!(self.debug_file, "{ds}")?;
        }
        self.debug_file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock_sync::ProbeResult;

    fn ok_sample(t1_ns: i64, t2_ns: i64, t3_ns: i64, t4_ns: i64, accepted: bool) -> RawSample {
        let off = ((t2_ns - t1_ns) + (t3_ns - t4_ns)) as f64 / 2.0 / 1_000_000.0;
        let rtt = ((t4_ns - t1_ns) - (t3_ns - t2_ns)) as f64 / 1_000_000.0;
        RawSample {
            t1_ns,
            t2_ns,
            t3_ns,
            t4_ns,
            offset_ms: off,
            rtt_ms: rtt,
            accepted,
            result: ProbeResult::Ok,
        }
    }

    fn timeout_sample(t1_ns: i64) -> RawSample {
        RawSample {
            t1_ns,
            t2_ns: 0,
            t3_ns: 0,
            t4_ns: 0,
            offset_ms: f64::NAN,
            rtt_ms: f64::NAN,
            accepted: false,
            result: ProbeResult::Timeout,
        }
    }

    #[test]
    fn writes_valid_jsonl_with_required_and_diagnostic_fields() {
        let dir =
            std::env::temp_dir().join(format!("runner-clock-sync-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut log = open_clock_sync_log(&dir, "alice", "run01").unwrap();
        let m = OffsetMeasurement {
            offset_ms: 1.234,
            rtt_ms: 0.5,
            samples: 32,
            min_rtt_ms: 0.4,
            max_rtt_ms: 12.3,
            raw_samples: vec![],
            outlier_rejected: false,
        };
        log.write("", "bob", Some(&m), &[]).unwrap();
        log.write("zenoh-replication", "bob", Some(&m), &[])
            .unwrap();

        let path = dir.join("alice-clock-sync-run01.jsonl");
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "clock_sync");
        assert_eq!(first["runner"], "alice");
        assert_eq!(first["run"], "run01");
        assert_eq!(first["variant"], "");
        assert_eq!(first["peer"], "bob");
        assert!((first["offset_ms"].as_f64().unwrap() - 1.234).abs() < 1e-9);
        assert!((first["rtt_ms"].as_f64().unwrap() - 0.5).abs() < 1e-9);
        // Diagnostic fields present.
        assert_eq!(first["samples"].as_i64().unwrap(), 32);
        assert!((first["min_rtt_ms"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        assert!((first["max_rtt_ms"].as_f64().unwrap() - 12.3).abs() < 1e-9);
        assert_eq!(first["outlier_rejected"], false);
        assert!(first["ts"].as_str().unwrap().ends_with('Z'));

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["variant"], "zenoh-replication");

        // Debug log was created (no attempts -> zero per-sample lines).
        let dpath = dir.join("alice-clock-sync-debug-run01.jsonl");
        assert!(dpath.exists(), "debug log must exist alongside canonical");
        let dcontents = std::fs::read_to_string(&dpath).unwrap();
        assert!(dcontents.lines().count() == 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_log_contains_one_line_per_raw_sample() {
        let dir = std::env::temp_dir().join(format!(
            "runner-clock-sync-debug-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut log = open_clock_sync_log(&dir, "alice", "run01").unwrap();
        let m = OffsetMeasurement {
            offset_ms: 0.5,
            rtt_ms: 0.3,
            samples: 3,
            min_rtt_ms: 0.3,
            max_rtt_ms: 0.5,
            raw_samples: vec![],
            outlier_rejected: false,
        };
        let attempts = vec![
            ok_sample(100, 200, 250, 400, false),
            ok_sample(1000, 1150, 1200, 1300, true),
            ok_sample(2000, 2200, 2250, 2500, false),
        ];
        log.write("v1", "bob", Some(&m), &attempts).unwrap();

        let dpath = dir.join("alice-clock-sync-debug-run01.jsonl");
        let dcontents = std::fs::read_to_string(&dpath).unwrap();
        let dlines: Vec<_> = dcontents.lines().collect();
        assert_eq!(dlines.len(), 3, "expected 3 sample lines");

        let first: serde_json::Value = serde_json::from_str(dlines[0]).unwrap();
        assert_eq!(first["event"], "clock_sync_sample");
        assert_eq!(first["sample_index"], 0);
        assert_eq!(first["t1_ns"], 100);
        assert_eq!(first["accepted"], false);
        assert_eq!(first["result"], "ok");
        let second: serde_json::Value = serde_json::from_str(dlines[1]).unwrap();
        assert_eq!(second["sample_index"], 1);
        assert_eq!(second["accepted"], true);
        assert_eq!(second["result"], "ok");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_log_records_timeout_attempts_when_no_measurement() {
        // T8.5 hardening: a totally-failed cohort must still write one debug
        // row per attempt with `result="timeout"`. Previously the file was
        // 0-byte in this case, leaving operators with no signal.
        let dir = std::env::temp_dir().join(format!(
            "runner-clock-sync-timeout-rows-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut log = open_clock_sync_log(&dir, "alice", "run01").unwrap();
        let attempts = vec![
            timeout_sample(1000),
            timeout_sample(2000),
            timeout_sample(3000),
        ];
        // Note: measurement is None.
        log.write("", "bob", None, &attempts).unwrap();

        // Canonical file must be empty -- we have no measurement to record.
        let path = dir.join("alice-clock-sync-run01.jsonl");
        let canonical = std::fs::read_to_string(&path).unwrap();
        assert_eq!(canonical.lines().count(), 0);

        // Debug file must contain one row per attempt with result=timeout.
        let dpath = dir.join("alice-clock-sync-debug-run01.jsonl");
        let dcontents = std::fs::read_to_string(&dpath).unwrap();
        let dlines: Vec<_> = dcontents.lines().collect();
        assert_eq!(dlines.len(), 3, "one debug row per attempt");
        for (i, line) in dlines.iter().enumerate() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(row["event"], "clock_sync_sample");
            assert_eq!(row["sample_index"], i as i64);
            assert_eq!(row["result"], "timeout");
            assert_eq!(row["accepted"], false);
            assert_eq!(row["t2_ns"], 0);
            // NaN-derived fields are omitted, not serialized.
            assert!(
                row.get("offset_ms").is_none(),
                "offset_ms must be absent for timeout rows"
            );
            assert!(
                row.get("rtt_ms").is_none(),
                "rtt_ms must be absent for timeout rows"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appends_to_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "runner-clock-sync-log-append-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let m = OffsetMeasurement {
            offset_ms: 0.0,
            rtt_ms: 1.0,
            samples: 1,
            min_rtt_ms: 1.0,
            max_rtt_ms: 1.0,
            raw_samples: vec![],
            outlier_rejected: false,
        };

        {
            let mut log = open_clock_sync_log(&dir, "r1", "run1").unwrap();
            log.write("", "r2", Some(&m), &[]).unwrap();
        }
        {
            let mut log = open_clock_sync_log(&dir, "r1", "run1").unwrap();
            log.write("v1", "r2", Some(&m), &[]).unwrap();
        }

        let path = dir.join("r1-clock-sync-run1.jsonl");
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().count(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_includes_runner_and_run() {
        let dir =
            std::env::temp_dir().join(format!("runner-clock-sync-log-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = open_clock_sync_log(&dir, "myname", "myrun").unwrap();
        let p = log.path();
        let fname = p.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(fname, "myname-clock-sync-myrun.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
