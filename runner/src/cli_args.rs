use crate::config::VariantConfig;

/// Convert a snake_case key to --kebab-case CLI argument.
fn to_kebab_flag(key: &str) -> String {
    format!("--{}", key.replace('_', "-"))
}

/// Format a TOML value as a CLI argument string.
fn toml_value_to_string(val: &toml::Value) -> String {
    match val {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Build the complete CLI argument vector for spawning a variant process.
///
/// Order: common args, specific args, runner-injected args.
/// This matches the variant-cli.md contract:
///   <binary> [common args...] [specific args...] --launch-ts <ts> --variant <v> --runner <r> --run <id>
pub fn build_variant_args(
    variant: &VariantConfig,
    run: &str,
    runner_name: &str,
    launch_ts: &str,
) -> Vec<String> {
    let mut args = Vec::new();

    // Common args from [variant.common] table.
    for (key, val) in &variant.common {
        args.push(to_kebab_flag(key));
        args.push(toml_value_to_string(val));
    }

    // Runner-injected args (before specific args, because specific args
    // are passed as trailing args after `--` and clap would absorb
    // runner-injected args if they came after unknown specific args).
    args.push("--launch-ts".to_string());
    args.push(launch_ts.to_string());
    args.push("--variant".to_string());
    args.push(variant.name.clone());
    args.push("--runner".to_string());
    args.push(runner_name.to_string());
    args.push("--run".to_string());
    args.push(run.to_string());

    // Specific args from [variant.specific] table (if present).
    // Separated by `--` so clap treats them as trailing/extra args.
    if let Some(ref specific) = variant.specific {
        if !specific.is_empty() {
            args.push("--".to_string());
            for (key, val) in specific {
                args.push(to_kebab_flag(key));
                args.push(toml_value_to_string(val));
            }
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BenchConfig;

    fn sample_config() -> BenchConfig {
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "zenoh-replication"
binary = "./zenoh-variant"
timeout_secs = 30

  [variant.common]
  tick_rate_hz = 100
  stabilize_secs = 5
  operate_secs = 30
  silent_secs = 3
  workload = "scalar-flood"
  values_per_tick = 10
  qos = 2
  log_dir = "./logs"

  [variant.specific]
  zenoh_mode = "peer"
  zenoh_listen = "udp/0.0.0.0:7447"
"#;
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn kebab_conversion() {
        assert_eq!(to_kebab_flag("tick_rate_hz"), "--tick-rate-hz");
        assert_eq!(to_kebab_flag("qos"), "--qos");
        assert_eq!(to_kebab_flag("log_dir"), "--log-dir");
        assert_eq!(to_kebab_flag("values_per_tick"), "--values-per-tick");
    }

    #[test]
    fn toml_value_formatting() {
        assert_eq!(toml_value_to_string(&toml::Value::Integer(42)), "42");
        assert_eq!(
            toml_value_to_string(&toml::Value::String("hello".into())),
            "hello"
        );
        assert_eq!(toml_value_to_string(&toml::Value::Boolean(true)), "true");
        assert_eq!(toml_value_to_string(&toml::Value::Float(3.14)), "3.14");
    }

    #[test]
    fn build_args_includes_all_sections() {
        let config = sample_config();
        let v = &config.variant[0];
        let args = build_variant_args(v, "run01", "a", "2025-01-01T00:00:00Z");

        // Common args should be present as --kebab-case.
        assert!(args.contains(&"--tick-rate-hz".to_string()));
        assert!(args.contains(&"100".to_string()));
        assert!(args.contains(&"--workload".to_string()));
        assert!(args.contains(&"scalar-flood".to_string()));
        assert!(args.contains(&"--qos".to_string()));
        assert!(args.contains(&"2".to_string()));

        // Specific args should be present.
        assert!(args.contains(&"--zenoh-mode".to_string()));
        assert!(args.contains(&"peer".to_string()));
        assert!(args.contains(&"--zenoh-listen".to_string()));
        assert!(args.contains(&"udp/0.0.0.0:7447".to_string()));

        // Runner-injected args should be at the end.
        let launch_idx = args.iter().position(|a| a == "--launch-ts").unwrap();
        let variant_idx = args.iter().position(|a| a == "--variant").unwrap();
        let runner_idx = args.iter().position(|a| a == "--runner").unwrap();
        let run_idx = args.iter().position(|a| a == "--run").unwrap();

        // Injected args come after common and specific.
        assert!(launch_idx > 0);
        assert!(variant_idx > launch_idx);
        assert!(runner_idx > variant_idx);
        assert!(run_idx > runner_idx);

        // Verify injected values.
        assert_eq!(args[launch_idx + 1], "2025-01-01T00:00:00Z");
        assert_eq!(args[variant_idx + 1], "zenoh-replication");
        assert_eq!(args[runner_idx + 1], "a");
        assert_eq!(args[run_idx + 1], "run01");
    }

    #[test]
    fn build_args_without_specific() {
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "simple"
binary = "./simple"

  [variant.common]
  tick_rate_hz = 10
  operate_secs = 5
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let v = &config.variant[0];
        let args = build_variant_args(v, "run01", "a", "2025-01-01T00:00:00Z");

        // Should still have common args and injected args, no specific section.
        assert!(args.contains(&"--tick-rate-hz".to_string()));
        assert!(args.contains(&"--launch-ts".to_string()));
        assert!(args.contains(&"--variant".to_string()));
        assert_eq!(
            args.iter().position(|a| a == "--variant").unwrap() + 1,
            args.iter().position(|a| a == "simple").unwrap()
        );
    }
}
