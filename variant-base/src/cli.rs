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
