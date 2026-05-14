use clap::Parser;

use crate::types::ThreadingMode;

/// Default value for `--recv-buffer-kb` when the runner does not
/// inject one. Matches `metak-shared/api-contracts/variant-cli.md`
/// (E14 additions section).
pub const DEFAULT_RECV_BUFFER_KB: u32 = 4096;

/// Inclusive lower bound for `--recv-buffer-kb`. 64 KiB sits below
/// the Windows default recv-buffer size but is harmless on every
/// platform; the value exists mostly to surface accidental zero / tiny
/// values rather than to gate legitimate sizes.
pub const MIN_RECV_BUFFER_KB: u32 = 64;

/// Inclusive upper bound for `--recv-buffer-kb`. 64 MiB is generous
/// on a Raspberry Pi 4 with 4 GB RAM under a two-peer benchmark and
/// well above what any sane variant would actually need.
pub const MAX_RECV_BUFFER_KB: u32 = 65_536;

/// Default value for `--progress-stdout-interval-ms` when the runner
/// does not inject one. Matches the E15 design (one progress line per
/// second per variant). See
/// `metak-shared/api-contracts/variant-cli.md` (E15 additions).
pub const DEFAULT_PROGRESS_STDOUT_INTERVAL_MS: u32 = 1000;

/// Default value for `--operate-idle-secs` when the runner does not
/// inject one. Matches the E15 design: when the variant observes no
/// progress on EITHER its `sent` or `received` counter for this many
/// seconds during the operate phase, it short-circuits the on-wire EOT
/// exchange and transitions directly to `silent`. See T15.5.
///
/// `0` disables variant-side idle detection: only the time-based
/// `operate_secs` transition fires (pre-E15 behaviour).
pub const DEFAULT_OPERATE_IDLE_SECS: u32 = 5;

/// Default value for `--watchdog-secs` when the runner does not inject
/// one. Matches the T15.11 design: when the variant's driver thread is
/// blocked inside a transport library call (no progress on either the
/// `sent` or `received` counter) for this many seconds during the
/// operate phase, a separate watchdog OS thread self-exits the process
/// with the documented exit code so the JSONL log can be flushed
/// cleanly via `logger.flush()` rather than truncated by an external
/// runner kill.
///
/// `0` disables the watchdog: no monitor thread is spawned and no
/// self-exit can fire (pre-T15.11 behaviour). The default of `30`
/// is chosen so the watchdog wins the race against typical runner
/// safety-net deadlines: many existing fixtures set
/// `default_timeout_secs = 60`, and the watchdog must fire well
/// before that to leave Drop impls + buffered I/O time to flush.
/// Empirically `30 s` is far longer than any cooperative
/// stabilize / operate / silent phase budget in the existing
/// fixture set (longest is ~12 s) yet comfortably under the
/// shortest reasonable runner deadline.
pub const DEFAULT_WATCHDOG_SECS: u32 = 30;

/// Validate that `kb` falls within the documented `--recv-buffer-kb`
/// range. Returned by clap's `value_parser`.
fn parse_recv_buffer_kb(s: &str) -> Result<u32, String> {
    let kb: u32 = s
        .parse()
        .map_err(|e| format!("invalid --recv-buffer-kb value '{s}': {e}"))?;
    if !(MIN_RECV_BUFFER_KB..=MAX_RECV_BUFFER_KB).contains(&kb) {
        return Err(format!(
            "--recv-buffer-kb must be in {MIN_RECV_BUFFER_KB}..={MAX_RECV_BUFFER_KB} (got {kb})"
        ));
    }
    Ok(kb)
}

