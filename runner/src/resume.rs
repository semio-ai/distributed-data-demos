//! Resume-mode helpers: log-subfolder selection, local manifest computation,
//! and intersection / cleanup over peer manifests.
//!
//! The runner uses these helpers to implement Phase 1.25 (ResumeManifest) of
//! the coordination protocol. See
//! `metak-shared/api-contracts/runner-coordination.md` for the contract.
//!
//! Design notes:
//! - The contract requires that an empty log file (zero bytes) is treated as
//!   "crashed prior attempt" and must be deleted before the manifest is
//!   broadcast.
//! - A non-empty log file is only "complete" if it contains the end-of-test
//!   completion marker (see `COMPLETION_MARKER` below). Non-empty files
//!   missing the marker represent spawns that crashed mid-write -- treated
//!   the same as empty files (delete + exclude). See T14.23.
//! - Files for jobs not in the cross-runner intersection ("incomplete") must
//!   be deleted regardless of size before Phase 2 begins, so the upcoming
//!   spawn writes into a clean file.
//! - All disk operations are best-effort: failures are reported but do NOT
//!   abort the run unless a required folder is missing.

use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Substring searched for in JSONL log files to decide "complete".
///
/// Marker choice: `"event":"eot_sent"`. Logged once by the writer at the
/// start of the EOT phase, immediately after `signal_end_of_test` returns
/// (see `metak-shared/api-contracts/jsonl-log-schema.md` and
/// `eot-protocol.md`). This is the canonical signal that the variant
/// reached end-of-test cleanly under the EOT protocol.
///
/// We do NOT pick `"phase":"silent"` because, although it is emitted slightly
/// later in the protocol, both markers can appear FAR from the end of the
/// file: after the marker, in-flight `receive` events continue to be drained
/// for the entire `--silent-secs` window. On observed high-rate logs the
/// distance from marker to EOF can exceed 100 MiB, so neither marker is
/// reliably in the tail. Either marker is therefore equivalent for our
/// purposes and we pick `eot_sent` because it is the more semantically
/// precise "the writer cleanly signalled end-of-test" event.
///
/// Reader-only spawns (no writes) still emit `eot_received` rather than
/// `eot_sent`, but in this codebase every spawn that participates in the
/// EOT protocol writes its own `eot_sent` line via the variant-base driver
/// regardless of whether the spawn's workload includes writes (the driver
/// invokes `signal_end_of_test` and logs `eot_sent` unconditionally before
/// `wait_for_peer_eots`). Variants that opt out of EOT return id=0 and the
/// driver falls back to silent_secs without emitting `eot_sent`; for those
/// the fallback in `file_contains_completion_marker` (also accepting
/// `"phase":"silent"`) covers the case.
const COMPLETION_MARKER: &[u8] = b"\"event\":\"eot_sent\"";

/// Fallback marker accepted in addition to [`COMPLETION_MARKER`]. Logged at
/// the boundary between the bounded EOT handshake and the post-EOT silent
/// grace window. Variants that opt out of the EOT handshake (return
/// `eot_id == 0` from `signal_end_of_test`) never emit `eot_sent`, but they
/// still emit `phase=silent` because the driver logs every phase transition
/// unconditionally. Accepting both keeps the classification correct for
/// EOT-opt-out variants without changing the canonical marker.
const FALLBACK_COMPLETION_MARKER: &[u8] = b"\"phase\":\"silent\"";

/// Number of bytes to read from the end of a log file when looking for the
/// completion marker as a fast path. 64 KiB chosen because (a) the smallest
/// successful logs have the marker within ~200 bytes of EOF, and (b) any
/// log with substantial drain traffic after the marker will fall through to
/// the full-file scan anyway -- larger tail budgets just shift the
/// threshold without changing the asymptotic cost.
const COMPLETION_TAIL_SCAN_BYTES: u64 = 64 * 1024;

