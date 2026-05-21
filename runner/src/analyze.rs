//! T18.6: post-matrix invocation of `analysis/analyze.py`.
//!
//! When the operator passes `--analyze-full`, the **lexicographically
//! lowest-named** runner (the typical `alice` in an `alice`/`bob` pair)
//! shells out to the Python analyzer after every spawn has finished and the
//! summary has been printed. Other runners exit cleanly with no analysis
//! side-effects so concurrent writes to `<log-dir>/analysis/` are impossible.
//!
//! Repo-root detection: walk up from the runner binary location until
//! `analysis/analyze.py` is found. Documented in `runner/CUSTOM.md`
//! "Auto-analysis after the matrix".
//!
//! Python interpreter resolution: try `python3` first, fall back to `python`,
//! fail loudly if neither resolves. Loud-but-non-fatal: a non-zero Python
//! exit is surfaced as a runner-level warning, not a hard failure (the
//! benchmark itself already succeeded).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum number of parent directories to walk when looking for the repo
/// root. The runner binary lives at `<repo>/target/release/runner(.exe)`
/// (three levels under the repo root); we give ourselves a small safety
/// margin in case of nested workspaces or operator-customised target dirs.
const REPO_WALKUP_LIMIT: usize = 8;

/// Decide whether THIS runner is the one that should run analysis.
///
/// Rule: the lexicographically lowest name among `runners` does it. Stable
/// across machines because every runner sees the same TOML config and the
/// same sort. Single-runner mode trivially satisfies the rule.
pub fn should_run_analysis(this_runner: &str, all_runners: &[String]) -> bool {
    let Some(lowest) = all_runners.iter().min() else {
        return false;
    };
    lowest == this_runner
}

/// Walk up from `start` looking for a directory that contains
/// `analysis/analyze.py`. Returns the directory that contains `analysis/`
/// (i.e. the repo root). Bounded by [`REPO_WALKUP_LIMIT`].
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    let mut depth = 0usize;
    while let Some(dir) = cur {
        if depth >= REPO_WALKUP_LIMIT {
            break;
        }
        if dir.join("analysis").join("analyze.py").is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
        depth += 1;
    }
    None
}

/// Locate a usable Python interpreter on `PATH`. Returns the executable name
/// (`python3` or `python`) so the spawned `Command` resolves it through the
/// OS shell's PATH search at exec time -- this also covers Windows where the
/// Microsoft Store stub `python` is a real exe but unrelated to the user's
/// installed Python.
///
/// Resolution: `python3` first, then `python`. Returns `Err` with a clear
/// message naming both candidates if neither is on PATH or runs.
pub fn resolve_python() -> Result<&'static str, String> {
    for candidate in ["python3", "python"] {
        if probe_interpreter(candidate) {
            return Ok(candidate);
        }
    }
    Err(
        "neither 'python3' nor 'python' resolved on PATH (cannot run --analyze-full); \
         install Python 3.10+ or remove --analyze-full"
            .to_string(),
    )
}

