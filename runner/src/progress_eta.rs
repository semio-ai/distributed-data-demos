//! Per-spawn progress + ETA helpers (T-ux.1).
//!
//! After each spawn finishes, the runner prints a single stderr line that
//! tells the operator where the run is in the matrix and roughly how much
//! wall-clock remains:
//!
//! ```text
//! [runner:<name>] progress: <i>/<total> done | elapsed <H>h <M>m <S>s | ETA ~<H>h <M>m <S>s
//! ```
//!
//! The ETA is hybrid: a config-derived sum of remaining phase durations
//! plus a measured per-spawn overhead correction. The overhead correction
//! is saturating at zero so an under-budget early stretch never produces
//! a negative ETA.
//!
//! ASCII only (no Unicode glyphs) so the line survives the legacy Windows
//! console without garbling. The exact line shape is part of the
//! diagnostic contract — see `runner/CUSTOM.md` "Per-spawn progress + ETA
//! line (T-ux.1)". Tests in this module pin the format breakpoints.
//!
//! This module deliberately contains NO I/O — `eprintln!` is the caller's
//! responsibility. Everything here is pure functions of `Duration`,
//! `VariantConfig`, and accumulator state, which keeps the estimator math
//! unit-testable without spinning up a runner process.

use std::time::Duration;

use crate::config::VariantConfig;

/// Read an optional non-negative integer key from `[variant.common]`. Used
/// for the three phase-duration keys (`stabilize_secs`, `operate_secs`,
/// `silent_secs`). Returns `None` if the key is absent, present but not an
/// integer, or present but negative.
fn read_optional_secs(variant: &VariantConfig, key: &str) -> Option<u64> {
    variant
        .common
        .get(key)
        .and_then(|v| v.as_integer())
        .filter(|n| *n >= 0)
        .map(|n| n as u64)
}

/// Compute the nominal wall-clock cost of a single spawn (in seconds),
/// derived from the source `VariantConfig`:
///
/// ```text
/// stabilize_secs + operate_secs + silent_secs + inter_spawn_grace_secs
/// ```
///
/// `inter_spawn_grace_ms` is the run-level `inter_qos_grace_ms` (default
/// 250 ms; configurable via the top-level TOML key). The grace component
/// is added to every spawn — even the first one inside an entry — because
/// the goal is a per-spawn budget, not a per-pair-boundary one. Over-
/// counting by one grace period across the whole matrix is harmless
/// noise next to the overhead-correction term.
///
/// **Fallback**: if any of the three phase-duration keys is missing on
/// the variant entry (legacy configs that pre-date E5 / E14), return the
/// variant's `timeout_secs` instead — a safe over-estimate beats `NaN`.
pub fn spawn_nominal_duration(variant: &VariantConfig, inter_spawn_grace_ms: u64) -> Duration {
    let stabilize = read_optional_secs(variant, "stabilize_secs");
    let operate = read_optional_secs(variant, "operate_secs");
    let silent = read_optional_secs(variant, "silent_secs");

    match (stabilize, operate, silent) {
        (Some(stab), Some(op), Some(sil)) => {
            let phases_ms = stab
                .saturating_add(op)
                .saturating_add(sil)
                .saturating_mul(1000);
            let total_ms = phases_ms.saturating_add(inter_spawn_grace_ms);
            Duration::from_millis(total_ms)
        }
        _ => {
            // Fallback: use the variant's effective timeout. The runner does
            // not know the run-level `default_timeout_secs` here, so we use
            // whatever the entry declared on its own; the caller already
            // guarantees template resolution has merged in any inherited
            // value. If the entry still has no explicit timeout, fall back
            // to a tiny conservative value (1s) — better than overflowing
            // the unwrap_or arithmetic chain.
            let timeout = variant.timeout_secs.unwrap_or(1);
            Duration::from_secs(timeout)
        }
    }
}

