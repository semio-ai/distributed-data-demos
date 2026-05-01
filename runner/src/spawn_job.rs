//! Per-spawn job expansion derived from `[[variant]]` entries.
//!
//! A single TOML `[[variant]]` entry can expand into one or more "spawn jobs"
//! when `[variant.common].qos` is omitted or specified as an array. Each job
//! captures the concrete QoS level for that spawn plus the synthesized
//! `effective_name` used for `--variant`, ready/done barriers, and log files.

use crate::config::VariantConfig;

/// One concrete spawn invocation derived from a `[[variant]]` entry.
///
/// `source_index` is the index into `BenchConfig::variant` from which this
/// job was derived; `effective_name` and `qos` are the per-spawn values used
/// when constructing CLI args and barrier identifiers.
#[derive(Debug, Clone)]
pub struct SpawnJob {
    /// Index of the source `[[variant]]` entry in `BenchConfig::variant`.
    /// Kept for debuggability and potential future use in summary tables;
    /// the main loop currently iterates entries directly.
    #[allow(dead_code)]
    pub source_index: usize,
    /// Synthesized variant name: original `name` if there is only one QoS
    /// level, or `<name>-qos<N>` when multiple levels expand.
    pub effective_name: String,
    /// Concrete QoS level for this spawn (1..=4).
    pub qos: u8,
}

/// Expand a single `[[variant]]` entry into one spawn job per QoS level.
///
/// Levels come from `VariantConfig::qos_spec()` and are returned in ascending
/// order, deduplicated. When the result has a single level, the effective
/// name preserves the original `variant.name`. When multiple levels are
/// expanded, the effective name becomes `<name>-qos<N>`.
pub fn expand_variant(
    source_index: usize,
    variant: &VariantConfig,
) -> anyhow::Result<Vec<SpawnJob>> {
    let levels = variant.qos_spec()?.levels();
    let multi = levels.len() > 1;
    let jobs = levels
        .into_iter()
        .map(|qos| {
            let effective_name = if multi {
                format!("{}-qos{}", variant.name, qos)
            } else {
                variant.name.clone()
            };
            SpawnJob {
                source_index,
                effective_name,
                qos,
            }
        })
        .collect();
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BenchConfig;

    fn parse_config(toml_str: &str) -> BenchConfig {
        toml::from_str(toml_str).unwrap()
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
  qos = 2
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].effective_name, "myvariant");
        assert_eq!(jobs[0].qos, 2);
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
        // Only one effective level -> no -qosN suffix.
        let cfg = parse_config(
            r#"
run = "r"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  qos = [2]
"#,
        );
        let jobs = expand_variant(0, &cfg.variant[0]).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].effective_name, "v");
        assert_eq!(jobs[0].qos, 2);
    }
}