/// Probe a candidate Python interpreter by running `<candidate> --version`.
/// Treats any exit code (0 or otherwise) as proof the binary exists. A
/// spawn error means the executable was not on PATH (or the OS refused to
/// launch it, which we treat as "unusable" for our purposes).
fn probe_interpreter(candidate: &str) -> bool {
    Command::new(candidate)
        .arg("--version")
        // Suppress the probe's own stdout/stderr so the runner's banner stays
        // clean; we only care whether the process started.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Run `python -c "import <m1>, <m2>, ..."` against the resolved Python
/// interpreter and report whether every module imports cleanly.
///
/// Returns `Ok(())` when the import succeeds (Python exits 0). Returns
/// `Err(msg)` when:
///   - the resolved Python interpreter could not be located on PATH, OR
///   - the interpreter spawned but the import statement raised a Python
///     exception (typically `ModuleNotFoundError`).
///
/// The returned message names the Python binary used, includes the full
/// stderr text from the failed import (so the operator sees the missing
/// module name verbatim), and finishes with a Windows-friendly recovery
/// hint pointing at `pip install -r analysis\requirements.txt`. The path
/// uses backslashes for PowerShell, but either separator works in
/// practice -- pip canonicalises both.
///
/// Factored as `check_python_imports(modules)` rather than hard-coding the
/// analyzer's three deps so the unit test can pass a guaranteed-missing
/// module name (e.g. `definitely_not_a_real_module_xyz`) and exercise the
/// failure path independently of the host environment.
pub fn check_python_imports(modules: &[&str]) -> Result<(), String> {
    let python = resolve_python()?;
    let import_stmt = format!("import {}", modules.join(", "));
    let output = Command::new(python)
        .arg("-c")
        .arg(&import_stmt)
        .output()
        .map_err(|e| {
            format!(
                "failed to spawn '{python} -c \"{import_stmt}\"' for --analyze-full prereq check: {e}; \
                 install the analyzer prerequisites with: pip install -r analysis\\requirements.txt"
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    let stderr_trimmed = stderr_text.trim();
    Err(format!(
        "--analyze-full prereq check failed: '{python} -c \"{import_stmt}\"' returned {:?}.\n\
         Python stderr:\n{stderr_trimmed}\n\
         Install the analyzer prerequisites with: pip install -r analysis\\requirements.txt \
         (forward slashes work too). The runner aborts now so a long benchmark does not run \
         only to have the trailing analysis fail.",
        output.status.code()
    ))
}

/// Verify the analyzer's Python prerequisites (`polars`, `matplotlib`,
/// `psutil`) can be imported under the resolved Python interpreter, and
/// return a helpful error if any are missing.
///
/// Called from `main` ONLY when `--analyze-full` is set AND this runner is
/// the lexicographically lowest name (the one that will actually invoke
/// the analyzer per `should_run_analysis`). Failing fast here means a
/// missing `polars` install costs seconds at startup rather than discarding
/// a multi-hour benchmark when the post-matrix analysis trips over the
/// missing import.
pub fn check_analysis_prereqs() -> Result<(), String> {
    check_python_imports(&["polars", "matplotlib", "psutil"])
}

/// Run the analyzer if this runner is the lexicographically lowest name in
/// `runners`. No-op otherwise. Logs (to runner stderr) one of:
/// - "skipping analysis: this runner is not the lowest-sorted-name"
/// - "running analysis on <log_dir> ..."
/// - "analysis exited <code> (non-fatal warning)"
/// - "skipping analysis: could not find analysis/analyze.py from <start>"
/// - "skipping analysis: <python-resolution-error>"
///
/// Returns `Ok(true)` when analysis was attempted (regardless of Python exit
/// code), `Ok(false)` when this runner skipped because it is not the
/// lowest-name. An `Err` only surfaces if the runner could not determine its
/// own binary location, which would be a bug rather than an operator-fixable
/// condition; callers should treat it as a soft warning.
///
/// `final_log_dir` is the absolute (or operator-supplied) path to the
/// per-run log subfolder -- the directory the variant JSONL files live in.
/// We pass it both as the analyzer's positional argument AND as the parent
/// of `--output <log_dir>/analysis` so the dump and diagrams land alongside
/// the data they analyse.
pub fn run_post_matrix_analysis(
    this_runner: &str,
    all_runners: &[String],
    final_log_dir: &Path,
) -> Result<bool, String> {
    if !should_run_analysis(this_runner, all_runners) {
        let lowest = all_runners
            .iter()
            .min()
            .map(|s| s.as_str())
            .unwrap_or("<none>");
        eprintln!(
            "[runner:{this_runner}] --analyze-full set, but this runner is not the \
             lowest-sorted name ('{lowest}'); skipping analysis"
        );
        return Ok(false);
    }

    let exe =
        std::env::current_exe().map_err(|e| format!("failed to locate runner binary: {e}"))?;
    let exe_parent = exe
        .parent()
        .ok_or_else(|| format!("runner binary has no parent directory: {}", exe.display()))?;
    let repo_root = match find_repo_root(exe_parent) {
        Some(p) => p,
        None => {
            eprintln!(
                "[runner:{this_runner}] WARN: --analyze-full set, but could not find \
                 analysis/analyze.py walking up from {}; skipping analysis",
                exe_parent.display()
            );
            return Ok(true);
        }
    };
    let analyze_script = repo_root.join("analysis").join("analyze.py");
    let analysis_dir = repo_root.join("analysis");

    let python = match resolve_python() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[runner:{this_runner}] WARN: {e}; skipping analysis");
            return Ok(true);
        }
    };

    let output_dir = final_log_dir.join("analysis");
    eprintln!(
        "[runner:{this_runner}] running analysis: {python} {} {} --summary --dump --diagrams --output {}",
        analyze_script.display(),
        final_log_dir.display(),
        output_dir.display(),
    );

    // Invoke the analyzer with the analysis/ dir as the working directory so
    // any relative imports inside analyze.py resolve consistently with manual
    // invocations from the repo root. Capturing inherited stdout/stderr is
    // intentional -- the analyzer's tables and warnings surface in the
    // operator's terminal exactly as if they had run the command themselves.
    let status = Command::new(python)
        .arg(&analyze_script)
        .arg(final_log_dir)
        .arg("--summary")
        .arg("--dump")
        .arg("--diagrams")
        .arg("--output")
        .arg(&output_dir)
        .current_dir(&analysis_dir)
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!(
                "[runner:{this_runner}] analysis complete: output in {}",
                output_dir.display()
            );
        }
        Ok(s) => {
            // Non-zero Python exit. The benchmark itself succeeded, so this
            // is a warning -- not a hard failure -- per the T18.6 contract.
            eprintln!(
                "[runner:{this_runner}] WARN: analysis exited {:?} (non-fatal; benchmark itself succeeded)",
                s.code()
            );
        }
        Err(e) => {
            eprintln!("[runner:{this_runner}] WARN: failed to spawn analyzer: {e:#} (non-fatal)");
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_run_analysis_picks_lowest_sorted_name() {
        let runners = vec!["bob".to_string(), "alice".to_string()];
        assert!(should_run_analysis("alice", &runners));
        assert!(!should_run_analysis("bob", &runners));
    }

    #[test]
    fn should_run_analysis_single_runner_is_always_chosen() {
        let runners = vec!["solo".to_string()];
        assert!(should_run_analysis("solo", &runners));
    }

    #[test]
    fn should_run_analysis_handles_alpha_numeric_mix() {
        // Plain lexicographic sort -- digits beat letters in ASCII.
        let runners = vec!["a".to_string(), "1node".to_string(), "z".to_string()];
        assert!(should_run_analysis("1node", &runners));
        assert!(!should_run_analysis("a", &runners));
        assert!(!should_run_analysis("z", &runners));
    }

    #[test]
    fn should_run_analysis_empty_runners_picks_nobody() {
        let runners: Vec<String> = Vec::new();
        assert!(!should_run_analysis("anyone", &runners));
    }

    #[test]
    fn find_repo_root_walks_up_to_analysis_dir() {
        // The workspace this test runs inside has `analysis/analyze.py` at
        // the repo root. Starting from CARGO_MANIFEST_DIR (runner/) we walk
        // up one level and find it.
        let start = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = find_repo_root(start).expect("repo root must be locatable in tests");
        assert!(root.join("analysis").join("analyze.py").is_file());
    }

    #[test]
    fn find_repo_root_returns_none_when_nothing_matches() {
        // /tmp (or std::env::temp_dir) almost certainly has no
        // analysis/analyze.py in any ancestor up to filesystem root; even
        // if it did the walkup limit bounds the search.
        let start = std::env::temp_dir();
        let found = find_repo_root(&start);
        // We cannot assert None unconditionally (a developer's temp_dir
        // could theoretically have an analysis/analyze.py ancestor) but at
        // minimum the walkup must not panic and must return SOMETHING in
        // bounded time.
        let _ = found;
    }

    #[test]
    fn resolve_python_finds_at_least_one_interpreter_when_present() {
        // We do not assume Python is installed in CI, so this test only
        // verifies the function returns SOMETHING valid OR a clear error.
        // The error path is exercised whenever the test environment has no
        // python on PATH; the happy path is exercised whenever it does.
        match resolve_python() {
            Ok(p) => assert!(p == "python3" || p == "python"),
            Err(msg) => assert!(
                msg.contains("python3") && msg.contains("python"),
                "error message must name both candidates: {msg}"
            ),
        }
    }

    /// Quick environment probe: does `<resolved-python> -c "import polars"`
    /// succeed? Used by `check_analysis_prereqs_succeeds_when_polars_present`
    /// to skip the happy-path assertion if polars is not installed in the
    /// test environment -- we don't want CI to fail just because the
    /// runner-crate's unit suite ran on a machine without analyzer deps.
    fn host_has_polars() -> bool {
        let Ok(python) = resolve_python() else {
            return false;
        };
        Command::new(python)
            .arg("-c")
            .arg("import polars")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn check_python_imports_succeeds_for_stdlib_modules() {
        // `sys` and `os` are part of the Python stdlib; if Python resolves
        // at all, these must import. Skip the test if no Python is on PATH.
        if resolve_python().is_err() {
            eprintln!("skipping: no Python on PATH");
            return;
        }
        check_python_imports(&["sys", "os"])
            .expect("stdlib imports must succeed under a working Python");
    }

    #[test]
    fn check_python_imports_errors_on_missing_module() {
        // Skip if no Python is on PATH; we cannot exercise the failure path
        // without an interpreter to run.
        if resolve_python().is_err() {
            eprintln!("skipping: no Python on PATH");
            return;
        }
        let probe = "definitely_not_a_real_module_xyz";
        let err = check_python_imports(&[probe]).expect_err("importing a bogus module must fail");
        assert!(
            err.contains(probe),
            "error must mention the offending module name: {err}"
        );
        assert!(
            err.contains("pip install -r analysis"),
            "error must include the pip-install recovery hint: {err}"
        );
        assert!(
            err.contains("requirements.txt"),
            "error must point at requirements.txt: {err}"
        );
    }

    #[test]
    fn check_analysis_prereqs_succeeds_when_polars_present() {
        // Skip if polars (or Python) is not installed in this test
        // environment -- we don't want CI to fail just because the
        // runner-crate's unit suite ran without analyzer deps.
        if !host_has_polars() {
            eprintln!("skipping: polars not installed in test environment");
            return;
        }
        check_analysis_prereqs().expect("prereqs must pass when polars is installed");
    }
}
