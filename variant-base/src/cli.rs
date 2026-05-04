use clap::Parser;

/// Common CLI arguments shared by all variant implementations.
///
/// Variant-specific arguments are collected as trailing arguments and
/// passed through to the variant implementation.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "variant",
    about = "Benchmark variant process",
    allow_external_subcommands = false,
    trailing_var_arg = true
)]
pub struct CliArgs {
    // -- Common arguments (from [variant.common]) --
    /// Target tick rate in Hz.
    #[arg(long)]
    pub tick_rate_hz: u32,

    /// Duration of the stabilize phase in seconds.
    #[arg(long)]
    pub stabilize_secs: u64,

    /// Duration of the operate phase in seconds.
    #[arg(long)]
    pub operate_secs: u64,

    /// Duration of the silent/drain phase in seconds.
    #[arg(long)]
    pub silent_secs: u64,

    /// Maximum duration in seconds the EOT phase will wait for peer EOTs
    /// before giving up and logging an `eot_timeout` event. When unset,
    /// the driver computes the default at runtime as `max(operate_secs, 5)`.
    #[arg(long)]
    pub eot_timeout_secs: Option<u64>,

    /// Workload profile name (e.g. `scalar-flood`).
    #[arg(long)]
    pub workload: String,

    /// Number of value updates per tick.
    #[arg(long)]
    pub values_per_tick: u32,

    /// QoS level (1-4).
    #[arg(long)]
    pub qos: u8,

    /// Directory for JSONL output.
    #[arg(long)]
    pub log_dir: String,

    // -- Runner-injected arguments --
    /// Wall-clock time recorded by the runner immediately before spawn (RFC 3339).
    #[arg(long)]
    pub launch_ts: String,

    /// The variant name from config.
    #[arg(long)]
    pub variant: String,

    /// The runner's name.
    #[arg(long)]
    pub runner: String,

    /// The run identifier from config.
    #[arg(long)]
    pub run: String,

    // -- Variant-specific pass-through arguments --
    /// Additional variant-specific arguments (collected as trailing args).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub extra: Vec<String>,
}

/// Extract a `--key value` pair from a slice of CLI arguments.
///
/// Returns the value if present, `None` otherwise. Used for
/// runner-injected flags that arrive in `CliArgs::extra` (because they
/// are not declared on the `CliArgs` struct).
pub fn parse_extra_arg(extra: &[String], key: &str) -> Option<String> {
    let flag = format!("--{key}");
    let mut iter = extra.iter();
    while let Some(arg) = iter.next() {
        if arg == &flag {
            return iter.next().cloned();
        }
    }
    None
}

