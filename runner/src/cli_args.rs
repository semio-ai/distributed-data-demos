use crate::config::{ThreadingMode, VariantConfig};
use anyhow::{anyhow, Result};
use std::collections::HashMap;

/// Convert a snake_case key to --kebab-case CLI argument.
fn to_kebab_flag(key: &str) -> String {
    format!("--{}", key.replace('_', "-"))
}

/// Parse raw `--variant-arg <variant>.<key>=<value>` entries into a nested
/// map keyed by variant name (T9.5).
///
/// Each entry is split on:
///   - the **first** `.` to separate the variant name from the rest, and
///   - the **first** `=` in the rest to separate the key from the value.
///
/// Empty value (e.g. `foo.bar=`) is **accepted**: the override is stored
/// as the empty string. This makes `--variant-arg` viable for flags whose
/// value is empty/optional. Variants that require a non-empty value should
/// reject empty values themselves at parse time.
///
/// Multiple entries for the same variant accumulate into one inner map.
/// Within a single variant, later occurrences of the same key overwrite
/// earlier ones (CLI-level "last one wins" — useful for shell wrappers
/// that append a default and then an override).
///
/// Returns `Err` for malformed input naming the offending entry.
pub fn parse_variant_arg_overrides(
    raw: &[String],
) -> Result<HashMap<String, HashMap<String, toml::Value>>> {
    let mut out: HashMap<String, HashMap<String, toml::Value>> = HashMap::new();
    for entry in raw {
        // Split off the variant name on the first `.`.
        let (variant, rest) = entry.split_once('.').ok_or_else(|| {
            anyhow!(
                "malformed --variant-arg '{}': expected '<variant>.<key>=<value>' (missing '.')",
                entry
            )
        })?;
        if variant.is_empty() {
            return Err(anyhow!(
                "malformed --variant-arg '{}': variant name (before the first '.') is empty",
                entry
            ));
        }
        // Split key from value on the first `=`.
        let (key, value) = rest.split_once('=').ok_or_else(|| {
            anyhow!(
                "malformed --variant-arg '{}': expected '<variant>.<key>=<value>' (missing '=')",
                entry
            )
        })?;
        if key.is_empty() {
            return Err(anyhow!(
                "malformed --variant-arg '{}': key (between '.' and '=') is empty",
                entry
            ));
        }
        out.entry(variant.to_string())
            .or_default()
            .insert(key.to_string(), toml::Value::String(value.to_string()));
    }
    Ok(out)
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
///
/// `cli_specific_overrides` (T9.5) is the parsed `--variant-arg` overrides
/// **for this spawn's variant** (i.e. caller has already looked up the
/// right inner map). When `Some`, CLI keys win over TOML keys on conflict
/// and CLI-only keys are appended. The merged keys are emitted in
/// lexicographic order for log diffability.
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
    cli_specific_overrides: Option<&HashMap<String, toml::Value>>,
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

    // Specific args from [variant.specific] table merged with the T9.5
    // CLI `--variant-arg` overrides. Precedence: CLI value wins over TOML
    // value on key conflicts; CLI-only keys are appended. Emitted in
    // lexicographic key order for log diffability.
    //
    // Separated by `--` so clap treats them as trailing/extra args.
    let merged = merge_specific_with_overrides(variant.specific.as_ref(), cli_specific_overrides);
    if !merged.is_empty() {
        args.push("--".to_string());
        for (key, val) in &merged {
            args.push(to_kebab_flag(key));
            args.push(toml_value_to_string(val));
        }
    }

    args
}