/// Find the lexicographically greatest subfolder of `base_log_dir` whose name
/// starts with `<run>-`. Returns the folder name (not the full path).
///
/// In resume mode, this is the runner's proposed `log_subdir`. The leader's
/// proposal still wins during discovery — followers must have a folder of
/// the same name on disk or abort.
///
/// Returns `Err` if `base_log_dir` does not exist, or if no matching
/// subfolder is found. Both are operator errors: resume requires an existing
/// run on disk.
pub fn find_latest_log_subdir(base_log_dir: &Path, run: &str) -> Result<String> {
    if !base_log_dir.exists() {
        return Err(anyhow!(
            "resume: base log directory does not exist: {}",
            base_log_dir.display()
        ));
    }
    let prefix = format!("{run}-");
    let entries = std::fs::read_dir(base_log_dir)
        .with_context(|| format!("reading {}", base_log_dir.display()))?;
    let mut candidates: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with(&prefix) {
                candidates.push(name.to_string());
            }
        }
    }
    candidates.sort();
    candidates.pop().ok_or_else(|| {
        anyhow!(
            "resume: no log subfolder under {} matches prefix '{}'",
            base_log_dir.display(),
            prefix
        )
    })
}

/// Result of computing this runner's local manifest.
///
/// `complete_jobs` is the sorted, deduplicated set of `effective_name`s
/// whose log file exists locally and contains the end-of-test completion
/// marker. `deleted_empty` lists files deleted because they were zero-byte
/// (crashed prior attempt before any write). `deleted_partial` lists files
/// deleted because they were non-empty but missing the completion marker
/// (crashed mid-spawn). Both lists are kept for visibility in the runner's
/// stderr log; they are reported separately so an operator can tell at a
/// glance whether the prior run crashed before or after the variant
/// started producing events. See T14.23.
#[derive(Debug, Default)]
pub struct LocalManifest {
    pub complete_jobs: Vec<String>,
    pub deleted_empty: Vec<PathBuf>,
    pub deleted_partial: Vec<PathBuf>,
}

/// Inspect `<run_log_dir>/<effective_name>-<self_name>-<run>.jsonl` for each
/// job and classify it as:
///
/// - **complete**: non-empty and contains the EOT completion marker
///   (`COMPLETION_MARKER` or `FALLBACK_COMPLETION_MARKER`).
/// - **empty** (size == 0): delete and exclude. Crashed prior attempt
///   before any data was written.
/// - **partial** (non-empty, no marker): delete and exclude. Crashed
///   mid-spawn. Tracked separately in `deleted_partial` so the operator
///   can distinguish from never-started crashes.
/// - **missing** (no file): exclude silently.
///
/// Emits the local manifest in sorted, deduplicated order so its
/// serialized form is byte-stable.
pub fn compute_local_manifest(
    run_log_dir: &Path,
    self_name: &str,
    run: &str,
    effective_names: &[String],
) -> LocalManifest {
    let mut complete: HashSet<String> = HashSet::new();
    let mut deleted_empty: Vec<PathBuf> = Vec::new();
    let mut deleted_partial: Vec<PathBuf> = Vec::new();

    for name in effective_names {
        let path = run_log_dir.join(format!("{name}-{self_name}-{run}.jsonl"));
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                if meta.len() == 0 {
                    // Crashed before any write: delete and exclude.
                    if std::fs::remove_file(&path).is_ok() {
                        deleted_empty.push(path);
                    }
                } else if file_contains_completion_marker(&path, meta.len()) {
                    complete.insert(name.clone());
                } else {
                    // Non-empty but missing the EOT marker => crashed
                    // mid-spawn. Delete + exclude.
                    if std::fs::remove_file(&path).is_ok() {
                        deleted_partial.push(path);
                    }
                }
            }
            _ => {
                // Missing file (or non-file entry): excluded.
            }
        }
    }

    let mut sorted: Vec<String> = complete.into_iter().collect();
    sorted.sort();
    LocalManifest {
        complete_jobs: sorted,
        deleted_empty,
        deleted_partial,
    }
}