/// Parse `--threading-mode <single|multi>` for clap's `value_parser`.
fn parse_threading_mode(s: &str) -> Result<ThreadingMode, String> {
    s.parse::<ThreadingMode>().map_err(|e| e.to_string())
}

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

    /// Threading execution model the variant is asked to use.
    ///
    /// Set by the runner from the expanded `threading_modes` dimension
    /// in TOML config (see `metak-shared/api-contracts/variant-cli.md`
    /// "E14 additions"). Variants declare which modes they support via
    /// `Variant::supported_threading_modes`.
    ///
    /// Optional during the E14 rollout with a default of `single`: the
    /// runner does not yet inject this arg (the runner-side change is
    /// T14.8). Once T14.8 lands and the runner always injects
    /// `--threading-mode`, this becomes effectively required. Existing
    /// variant binaries and runner integration tests that pre-date
    /// T14.1 keep working unchanged because the default preserves the
    /// pre-E14 single-threaded behaviour.
    #[arg(long, value_parser = parse_threading_mode, default_value_t = ThreadingMode::Single)]
    pub threading_mode: ThreadingMode,

    /// OS-level recv buffer size in kibibytes (1024-byte units).
    ///
    /// Default `4096` (4 MiB), range `64..=65536` (64 KiB to 64 MiB).
    /// Variants must call `setsockopt(SO_RCVBUF, recv_buffer_kb * 1024)`
    /// on every recv-side socket they own. Variants whose transport
    /// library does not expose the underlying socket may treat this as
    /// advisory but must still record the value in the `connected`
    /// JSONL event.
    #[arg(long, value_parser = parse_recv_buffer_kb, default_value_t = DEFAULT_RECV_BUFFER_KB)]
    pub recv_buffer_kb: u32,

    /// Cadence in milliseconds at which the variant emits its
    /// stdout progress line (see E15 / T15.1). `0` disables emission
    /// entirely -- the back-compat behaviour for callers that pre-date
    /// E15 (no runner-side stdout reader). Default `1000` (one line
    /// per second) matches the E15 design.
    ///
    /// The emitted line shape is documented in
    /// `metak-shared/api-contracts/variant-cli.md` (E15 additions).
    /// Variant code is responsible for keeping stdout otherwise empty
    /// so the runner can parse the stream as line-delimited JSON.
    #[arg(long, default_value_t = DEFAULT_PROGRESS_STDOUT_INTERVAL_MS)]
    pub progress_stdout_interval_ms: u32,

    /// Variant-side idle-detection threshold in seconds (see E15 / T15.5).
    ///
    /// During the operate phase, if BOTH the local `sent` and `received`
    /// counters have not advanced for this many seconds, the variant
    /// emits `eot_sent` to its JSONL log and transitions internally to
    /// the `silent` phase -- without engaging the on-wire EOT exchange.
    ///
    /// `0` disables variant-side idle detection: only the time-based
    /// `operate_secs` transition fires (pre-E15 behaviour). Default
    /// `5` matches the runner-side `operate_idle_secs` default.
    #[arg(long, default_value_t = DEFAULT_OPERATE_IDLE_SECS)]
    pub operate_idle_secs: u32,

    /// Internal-stall watchdog threshold in seconds (see T15.11).
    ///
    /// A separate OS thread in the variant samples the `sent` and
    /// `received` counters once per second. If BOTH counters remain
    /// flat for this many consecutive seconds while the variant's
    /// phase is `operate`, the watchdog flushes the JSONL logger and
    /// calls `std::process::exit(2)` (the documented
    /// "internal-stall self-exit" code). The runner observes a clean
    /// `failed` outcome with a flushed JSONL and stderr containing
    /// the substring `watchdog: no progress`; analysis classifies the
    /// row as `variant_self_killed_idle`.
    ///
    /// Unlike `operate_idle_secs` (the inline T15.5 detector that runs
    /// on the driver thread), the watchdog runs on its OWN thread and
    /// remains effective when the driver thread is blocked inside a
    /// transport library call. The two detectors are complementary:
    /// `operate_idle_secs` covers the cooperative case (transport
    /// returns control), `watchdog_secs` covers the wedged case
    /// (transport never returns).
    ///
    /// `0` disables the watchdog entirely: no monitor thread is
    /// spawned and no self-exit can fire (pre-T15.11 behaviour).
    /// Default `30` is chosen to win the race against typical
    /// runner safety-net deadlines (the existing stress fixtures
    /// use `default_timeout_secs = 60`); the watchdog must fire well
    /// before the runner's safety-net kill so the JSONL flush has
    /// time to complete.
    #[arg(long, default_value_t = DEFAULT_WATCHDOG_SECS)]
    pub watchdog_secs: u32,

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
            "--threading-mode",
            "single",
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
        assert_eq!(args.threading_mode, ThreadingMode::Single);
        // `--recv-buffer-kb` was not provided -> falls back to the default.
        assert_eq!(args.recv_buffer_kb, DEFAULT_RECV_BUFFER_KB);
    }

    #[test]
    fn parse_threading_mode_multi_and_recv_buffer_override() {
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
            "--threading-mode",
            "multi",
            "--recv-buffer-kb",
            "8192",
        ]);
        assert_eq!(args.threading_mode, ThreadingMode::Multi);
        assert_eq!(args.recv_buffer_kb, 8192);
    }

    #[test]
    fn recv_buffer_kb_rejects_out_of_range() {
        let res = CliArgs::try_parse_from([
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
            "--threading-mode",
            "single",
            "--recv-buffer-kb",
            "0",
        ]);
        assert!(
            res.is_err(),
            "--recv-buffer-kb=0 is below the 64-KiB minimum and must be rejected"
        );
        let res = CliArgs::try_parse_from([
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
            "--threading-mode",
            "single",
            "--recv-buffer-kb",
            "1000000",
        ]);
        assert!(
            res.is_err(),
            "--recv-buffer-kb=1_000_000 exceeds the 64-MiB maximum and must be rejected"
        );
    }

    #[test]
    fn threading_mode_defaults_to_single_during_e14_rollout() {
        // Until T14.8 lands the runner does not inject --threading-mode.
        // To keep existing runner integration tests working, the CLI
        // arg defaults to `single` (the pre-E14 effective behaviour).
        // Once T14.8 lands, the runner always injects this arg.
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
        assert_eq!(args.threading_mode, ThreadingMode::Single);
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
            "--threading-mode",
            "single",
            "--",
            "--zenoh-mode",
            "peer",
        ]);
        assert_eq!(args.variant, "zenoh");
        assert_eq!(args.extra, vec!["--zenoh-mode", "peer"]);
    }
}
