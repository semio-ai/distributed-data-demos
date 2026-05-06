//! Per-spawn job expansion derived from `[[variant]]` entries.
//!
//! A single TOML `[[variant]]` entry can expand into one or more "spawn jobs"
//! across three dimensions: `tick_rate_hz`, `values_per_tick`, and `qos`.
//! Each job captures the concrete per-spawn scalars plus the synthesized
//! `effective_name` used for `--variant`, ready/done barriers, and log files.

use crate::config::VariantConfig;

/// One concrete spawn invocation derived from a `[[variant]]` entry.
///
/// `source_index` is the index into `BenchConfig::variant` from which this
/// job was derived. The remaining fields carry per-spawn scalars used when
/// constructing CLI args and barrier identifiers.
#[derive(Debug, Clone)]
pub struct SpawnJob {
    /// Index of the source `[[variant]]` entry in `BenchConfig::variant`.
    /// Kept for debuggability and potential future use in summary tables.
    #[allow(dead_code)]
    pub source_index: usize,
    /// Synthesized variant name: `<base>[-<vpt>x<hz>hz][-qos<N>]`. Suffixes
    /// only appear when their dimension expanded to multiple effective values.
    pub effective_name: String,
    /// Concrete tick rate (Hz) for this spawn.
    pub tick_rate_hz: u32,
    /// Concrete values-per-tick for this spawn.
    pub values_per_tick: u32,
    /// Concrete QoS level for this spawn (1..=4).
    pub qos: u8,
}

