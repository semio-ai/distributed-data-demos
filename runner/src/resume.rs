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
//! - Files for jobs not in the cross-runner intersection ("incomplete") must
//!   be deleted regardless of size before Phase 2 begins, so the upcoming
//!   spawn writes into a clean file.
//! - All disk operations are best-effort: failures are reported but do NOT
//!   abort the run unless a required folder is missing.

use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
/// whose log file exists locally and is non-empty. `deleted_empty` lists
/// the files that were deleted because they were zero-byte (crashed prior
/// attempt) — kept for visibility in the runner's stderr log.
#[derive(Debug, Default)]
pub struct LocalManifest {
    pub complete_jobs: Vec<String>,
    pub deleted_empty: Vec<PathBuf>,
}

/// Inspect `<run_log_dir>/<effective_name>-<self_name>-<run>.jsonl` for each
/// job and classify it as complete (non-empty), empty (delete and exclude),
/// or missing (exclude). Emits the local manifest in sorted, deduplicated
/// order so its serialized form is byte-stable.
pub fn compute_local_manifest(
    run_log_dir: &Path,
    self_name: &str,
    run: &str,
    effective_names: &[String],
) -> LocalManifest {
    let mut complete: HashSet<String> = HashSet::new();
    let mut deleted_empty: Vec<PathBuf> = Vec::new();

    for name in effective_names {
        let path = run_log_dir.join(format!("{name}-{self_name}-{run}.jsonl"));
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                if meta.len() == 0 {
                    // Crashed prior attempt: delete and exclude.
                    if std::fs::remove_file(&path).is_ok() {
                        deleted_empty.push(path);
                    }
                } else {
                    complete.insert(name.clone());
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
    }
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

    #[test]
    fn local_manifest_classifies_files_correctly() {
        let dir = unique_temp_dir("manifest");
        // Non-empty file: complete.
        fs::write(dir.join("v1-self-r1.jsonl"), b"{}").unwrap();
        // Empty file: should be deleted and excluded.
        fs::write(dir.join("v2-self-r1.jsonl"), b"").unwrap();
        // Missing file: excluded silently.
        // (no v3 file)

        let names = vec!["v1".to_string(), "v2".to_string(), "v3".to_string()];
        let manifest = compute_local_manifest(&dir, "self", "r1", &names);
        assert_eq!(manifest.complete_jobs, vec!["v1".to_string()]);
        assert!(
            !dir.join("v2-self-r1.jsonl").exists(),
            "empty file should be deleted"
        );
        assert_eq!(manifest.deleted_empty.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_manifest_complete_jobs_are_sorted_and_deduped() {
        let dir = unique_temp_dir("manifest-sort");
        for name in &["beta", "alpha", "gamma"] {
            fs::write(dir.join(format!("{name}-self-r1.jsonl")), b"{}").unwrap();
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
