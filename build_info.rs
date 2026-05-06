//! Shared build script used by every binary crate in the workspace.
//!
//! Records the current git short SHA, dirty flag, and rustc version into
//! compile-time environment variables. Each binary's `build` field in its
//! `Cargo.toml` references this file via a relative path.
//!
//! Why a single shared script: the workspace-target convention means every
//! binary lands in `target/release/` together. We want every binary to print
//! the *same* build identifier on startup so a stale binary on a secondary
//! machine is immediately visible (see the post-mortem of the runner stale-
//! binary incident in `metak-orchestrator/STATUS.md`).
//!
//! Variables emitted:
//!   BUILD_GIT_SHA   -- 7-char short SHA, or "unknown" if git is unavailable
//!   BUILD_GIT_DIRTY -- "true" if the working tree has uncommitted changes
//!   BUILD_RUSTC     -- rustc version reported by `$RUSTC --version`

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let rustc = std::env::var("RUSTC")
        .ok()
        .and_then(|p| Command::new(p).arg("--version").output().ok())
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            // "rustc 1.83.0 (90b35a623 2024-11-26)" -> "1.83.0"
            s.split_whitespace().nth(1).unwrap_or("unknown").to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=BUILD_GIT_DIRTY={}", if dirty { "true" } else { "false" });
    println!("cargo:rustc-env=BUILD_RUSTC={rustc}");

    // Re-run when HEAD moves or the index changes (covers commits, switches,
    // and stages). Probing untracked files is intentionally skipped above so
    // editor scratch files do not flip the dirty flag.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