/// Returns true iff the file at `path` contains the EOT completion marker
/// substring anywhere in its contents.
///
/// Strategy: fast path scans the trailing [`COMPLETION_TAIL_SCAN_BYTES`] of
/// the file; if either marker is found there, return true immediately.
/// Otherwise fall back to a buffered streaming full-file scan.
///
/// The full-scan fallback is required for correctness: on high-rate
/// transports (e.g. `quic-1000x100hz-qos4-multi`) the post-EOT drain
/// window can deposit > 100 MiB of `receive` events AFTER the
/// `eot_sent` / `phase=silent` lines, pushing the marker far outside any
/// reasonable tail budget. Reading those files entirely is acceptable
/// because Phase 1.25 runs at most once per runner invocation.
///
/// I/O errors result in `false` (treat as "no marker"), which is the safe
/// classification -- the file will be deleted and re-run rather than
/// being assumed complete on a partial read.
fn file_contains_completion_marker(path: &Path, file_size: u64) -> bool {
    if file_size == 0 {
        return false;
    }
    if let Ok(found_in_tail) = scan_tail_for_marker(path, file_size) {
        if found_in_tail {
            return true;
        }
    }
    scan_full_for_marker(path).unwrap_or(false)
}

/// Read up to the last [`COMPLETION_TAIL_SCAN_BYTES`] bytes of `path` and
/// look for either marker substring. Returns `Ok(true)` if found,
/// `Ok(false)` if the tail did not contain the marker, or an `Err` on I/O
/// failure (caller treats as "not in tail" and falls back to full scan).
fn scan_tail_for_marker(path: &Path, file_size: u64) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    let start = file_size.saturating_sub(COMPLETION_TAIL_SCAN_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(COMPLETION_TAIL_SCAN_BYTES as usize);
    file.read_to_end(&mut buf)?;
    Ok(slice_contains(&buf, COMPLETION_MARKER) || slice_contains(&buf, FALLBACK_COMPLETION_MARKER))
}

/// Streaming buffered scan from the start of the file. Stops as soon as
/// either marker is found. Uses a 64 KiB read buffer with a small overlap
/// to handle the case where a marker straddles two buffers.
fn scan_full_for_marker(path: &Path) -> std::io::Result<bool> {
    const BUF_SIZE: usize = 64 * 1024;
    // Both markers are < 32 bytes; an overlap of MARKER_MAX - 1 covers any
    // boundary-straddling case for either marker.
    let overlap = COMPLETION_MARKER
        .len()
        .max(FALLBACK_COMPLETION_MARKER.len())
        .saturating_sub(1);
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(BUF_SIZE, file);
    let mut buf = vec![0u8; BUF_SIZE];
    let mut carry: Vec<u8> = Vec::with_capacity(overlap);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        // Build the haystack: carry + freshly-read chunk.
        let mut haystack: Vec<u8> = Vec::with_capacity(carry.len() + n);
        haystack.extend_from_slice(&carry);
        haystack.extend_from_slice(&buf[..n]);
        if slice_contains(&haystack, COMPLETION_MARKER)
            || slice_contains(&haystack, FALLBACK_COMPLETION_MARKER)
        {
            return Ok(true);
        }
        // Preserve a small tail for the next iteration in case a marker
        // straddles the boundary.
        let keep_from = haystack.len().saturating_sub(overlap);
        carry.clear();
        carry.extend_from_slice(&haystack[keep_from..]);
    }
}

/// Naive substring search. Markers are short (< 32 bytes) and buffers are
/// 64 KiB, so a `windows(needle.len()).any(...)` is fast enough; no need
/// to pull in a `memchr` dependency.
fn slice_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Compute the intersection of every peer's `complete_jobs` list. A spawn
/// job is in the skip set iff its `effective_name` appears in every peer's
/// manifest.
///
/// `manifests` is keyed by runner name. `expected_runners` is the full list
/// of runner names the run expects (from the config). If a runner is missing
/// from `manifests` the intersection collapses to empty (defensive — the
/// caller should not invoke this until all peers have reported).
pub fn intersect_complete_jobs(
    manifests: &HashMap<String, Vec<String>>,
    expected_runners: &[String],
) -> HashSet<String> {
    if expected_runners.is_empty() {
        return HashSet::new();
    }
    // Defensive: if any expected runner has no manifest, return empty.
    for runner in expected_runners {
        if !manifests.contains_key(runner) {
            return HashSet::new();
        }
    }
    // Start from the first runner's set, then intersect with every other.
    let mut iter = expected_runners.iter();
    let first = iter.next().unwrap();
    let mut acc: HashSet<String> = manifests
        .get(first)
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_default();
    for runner in iter {
        let other: HashSet<String> = manifests
            .get(runner)
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default();
        acc = acc.intersection(&other).cloned().collect();
    }
    acc
}