/// Merge the variant's `[variant.specific]` TOML table with the T9.5 CLI
/// `--variant-arg` overrides for this variant. CLI wins on key conflicts;
/// CLI-only keys are appended. Result is sorted lexicographically by key
/// so spawn-log lines diff cleanly across runs.
fn merge_specific_with_overrides(
    toml_specific: Option<&toml::value::Table>,
    cli_overrides: Option<&HashMap<String, toml::Value>>,
) -> Vec<(String, toml::Value)> {
    let mut merged: HashMap<String, toml::Value> = HashMap::new();
    if let Some(specific) = toml_specific {
        for (key, val) in specific {
            merged.insert(key.clone(), val.clone());
        }
    }
    if let Some(overrides) = cli_overrides {
        for (key, val) in overrides {
            // CLI wins on conflict.
            merged.insert(key.clone(), val.clone());
        }
    }
    let mut entries: Vec<(String, toml::Value)> = merged.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Provenance of a single effective specific arg, used for the spawn-time
/// provenance log line (T9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecificArgProvenance {
    Toml,
    Cli,
}

/// Return the per-spawn effective `[variant.specific]` keys plus where
/// each value came from (`toml`, `cli`). Used by `main.rs` to emit the
/// provenance log line. Result is sorted lexicographically by key.
///
/// `cli_overrides` is the inner map for **this spawn's variant** only
/// (caller looked up the right entry already).
pub fn specific_arg_provenance(
    toml_specific: Option<&toml::value::Table>,
    cli_overrides: Option<&HashMap<String, toml::Value>>,
) -> Vec<(String, String, SpecificArgProvenance)> {
    let mut entries: HashMap<String, (toml::Value, SpecificArgProvenance)> = HashMap::new();
    if let Some(specific) = toml_specific {
        for (key, val) in specific {
            entries.insert(key.clone(), (val.clone(), SpecificArgProvenance::Toml));
        }
    }
    if let Some(overrides) = cli_overrides {
        for (key, val) in overrides {
            // CLI wins; tag as CLI regardless of whether it was a new key
            // or an override of a TOML key.
            entries.insert(key.clone(), (val.clone(), SpecificArgProvenance::Cli));
        }
    }
    let mut out: Vec<(String, String, SpecificArgProvenance)> = entries
        .into_iter()
        .map(|(k, (v, p))| (k, toml_value_to_string(&v), p))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        );
        assert_kebab_flag(&args, "--blob-size", "500");
    }

    // -----------------------------------------------------------------
    // T9.5: --variant-arg passthrough — parser + merge tests.
    // -----------------------------------------------------------------

    #[test]
    fn parse_variant_arg_rejects_entry_with_no_dot_or_equals() {
        let raw = vec!["foo".to_string()];
        let err = parse_variant_arg_overrides(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("'foo'"),
            "error must name the bad entry: {msg}"
        );
        assert!(
            msg.contains("missing '.'"),
            "error must mention missing '.': {msg}"
        );
    }

    #[test]
    fn parse_variant_arg_rejects_entry_with_no_equals() {
        let raw = vec!["foo.bar".to_string()];
        let err = parse_variant_arg_overrides(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("'foo.bar'"), "{msg}");
        assert!(msg.contains("missing '='"), "{msg}");
    }

    #[test]
    fn parse_variant_arg_rejects_empty_variant() {
        let raw = vec!["=bar".to_string()];
        let err = parse_variant_arg_overrides(&raw).unwrap_err();
        let msg = format!("{err}");
        // `=bar` -> split_once('.') returns None -> "missing '.'" error.
        // That's acceptable; the entry IS malformed and the offending text
        // appears in the message.
        assert!(msg.contains("'=bar'"), "{msg}");
    }

    #[test]
    fn parse_variant_arg_rejects_dotless_after_first_eq() {
        // `=bar` — variant is empty before the first `.`, but split_once('.')
        // returns None first. To exercise the empty-variant guard we need an
        // entry that has a `.` but starts with it.
        let raw = vec![".key=value".to_string()];
        let err = parse_variant_arg_overrides(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("variant name") && msg.contains("empty"),
            "expected empty-variant-name error, got: {msg}"
        );
    }

    #[test]
    fn parse_variant_arg_rejects_empty_key() {
        // variant present, but key (between '.' and '=') is empty.
        let raw = vec!["zenoh.=value".to_string()];
        let err = parse_variant_arg_overrides(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("key") && msg.contains("empty"),
            "expected empty-key error, got: {msg}"
        );
    }

    #[test]
    fn parse_variant_arg_accepts_empty_value() {
        // Per the parser doc, empty value is accepted (some flags are
        // flag-only / take an empty value).
        let raw = vec!["zenoh.flag=".to_string()];
        let out = parse_variant_arg_overrides(&raw).unwrap();
        let zenoh = out.get("zenoh").unwrap();
        assert_eq!(
            zenoh.get("flag").unwrap(),
            &toml::Value::String(String::new())
        );
    }

    #[test]
    fn parse_variant_arg_groups_multiple_entries_for_same_variant() {
        let raw = vec![
            "zenoh.multicast_interface=192.168.1.68".to_string(),
            "zenoh.zenoh_mode=peer".to_string(),
        ];
        let out = parse_variant_arg_overrides(&raw).unwrap();
        assert_eq!(out.len(), 1, "exactly one variant expected: {out:?}");
        let zenoh = out.get("zenoh").unwrap();
        assert_eq!(zenoh.len(), 2);
        assert_eq!(
            zenoh.get("multicast_interface").unwrap(),
            &toml::Value::String("192.168.1.68".to_string())
        );
        assert_eq!(
            zenoh.get("zenoh_mode").unwrap(),
            &toml::Value::String("peer".to_string())
        );
    }

    #[test]
    fn parse_variant_arg_groups_by_variant_when_multiple_variants() {
        let raw = vec![
            "zenoh.multicast_interface=192.168.1.68".to_string(),
            "quic.cert_path=/etc/quic.pem".to_string(),
        ];
        let out = parse_variant_arg_overrides(&raw).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("zenoh"));
        assert!(out.contains_key("quic"));
    }

    #[test]
    fn parse_variant_arg_value_with_equals_in_value() {
        // The value half can itself contain `=`; we split on the FIRST `=`.
        let raw = vec!["zenoh.connection_string=key=value;other=stuff".to_string()];
        let out = parse_variant_arg_overrides(&raw).unwrap();
        let zenoh = out.get("zenoh").unwrap();
        assert_eq!(
            zenoh.get("connection_string").unwrap(),
            &toml::Value::String("key=value;other=stuff".to_string())
        );
    }

    #[test]
    fn parse_variant_arg_value_with_dot_in_value() {
        // The value half can also contain `.` (IP addresses, paths). We
        // split on the FIRST `.` so dotted values after the first split
        // survive intact.
        let raw = vec!["zenoh.multicast_interface=192.168.1.68".to_string()];
        let out = parse_variant_arg_overrides(&raw).unwrap();
        let zenoh = out.get("zenoh").unwrap();
        assert_eq!(
            zenoh.get("multicast_interface").unwrap(),
            &toml::Value::String("192.168.1.68".to_string())
        );
    }

    #[test]
    fn build_args_merges_cli_overrides_with_toml_specific() {
        // TOML has a=1, b=2. CLI has b=3, c=4. Merged: a=1 (toml), b=3
        // (cli wins), c=4 (cli-only). Emit order lexicographic: a, b, c.
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
  [variant.specific]
  a = "1"
  b = "2"
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let v = &config.variant[0];
        let peers = empty_peers();

        let mut overrides: HashMap<String, toml::Value> = HashMap::new();
        overrides.insert("b".to_string(), toml::Value::String("3".to_string()));
        overrides.insert("c".to_string(), toml::Value::String("4".to_string()));

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
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
            Some(&overrides),
        );

        // Find the `--` separator that introduces the specific block.
        let sep_idx = args
            .iter()
            .position(|a| a == "--")
            .expect("specific section must be present");
        let specific = &args[sep_idx + 1..];

        // Expected emission order (lexicographic): --a 1 --b 3 --c 4
        assert_eq!(
            specific,
            &[
                "--a".to_string(),
                "1".to_string(),
                "--b".to_string(),
                "3".to_string(),
                "--c".to_string(),
                "4".to_string(),
            ],
            "merged specific args mismatch (TOML a=1,b=2 + CLI b=3,c=4)"
        );
    }

    #[test]
    fn build_args_cli_only_specific_no_toml() {
        // No `[variant.specific]` table at all; CLI provides one override.
        // The `--` separator + the override must be emitted.
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

        let mut overrides: HashMap<String, toml::Value> = HashMap::new();
        overrides.insert(
            "multicast_interface".to_string(),
            toml::Value::String("192.168.1.68".to_string()),
        );

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
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
            Some(&overrides),
        );

        let sep_idx = args
            .iter()
            .position(|a| a == "--")
            .expect("specific section must be present (CLI-only override)");
        assert_eq!(args[sep_idx + 1], "--multicast-interface");
        assert_eq!(args[sep_idx + 2], "192.168.1.68");
    }

    #[test]
    fn build_args_no_specific_and_no_overrides_emits_no_separator() {
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
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
            None,
        );
        // No `--` separator since there are no specific args at all.
        assert!(
            !args.iter().any(|a| a == "--"),
            "no `--` separator expected, got: {args:?}"
        );
    }

    #[test]
    fn build_args_cli_overrides_for_other_variant_do_not_leak() {
        // The caller is responsible for passing only the overrides for
        // THIS spawn's variant. Verify build_variant_args applies
        // exactly what was passed in: no implicit lookup, no leak.
        let toml_str = r#"
run = "run01"
runners = ["a"]
default_timeout_secs = 60

[[variant]]
name = "y"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1
  qos = 1
  [variant.specific]
  k = "from-toml"
"#;
        let config: BenchConfig = toml::from_str(toml_str).unwrap();
        let v = &config.variant[0];
        let peers = empty_peers();
        // The caller (main.rs) would have looked up `variant_arg_overrides
        // .get(&variant.name)` and would not have found "y" — so it
        // passes None here. The build must emit ONLY the TOML value.
        let args = build_variant_args(
            v,
            "run01",
            "a",
            "2025-01-01T00:00:00Z",
            None,
            "y",
            1,
            100,
            1,
            ThreadingMode::Single,
            crate::config::DEFAULT_RECV_BUFFER_KB,
            &peers,
            None,
        );
        let sep_idx = args.iter().position(|a| a == "--").unwrap();
        let specific = &args[sep_idx + 1..];
        assert_eq!(
            specific,
            &["--k".to_string(), "from-toml".to_string()],
            "specific args should reflect TOML only (no leak from another variant)"
        );
    }

    #[test]
    fn specific_arg_provenance_tags_correctly() {
        // TOML: a=1, b=2. CLI: b=3, c=4.
        // Expected provenance: a=1 (toml), b=3 (cli), c=4 (cli).
        let mut toml_table = toml::value::Table::new();
        toml_table.insert("a".into(), toml::Value::String("1".into()));
        toml_table.insert("b".into(), toml::Value::String("2".into()));

        let mut overrides: HashMap<String, toml::Value> = HashMap::new();
        overrides.insert("b".to_string(), toml::Value::String("3".to_string()));
        overrides.insert("c".to_string(), toml::Value::String("4".to_string()));

        let prov = specific_arg_provenance(Some(&toml_table), Some(&overrides));
        let by_key: HashMap<String, (String, SpecificArgProvenance)> = prov
            .iter()
            .map(|(k, v, p)| (k.clone(), (v.clone(), *p)))
            .collect();
        assert_eq!(by_key["a"], ("1".to_string(), SpecificArgProvenance::Toml));
        assert_eq!(by_key["b"], ("3".to_string(), SpecificArgProvenance::Cli));
        assert_eq!(by_key["c"], ("4".to_string(), SpecificArgProvenance::Cli));
        // Lexicographic emit order.
        let keys: Vec<&String> = prov.iter().map(|(k, _, _)| k).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }
}