/// Extract just the runner names from a runner-injected `--peers` value.
///
/// `--peers` is a comma-separated list of `name=host` pairs (see
/// `metak-shared/api-contracts/variant-cli.md`). The driver only needs
/// the names for EOT scoping; host parsing belongs to the concrete
/// variant. Returns an empty vec if `--peers` is absent or the value
/// is empty/malformed.
pub fn parse_peer_names_from_extra(extra: &[String]) -> Vec<String> {
    let raw = match parse_extra_arg(extra, "peers") {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut names: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let name = match part.split_once('=') {
            Some((n, _)) => n.trim(),
            None => continue,
        };
        if !name.is_empty() {
            names.push(name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_args() {
        let args = CliArgs::parse_from([
            "variant-dummy",
            "--tick-rate-hz",
            "100",
            "--stabilize-secs",
            "5",
            "--operate-secs",
            "10",
            "--silent-secs",
            "3",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "10",
            "--qos",
            "1",
            "--log-dir",
            "/tmp/logs",
            "--launch-ts",
            "2026-04-12T14:00:00.000000000Z",
            "--variant",
            "dummy",
            "--runner",
            "a",
            "--run",
            "run01",
        ]);
        assert_eq!(args.tick_rate_hz, 100);
        assert_eq!(args.stabilize_secs, 5);
        assert_eq!(args.operate_secs, 10);
        assert_eq!(args.silent_secs, 3);
        assert_eq!(args.workload, "scalar-flood");
        assert_eq!(args.values_per_tick, 10);
        assert_eq!(args.qos, 1);
        assert_eq!(args.log_dir, "/tmp/logs");
        assert_eq!(args.launch_ts, "2026-04-12T14:00:00.000000000Z");
        assert_eq!(args.variant, "dummy");
        assert_eq!(args.runner, "a");
        assert_eq!(args.run, "run01");
    }

    #[test]
    fn parse_eot_timeout_secs_when_provided() {
        let args = CliArgs::parse_from([
            "variant-dummy",
            "--tick-rate-hz",
            "100",
            "--stabilize-secs",
            "0",
            "--operate-secs",
            "1",
            "--silent-secs",
            "0",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "1",
            "--qos",
            "1",
            "--log-dir",
            "/tmp/logs",
            "--launch-ts",
            "2026-04-12T14:00:00.000000000Z",
            "--variant",
            "dummy",
            "--runner",
            "a",
            "--run",
            "run01",
            "--eot-timeout-secs",
            "7",
        ]);
        assert_eq!(args.eot_timeout_secs, Some(7));
    }

    #[test]
    fn parse_eot_timeout_secs_default_none_when_absent() {
        let args = CliArgs::parse_from([
            "variant-dummy",
            "--tick-rate-hz",
            "100",
            "--stabilize-secs",
            "0",
            "--operate-secs",
            "1",
            "--silent-secs",
            "0",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "1",
            "--qos",
            "1",
            "--log-dir",
            "/tmp/logs",
            "--launch-ts",
            "2026-04-12T14:00:00.000000000Z",
            "--variant",
            "dummy",
            "--runner",
            "a",
            "--run",
            "run01",
        ]);
        assert_eq!(args.eot_timeout_secs, None);
    }

    #[test]
    fn parse_extra_arg_finds_value() {
        let extra: Vec<String> = vec![
            "--peers".into(),
            "alice=127.0.0.1,bob=127.0.0.1".into(),
            "--other".into(),
            "x".into(),
        ];
        assert_eq!(
            parse_extra_arg(&extra, "peers"),
            Some("alice=127.0.0.1,bob=127.0.0.1".into())
        );
        assert_eq!(parse_extra_arg(&extra, "other"), Some("x".into()));
        assert_eq!(parse_extra_arg(&extra, "missing"), None);
    }

    #[test]
    fn parse_peer_names_from_extra_handles_multiple_peers() {
        let extra: Vec<String> = vec![
            "--peers".into(),
            "alice=127.0.0.1,bob=127.0.0.1,carol=10.0.0.5".into(),
        ];
        let names = parse_peer_names_from_extra(&extra);
        assert_eq!(names, vec!["alice", "bob", "carol"]);
    }

    #[test]
    fn parse_peer_names_from_extra_handles_single_peer() {
        let extra: Vec<String> = vec!["--peers".into(), "self=127.0.0.1".into()];
        let names = parse_peer_names_from_extra(&extra);
        assert_eq!(names, vec!["self"]);
    }

    #[test]
    fn parse_peer_names_from_extra_returns_empty_when_absent() {
        let extra: Vec<String> = vec!["--something-else".into(), "v".into()];
        let names = parse_peer_names_from_extra(&extra);
        assert!(names.is_empty());
    }

    #[test]
    fn parse_peer_names_from_extra_trims_whitespace() {
        let extra: Vec<String> = vec![
            "--peers".into(),
            " alice = 127.0.0.1 , bob = 127.0.0.1 ".into(),
        ];
        let names = parse_peer_names_from_extra(&extra);
        assert_eq!(names, vec!["alice", "bob"]);
    }

    #[test]
    fn parse_with_extra_args() {
        let args = CliArgs::parse_from([
            "variant-zenoh",
            "--tick-rate-hz",
            "50",
            "--stabilize-secs",
            "2",
            "--operate-secs",
            "5",
            "--silent-secs",
            "1",
            "--workload",
            "scalar-flood",
            "--values-per-tick",
            "5",
            "--qos",
            "2",
            "--log-dir",
            "/tmp/logs",
            "--launch-ts",
            "2026-04-12T14:00:00.000000000Z",
            "--variant",
            "zenoh",
            "--runner",
            "b",
            "--run",
            "run02",
            "--",
            "--zenoh-mode",
            "peer",
        ]);
        assert_eq!(args.variant, "zenoh");
        assert_eq!(args.extra, vec!["--zenoh-mode", "peer"]);
    }
}
