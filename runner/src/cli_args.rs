use crate::config::{ThreadingMode, VariantConfig};
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
/// from `variant.name` when array expansion synthesizes a suffixed name.
/// `effective_qos`, `effective_tick_rate_hz`, and `effective_values_per_tick`
/// are the concrete per-spawn scalars; they override any array (or omitted)
/// values in `[variant.common]`.
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
    effective_tick_rate_hz: u32,
    effective_values_per_tick: u32,
    effective_threading_mode: ThreadingMode,
    effective_recv_buffer_kb: u32,
    peer_hosts: &HashMap<String, String>,
) -> Vec<String> {
    let mut args = Vec::new();

    // Common args from [variant.common] table. Per-spawn dimensions
    // (qos, tick_rate_hz, values_per_tick, threading_modes, recv_buffer_kb)
    // are skipped here -- the runner-injected scalars below carry the
    // concrete per-spawn values and override any array/omitted form in the
    // common table.
    for (key, val) in &variant.common {
        if matches!(
            key.as_str(),
            "qos" | "tick_rate_hz" | "values_per_tick" | "threading_modes" | "recv_buffer_kb"
        ) {
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

    // Runner-injected per-spawn scalars (override any array/omitted form
    // in [variant.common]).
    args.push("--tick-rate-hz".to_string());
    args.push(effective_tick_rate_hz.to_string());
    args.push("--values-per-tick".to_string());
    args.push(effective_values_per_tick.to_string());
    args.push("--qos".to_string());
    args.push(effective_qos.to_string());
    // E14 (T14.8): always inject `--threading-mode` and `--recv-buffer-kb`.
    // The variant-base CLI defaults `--threading-mode` to `single` during
    // the rollout window, but from T14.8 onward the runner emits it
    // unconditionally so analysis can rely on it being present in every
    // log file.
    args.push("--threading-mode".to_string());
    args.push(effective_threading_mode.as_str().to_string());
    args.push("--recv-buffer-kb".to_string());
    args.push(effective_recv_buffer_kb.to_string());

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
            100,
            10,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
        );

        // Common args should be present as --kebab-case.
        assert!(args.contains(&"--tick-rate-hz".to_string()));
        assert!(args.contains(&"100".to_string()));
        assert!(args.contains(&"--workload".to_string()));
        assert!(args.contains(&"scalar-flood".to_string()));
        assert!(args.contains(&"--qos".to_string()));
        assert!(args.contains(&"2".to_string()));
        assert!(args.contains(&"--values-per-tick".to_string()));
        assert!(args.contains(&"10".to_string()));

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
            100,
            10,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
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
  values_per_tick = 5
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
            10,
            5,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
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
        // When expansion synthesizes a name like "v-qos3", build_variant_args
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
            100,
            10,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
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
    fn build_args_overrides_array_dimensions_with_per_spawn_scalars() {
        // When [variant.common] uses arrays for tick_rate_hz / values_per_tick,
        // build_variant_args must NOT emit those arrays. The runner-injected
        // per-spawn scalars are the only --tick-rate-hz / --values-per-tick
        // values the variant ever sees.
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = [10, 100]
  values_per_tick = [1, 1000]
  workload = "scalar-flood"
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
            "v-1000x100hz",
            1,
            100,
            1000,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
        );

        let hz_indices: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--tick-rate-hz")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(hz_indices.len(), 1, "--tick-rate-hz must appear once");
        assert_eq!(args[hz_indices[0] + 1], "100");

        let vpt_indices: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--values-per-tick")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(vpt_indices.len(), 1, "--values-per-tick must appear once");
        assert_eq!(args[vpt_indices[0] + 1], "1000");
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
            100,
            10,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
        );
        let peers_idx = args.iter().position(|a| a == "--peers").unwrap();
        assert_eq!(args[peers_idx + 1], "alpha=127.0.0.1,zeta=192.168.1.20");
    }

    // T15.8: removed `build_args_passes_eot_timeout_secs_when_present_in_common`
    // and `build_args_omits_eot_timeout_secs_when_absent_from_common`.
    // The `--eot-timeout-secs` variant CLI arg is gone; carrying
    // `eot_timeout_secs` in `[variant.common]` would now spawn the
    // variant with an unknown argument and fail.

    // -----------------------------------------------------------------
    // T14.8: --threading-mode and --recv-buffer-kb injection.
    // -----------------------------------------------------------------

    #[test]
    fn build_args_injects_threading_mode_unconditionally() {
        // From T14.8 the runner emits `--threading-mode` for every spawn
        // regardless of whether the source config declared `threading_modes`.
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1
  qos = 1
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
            "v",
            1,
            100,
            1,
            ThreadingMode::Multi,
            8192,
            &peers,
        );

        let mode_idx = args
            .iter()
            .position(|a| a == "--threading-mode")
            .expect("--threading-mode must be injected unconditionally");
        assert_eq!(args[mode_idx + 1], "multi");

        let buf_idx = args
            .iter()
            .position(|a| a == "--recv-buffer-kb")
            .expect("--recv-buffer-kb must be injected unconditionally");
        assert_eq!(args[buf_idx + 1], "8192");
    }

    #[test]
    fn build_args_threading_modes_common_value_does_not_leak() {
        // If a user puts `threading_modes = [...]` in [variant.common]
        // it must NOT leak through as a stray `--threading-modes` flag.
        // The per-spawn `--threading-mode` injection is the only signal.
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1
  qos = 1
  threading_modes = ["single", "multi"]
  recv_buffer_kb = 16384
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
            "v-single",
            1,
            100,
            1,
            ThreadingMode::Single,
            16384,
            &peers,
        );

        // The raw common-section keys must NOT leak as CLI args.
        assert!(
            !args.iter().any(|a| a == "--threading-modes"),
            "--threading-modes (the plural array form) must not leak through, got {args:?}"
        );

        // The injected per-spawn flags ARE present and correct.
        let mode_idx = args.iter().position(|a| a == "--threading-mode").unwrap();
        assert_eq!(args[mode_idx + 1], "single");

        let buf_idx = args.iter().position(|a| a == "--recv-buffer-kb").unwrap();
        assert_eq!(args[buf_idx + 1], "16384");

        // Each appears exactly once.
        assert_eq!(
            args.iter().filter(|a| *a == "--threading-mode").count(),
            1,
            "--threading-mode must appear exactly once"
        );
        assert_eq!(
            args.iter().filter(|a| *a == "--recv-buffer-kb").count(),
            1,
            "--recv-buffer-kb must appear exactly once"
        );
    }

    // -----------------------------------------------------------------
    // T19.4 / E19: workload-shape keys forwarded verbatim from
    // [variant.common] via the generic snake_case -> --kebab-case loop.
    // -----------------------------------------------------------------

    /// Helper: build a config with the given extra `[variant.common]` keys
    /// appended after the required ones, then build args for the (single)
    /// variant. Returns the args vector for assertion.
    fn build_args_with_common(extra_common: &str) -> Vec<String> {
        let toml_str = format!(
            r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "v"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 100
  qos = 1
  workload = "block-flood"
  {extra_common}
"#
        );
        let config: BenchConfig = toml::from_str(&toml_str).unwrap();
        let v = &config.variant[0];
        let peers = empty_peers();
        build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            None,
            "v",
            1,
            100,
            100,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
        )
    }

    /// Helper: assert that the given `--kebab` flag appears exactly once and
    /// is followed by `expected_value`.
    fn assert_kebab_flag(args: &[String], flag: &str, expected_value: &str) {
        let count = args.iter().filter(|a| *a == flag).count();
        assert_eq!(count, 1, "{flag} must appear exactly once, got {args:?}");
        let idx = args.iter().position(|a| a == flag).unwrap();
        assert_eq!(args[idx + 1], expected_value, "{flag} value mismatch");
    }

    #[test]
    fn build_args_forwards_blob_size() {
        let args = build_args_with_common("blob_size = 100");
        assert_kebab_flag(&args, "--blob-size", "100");
    }

    #[test]
    fn build_args_forwards_all_seven_workload_shape_keys() {
        let extra = "
        blob_size = 50
        mixed_scalars_min = 1
        mixed_scalars_max = 7
        mixed_arrays_min = 0
        mixed_arrays_max = 3
        mixed_dict_split_max = 4
        workload_seed = 42424242
        ";
        let args = build_args_with_common(extra);
        assert_kebab_flag(&args, "--blob-size", "50");
        assert_kebab_flag(&args, "--mixed-scalars-min", "1");
        assert_kebab_flag(&args, "--mixed-scalars-max", "7");
        assert_kebab_flag(&args, "--mixed-arrays-min", "0");
        assert_kebab_flag(&args, "--mixed-arrays-max", "3");
        assert_kebab_flag(&args, "--mixed-dict-split-max", "4");
        assert_kebab_flag(&args, "--workload-seed", "42424242");
    }

    #[test]
    fn build_args_omits_workload_shape_keys_when_absent() {
        // Backward compat: a TOML that does not declare any of the new keys
        // must NOT emit any of the seven flags.
        let args = build_args_with_common("");
        for flag in [
            "--blob-size",
            "--mixed-scalars-min",
            "--mixed-scalars-max",
            "--mixed-arrays-min",
            "--mixed-arrays-max",
            "--mixed-dict-split-max",
            "--workload-seed",
        ] {
            assert!(
                !args.contains(&flag.to_string()),
                "{flag} must not appear when the TOML omits the key, got {args:?}"
            );
        }
    }

    #[test]
    fn build_args_workload_seed_accepts_large_u64() {
        // workload_seed is documented as a u64; the runner forwards it
        // verbatim as a stringified integer. TOML's integer type is i64, so
        // the largest representable value is i64::MAX.
        let args = build_args_with_common(&format!("workload_seed = {}", i64::MAX));
        assert_kebab_flag(&args, "--workload-seed", &i64::MAX.to_string());
    }

    #[test]
    fn build_args_forwards_blob_size_inherited_from_template() {
        // Template declares blob_size; variant entry omits it. After
        // resolve_templates() the variant's common table should carry
        // blob_size and the generic CLI loop emits --blob-size.
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant_template]]
name = "blockflood-base"
binary = "./x"
  [variant_template.common]
  blob_size = 250
  workload = "block-flood"

[[variant]]
template = "blockflood-base"
name = "v"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1000
  qos = 1
"#;
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
        let v = &config.variant[0];
        let peers = empty_peers();
        let args = build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            None,
            "v",
            1,
            100,
            1000,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
        );
        assert_kebab_flag(&args, "--blob-size", "250");
        // The template's workload value also propagated.
        assert_kebab_flag(&args, "--workload", "block-flood");
    }

    #[test]
    fn build_args_variant_blob_size_overrides_template() {
        // Both template and variant declare blob_size; the variant entry
        // wins per the template-merge contract.
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant_template]]
name = "blockflood-base"
binary = "./x"
  [variant_template.common]
  blob_size = 250
  workload = "block-flood"

[[variant]]
template = "blockflood-base"
name = "v"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1000
  qos = 1
  blob_size = 500
"#;
        let mut config: BenchConfig = toml::from_str(toml_str).unwrap();
        config.resolve_templates().unwrap();
        let v = &config.variant[0];
        let peers = empty_peers();
        let args = build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            None,
            "v",
            1,
            100,
            1000,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
        );
        assert_kebab_flag(&args, "--blob-size", "500");
    }
}