/// Format a `Duration` in compact `<H>h <M>m <S>s` shape with three width
/// breakpoints, ASCII only:
///
/// - `>= 1h`  -> `1h 02m 17s`
/// - `>= 1m`  -> `12m 09s`
/// - `< 1m`   -> `47s`
/// - zero     -> `0s`
///
/// Seconds round down (`as_secs`); sub-second remainder is dropped. The
/// shape is deliberately narrow so it survives 80-column terminals when
/// appended to a `[runner:<name>] progress: i/total done | elapsed ...`
/// line.
pub fn format_hms(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Hybrid ETA estimator: config-derived remaining cost plus a measured
/// per-spawn overhead correction.
///
/// Inputs:
/// - `elapsed`: wall-clock seconds since the spawn loop started (i.e. since
///   the top of Phase 2, **excluding** discovery and initial clock-sync).
/// - `nominal_so_far`: sum of `spawn_nominal_duration` for jobs `1..=i`,
///   including skipped jobs (those contribute 0).
/// - `nominal_remaining`: sum of `spawn_nominal_duration` for jobs `i+1..=total`.
/// - `completed`: the 1-based cursor `i` (number of spawns finished,
///   counting skipped).
/// - `total`: the matrix size `all_jobs.len()`.
///
/// Returns `None` when `completed == total` (the run is finished — no ETA
/// to emit) OR when `completed == 0` (defensive; the caller never invokes
/// the estimator with zero spawns done).
///
/// Otherwise:
///
/// ```text
/// overhead_per_spawn = max(0, elapsed - nominal_so_far) / completed
/// eta = nominal_remaining + overhead_per_spawn * (total - completed)
/// ```
///
/// Saturating: an under-budget early stretch (`elapsed < nominal_so_far`)
/// is treated as zero overhead per spawn, NOT as negative drift. The
/// nominal-remaining term keeps the ETA from collapsing to zero in that
/// case.
pub fn estimate_eta(
    elapsed: Duration,
    nominal_so_far: Duration,
    nominal_remaining: Duration,
    completed: usize,
    total: usize,
) -> Option<Duration> {
    if completed == 0 || completed >= total {
        return None;
    }
    // Saturating: an under-budget early stretch -> zero overhead per spawn.
    let overhead_total = elapsed.saturating_sub(nominal_so_far);
    let overhead_per_spawn = overhead_total / completed as u32;
    let remaining = (total - completed) as u32;
    Some(nominal_remaining + overhead_per_spawn * remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BenchConfig;

    // ---------- format_hms ----------

    #[test]
    fn format_hms_zero() {
        assert_eq!(format_hms(Duration::ZERO), "0s");
    }

    #[test]
    fn format_hms_seconds_only() {
        assert_eq!(format_hms(Duration::from_secs(47)), "47s");
    }

    #[test]
    fn format_hms_minutes_and_seconds() {
        assert_eq!(format_hms(Duration::from_secs(12 * 60 + 9)), "12m 09s");
    }

    #[test]
    fn format_hms_hours_minutes_seconds() {
        assert_eq!(
            format_hms(Duration::from_secs(3600 + 2 * 60 + 17)),
            "1h 02m 17s"
        );
    }

    #[test]
    fn format_hms_rounds_down_sub_second_remainder() {
        // 47.9s -> 47s (we deliberately drop sub-second precision because
        // the line is operator-facing and 100 ms matters for nothing here).
        assert_eq!(format_hms(Duration::from_millis(47_900)), "47s");
    }

    #[test]
    fn format_hms_pads_zero_seconds() {
        // 1 minute exactly -> "1m 00s" (NOT "1m 0s").
        assert_eq!(format_hms(Duration::from_secs(60)), "1m 00s");
    }

    #[test]
    fn format_hms_pads_zero_minutes_in_hours() {
        // 1h exactly -> "1h 00m 00s".
        assert_eq!(format_hms(Duration::from_secs(3600)), "1h 00m 00s");
    }

    // ---------- spawn_nominal_duration ----------

    fn parse_config(toml_str: &str) -> BenchConfig {
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        cfg
    }

    #[test]
    fn nominal_duration_sums_all_four_contributions() {
        // stabilize=2, operate=10, silent=3, grace=250ms => 15.25s.
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 60
inter_qos_grace_ms = 250

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = 1
  stabilize_secs = 2
  operate_secs = 10
  silent_secs = 3
"#,
        );
        let got = spawn_nominal_duration(&cfg.variant[0], cfg.inter_qos_grace_ms());
        assert_eq!(got, Duration::from_millis(15_250));
    }

    #[test]
    fn nominal_duration_uses_default_grace_when_unset() {
        // grace defaults to 250 ms when the top-level key is omitted.
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = 1
  stabilize_secs = 1
  operate_secs = 1
  silent_secs = 1
"#,
        );
        let got = spawn_nominal_duration(&cfg.variant[0], cfg.inter_qos_grace_ms());
        // 3s phases + 250ms grace.
        assert_eq!(got, Duration::from_millis(3_250));
    }

    #[test]
    fn nominal_duration_falls_back_to_timeout_when_any_phase_missing() {
        // operate_secs is missing -> fall back to timeout_secs.
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "v"
binary = "./x"
timeout_secs = 42
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = 1
  stabilize_secs = 2
  silent_secs = 3
"#,
        );
        let got = spawn_nominal_duration(&cfg.variant[0], cfg.inter_qos_grace_ms());
        assert_eq!(got, Duration::from_secs(42));
    }

    #[test]
    fn nominal_duration_falls_back_when_all_phase_keys_missing() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "v"
