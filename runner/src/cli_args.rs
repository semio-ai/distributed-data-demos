use crate::config::VariantConfig;
use std::collections::HashMap;

/// Convert a snake_case key to --kebab-case CLI argument.
fn to_kebab_flag(key: &str) -> String {
    format!("--{}", key.replace('_', "-"))
}

/// Format a TOML value as a CLI argument string.
pub(crate) fn toml_value_to_string(val: &toml::Value) -> String {
    match val {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Format the `--peers` argument value: comma-separated `name=host` pairs,
/// sorted by name for determinism.
pub fn format_peers_arg(peer_hosts: &HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, &String)> = peer_hosts.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(name, host)| format!("{name}={host}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Build the complete CLI argument vector for spawning a variant process.
///
/// Order: common args, runner-injected args, then specific args after `--`.
/// This matches the variant-cli.md contract.
///
/// `effective_variant_name` is the value passed via `--variant`; it differs
/// from `variant.name` when QoS expansion synthesizes a `<name>-qosN` name.
/// `effective_qos` is the concrete QoS level for this spawn; it overrides
/// the `qos` value in `[variant.common]` (which may be a list or omitted).
///
/// `peer_hosts` is the discovery-time map of runner names to host strings
/// (with same-host peers collapsed to `127.0.0.1`). Always emitted, even
/// in single-runner mode (the map will contain only this runner).
#[allow(clippy::too_many_arguments)]
pub fn build_variant_args(
    variant: &VariantConfig,
    run: &str,
    runner_name: &str,
    launch_ts: &str,
    log_dir_override: Option<&str>,
    effective_variant_name: &str,
    effective_qos: u8,
    peer_hosts: &HashMap<String, String>,
) -> Vec<String> {
    let mut args = Vec::new();

    // Common args from [variant.common] table. Two keys get special handling:
    //   - log_dir: replaced with log_dir_override if provided.
    //   - qos: skipped here -- the runner-injected --qos below carries the
    //     concrete per-spawn level (overrides any common qos which may be a
    //     list, omitted, or any single integer).
    for (key, val) in &variant.common {
        if key == "qos" {
            continue;
        }
        args.push(to_kebab_flag(key));
        if key == "log_dir" {
            if let Some(override_val) = log_dir_override {
                args.push(override_val.to_string());
            } else {
                args.push(toml_value_to_string(val));
            }
        } else {
            args.push(toml_value_to_string(val));
        }
    }

    // Runner-injected --qos with the per-spawn concrete level.
    args.push("--qos".to_string());
    args.push(effective_qos.to_string());

    // Runner-injected args (before specific args, because specific args
    // are passed as trailing args after `--` and clap would absorb
    // runner-injected args if they came after unknown specific args).
    args.push("--launch-ts".to_string());
    args.push(launch_ts.to_string());
    args.push("--variant".to_string());
    args.push(effective_variant_name.to_string());
    args.push("--runner".to_string());
    args.push(runner_name.to_string());
    args.push("--run".to_string());
    args.push(run.to_string());
    args.push("--peers".to_string());
    args.push(format_peers_arg(peer_hosts));

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
        assert_eq!(toml_value_to_string(&toml::Value::Float(2.5)), "2.5");
    }

    fn empty_peers() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("a".into(), "127.0.0.1".into());
        m
    }

    #[test]
    fn build_args_includes_all_sections() {
        let config = sample_config();
        let v = &config.variant[0];
        let peers = empty_peers();
        let args = build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            None,
            "zenoh-replication",
            2,
            &peers,
        );

        // Common args should be present as --kebab-case.
        assert!(args.contains(&"--tick-rate-hz".to_string()));
        assert!(args.contains(&"100".to_string()));
        assert!(args.contains(&"--workload".to_string()));
        assert!(args.contains(&"scalar-flood".to_string()));
        assert!(args.contains(&"--qos".to_string()));
        assert!(args.contains(&"2".to_string()));

        // log_dir should use the config value when no override is given.
        assert!(args.contains(&"--log-dir".to_string()));
        assert!(args.contains(&"./logs".to_string()));

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
        let peers_idx = args.iter().position(|a| a == "--peers").unwrap();

        // Injected args come after common and specific.
        assert!(launch_idx > 0);
        assert!(variant_idx > launch_idx);
        assert!(runner_idx > variant_idx);
        assert!(run_idx > runner_idx);
        assert!(peers_idx > run_idx);

        // Verify injected values.
        assert_eq!(args[launch_idx + 1], "2025-01-01T00:00:00Z");
        assert_eq!(args[variant_idx + 1], "zenoh-replication");
        assert_eq!(args[runner_idx + 1], "a");
        assert_eq!(args[run_idx + 1], "run01");
        assert_eq!(args[peers_idx + 1], "a=127.0.0.1");
    }

    #[test]
    fn build_args_log_dir_override() {
        let config = sample_config();
        let v = &config.variant[0];
        let peers = empty_peers();
        let args = build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            Some("./logs/run01-20260415_143022"),
            "zenoh-replication",
            2,
            &peers,
        );

        // --log-dir should use the override value, not the config value.
        let log_dir_idx = args.iter().position(|a| a == "--log-dir").unwrap();
        assert_eq!(args[log_dir_idx + 1], "./logs/run01-20260415_143022");
        // The original config value should not be present.
        assert!(!args.contains(&"./logs".to_string()));
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
        let peers = empty_peers();
        let args = build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            None,
            "simple",
            1,
            &peers,
        );

        // Should still have common args and injected args, no specific section.
        assert!(args.contains(&"--tick-rate-hz".to_string()));
        assert!(args.contains(&"--launch-ts".to_string()));
        assert!(args.contains(&"--variant".to_string()));
        assert!(args.contains(&"--peers".to_string()));
        assert_eq!(
            args.iter().position(|a| a == "--variant").unwrap() + 1,
            args.iter().position(|a| a == "simple").unwrap()
        );
    }

    #[test]
    fn build_args_uses_effective_variant_name_and_qos() {
        // When QoS expansion synthesizes a name like "v-qos3", build_variant_args
        // must use it for --variant and override --qos.
        let config = sample_config();
        let v = &config.variant[0];
        let peers = empty_peers();
        let args = build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            None,
            "zenoh-replication-qos3",
            3,
            &peers,
        );

        let variant_idx = args.iter().position(|a| a == "--variant").unwrap();
        assert_eq!(args[variant_idx + 1], "zenoh-replication-qos3");

        // --qos should appear exactly once (the runner-injected one with value 3),
        // not the common-section value of 2.
        let qos_indices: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--qos")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(qos_indices.len(), 1, "--qos should appear exactly once");
        assert_eq!(args[qos_indices[0] + 1], "3");
    }

    #[test]
    fn format_peers_arg_sorts_by_name() {
        let mut peers = HashMap::new();
        peers.insert("charlie".into(), "127.0.0.1".into());
        peers.insert("alice".into(), "192.168.1.10".into());
        peers.insert("bob".into(), "127.0.0.1".into());
        let s = format_peers_arg(&peers);
        assert_eq!(s, "alice=192.168.1.10,bob=127.0.0.1,charlie=127.0.0.1");
    }

    #[test]
    fn format_peers_arg_single_entry() {
        let mut peers = HashMap::new();
        peers.insert("solo".into(), "127.0.0.1".into());
        assert_eq!(format_peers_arg(&peers), "solo=127.0.0.1");
    }

    #[test]
    fn build_args_includes_peers_pairs_sorted() {
        let config = sample_config();
        let v = &config.variant[0];
        let mut peers = HashMap::new();
        peers.insert("zeta".into(), "192.168.1.20".into());
        peers.insert("alpha".into(), "127.0.0.1".into());
        let args = build_variant_args(
            v,
            "run01",
            "alpha",
            "2025-01-01T00:00:00Z",
            None,
            "zenoh-replication",
            1,
            &peers,
        );
        let peers_idx = args.iter().position(|a| a == "--peers").unwrap();
        assert_eq!(args[peers_idx + 1], "alpha=127.0.0.1,zeta=192.168.1.20");
    }
}
