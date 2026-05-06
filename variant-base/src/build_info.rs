//! Build identifier helpers for the variant binaries.
//!
//! Every binary in the workspace prints a one-line build banner on startup
//! so version skew between machines is immediately visible. The shared
//! workspace `build.rs` (`../build_info.rs`, referenced from each crate's
//! `Cargo.toml`) emits three compile-time env vars:
//!
//!   * `BUILD_GIT_SHA`   -- 7-char short SHA, or "unknown" when unavailable.
//!   * `BUILD_GIT_DIRTY` -- "true" if the working tree has uncommitted
//!                          changes at compile time, "false" otherwise.
//!   * `BUILD_RUSTC`     -- rustc version string.
//!
//! Binaries pass those values into [`format_banner`] (or [`print_banner`])
//! using the `env!` macro, so the resulting string is baked into the
//! binary at link time and cannot drift from what was actually compiled.
//!
//! Why this exists: two production incidents in one day were both caused
//! by a single sub-crate being rebuilt on machine B while another stayed
//! stale (pre-clock-sync runner; pre-EOT custom-udp). Printing the build
//! identifier on startup turns "stale binary" into a one-line visual diff.

/// Returns `true` when `dirty_str` is the literal string `"true"`.
///
/// Defined as a separate function so the env-var convention has exactly
/// one source of truth (the workspace `build.rs`) and the parsing logic
/// is unit-testable without a build-script run.
pub fn is_dirty(dirty_str: &str) -> bool {
    dirty_str == "true"
}

/// Format the standard one-line build banner.
///
/// Shape: `[<label>] build: <sha>[+dirty] (rustc <version>)`
///
/// `label` is whatever identifier the caller wants to use. The runner
/// prefixes its own name (`[runner:alice]`); each variant uses its
/// short variant name (`[custom-udp]`).
pub fn format_banner(label: &str, sha: &str, dirty: bool, rustc: &str) -> String {
    let dirty_suffix = if dirty { "+dirty" } else { "" };
    format!("[{label}] build: {sha}{dirty_suffix} (rustc {rustc})")
}

/// Print [`format_banner`] to stderr.
///
/// Stderr (not stdout) on purpose: stdout is reserved for the runner's
/// summary table and JSONL log files in some deployments. Variants and
/// the runner already write all diagnostic output (`[runner:...]
/// config loaded: ...` etc) to stderr.
pub fn print_banner(label: &str, sha: &str, dirty: bool, rustc: &str) {
    eprintln!("{}", format_banner(label, sha, dirty, rustc));
}

/// Convenience macro: print the standard build banner using the calling
/// crate's compile-time `BUILD_GIT_SHA` / `BUILD_GIT_DIRTY` / `BUILD_RUSTC`
/// env vars (set by the workspace `build.rs`).
///
/// `env!` is expanded at the **caller's** compile time, which is what we
/// want -- the banner reflects what *that binary* was compiled from, not
/// what `variant-base` was compiled from.
///
/// Usage in a variant's `main`:
///
/// ```ignore
/// fn main() {
///     variant_base::print_build_banner!("custom-udp");
///     // ... rest of startup ...
/// }
/// ```
#[macro_export]
macro_rules! print_build_banner {
    ($label:expr) => {{
        let dirty = $crate::build_info::is_dirty(env!("BUILD_GIT_DIRTY"));
        $crate::build_info::print_banner(
            $label,
            env!("BUILD_GIT_SHA"),
            dirty,
            env!("BUILD_RUSTC"),
        );
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_includes_dirty_marker_when_dirty() {
        let s = format_banner("variant", "abc1234", true, "1.83.0");
        assert_eq!(s, "[variant] build: abc1234+dirty (rustc 1.83.0)");
    }

    #[test]
    fn banner_omits_dirty_marker_when_clean() {
        let s = format_banner("custom-udp", "81ec8ab", false, "1.83.0");
        assert_eq!(s, "[custom-udp] build: 81ec8ab (rustc 1.83.0)");
    }

    #[test]
    fn is_dirty_matches_literal_true() {
        assert!(is_dirty("true"));
        assert!(!is_dirty("false"));
        assert!(!is_dirty(""));
        assert!(!is_dirty("True"));
    }

    #[test]
    fn banner_supports_runner_with_name_prefix() {
        // The runner uses `[runner:<name>]` as its label so logs are
        // attributable when both runners share a stdout/stderr stream.
        let s = format_banner("runner:alice", "81ec8ab", false, "1.83.0");
        assert_eq!(s, "[runner:alice] build: 81ec8ab (rustc 1.83.0)");
    }
}