binary = "./x"
timeout_secs = 30
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = 1
"#,
        );
        let got = spawn_nominal_duration(&cfg.variant[0], cfg.inter_qos_grace_ms());
        assert_eq!(got, Duration::from_secs(30));
    }

    // ---------- estimate_eta ----------

    #[test]
    fn estimate_eta_deterministic_overhead_case() {
        // TASKS.md pinned case: 4 spawns with nominal [30, 30, 30, 30].
        // After spawn 1 (i=1, completed=1), elapsed = 70s.
        // nominal_so_far = 30s, nominal_remaining = 90s.
        // overhead_total = 70 - 30 = 40s -> overhead_per_spawn = 40s / 1 = 40s.
        // remaining = 3.
        // eta = 90 + 40 * 3 = 210s.
        let got = estimate_eta(
            Duration::from_secs(70),
            Duration::from_secs(30),
            Duration::from_secs(90),
            1,
            4,
        )
        .unwrap();
        assert_eq!(got, Duration::from_secs(210));
    }

    #[test]
    fn estimate_eta_returns_none_on_final_spawn() {
        // i == total -> no ETA line should print.
        let got = estimate_eta(
            Duration::from_secs(120),
            Duration::from_secs(120),
            Duration::ZERO,
            4,
            4,
        );
        assert!(got.is_none(), "ETA must be None when completed == total");
    }

    #[test]
    fn estimate_eta_returns_none_when_completed_is_zero() {
        // Defensive: callers never invoke with completed==0, but guard
        // against the division-by-zero panic just in case.
        let got = estimate_eta(
            Duration::ZERO,
            Duration::ZERO,
            Duration::from_secs(120),
            0,
            4,
        );
        assert!(got.is_none());
    }

    #[test]
    fn estimate_eta_saturates_on_under_budget_run() {
        // elapsed < nominal_so_far -> overhead saturates to zero. The ETA
        // is just the nominal remaining; we do NOT predict a negative
        // remainder.
        let got = estimate_eta(
            Duration::from_secs(10),
            Duration::from_secs(30),
            Duration::from_secs(90),
            1,
            4,
        )
        .unwrap();
        assert_eq!(got, Duration::from_secs(90));
    }

    #[test]
    fn estimate_eta_amortizes_overhead_over_completed_spawns() {
        // After 2 spawns (each nominally 30s), elapsed = 80s.
        // nominal_so_far = 60s, overhead_total = 20s, per_spawn = 10s.
        // 2 spawns remain, nominal_remaining = 60s.
        // eta = 60 + 10 * 2 = 80s.
        let got = estimate_eta(
            Duration::from_secs(80),
            Duration::from_secs(60),
            Duration::from_secs(60),
            2,
            4,
        )
        .unwrap();
        assert_eq!(got, Duration::from_secs(80));
    }
}