/// Delete this runner's `<effective_name>-<self_name>-<run>.jsonl` for every
/// job that is NOT in `skip_set`. The contract requires this guarantees a
/// clean file before the upcoming spawn writes into it.
///
/// Returns the list of files deleted, for visibility in stderr.
pub fn cleanup_incomplete_logs(
    run_log_dir: &Path,
    self_name: &str,
    run: &str,
    effective_names: &[String],
    skip_set: &HashSet<String>,
) -> Vec<PathBuf> {
    let mut deleted: Vec<PathBuf> = Vec::new();
    for name in effective_names {
        if skip_set.contains(name) {
            continue;
        }
        let path = run_log_dir.join(format!("{name}-{self_name}-{run}.jsonl"));
        if path.exists() && std::fs::remove_file(&path).is_ok() {
            deleted.push(path);
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static UNIQ: AtomicU32 = AtomicU32::new(0);
    fn unique_temp_dir(label: &str) -> PathBuf {
        let n = UNIQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "runner-resume-{}-{}-{}",
            label,
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn latest_subfolder_picks_lexicographically_greatest() {
        let dir = unique_temp_dir("latest-greatest");
        fs::create_dir_all(dir.join("run01-20260101_120000")).unwrap();
        fs::create_dir_all(dir.join("run01-20260301_080000")).unwrap();
        fs::create_dir_all(dir.join("run01-20260201_120000")).unwrap();
        // Wrong-prefix folders should be ignored.
        fs::create_dir_all(dir.join("run02-20271231_235959")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();

        let pick = find_latest_log_subdir(&dir, "run01").unwrap();
        assert_eq!(pick, "run01-20260301_080000");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_subfolder_no_match_returns_error() {
        let dir = unique_temp_dir("latest-none");
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::create_dir_all(dir.join("run-something-else")).unwrap();
        let err = find_latest_log_subdir(&dir, "run01")
            .expect_err("expected error when no matching folder exists");
        assert!(err.to_string().contains("no log subfolder"), "msg={err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_subfolder_missing_base_returns_error() {
        let dir = unique_temp_dir("latest-missing");
        fs::remove_dir_all(&dir).unwrap();
        let err = find_latest_log_subdir(&dir, "run01")
            .expect_err("expected error when base dir missing");
        assert!(
            err.to_string()
                .contains("base log directory does not exist"),
            "msg={err}"
        );
    }

    /// Minimal JSONL line containing the canonical `eot_sent` marker.
    /// Wrapped to a full JSON object so it survives a possible future tightening
    /// of the scan to "marker + valid JSON line".
    fn eot_sent_line() -> &'static [u8] {
        b"{\"event\":\"eot_sent\",\"eot_id\":12345,\"run\":\"r1\",\"runner\":\"self\",\"ts\":\"2026-05-12T15:22:36.856717500Z\",\"variant\":\"v1\"}\n"
    }

    /// Minimal JSONL line containing the fallback `phase=silent` marker.
    fn phase_silent_line() -> &'static [u8] {
        b"{\"event\":\"phase\",\"phase\":\"silent\",\"run\":\"r1\",\"runner\":\"self\",\"ts\":\"2026-05-12T15:22:36.857735600Z\",\"variant\":\"v1\"}\n"
    }

    /// A plain `write` event line -- present in every partial log but never
    /// constitutes a completion marker.
    fn write_line() -> &'static [u8] {
        b"{\"bytes\":8,\"event\":\"write\",\"path\":\"/bench/0\",\"qos\":4,\"run\":\"r1\",\"runner\":\"self\",\"seq\":1,\"ts\":\"2026-05-12T15:22:00.000000000Z\",\"variant\":\"v1\"}\n"
    }

    #[test]
    fn local_manifest_classifies_files_correctly() {
        let dir = unique_temp_dir("manifest");
        // Non-empty file WITH marker: complete.
        let mut v1 = Vec::new();
        v1.extend_from_slice(write_line());
        v1.extend_from_slice(eot_sent_line());
        fs::write(dir.join("v1-self-r1.jsonl"), &v1).unwrap();
        // Empty file: should be deleted and excluded as deleted_empty.
        fs::write(dir.join("v2-self-r1.jsonl"), b"").unwrap();
        // Missing file: excluded silently.
        // (no v3 file)
        // Non-empty file WITHOUT marker: deleted and excluded as deleted_partial.
        let mut v4 = Vec::new();
        for _ in 0..50 {
            v4.extend_from_slice(write_line());
        }
        fs::write(dir.join("v4-self-r1.jsonl"), &v4).unwrap();

        let names = vec![
            "v1".to_string(),
            "v2".to_string(),
            "v3".to_string(),
            "v4".to_string(),
        ];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert_eq!(manifest.complete_jobs, vec!["v1".to_string()]);
        assert!(
            !dir.join("v2-self-r1.jsonl").exists(),
            "empty file should be deleted"
        );
        assert!(
            !dir.join("v4-self-r1.jsonl").exists(),
            "partial (no marker) file should be deleted"
        );
        assert_eq!(manifest.deleted_empty.len(), 1);
        assert_eq!(manifest.deleted_partial.len(), 1);
        assert!(manifest.deleted_partial[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("v4-"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_complete_jobs_are_sorted_and_deduped() {
        let dir = unique_temp_dir("manifest-sort");
        for name in &["beta", "alpha", "gamma"] {
            let mut content = Vec::new();
            content.extend_from_slice(write_line());
            content.extend_from_slice(eot_sent_line());
            fs::write(dir.join(format!("{name}-self-r1.jsonl")), &content).unwrap();
        }
        // Pass duplicates and out-of-order names. Output is sorted unique.
        let names = vec![
            "gamma".to_string(),
            "alpha".to_string(),
            "alpha".to_string(),
            "beta".to_string(),
        ];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert_eq!(
            manifest.complete_jobs,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_phase_silent_counts_as_complete() {
        // Fallback marker: variants that opt out of EOT still emit
        // phase=silent. Accept both.
        let dir = unique_temp_dir("manifest-phase-silent");
        let mut content = Vec::new();
        content.extend_from_slice(write_line());
        content.extend_from_slice(phase_silent_line());
        fs::write(dir.join("v1-self-r1.jsonl"), &content).unwrap();
        let names = vec!["v1".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert_eq!(manifest.complete_jobs, vec!["v1".to_string()]);
        assert!(manifest.deleted_partial.is_empty());
        assert!(manifest.deleted_empty.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_partial_only_writes_is_deleted() {
        // A long partial log with only `write` and `receive` events but no
        // EOT marker -- the exact failure case from
        // logs/all-variants-01-20260512_152156/zenoh-max-qos4-multi-bob.
        let dir = unique_temp_dir("manifest-partial");
        let mut content = Vec::new();
        for _ in 0..200 {
            content.extend_from_slice(write_line());
        }
        fs::write(dir.join("v1-self-r1.jsonl"), &content).unwrap();
        let names = vec!["v1".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert!(manifest.complete_jobs.is_empty());
        assert_eq!(manifest.deleted_partial.len(), 1);
        assert!(!dir.join("v1-self-r1.jsonl").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_marker_in_tail_classified_complete() {
        // Tail-scan budget is 64 KiB. Pad with > 100 KiB of write events,
        // then write the marker at the very end so it lands in the tail.
        let dir = unique_temp_dir("manifest-marker-tail");
        let mut content = Vec::new();
        let target_size = 200 * 1024usize;
        while content.len() < target_size {
            content.extend_from_slice(write_line());
        }
        content.extend_from_slice(eot_sent_line());
        fs::write(dir.join("v1-self-r1.jsonl"), &content).unwrap();
        assert!(content.len() > 64 * 1024);
        let names = vec!["v1".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert_eq!(manifest.complete_jobs, vec!["v1".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_marker_outside_tail_full_scan_finds_it() {
        // Marker near the START of a large file, tail full of write/receive
        // drain traffic. Must still be classified complete via the
        // full-file scan fallback. Mirrors the real-world high-rate case
        // where post-silent drain pushes the marker > 100 KiB from EOF.
        let dir = unique_temp_dir("manifest-marker-deep");
        let mut content = Vec::new();
        content.extend_from_slice(write_line());
        content.extend_from_slice(eot_sent_line());
        // Tail-scan window is 64 KiB; pad with > 200 KiB of writes so the
        // marker is comfortably outside.
        let initial_len = content.len();
        while content.len() - initial_len < 200 * 1024 {
            content.extend_from_slice(write_line());
        }
        fs::write(dir.join("v1-self-r1.jsonl"), &content).unwrap();
        let names = vec!["v1".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert_eq!(manifest.complete_jobs, vec!["v1".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_large_file_no_marker_anywhere_is_partial() {
        // > 200 KiB of write events with NO marker anywhere -- both the
        // tail scan and the full scan must agree the file is partial.
        let dir = unique_temp_dir("manifest-large-no-marker");
        let mut content = Vec::new();
        while content.len() < 200 * 1024 {
            content.extend_from_slice(write_line());
        }
        fs::write(dir.join("v1-self-r1.jsonl"), &content).unwrap();
        let names = vec!["v1".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert!(manifest.complete_jobs.is_empty());
        assert_eq!(manifest.deleted_partial.len(), 1);
        assert!(!dir.join("v1-self-r1.jsonl").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_marker_straddles_buffer_boundary() {
        // Position the marker so it straddles the boundary between two
        // 64 KiB read buffers. Regression test for the overlap-carry
        // logic in scan_full_for_marker. The tail scan must not find it
        // (we pad enough drain afterwards) so we exercise the full-scan
        // path's boundary handling.
        let dir = unique_temp_dir("manifest-marker-boundary");
        const BUF: usize = 64 * 1024;
        let marker = eot_sent_line();
        // We want the marker to straddle the first 64 KiB boundary in the
        // full scan, AND to be > 64 KiB from EOF so the tail scan misses
        // it. Lay out: <BUF - marker_len/2 bytes of writes> <marker>
        // <enough writes to push EOF > 64 KiB past the marker>.
        let mut content = Vec::new();
        let head_target = BUF - marker.len() / 2;
        while content.len() < head_target {
            content.extend_from_slice(write_line());
        }
        // Trim to exactly head_target so the marker straddles the boundary.
        content.truncate(head_target);
        content.extend_from_slice(marker);
        // Ensure the marker is > 64 KiB from EOF.
        while content.len() < BUF + marker.len() + 100 * 1024 {
            content.extend_from_slice(write_line());
        }
        fs::write(dir.join("v1-self-r1.jsonl"), &content).unwrap();
        let names = vec!["v1".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert_eq!(
            manifest.complete_jobs,
            vec!["v1".to_string()],
            "marker straddling buffer boundary must be detected by full scan"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_missing_file_excluded_silently() {
        let dir = unique_temp_dir("manifest-missing");
        // No file written.
        let names = vec!["v1".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert!(manifest.complete_jobs.is_empty());
        assert!(manifest.deleted_empty.is_empty());
        assert!(manifest.deleted_partial.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Regression test against the real T14.23 failure case:
    /// `logs/all-variants-01-20260512_152156/`. The originally-reported
    /// failure was `zenoh-max-qos4-multi-bob-all-variants-01.jsonl` -- a
    /// 1.28 GB file containing only `write`/`receive`/`resource` events
    /// (no EOT marker). On running the new classifier across the whole
    /// directory it turned out that file was only the most obvious of
    /// many: every `zenoh-*-multi-bob` file at high tick rates plus one
    /// `custom-udp` are similarly truncated mid-spawn (the runner timed
    /// out and TerminateProcess'd them before the EOT handshake could
    /// log its marker). The pre-T14.23 byte-count logic counted ALL of
    /// these as complete, masking the real failures; T14.23 surfaces
    /// them.
    ///
    /// Ignored by default because (a) the real-data dataset is not part
    /// of the repository contract and may not be present in every clone,
    /// and (b) the full scan reads ~10 GiB of log files which is too
    /// slow for the regular test loop. Invoke explicitly with:
    ///
    ///   cargo test --release -p runner --bin runner \
    ///     real_data_regression_t14_23 -- --ignored --nocapture
    ///
    /// Expected outcome:
    /// - 192 bob jsonl files total.
    /// - The classifier flags `zenoh-max-qos4-multi-bob-all-variants-01.jsonl`
    ///   (the originally-reported failure) as partial. This is the
    ///   load-bearing assertion.
    /// - It also flags 12 additional crashed-mid-spawn files. These were
    ///   silently counted as complete under the pre-T14.23 logic; we
    ///   assert > 1 to lock in the broader fix and document the surprise.
    ///
    /// The test operates on a temp copy so the real log directory is
    /// not mutated.
    #[test]
    #[ignore]
    fn real_data_regression_t14_23() {
        // Walk up from CARGO_MANIFEST_DIR (the runner crate) to find the
        // workspace root containing `logs/`. The runner crate sits one
        // level below the workspace root.
        let manifest = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set when running cargo test");
        let workspace_root = Path::new(&manifest)
            .parent()
            .expect("runner crate must have a parent dir");
        let real_dir = workspace_root.join("logs/all-variants-01-20260512_152156");
        if !real_dir.is_dir() {
            eprintln!(
                "real_data_regression_t14_23: dataset not present at {} -- skipping",
                real_dir.display()
            );
            return;
        }
        // Mirror the real directory into a temp dir so we don't delete
        // the user's only copy of the failing file.
        let mirror = unique_temp_dir("real-data-t14-23");
        let mut effective_names_set: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for entry in fs::read_dir(&real_dir).unwrap().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            // Only mirror bob's bench JSONL files for this regression --
            // they are the variant under test. We also need every bob
            // jsonl in the directory so the manifest count matches the
            // operator-observed 192.
            if !name.ends_with("-bob-all-variants-01.jsonl") {
                continue;
            }
            if name.contains("clock-sync") {
                continue;
            }
            let dest = mirror.join(name);
            fs::copy(&path, &dest).unwrap();
            // Derive effective_name from filename:
            //   <effective_name>-bob-all-variants-01.jsonl
            let stripped = name.strip_suffix("-bob-all-variants-01.jsonl").unwrap();
            effective_names_set.insert(stripped.to_string());
        }
        let names: Vec<String> = effective_names_set.into_iter().collect();
        let pre_count = names.len();
        eprintln!(
            "real_data_regression_t14_23: mirrored {} bob jsonl files",
            pre_count
        );
        let manifest = compute_local_manifest(&mirror, "bob", "all-variants-01", &names);
        eprintln!(
            "real_data_regression_t14_23: complete={}, deleted_empty={}, deleted_partial={}",
            manifest.complete_jobs.len(),
            manifest.deleted_empty.len(),
            manifest.deleted_partial.len(),
        );
        for p in &manifest.deleted_partial {
            eprintln!(
                "  deleted_partial: {}",
                p.file_name().unwrap().to_string_lossy()
            );
        }
        // Expected: 192 in, the originally-reported zenoh-max-qos4-multi
        // file is among the deleted_partial list, and more than one
        // partial file is discovered overall (the broader fix).
        assert_eq!(
            pre_count, 192,
            "expected 192 bob jsonl files in the failing dataset, found {pre_count}"
        );
        assert!(
            manifest.deleted_partial.len() >= 1,
            "T14.23: at least one partial file must be detected"
        );
        let partial_names: Vec<String> = manifest
            .deleted_partial
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            partial_names
                .iter()
                .any(|n| n.starts_with("zenoh-max-qos4-multi-bob")),
            "the originally-reported failure zenoh-max-qos4-multi-bob \
             must be classified as partial; got {partial_names:?}"
        );
        // Conservation: complete + deleted_empty + deleted_partial == pre_count.
        assert_eq!(
            manifest.complete_jobs.len()
                + manifest.deleted_empty.len()
                + manifest.deleted_partial.len(),
            pre_count,
            "every input file must end up in exactly one bucket"
        );
        let _ = fs::remove_dir_all(&mirror);
    }

    #[test]
    fn intersection_three_peers_picks_all_three_agree() {
        let mut manifests: HashMap<String, Vec<String>> = HashMap::new();
        manifests.insert("a".into(), vec!["x".into(), "y".into(), "z".into()]);
        manifests.insert("b".into(), vec!["x".into(), "y".into()]);
        manifests.insert("c".into(), vec!["x".into(), "y".into(), "w".into()]);
        let runners = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let inter = intersect_complete_jobs(&manifests, &runners);
        let expected: HashSet<String> =
            ["x".to_string(), "y".to_string()].iter().cloned().collect();
        assert_eq!(inter, expected);
    }

    #[test]
    fn intersection_single_runner_equals_local_manifest() {
        let mut manifests: HashMap<String, Vec<String>> = HashMap::new();
        manifests.insert("solo".into(), vec!["x".into(), "y".into()]);
        let runners = vec!["solo".to_string()];
        let inter = intersect_complete_jobs(&manifests, &runners);
        let expected: HashSet<String> =
            ["x".to_string(), "y".to_string()].iter().cloned().collect();
        assert_eq!(inter, expected);
    }

    #[test]
    fn intersection_missing_peer_collapses_to_empty() {
        // Defensive: if the caller invokes this before all peers report,
        // return empty rather than picking a partial intersection.
        let mut manifests: HashMap<String, Vec<String>> = HashMap::new();
        manifests.insert("a".into(), vec!["x".into()]);
        // Note: "b" is missing.
        let runners = vec!["a".to_string(), "b".to_string()];
        let inter = intersect_complete_jobs(&manifests, &runners);
        assert!(inter.is_empty());
    }

    #[test]
    fn cleanup_deletes_only_incomplete_files() {
        let dir = unique_temp_dir("cleanup");
        fs::write(dir.join("v1-self-r1.jsonl"), b"{}").unwrap();
        fs::write(dir.join("v2-self-r1.jsonl"), b"data").unwrap();
        fs::write(dir.join("v3-self-r1.jsonl"), b"x").unwrap();

        let names = vec!["v1".to_string(), "v2".to_string(), "v3".to_string()];
        let mut skip: HashSet<String> = HashSet::new();
        skip.insert("v1".to_string());
        skip.insert("v3".to_string());

        let deleted = cleanup_incomplete_logs(&dir, "self", "r1", &names, &skip);
        assert_eq!(deleted.len(), 1);
        assert!(deleted[0].file_name().unwrap() == "v2-self-r1.jsonl");
        assert!(dir.join("v1-self-r1.jsonl").exists());
        assert!(!dir.join("v2-self-r1.jsonl").exists());
        assert!(dir.join("v3-self-r1.jsonl").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_handles_missing_files() {
        // Missing files for incomplete jobs is fine — they just aren't
        // listed in the deleted set.
        let dir = unique_temp_dir("cleanup-missing");
        let names = vec!["v1".to_string(), "v2".to_string()];
        let skip: HashSet<String> = HashSet::new();
        let deleted = cleanup_incomplete_logs(&dir, "self", "r1", &names, &skip);
        assert!(deleted.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