/// Expand a single `[[variant]]` entry into the Cartesian product of its
/// `tick_rate_hz`, `values_per_tick`, and `qos` dimensions.
///
/// Iteration order is stable and ascending: `tick_rate_hz` (outer) →
/// `values_per_tick` (middle) → `qos` (inner). Each dimension's concrete
/// values come from its `*_spec()` helper, which dedupes and sorts ascending.
///
/// Auto-naming follows the contract:
/// - `<base>` always starts with the post-template `variant.name`.
/// - `-<vpt>x<hz>hz` is appended whenever either `tick_rate_hz` OR
///   `values_per_tick` produced more than one effective value (both numbers
///   always appear so the suffix uniquely identifies the spawn).
/// - `-qos<N>` is appended whenever `qos` produced more than one effective
///   level.
pub fn expand_variant(
    source_index: usize,
    variant: &VariantConfig,
) -> anyhow::Result<Vec<SpawnJob>> {
    let hz_values = variant.tick_rate_spec()?.values();
    let vpt_values = variant.values_per_tick_spec()?.values();
    let qos_levels = variant.qos_spec()?.levels();

    let hz_vpt_expanded = hz_values.len() > 1 || vpt_values.len() > 1;
    let qos_expanded = qos_levels.len() > 1;

    let mut jobs = Vec::with_capacity(hz_values.len() * vpt_values.len() * qos_levels.len());
    for hz in &hz_values {
        for vpt in &vpt_values {
            for qos in &qos_levels {
                let mut effective_name = variant.name.clone();
                if hz_vpt_expanded {
                    effective_name.push_str(&format!("-{vpt}x{hz}hz"));
                }
                if qos_expanded {
                    effective_name.push_str(&format!("-qos{qos}"));
                }
                jobs.push(SpawnJob {
                    source_index,
                    effective_name,
                    tick_rate_hz: *hz,
                    values_per_tick: *vpt,
                    qos: *qos,
                });
            }
        }
    }
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BenchConfig;

    fn parse_config(toml_str: &str) -> BenchConfig {
        let mut cfg: BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        cfg
    }

    #[test]
    fn single_integer_qos_keeps_original_name() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "myvariant"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = 2
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].effective_name, "myvariant");
        assert_eq!(jobs[0].qos, 2);
        assert_eq!(jobs[0].tick_rate_hz, 100);
        assert_eq!(jobs[0].values_per_tick, 10);
        assert_eq!(jobs[0].source_index, 0);
    }

    #[test]
    fn array_qos_expands_to_multiple_jobs_with_suffix() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "myvariant"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = [1, 3]
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].effective_name, "myvariant-qos1");
        assert_eq!(jobs[0].qos, 1);
        assert_eq!(jobs[1].effective_name, "myvariant-qos3");
        assert_eq!(jobs[1].qos, 3);
    }

    #[test]
    fn omitted_qos_expands_to_all_four_levels() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 4);
        for (i, expected_qos) in [1, 2, 3, 4].iter().enumerate() {
            assert_eq!(jobs[i].qos, *expected_qos);
            assert_eq!(jobs[i].effective_name, format!("v-qos{expected_qos}"));
        }
    }

    #[test]
    fn duplicates_are_deduplicated() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = [3, 1, 3, 4, 1]
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].qos, 1);
        assert_eq!(jobs[1].qos, 3);
        assert_eq!(jobs[2].qos, 4);
    }

    #[test]
    fn single_element_array_keeps_original_name() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 10
  qos = [2]
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].effective_name, "v");
        assert_eq!(jobs[0].qos, 2);
    }

    #[test]
    fn single_element_arrays_on_hz_and_vpt_count_as_scalar() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [100]
  values_per_tick = [10]
  qos = 1
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].effective_name, "v");
        assert_eq!(jobs[0].tick_rate_hz, 100);
        assert_eq!(jobs[0].values_per_tick, 10);
    }

    #[test]
    fn hz_array_expands_with_vpt_in_suffix() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [10, 100]
  values_per_tick = 1000
  qos = 1
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].effective_name, "v-1000x10hz");
        assert_eq!(jobs[0].tick_rate_hz, 10);
        assert_eq!(jobs[0].values_per_tick, 1000);
        assert_eq!(jobs[1].effective_name, "v-1000x100hz");
        assert_eq!(jobs[1].tick_rate_hz, 100);
    }

    #[test]
    fn vpt_array_expands_with_hz_in_suffix() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = [10, 1000]
  qos = 1
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 2);
        // vpt is sorted ascending; suffix shows BOTH numbers.
        assert_eq!(jobs[0].effective_name, "v-10x100hz");
        assert_eq!(jobs[0].values_per_tick, 10);
        assert_eq!(jobs[1].effective_name, "v-1000x100hz");
        assert_eq!(jobs[1].values_per_tick, 1000);
    }

    #[test]
    fn cartesian_order_hz_outer_vpt_middle_qos_inner() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [10, 100]
  values_per_tick = [1, 10]
  qos = [1, 2]
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 8);

        // Expected stable order: hz outer, vpt middle, qos inner; each ascending.
        let expected: Vec<(u32, u32, u8, &str)> = vec![
            (10, 1, 1, "v-1x10hz-qos1"),
            (10, 1, 2, "v-1x10hz-qos2"),
            (10, 10, 1, "v-10x10hz-qos1"),
            (10, 10, 2, "v-10x10hz-qos2"),
            (100, 1, 1, "v-1x100hz-qos1"),
            (100, 1, 2, "v-1x100hz-qos2"),
            (100, 10, 1, "v-10x100hz-qos1"),
            (100, 10, 2, "v-10x100hz-qos2"),
        ];

        for (i, (hz, vpt, qos, name)) in expected.into_iter().enumerate() {
            assert_eq!(jobs[i].tick_rate_hz, hz, "job {i} hz");
            assert_eq!(jobs[i].values_per_tick, vpt, "job {i} vpt");
            assert_eq!(jobs[i].qos, qos, "job {i} qos");
            assert_eq!(jobs[i].effective_name, name, "job {i} name");
        }
    }

    #[test]
    fn hz_array_with_omitted_qos_carries_both_suffixes() {
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [10, 100]
  values_per_tick = 1000
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        // 2 hz * 1 vpt * 4 qos = 8 jobs
        assert_eq!(jobs.len(), 8);
        // First job: lowest hz, only vpt, qos 1.
        assert_eq!(jobs[0].effective_name, "v-1000x10hz-qos1");
        assert_eq!(jobs[3].effective_name, "v-1000x10hz-qos4");
        assert_eq!(jobs[4].effective_name, "v-1000x100hz-qos1");
        assert_eq!(jobs[7].effective_name, "v-1000x100hz-qos4");
    }
}
