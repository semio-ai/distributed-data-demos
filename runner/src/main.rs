mod analyze;
mod barrier_coord;
mod cli_args;
mod clock_sync;
mod clock_sync_log;
mod config;
mod local_addrs;
mod message;
mod panic_hook;
mod progress;
mod progress_coord;
mod progress_eta;
mod protocol;
mod resume;
mod spawn;
mod spawn_job;
mod termination;

use anyhow::{bail, Result};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Benchmark runner -- coordinates benchmark execution across machines.
#[derive(Parser, Debug)]
#[command(name = "runner")]
struct Cli {
    /// This runner's name (must match one of the names in the config).
    #[arg(long)]
    name: String,

    /// Path to the TOML benchmark config file.
    #[arg(long)]
    config: PathBuf,

    /// UDP coordination port (default: 19876).
    #[arg(long, default_value_t = 19876)]
    port: u16,

    /// Emit verbose clock-sync tracing to stderr.
    ///
    /// When enabled, the runner prints (a) which branch
    /// `is_single_runner()` and `clock_sync_engine()` returned, and (b)
    /// for every datagram received during a probe-wait window, why it was
    /// accepted or rejected (wrong `to`, wrong `from`, wrong `id`, wrong
    /// `t1`, parse failure, or non-Probe variant). Off by default — use
    /// only when diagnosing why a real-LAN run produced empty clock-sync
    /// JSONL files. See `metak-shared/LEARNED.md`, "Diagnosing clock-sync
    /// failure on a real LAN".
    #[arg(long, default_value_t = false)]
    verbose_clock_sync: bool,

    /// Emit verbose coordination-protocol tracing to stderr.
    ///
    /// When enabled, the runner prints one line per inbound coordination
    /// message handled inside `ready_barrier`, `done_barrier`, and
    /// `exchange_resume_manifest` — recording the message type, sender,
    /// variant/run fields, and whether it was accepted or rejected (and
    /// why). Off by default. Used to diagnose mid-run barrier hangs (see
    /// `metak-orchestrator/DECISIONS.md` T-coord.1 entry). The default
    /// path produces no extra output.
    #[arg(long, default_value_t = false)]
    verbose_coord: bool,

    /// Resume an interrupted multi-runner benchmark.
    ///
    /// When set, the runner picks the lexicographically greatest existing
    /// log subfolder under `<base_log_dir>/` whose name starts with
    /// `<bench_config.run>-` (instead of creating a fresh
    /// `<run>-<now-ts>` folder). All runners must be launched with the
    /// same `--resume` flag value or discovery aborts.
    ///
    /// Phase 1.25 (ResumeManifest) then exchanges per-runner inventories
    /// of locally complete spawn jobs across the coordination group. The
    /// run's "skip set" is the intersection of those inventories: jobs
    /// fully complete on every peer are bypassed in Phase 2.
    ///
    /// Empty log files are deleted before the manifest is broadcast (they
    /// represent crashed prior attempts and must be re-run cleanly).
    /// Files for jobs not in the skip set are also deleted before the
    /// upcoming spawn binds them.
    ///
    /// Clock sync is always re-run in resume mode; the clock-sync log is
    /// opened in append mode so prior measurements are preserved.
    #[arg(long, default_value_t = false)]
    resume: bool,

    /// Per-barrier timeout in seconds (default: 120).
    ///
    /// Applies to the ready barrier, done barrier, and the Phase 1.25
    /// ResumeManifest exchange. If any one of these fails to reach quorum
    /// within this duration the runner exits with code 75 (`EX_TEMPFAIL`)
    /// and a clear stderr line describing which barrier and which peers
    /// were still missing. The wrapper scripts in `scripts/` re-launch the
    /// runner with `--resume` appended on exit 75 and propagate every other
    /// non-zero exit unchanged.
    ///
    /// Discovery is intentionally NOT bounded by this timeout — a stuck
    /// discovery is a config/firewall problem (mismatched runner names,
    /// blocked UDP multicast) that retrying will not fix. Only the
    /// post-discovery barriers, where a hang typically means a peer
    /// crashed mid-run, are subject to the timeout.
    #[arg(long, default_value_t = 120)]
    barrier_timeout_secs: u64,

    /// Operate-phase idle threshold in seconds (T15.4 / E15).
    ///
    /// During a spawn's `operate` phase the runner watches the local
    /// `LocalProgressTracker` and every remote peer's `RemoteSpawnSnapshot`.
    /// When local AND every remote peer's variant report no advance on
    /// `(sent, received)` counters for this many seconds, the runner
    /// records "operate done" -- but does NOT signal the variant. The
    /// variant is independently observing the same idle condition (T15.5)
    /// and will transition itself to `silent` then `done`. The runner
    /// keeps polling until the child exits naturally.
    ///
    /// Default `5` matches the variant-side `--operate-idle-secs` default
    /// in `variant-base/src/cli.rs` so cross-runner agreement on idle
    /// fires at roughly the same time as the variant's self-transition.
    #[arg(long, default_value_t = 5)]
    operate_idle_secs: u32,

    /// Per-spawn safety-net wall-clock deadline in seconds (T15.4 / E15).
    ///
    /// Replaces the role of the per-variant `timeout_secs` (and the
    /// top-level `default_timeout_secs`) as the absolute upper bound on
    /// any one spawn. The phase-aware termination state machine (driven
    /// by `--operate-idle-secs` and the variant's own idle detection)
    /// is the primary termination signal; this safety net only fires
    /// when something has gone wrong enough that neither the variant
    /// nor the activity detector ever advance the spawn to `done`.
    ///
    /// Default `300` (5 minutes) is well above the longest plausible
    /// variant-driven phase budget (stabilize + operate + eot + silent)
    /// for any benchmark in the existing fixture set. Operators who
    /// want a tighter bound can shrink it; very long stress runs may
    /// need to raise it.
    #[arg(long, default_value_t = 300)]
    max_spawn_secs: u32,

    /// Override the base log directory for this run (T18.5).
    ///
    /// When set, the runner uses this path as the parent of the
    /// per-run session subfolder for both its own coordination logs
    /// (clock-sync JSONL) AND every spawned variant's `--log-dir`. The
    /// path is cross-platform: UNC paths on Windows (`\\server\share\...`)
    /// and mounted NFS / SMB paths on Linux are treated as opaque
    /// filesystem paths.
    ///
    /// Precedence (highest wins):
    /// 1. `--log-dir <path>` (this CLI flag).
    /// 2. `[runner] log_dir = "..."` in the TOML config.
    /// 3. The first `[variant.common].log_dir` found in the config
    ///    (legacy fallback).
    /// 4. `./logs` (final fallback when nothing else is set).
    ///
    /// The runner validates writability at startup (creates the
    /// directory if missing, writes a tiny probe file, deletes it).
    /// A non-writable path aborts the run before discovery with a
    /// clear error.
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Run the analysis tool over the final log directory after the
    /// matrix completes (T18.6).
    ///
    /// When set, **only the lexicographically lowest-named runner**
    /// (the typical `alice` in an `alice`/`bob` pair) shells out to
    /// `python analysis/analyze.py <log-dir> --summary --dump
    /// --diagrams --output <log-dir>/analysis` after every spawn has
    /// finished and the summary has been printed. The other runners
    /// exit cleanly with no analysis side-effects so concurrent writes
    /// to `<log-dir>/analysis/` are impossible.
    ///
    /// Python interpreter resolution: tries `python3` first, falls
    /// back to `python`; if neither resolves, the analysis is skipped
    /// with a clear warning on the runner's stderr (the benchmark
    /// itself still exits 0 because the matrix succeeded).
    ///
    /// Repo-root detection: walks up from the runner binary location
    /// until it finds `analysis/analyze.py`. Documented in
    /// `runner/CUSTOM.md` "Auto-analysis after the matrix".
    ///
    /// A non-zero Python exit is surfaced as a runner-level warning,
    /// not a hard failure — the benchmark already succeeded.
    #[arg(long, default_value_t = false)]
    analyze_full: bool,

    /// Variant-specific arg passthrough: `--variant-arg <selector>.<key>=<value>` (T9.5 / T9.5a).
    ///
    /// The `<selector>` is glob-matched against `[[variant]].name`
    /// (post-template-resolution, pre-array-expansion). `*` matches
    /// zero-or-more characters, `?` matches exactly one. No character
    /// classes, no escape sequences. A selector with no glob
    /// metacharacters is a literal full-string match.
    ///
    /// Repeatable. Each entry is split on the first `.` (selector / key
    /// boundary) and the first `=` (key / value boundary). At spawn time
    /// the parsed entries are walked in CLI order and every entry whose
    /// selector matches the spawn's source variant name contributes its
    /// `(key, value)` to a per-spawn override map; later entries overwrite
    /// earlier ones on key conflict (so `'*.X=default'` followed by
    /// `'zenoh-*.X=override'` gives the override on zenoh-* spawns and the
    /// default elsewhere). The resolved map is then merged into the
    /// `[variants.<variant>.specific]` table: CLI keys win over TOML keys
    /// on conflict, CLI-only keys are appended. The runner forwards the
    /// merged values verbatim through the variant CLI -- it does NOT
    /// interpret the key names beyond split-and-forward. Variants validate
    /// their own arg values.
    ///
    /// Example:
    ///   --variant-arg 'zenoh-*.multicast_interface=192.168.1.68'
    ///   --variant-arg '*.workload_seed=42'
    ///
    /// Quote the selector when it contains `*` or `?` so the shell does
    /// not expand the glob against the local filesystem (matters on
    /// PowerShell and POSIX shells alike).
    ///
    /// Filed as T9.5 (literal selector) and widened to glob in T9.5a.
    #[arg(long, action = clap::ArgAction::Append)]
    variant_arg: Vec<String>,
}

/// Exit code returned to the OS when a coordination barrier hits its timeout.
///
/// 75 is `EX_TEMPFAIL` from `<sysexits.h>` — "service unavailable, retry
/// later". Picked because (a) it is a stable, well-known transient-failure
/// code, (b) it is unlikely to collide with whatever a variant binary might
/// use to signal real failure (variants exit 0/1/2 in practice), and (c) the
/// wrapper scripts use it as the single signal to re-launch with `--resume`.
/// Any other non-zero exit (panic, config error, variant failure, child
/// timeout) propagates as-is and stops the wrapper loop.
pub const EX_TEMPFAIL: i32 = 75;

/// Build identifier baked in at compile time by `../build_info.rs`.
///
/// Printed once on startup so version skew between machines is visible at
/// a glance. See `metak-orchestrator/STATUS.md` for the post-mortem of the
/// stale-runner-binary incident on machine B that motivated this banner.
const BUILD_GIT_SHA: &str = env!("BUILD_GIT_SHA");
const BUILD_GIT_DIRTY: &str = env!("BUILD_GIT_DIRTY");
const BUILD_RUSTC: &str = env!("BUILD_RUSTC");

fn main() {
    let cli = Cli::parse();

    // Install a process-wide panic hook BEFORE anything else can run so
    // any thread panic — main or background — surfaces a clearly labelled
    // stderr line AND immediately kills the process. Motivated by an
    // operator report (2026-05-21) where two runners on a multi-hour
    // matrix exited silently between the `spawning '...'` and `final
    // progress:` lines for the second spawn; both terminals returned to
    // the shell prompt with no panic message, no FATAL line, no anyhow
    // `Error:` output, and no `finished:` line. Whether the silent exit
    // was a background thread panic that left the main thread in an
    // unrecoverable state, an OS-level kill, or an as-yet-unidentified
    // panic path inside `spawn_and_monitor`, this hook converts the
    // entire class of "silent runner disappearance" into a loud,
    // attributable `[runner:<name>] PANIC:` line followed by an
    // immediate `process::abort()`. The abort path is preferred over
    // letting the thread unwind and silently dying:
    //
    // - A panicking background thread (progress_coord reader, barrier_coord
    //   reader, stdout-progress reader) by default only kills that thread
    //   and leaves the main loop intact — but if the main loop later
    //   waits on a joined thread's data, the wedge can be invisible.
    //   `abort()` from the hook turns this into an immediate hard exit,
    //   never silent.
    // - The hook prints the panic payload, the location, AND the
    //   thread name (`<unnamed>` when not set) so an operator collecting
    //   stderr from multiple runners knows which runner died and where.
    // - `process::abort()` produces a non-zero exit code that is NOT 75
    //   (`EX_TEMPFAIL`). The wrapper scripts re-launch with --resume
    //   ONLY on exit 75, so an aborted runner stops the wrapper loop
    //   and forces operator attention — exactly the right behaviour for
    //   a real panic.
    //
    // Wired before the build banner so a panic during CLI parse (e.g.
    // a future feature that allocates inside `Cli::parse`) is still
    // attributable. The hook reads `cli.name` so it is wired *after*
    // `Cli::parse()` but *before* the banner.
    panic_hook::install_panic_hook(cli.name.clone());

    // Print the build banner immediately after CLI parse, before any
    // discovery/protocol work. The label is `runner:<name>` so the line
    // is attributable when stdout/stderr from multiple runners are
    // collected in one place.
    let dirty = BUILD_GIT_DIRTY == "true";
    let dirty_suffix = if dirty { "+dirty" } else { "" };
    eprintln!(
        "[runner:{}] build: {}{} (rustc {})",
        cli.name, BUILD_GIT_SHA, dirty_suffix, BUILD_RUSTC
    );

    // Run the actual benchmark, intercepting `BarrierTimeoutError` so we
    // can map it to exit code 75 (EX_TEMPFAIL) for the wrapper scripts.
    // Any other anyhow::Error is printed with an explicit `FATAL:`
    // prefix and exits with code 1 so the operator never has to wonder
    // whether the runner died with no signal at all. (Pre-2026-05-21
    // this path returned `Err(e)` from `main()` and relied on the Rust
    // runtime's default `Debug` impl to print `Error: ...`. That
    // implicit path was the suspected origin of one of the silent-exit
    // hypotheses; making the print explicit and labelled removes the
    // ambiguity.)
    match run(&cli) {
        Ok(()) => {}
        Err(e) => {
            if let Some(bt) = e.downcast_ref::<protocol::BarrierTimeoutError>() {
                // Single, specific stderr line so the wrapper logs are
                // greppable. The runner has already cleaned up any in-flight
                // child process: spawn::spawn_and_monitor is synchronous,
                // so by the time we are inside a barrier the child has
                // already exited (ready barrier: not yet spawned; done
                // barrier: already collected). No orphan to kill.
                eprintln!(
                    "[runner:{}] FATAL: {} — exiting {} (EX_TEMPFAIL); wrapper should retry with --resume",
                    cli.name, bt, EX_TEMPFAIL
                );
                std::process::exit(EX_TEMPFAIL);
            }
            // Any other anyhow::Error: print loudly and exit non-zero.
            // `{:#}` collapses the anyhow chain onto one line so the
            // FATAL line stays grep-friendly. The exit code is 1 so it
            // is distinguishable from both EX_TEMPFAIL (75) and the
            // any-variant-failed end-of-matrix exit (also 1, but reached
            // only after the summary table is printed -- so an operator
            // seeing exit=1 without a summary table knows it is this
            // pre-summary failure path).
            eprintln!("[runner:{}] FATAL: {e:#}", cli.name);
            std::process::exit(1);
        }
    }
}

/// Body of the runner. Extracted from `main` so the `BarrierTimeoutError`
/// interception in `main` has a single place to catch it.
fn run(cli: &Cli) -> Result<()> {
    // Wire the process-wide clock-sync verbose toggle so the engine and
    // coordinator emit per-datagram traces while diagnosing field issues.
    clock_sync::set_verbose(cli.verbose_clock_sync);

    // Wire the process-wide coordination-protocol verbose toggle so the
    // barrier loops emit per-message traces while diagnosing barrier hangs.
    protocol::set_verbose_coord(cli.verbose_coord);

    let barrier_timeout = Duration::from_secs(cli.barrier_timeout_secs);
    eprintln!(
        "[runner:{}] barrier timeout: {}s",
        cli.name, cli.barrier_timeout_secs
    );

    // T9.5 / T9.5a: parse the per-variant `--variant-arg
    // <selector>.<key>=<value>` overrides up front so a malformed entry
    // aborts before discovery. Returns an empty Vec when no flags were
    // given (the typical case).
    //
    // The selector is glob-matched against `[[variant]].name` at spawn
    // time; CLI order is preserved here and resolution (per-spawn) is
    // last-CLI-position-wins. The startup banner emits one line per CLI
    // entry, naming the selector verbatim, so an operator can confirm
    // the glob they typed is what the runner saw (a frequent T9.5 trap
    // was a silently-dropped literal selector — making the selector
    // visible up-front closes that gap).
    let variant_arg_overrides = cli_args::parse_variant_arg_overrides(&cli.variant_arg)?;
    for entry in &variant_arg_overrides {
        eprintln!(
            "[runner:{}] --variant-arg selector '{}': {}={}",
            cli.name,
            entry.selector,
            entry.key,
            cli_args::toml_value_to_string(&entry.value)
        );
    }

    // Load and validate config.
    let (bench_config, config_hash) = config::BenchConfig::from_file(&cli.config)?;

    // Validate that --name is in the runners list.
    if !bench_config.runners.contains(&cli.name) {
        bail!(
            "runner name '{}' is not in the config runners list: {:?}",
            cli.name,
            bench_config.runners
        );
    }

    // Validate that all variant binaries exist.
    for v in &bench_config.variant {
        if !std::path::Path::new(&v.binary).exists() {
            bail!("variant '{}' binary not found: {}", v.name, v.binary);
        }
    }

    eprintln!(
        "[runner:{}] config loaded: run={}, {} variant(s), {} runner(s), hash={}",
        cli.name,
        bench_config.run,
        bench_config.variant.len(),
        bench_config.runners.len(),
        &config_hash[..12]
    );

    // Resolve base log directory up front — both fresh-mode subfolder
    // generation and resume-mode latest-folder selection need it. T18.5
    // precedence:
    //   1. `--log-dir <path>` CLI flag (operator override; highest).
    //   2. `[runner] log_dir = "..."` TOML key.
    //   3. First `[variant.common].log_dir` found in the config
    //      (legacy fallback that pre-dates T18.5).
    //   4. `./logs` (final fallback for ad-hoc single-runner runs).
    //
    // The chosen value is then writability-probed (create_dir_all + write
    // a tiny probe file + delete) so a typo'd UNC path or unmounted NFS
    // mount fails fast BEFORE discovery, with a clear error message.
    let (base_log_dir, base_log_dir_source) = if let Some(cli_path) = cli.log_dir.as_ref() {
        (cli_path.to_string_lossy().to_string(), "--log-dir CLI flag")
    } else if let Some(runner_path) = bench_config.runner_log_dir() {
        (runner_path.to_string(), "[runner] log_dir TOML key")
    } else if let Some(variant_path) = bench_config
        .variant
        .iter()
        .find_map(|v| v.common.get("log_dir"))
        .map(cli_args::toml_value_to_string)
    {
        (variant_path, "[variant.common].log_dir (legacy fallback)")
    } else {
        ("./logs".to_string(), "default './logs'")
    };
    eprintln!(
        "[runner:{}] base log dir: {} (source: {})",
        cli.name, base_log_dir, base_log_dir_source
    );

    // Validate the chosen base log dir is writable. Cross-platform: works for
    // UNC paths (Windows), mounted NFS / SMB (Linux), local disk, etc. Fails
    // fast with a clear error if create_dir_all or the probe write fails.
    if let Err(e) = config::validate_log_dir_writable(std::path::Path::new(&base_log_dir)) {
        bail!(
            "log directory writability check failed: {e:#} \
             (source: {base_log_dir_source}; \
             ensure the path exists or can be created and is writable by this process)"
        );
    }

    // --analyze-full prereq check (fast-fail before discovery / matrix).
    //
    // Motivation: the analyzer needs polars / matplotlib / psutil. Before
    // this check existed, a missing `polars` install was not discovered
    // until AFTER the matrix completed -- e.g. a 2-hour benchmark followed
    // by a `ModuleNotFoundError`. We now run the import probe at startup
    // and abort with a clear message before the runner even reaches
    // discovery, so the operator fixes the issue in seconds rather than
    // hours.
    //
    // Gating: only the runner that will actually invoke the analyzer
    // (`should_run_analysis` -> lexicographically lowest name) runs the
    // probe. The other runner(s) print a one-line note and proceed.
    // Rationale: if alice is the analyzer and only alice has polars
    // installed, bob should not abort just because his Python lacks it.
    if cli.analyze_full {
        if analyze::should_run_analysis(&cli.name, &bench_config.runners) {
            if let Err(msg) = analyze::check_analysis_prereqs() {
                bail!(msg);
            }
        } else {
            eprintln!(
                "[runner:{}] --analyze-full set; skipping prereq check (not the analysis runner)",
                cli.name
            );
        }
    }

    // Generate a proposed log subfolder name before discovery so it can be
    // negotiated with other runners. The leader (first in the runners list)
    // decides the final name so all runners use the same subfolder.
    //
    // Fresh mode: <bench_config.run>-<now-ts>.
    // Resume mode: lexicographically greatest existing <run>-* subfolder.
    let proposed_log_subdir = if cli.resume {
        let base = std::path::Path::new(&base_log_dir);
        match resume::find_latest_log_subdir(base, &bench_config.run) {
            Ok(name) => {
                eprintln!(
                    "[runner:{}] resume: selected latest log subfolder '{}' under {}",
                    cli.name, name, base_log_dir
                );
                name
            }
            Err(e) => {
                bail!(
                    "resume mode: could not select an existing log subfolder under '{}': {e:#}",
                    base_log_dir
                );
            }
        }
    } else {
        let run_ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        format!("{}-{}", bench_config.run, run_ts)
    };

    // Create coordinator and run discovery.
    let coordinator = protocol::Coordinator::new(
        cli.name.clone(),
        &bench_config.runners,
        config_hash,
        cli.port,
        proposed_log_subdir,
        bench_config.run.clone(),
        cli.resume,
    )?;

    eprintln!("[runner:{}] starting discovery...", cli.name);
    let log_subdir = coordinator.discover()?;
    eprintln!("[runner:{}] discovery complete", cli.name);

    eprintln!("[runner:{}] log subfolder: {}", cli.name, log_subdir);

    // Snapshot the peer host map captured during discovery. This is passed
    // to every variant spawn as `--peers name=host,...` (sorted by name).
    let peer_hosts = coordinator.peer_hosts();
    eprintln!("[runner:{}] peer_hosts: {:?}", cli.name, peer_hosts);

    // Resolve the per-run log directory used for the clock-sync JSONL file
    // and the resume-mode log inventory. The base directory was resolved
    // before discovery so resume's latest-folder picker could use it.
    let run_log_dir: PathBuf = PathBuf::from(format!("{base_log_dir}/{log_subdir}"));

    // Resume mode: verify that the agreed log subfolder exists locally. If
    // the leader's pick differs from this runner's latest folder name, the
    // follower aborts with a clear error. Single-runner resume runs always
    // pass this check because the proposal is this runner's own pick.
    if cli.resume && !run_log_dir.exists() {
        bail!(
            "resume mode: agreed log subfolder '{}' does not exist locally at {} \
             (the leader's proposed folder is not present on this runner — abort)",
            log_subdir,
            run_log_dir.display()
        );
    }

    // Expand the variant config into the same ordered list of spawn jobs that
    // Phase 2 will iterate. Done once up front so resume-mode inventory and
    // the Phase 2 loop agree on the `effective_name` set.
    //
    // T14.8 capability gating runs HERE, before resume-inventory and Phase 2,
    // so the skip set / barrier identifiers / log files are all aligned on
    // the post-gating job list. Per-variant entries declare their supported
    // threading modes via `[[variant]].supported_modes = [...]` (Option A
    // -- static TOML declaration; see CUSTOM.md "Threading-mode dimension"
    // for the rationale and the permissive default for entries that omit
    // the field).
    let all_jobs = expand_and_gate_jobs(&bench_config, &cli.name, |line| eprintln!("{line}"))?;
    let all_effective_names: Vec<String> = all_jobs
        .iter()
        .map(|(_, j)| j.effective_name.clone())
        .collect();

    // Phase 1.25 — Resume Inventory. Compute the local manifest, broadcast
    // it, collect every peer's, intersect, and clean up incomplete files.
    // Returns the "skip set" used in Phase 2. In fresh mode this whole
    // section is bypassed — `skip_set` is just an empty set.
    let skip_set: std::collections::HashSet<String> = if cli.resume {
        let local = resume::compute_local_manifest(
            &run_log_dir,
            &cli.name,
            &bench_config.run,
            &all_effective_names,
        );
        for path in &local.deleted_empty {
            eprintln!(
                "[runner:{}] resume: deleted empty log {}",
                cli.name,
                path.display()
            );
        }
        for path in &local.deleted_partial {
            eprintln!(
                "[runner:{}] resume: deleted partial log (crashed mid-spawn, no EOT marker) {}",
                cli.name,
                path.display()
            );
        }
        eprintln!(
            "[runner:{}] resume: local manifest has {} complete job(s)",
            cli.name,
            local.complete_jobs.len()
        );

        // Exchange manifests with peers. Single-runner mode short-circuits
        // and returns just our own. A barrier timeout here propagates as a
        // `BarrierTimeoutError` and is caught at the top of `main` to map
        // to exit code 75 — do NOT wrap it in another anyhow::anyhow! that
        // would erase the type and prevent that downcast.
        let manifests =
            coordinator.exchange_resume_manifest(local.complete_jobs.clone(), barrier_timeout)?;

        let inter = resume::intersect_complete_jobs(&manifests, &bench_config.runners);
        eprintln!(
            "[runner:{}] resume: skip set has {} job(s) (intersection of {} peer manifest(s))",
            cli.name,
            inter.len(),
            manifests.len()
        );

        // Cleanup: delete this runner's log files for every job NOT in the
        // skip set so the upcoming spawn writes into a clean file.
        let deleted = resume::cleanup_incomplete_logs(
            &run_log_dir,
            &cli.name,
            &bench_config.run,
            &all_effective_names,
            &inter,
        );
        for path in &deleted {
            eprintln!(
                "[runner:{}] resume: deleted incomplete log {}",
                cli.name,
                path.display()
            );
        }

        inter
    } else {
        std::collections::HashSet::new()
    };

    // Open the clock-sync log file (skipped in single-runner mode -- no peers
    // means no sync events would ever be written, and the contract permits
    // an absent file in that case). Emit a visible log line either way so an
    // operator can confirm from stdout/stderr which branch was taken (T8.5).
    let single_runner = coordinator.is_single_runner();
    if single_runner {
        eprintln!(
            "[runner:{}] skipping clock-sync: single-runner mode (no peers in config; \
             single_runner=true). No clock-sync log file will be created.",
            cli.name
        );
    } else if cli.verbose_clock_sync {
        eprintln!(
            "[runner:{}] clock-sync: multi-runner mode, runners={:?}",
            cli.name, bench_config.runners
        );
    }
    let mut clock_sync_log = if !single_runner {
        std::fs::create_dir_all(&run_log_dir).ok();
        match clock_sync_log::open_clock_sync_log(&run_log_dir, &cli.name, &bench_config.run) {
            Ok(l) => {
                eprintln!(
                    "[runner:{}] clock-sync log opened at {}",
                    cli.name,
                    run_log_dir.display()
                );
                Some(l)
            }
            Err(e) => {
                eprintln!(
                    "[runner:{}] WARN: failed to open clock-sync log: {e:#}",
                    cli.name
                );
                None
            }
        }
    } else {
        None
    };

    // Build the clock-sync engine. None in single-runner mode (no socket).
    let clock_sync_engine = coordinator.clock_sync_engine();
    if cli.verbose_clock_sync {
        eprintln!(
            "[runner:{}] clock_sync_engine() returned {} (None means no socket -> single-runner)",
            cli.name,
            if clock_sync_engine.is_some() {
                "Some(engine)"
            } else {
                "None"
            }
        );
    }

    // Phase 1.5: initial clock sync. Logged with `variant=""`.
    let peer_names: Vec<String> = bench_config
        .runners
        .iter()
        .filter(|n| *n != &cli.name)
        .cloned()
        .collect();

    // Track which peers the initial sync produced no samples for. T8.5:
    // initial-sync zero-sample is FATAL because cross-machine latency
    // numbers without an offset correction are statistically meaningless.
    // Per-variant resyncs may still be soft warnings (analysis falls back
    // to the most recent valid measurement).
    let mut initial_failed_peers: Vec<String> = Vec::new();

    if let (Some(engine), Some(log)) = (clock_sync_engine.as_ref(), clock_sync_log.as_mut()) {
        if !peer_names.is_empty() {
            eprintln!(
                "[runner:{}] initial clock sync against {} peer(s)...",
                cli.name,
                peer_names.len()
            );
            let measurements = engine.measure_offsets(&peer_names, clock_sync::DEFAULT_SAMPLES);
            for peer in &peer_names {
                let pm = measurements.get(peer);
                let (m_opt, attempts) = match pm {
                    Some(pm) => (pm.measurement.as_ref(), pm.attempts.as_slice()),
                    None => (None, &[][..]),
                };
                if let Err(e) = log.write("", peer, m_opt, attempts) {
                    eprintln!(
                        "[runner:{}] WARN: clock-sync log write failed: {e:#}",
                        cli.name
                    );
                }
                if let Some(m) = m_opt {
                    eprintln!(
                        "[runner:{}] clock_sync (initial) peer={peer} offset_ms={:.3} rtt_ms={:.3}",
                        cli.name, m.offset_ms, m.rtt_ms
                    );
                } else {
                    eprintln!(
                        "[runner:{}] WARN: no clock-sync samples received from peer={peer}",
                        cli.name
                    );
                    initial_failed_peers.push(peer.clone());
                }
            }
        }
    } else if !single_runner && !peer_names.is_empty() {
        // The engine or the log slot is None despite multi-runner mode and a
        // non-empty peer list -- that means open_clock_sync_log failed. Treat
        // this as fatal: the run would silently produce uncorrected data.
        bail!(
            "clock-sync was enabled (multi-runner, {} peer(s)) but the engine or log slot is \
             unavailable; refusing to start to avoid producing uncorrected cross-machine data. \
             Re-run with --verbose-clock-sync for diagnostics.",
            peer_names.len()
        );
    }

    // T8.5 fail-fast: if the initial sync produced zero samples for any
    // expected peer, abort with non-zero exit BEFORE the first ready
    // barrier. This guarantees we never produce a benchmark run whose
    // cross-machine latency numbers are uncorrected by an undetected silent
    // failure. (Per-variant resyncs remain soft warnings -- analysis can
    // fall back to the most recent valid measurement; only the initial
    // sync is load-bearing for correctness.)
    if let Err(e) = require_initial_sync_complete(&initial_failed_peers) {
        eprintln!(
            "[runner:{}] FATAL: initial clock-sync produced zero samples for peer(s): {:?}.",
            cli.name, initial_failed_peers
        );
        eprintln!(
            "[runner:{}]        Cross-machine latencies cannot be corrected without an offset \
             measurement.",
            cli.name
        );
        eprintln!(
            "[runner:{}]        Re-run with --verbose-clock-sync for per-datagram tracing, and \
             see metak-shared/LEARNED.md (\"Diagnosing clock-sync failure on a real LAN\").",
            cli.name
        );
        return Err(e);
    }

    let inter_qos_grace = Duration::from_millis(bench_config.inter_qos_grace_ms());

    // T15.3: start the per-peer progress-update exchange. Each runner
    // opens a long-lived TCP connection to every other runner over a
    // dedicated port range (base + PROGRESS_TCP_OFFSET + index) and
    // exchanges `ProgressUpdate` frames during Phase 2. This is the
    // receive-side counterpart of the per-spawn snapshot we publish on
    // each tick from inside the spawn loop below. In single-runner mode
    // `start()` is a no-op.
    let remote_view = progress::RemoteProgressViewHandle::new();
    let progress_coord = progress_coord::ProgressCoordinator::new(
        cli.name.clone(),
        bench_config.runners.clone(),
        cli.port,
        remote_view.clone(),
    );
    if let Err(e) = progress_coord.start(&peer_hosts) {
        // Failure to start the progress channel is non-fatal -- T15.4's
        // safety-net `max_spawn_secs` still bounds a stuck spawn. Log
        // and continue.
        eprintln!(
            "[runner:{}] progress_coord: start failed: {e:#}; \
             proceeding without cross-runner progress visibility",
            cli.name
        );
    } else if !progress_coord.is_single_runner() {
        eprintln!(
            "[runner:{}] progress_coord: started ({} peer(s) connected)",
            cli.name,
            remote_view.snapshot().peer_count().max(
                // peers connect on both sides; peer_count() may briefly
                // be zero before any frame has been folded in, so print
                // the configured peer count as the operator-facing
                // upper bound.
                bench_config
                    .runners
                    .iter()
                    .filter(|n| *n != &cli.name)
                    .count()
            )
        );
    }

    // T15.10: start the per-peer barrier exchange. Each runner opens
    // a long-lived TCP connection to every other runner over a
    // dedicated port range (base + BARRIER_TCP_OFFSET + index) and
    // exchanges `Ready` / `Done` frames during Phase 2. Replaces the
    // pre-T15.10 UDP multicast barrier whose datagram-loss-under-
    // pressure failure mode was the root cause of the
    // configs/two-runner-stress-e14.toml mid-run timeouts. The UDP
    // socket remains in use for clock-sync, probe responses,
    // discovery, and the legacy stale-done / late-discover recovery
    // re-emission paths. In single-runner mode `start()` is a no-op.
    let barrier_coord = std::sync::Arc::new(barrier_coord::BarrierCoordinator::new(
        cli.name.clone(),
        bench_config.runners.clone(),
        cli.port,
    ));
    if let Err(e) = barrier_coord.start(&peer_hosts) {
        // Failure to bind the listener is fatal -- without the TCP
        // barrier channel we would silently fall back to the legacy
        // UDP path which exhibits the very loss pattern T15.10
        // fixes. Surface the error and exit; the wrapper will not
        // retry because this is not EX_TEMPFAIL (port collision /
        // permission, not a transient peer condition).
        bail!(
            "barrier_coord: failed to start TCP barrier transport: {e:#}. \
             T15.10 expects every runner to bind base_port + 96 + index; \
             check for port collisions or firewall rules."
        );
    }
    if !barrier_coord.is_single_runner() {
        let connected = barrier_coord.connected_peers();
        eprintln!(
            "[runner:{}] barrier_coord: started ({} peer(s) connected: {:?})",
            cli.name,
            connected.len(),
            connected
        );
    }
    coordinator.install_barrier_coordinator(barrier_coord.clone());

    // Track results for summary table.
    let mut summary: Vec<SummaryRow> = Vec::new();

    // T-ux.1: precompute the nominal wall-clock cost (per spawn) for the
    // progress + ETA line printed after every spawn. Skipped jobs in the
    // resume skip set contribute 0 -- they don't take real wall-clock to
    // "complete" and must not bias the overhead correction.
    //
    // The total nominal sum is captured for `nominal_remaining` bookkeeping;
    // the per-job vector is indexed in lockstep with the spawn loop below.
    let grace_ms = bench_config.inter_qos_grace_ms();
    let nominal_per_job: Vec<Duration> = all_jobs
        .iter()
        .map(|(src_idx, job)| {
            if skip_set.contains(&job.effective_name) {
                Duration::ZERO
            } else {
                progress_eta::spawn_nominal_duration(&bench_config.variant[*src_idx], grace_ms)
            }
        })
        .collect();
    let nominal_total: Duration = nominal_per_job.iter().copied().sum();
    let mut nominal_so_far = Duration::ZERO;

    // T-ux.1: wall-clock anchor for "elapsed" in the progress+ETA line.
    // Captured here at the TOP of the spawn loop, before the first ready
    // barrier of Phase 2, so discovery + initial clock-sync time is NOT
    // mixed into the predictive elapsed/ETA arithmetic. (Those phases
    // happen once per run, not per spawn, and have no predictive value.)
    let spawn_loop_start = Instant::now();
    let total_jobs = all_jobs.len();

    // Count, per source variant entry, how many jobs are after the current
    // one (so the inter-spawn grace fires only between consecutive non-
    // skipped pairs from the same entry). The slice [src_idx..] is enough
    // because we know all_jobs are grouped by source_index in stable order.
    //
    // The grace rule (judgment call documented in the completion report):
    // we only sleep AFTER an actual spawn AND when there is a remaining
    // non-skipped job from the same source entry. This avoids burning grace
    // periods on long stretches of skipped jobs but still lets sockets
    // release between consecutive real spawns.
    //
    // Execute each spawn job in stable order. Each [[variant]] expands into
    // one or more spawn jobs across the Cartesian product of its
    // tick_rate_hz, values_per_tick, and qos dimensions. Jobs from one
    // entry run sequentially in stable ascending order (hz outer, vpt
    // middle, qos inner). In resume mode, jobs whose effective_name is in
    // the skip set bypass ready barrier, spawn, per-variant resync, and
    // done barrier entirely.
    let mut last_spawn_was_real_in_entry: HashMap<usize, bool> = HashMap::new();
    for (job_idx_in_all, (src_idx, job)) in all_jobs.iter().enumerate() {
        let variant = &bench_config.variant[*src_idx];
        let timeout_secs = variant.effective_timeout(bench_config.default_timeout_secs);

        // Resume skip path: bypass everything for jobs in the skip set.
        if skip_set.contains(&job.effective_name) {
            eprintln!(
                "[runner:{}] skipping '{}' (resume: complete on all peers)",
                cli.name, job.effective_name
            );
            // Skipped jobs count as success for the local runner only;
            // remote runners would also have skipped this same job under
            // the cross-runner intersection rule, so we do not synthesize
            // peer rows here.
            summary.push(SummaryRow {
                variant: job.effective_name.clone(),
                runner: cli.name.clone(),
                status: "skipped".to_string(),
                exit_code: 0,
            });
            // Mark that the most recent action in this entry was NOT a real
            // spawn so the grace check below does not fire on the next real
            // spawn either (no socket actually held a port).
            last_spawn_was_real_in_entry.insert(*src_idx, false);

            // T-ux.1: emit the progress + ETA line for the skipped job
            // too. A run that resumes through a burst of skipped spawns
            // should still show `progress: i/total done` rather than
            // stay silent until the first real spawn finishes.
            nominal_so_far += nominal_per_job[job_idx_in_all];
            let completed = job_idx_in_all + 1;
            emit_progress_eta(
                &cli.name,
                completed,
                total_jobs,
                spawn_loop_start.elapsed(),
                nominal_so_far,
                nominal_total.saturating_sub(nominal_so_far),
            );
            continue;
        }

        // If a previous real spawn from this same source entry is the most
        // recent action AND there are more spawn jobs queued from this same
        // entry, we should have applied a grace AFTER that previous spawn.
        // Apply it here, just before the next real spawn from the same entry.
        if last_spawn_was_real_in_entry
            .get(src_idx)
            .copied()
            .unwrap_or(false)
            && !inter_qos_grace.is_zero()
        {
            std::thread::sleep(inter_qos_grace);
        }

        eprintln!(
            "[runner:{}] ready barrier for spawn '{}' (hz={}, vpt={}, qos={})",
            cli.name, job.effective_name, job.tick_rate_hz, job.values_per_tick, job.qos
        );
        coordinator.ready_barrier(&job.effective_name, barrier_timeout)?;

        // Per-variant clock resync: catches drift across the run. Logged
        // with the spawn's effective name so analysis joins the latest
        // measurement preceding the variant's writes. No-op in
        // single-runner mode (engine/log are None). Per-variant zero-
        // sample is a soft warning, NOT fatal -- the most recent valid
        // measurement (the initial sync, or a successful prior resync)
        // remains available to analysis.
        if let (Some(engine), Some(log)) = (clock_sync_engine.as_ref(), clock_sync_log.as_mut()) {
            if !peer_names.is_empty() {
                let measurements = engine.measure_offsets(&peer_names, clock_sync::DEFAULT_SAMPLES);
                for peer in &peer_names {
                    let pm = measurements.get(peer);
                    let (m_opt, attempts) = match pm {
                        Some(pm) => (pm.measurement.as_ref(), pm.attempts.as_slice()),
                        None => (None, &[][..]),
                    };
                    if let Err(e) = log.write(&job.effective_name, peer, m_opt, attempts) {
                        eprintln!(
                            "[runner:{}] WARN: clock-sync log write failed: {e:#}",
                            cli.name
                        );
                    }
                    if let Some(m) = m_opt {
                        eprintln!(
                            "[runner:{}] clock_sync ({}) peer={peer} offset_ms={:.3} rtt_ms={:.3}",
                            cli.name, job.effective_name, m.offset_ms, m.rtt_ms
                        );
                    } else {
                        eprintln!(
                            "[runner:{}] WARN: no clock-sync samples received from peer={peer} for variant {}",
                            cli.name, job.effective_name
                        );
                    }
                }
            }
        }

        let launch_ts = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.9fZ")
            .to_string();

        // Resolve the log directory the variant child writes JSONL into.
        // T18.5 precedence:
        //   - If the runner has a base override (--log-dir or [runner].log_dir),
        //     ALWAYS use it (with the session subfolder appended) so the
        //     variant writes alongside the runner's own coordination logs.
        //   - Otherwise fall back to `[variant.common].log_dir` if the variant
        //     declared one (legacy pre-T18.5 behaviour).
        //   - Otherwise (no variant log_dir either) we pass `None` and the
        //     variant uses its own CLI default. The default-`./logs` branch
        //     above already handled the operator-facing "where does this
        //     end up" question.
        let runner_override_base = cli.log_dir.is_some() || bench_config.runner_log_dir().is_some();
        let log_dir_resolved = if runner_override_base {
            Some(format!("{base_log_dir}/{log_subdir}"))
        } else {
            variant.common.get("log_dir").map(|log_dir_val| {
                let base = cli_args::toml_value_to_string(log_dir_val);
                format!("{}/{}", base, log_subdir)
            })
        };

        // T9.5 / T9.5a: resolve the per-variant `--variant-arg` overrides
        // for this spawn. The lookup uses the source `variant.name`
        // (post-template-resolution, pre-array-expansion), NOT the
        // effective_name which carries `-qos<N>` / `-<vpt>x<hz>hz`
        // suffixes -- the CLI flag is variant-typed, not spawn-typed.
        //
        // `resolve_for_variant` walks every CLI entry in order and
        // glob-matches its selector against `variant.name`; later
        // entries overwrite earlier ones on key conflict.
        let resolved_with_selectors =
            cli_args::resolve_for_variant(&variant_arg_overrides, &variant.name);
        let cli_overrides_for_spawn = cli_args::drop_selector_provenance(&resolved_with_selectors);
        let cli_overrides_arg = if cli_overrides_for_spawn.is_empty() {
            None
        } else {
            Some(&cli_overrides_for_spawn)
        };

        let args = cli_args::build_variant_args(
            variant,
            &bench_config.run,
            &cli.name,
            &launch_ts,
            log_dir_resolved.as_deref(),
            &job.effective_name,
            job.qos,
            job.tick_rate_hz,
            job.values_per_tick,
            job.threading_mode,
            job.recv_buffer_kb,
            &peer_hosts,
            cli_overrides_arg,
        );

        // T9.5 / T9.5a: provenance log line so operators see exactly which
        // specific args were effective for this spawn and where each
        // value came from. CLI-sourced keys carry the selector that
        // contributed them so the user can see e.g.
        // `multicast_interface=192.168.1.80 (cli: zenoh-*)`. Suppressed
        // when there are no specific args at all (don't be noisy on
        // variants without `[variant.specific]` and no matching
        // `--variant-arg`).
        let cli_overrides_with_selectors_arg = if resolved_with_selectors.is_empty() {
            None
        } else {
            Some(&resolved_with_selectors)
        };
        let provenance = cli_args::specific_arg_provenance(
            variant.specific.as_ref(),
            cli_overrides_with_selectors_arg,
        );
        if !provenance.is_empty() {
            let rendered = provenance
                .iter()
                .map(|(k, v, p, sel)| match p {
                    cli_args::SpecificArgProvenance::Toml => format!("{}={} (toml)", k, v),
                    cli_args::SpecificArgProvenance::Cli => match sel {
                        Some(s) => format!("{}={} (cli: {})", k, v, s),
                        // Defensive: a Cli provenance entry should always
                        // carry the selector that produced it. If for any
                        // reason it does not, fall back to the bare `(cli)`
                        // tag rather than panicking.
                        None => format!("{}={} (cli)", k, v),
                    },
                })
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "[runner:{}] spawn '{}' specific args: {}",
                cli.name, job.effective_name, rendered
            );
        }

        eprintln!(
            "[runner:{}] spawning '{}' (hz={}, vpt={}, qos={}, timeout: {}s)",
            cli.name,
            job.effective_name,
            job.tick_rate_hz,
            job.values_per_tick,
            job.qos,
            timeout_secs
        );

        // Per-spawn stderr capture. The child's stderr is redirected to
        // `<log_subdir>/<effective_name>-<runner_name>-stderr.txt` so a
        // post-mortem can see panic / abort / OS-error messages even when
        // the JSONL log was truncated mid-write. The directory is the same
        // one the variant's JSONL log goes into: `log_dir_resolved` when
        // the variant declared its own `log_dir`, otherwise the run-level
        // `run_log_dir`. The file is truncated on every spawn so a
        // `--resume` re-spawn cleanly overwrites the previous attempt.
        let stderr_dir: PathBuf = log_dir_resolved
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| run_log_dir.clone());
        let stderr_capture =
            spawn::stderr_capture_path(&stderr_dir, &job.effective_name, &cli.name);

        // T15.2: per-spawn stdout tracker. The reader thread parses
        // the variant's stdout-progress stream (T15.1) and folds each
        // event into the local tracker. T15.3 reads from this tracker
        // on every progress tick and broadcasts the snapshot to every
        // other runner via the per-peer TCP `ProgressCoordinator`.
        let tracker = progress::TrackerHandle::new(job.effective_name.clone());

        // T15.3: per-tick publisher closure passed to spawn_and_monitor.
        // It runs on the spawn loop's poll thread every
        // PROGRESS_BROADCAST_INTERVAL (~1s) while the child is alive.
        // In single-runner mode the broadcaster short-circuits inside
        // `publish`, so we can hand the closure unconditionally.
        let spawn_name_for_publish = job.effective_name.clone();
        let publish_fn = |snap: &progress::LocalProgressTracker| {
            progress_coord.publish(
                &spawn_name_for_publish,
                &snap.phase,
                snap.sent,
                snap.received,
                snap.eot_sent,
                snap.eot_received,
            );
        };

        // T15.4: build the phase-aware termination context for this
        // spawn. The state machine inside `spawn_and_monitor` reads
        // the local tracker (T15.2) and the cross-runner view (T15.3)
        // on every tick and decides whether to keep polling, log an
        // operate-idle observation, or fire the safety net.
        //
        // `max_spawn_secs` is bounded by the existing per-variant
        // `timeout_secs` so pre-T15.4 tests that pass small timeouts
        // still trip on the original deadline. New configs that
        // rely on idle detection should set `timeout_secs` (or
        // `default_timeout_secs`) generously and tune the safety
        // net via `--max-spawn-secs` on the runner CLI.
        let term_config = termination::TerminationConfig::with_bounded_max(
            cli.operate_idle_secs,
            cli.max_spawn_secs,
            timeout_secs,
        );
        let peers_expected: Vec<String> = bench_config
            .runners
            .iter()
            .filter(|n| **n != cli.name)
            .cloned()
            .collect();
        let termination_ctx = spawn::TerminationContext {
            config: term_config,
            remote_view: &remote_view,
            spawn_name: job.effective_name.clone(),
            peers_expected,
        };

        let outcome = spawn::spawn_and_monitor(
            &variant.binary,
            &args,
            Duration::from_secs(timeout_secs),
            Some(&stderr_capture),
            Some((
                tracker.clone(),
                cli.name.as_str(),
                job.effective_name.as_str(),
            )),
            Some(&publish_fn),
            Some(termination_ctx),
        )?;

        // Snapshot the final tracker state to stderr so an operator can
        // see what the variant reported on its last progress tick. The
        // line is purely diagnostic (T15.4 will use the same snapshot
        // for termination decisions). Costs one mutex acquisition.
        let snap = tracker.snapshot();
        eprintln!(
            "[runner:{}] '{}' final progress: phase={} sent={} received={} eot_sent={} eot_received={}",
            cli.name,
            job.effective_name,
            snap.phase,
            snap.sent,
            snap.received,
            snap.eot_sent,
            snap.eot_received
        );

        let status = outcome.status_str();
        let exit_code = outcome.exit_code();

        eprintln!(
            "[runner:{}] '{}' finished: status={}, exit_code={}",
            cli.name, job.effective_name, status, exit_code
        );

        // T-ux.1: emit the progress + ETA line immediately after the
        // existing "finished:" eprintln (the existing line shape is
        // frozen by T-impl.9 diagnostics -- do NOT modify it). Suppressed
        // on the final spawn since there is nothing to estimate beyond it.
        nominal_so_far += nominal_per_job[job_idx_in_all];
        let completed = job_idx_in_all + 1;
        emit_progress_eta(
            &cli.name,
            completed,
            total_jobs,
            spawn_loop_start.elapsed(),
            nominal_so_far,
            nominal_total.saturating_sub(nominal_so_far),
        );

        // On a non-success spawn (failed or timeout), surface diagnostic
        // context so the operator can investigate without scavenging the
        // logs directory:
        //   1. absolute path to the stderr capture file
        //   2. absolute path to the variant's JSONL log file (if present)
        //   3. either a tail of the capture, or an empty-capture notice
        //
        // The motivating case: a websocket variant on a 60s runner timeout
        // was TerminateProcess'd before writing anything to stderr, the
        // JSONL log was truncated mid-record, and the original status line
        // gave no pointer at all. This block makes that situation
        // diagnosable next time.
        //
        // Successful and skipped spawns stay silent (existing behaviour).
        if status != "success" && status != "skipped" {
            print_failure_diagnostics(
                &cli.name,
                &job.effective_name,
                &stderr_capture,
                &stderr_dir,
                &bench_config.run,
            );
        }

        // Done barrier identified by the effective spawn name.
        // The variant child has already exited (spawn_and_monitor is
        // synchronous), so on a `BarrierTimeoutError` here there is no
        // in-flight child to clean up; the error simply propagates to
        // `main` and triggers the EX_TEMPFAIL exit.
        let done_results =
            coordinator.done_barrier(&job.effective_name, status, exit_code, barrier_timeout)?;

        for (runner_name, (s, c)) in &done_results {
            summary.push(SummaryRow {
                variant: job.effective_name.clone(),
                runner: runner_name.clone(),
                status: s.clone(),
                exit_code: *c,
            });
        }

        // Mark that the most recent action in this entry was a real spawn,
        // so the grace fires before the next real spawn from this same entry
        // (deferred to the top of the next iteration so we don't sleep when
        // the next iteration is a different source entry — its ready barrier
        // provides a natural boundary).
        let more_in_entry =
            job_idx_in_all + 1 < all_jobs.len() && all_jobs[job_idx_in_all + 1].0 == *src_idx;
        last_spawn_was_real_in_entry.insert(*src_idx, more_in_entry);
    }

    // T15.3: shut down the progress coordinator before printing the
    // summary so reader threads exit cleanly and the process can
    // terminate without lingering TCP fds. `Drop` would also call this,
    // but the explicit call surfaces any cleanup errors deterministically.
    progress_coord.shutdown();

    // T15.10: shut down the barrier coordinator for the same reason.
    barrier_coord.shutdown();

    // Print summary table.
    print_summary(&bench_config.run, &summary);

    // T18.6: optional post-matrix analyzer invocation. Only the
    // lowest-sorted-name runner shells out so concurrent writes to
    // `<log-dir>/analysis/` are impossible. Soft-fail on Python errors --
    // the benchmark itself already completed. We run this even on partial
    // failures so the analyzer can still report on whatever was collected.
    if cli.analyze_full {
        let _ = analyze::run_post_matrix_analysis(&cli.name, &bench_config.runners, &run_log_dir);
    }

    // Resume-mode summary line: count reused vs executed vs failed.
    if cli.resume {
        let reused = summary.iter().filter(|r| r.status == "skipped").count();
        let executed_failed = summary
            .iter()
            .filter(|r| r.status != "skipped" && r.status != "success")
            .count();
        let executed_succeeded = summary.iter().filter(|r| r.status == "success").count();
        println!(
            "Resume: {reused} spawns reused, {} spawns executed, {executed_failed} failed.",
            executed_succeeded + executed_failed
        );
    }

    // Exit non-zero if any variant failed. Skipped rows count as success.
    let any_failure = summary
        .iter()
        .any(|r| r.status != "success" && r.status != "skipped");
    if any_failure {
        std::process::exit(1);
    }

    Ok(())
}

/// Decide whether the initial clock-sync produced enough data to safely
/// proceed with the run.
///
/// Extracted so it is unit-testable independently of the network plumbing.
/// Returns `Err` iff at least one peer produced zero samples and the run
/// must be aborted before the first ready barrier (T8.5 acceptance
/// criterion).
fn require_initial_sync_complete(failed_peers: &[String]) -> Result<()> {
    if failed_peers.is_empty() {
        Ok(())
    } else {
        bail!("initial clock-sync failed for peers: {failed_peers:?}")
    }
}

/// Expand every `[[variant]]` entry into spawn jobs and apply T14.8
/// threading-mode capability gating.
///
/// Each `[[variant]]` entry may declare `supported_modes = [...]` to gate
/// the expansion. Behaviour:
///
/// - **Declared, mode supported:** the spawn job is included.
/// - **Declared, mode not supported:** the spawn is silently skipped, a
///   single stderr line is emitted via the `report` callback, and the
///   spawn does NOT appear in the run summary table.
/// - **Not declared:** every requested mode is treated as supported and a
///   one-time stderr note is emitted per source entry. Permissive default
///   for the T14.2-T14.7 rollout window; documented in `runner/CUSTOM.md`.
///
/// The exact stderr-line shape is part of the contract surface — see
/// `metak-orchestrator/TASKS.md` T14.8 and the integration test in
/// `tests/integration.rs`. The `report` callback is a parameter so unit
/// tests can capture the lines without relying on a global stderr.
fn expand_and_gate_jobs<F>(
    bench_config: &config::BenchConfig,
    runner_name: &str,
    mut report: F,
) -> Result<Vec<(usize, spawn_job::SpawnJob)>>
where
    F: FnMut(&str),
{
    let mut out: Vec<(usize, spawn_job::SpawnJob)> = Vec::new();
    let mut warned_permissive: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, variant) in bench_config.variant.iter().enumerate() {
        let declared = variant.supported_modes_resolved()?;
        for job in spawn_job::expand_variant(idx, variant)? {
            match declared.as_ref() {
                Some(modes) if !modes.contains(&job.threading_mode) => {
                    // Capability gating: skip with stderr notice, do not
                    // append to out (excluded from summary). The exact
                    // shape of the line is part of the T14.8 contract.
                    report(&format!(
                        "[runner:{runner_name}] skipping {}: variant does not support threading_mode={}",
                        job.effective_name,
                        job.threading_mode.as_str()
                    ));
                    continue;
                }
                None if warned_permissive.insert(idx) => {
                    // One-time stderr note per variant entry that did
                    // not declare its capability. Keeps the rollout
                    // window forward-compatible while T14.2-T14.7 land
                    // per-variant capability declarations.
                    report(&format!(
                        "[runner:{runner_name}] note: variant '{}' has no supported_modes \
                         declared; treating every requested threading_mode as supported",
                        variant.name
                    ));
                }
                _ => {}
            }
            out.push((idx, job));
        }
    }
    Ok(out)
}

struct SummaryRow {
    variant: String,
    runner: String,
    status: String,
    exit_code: i32,
}

fn print_summary(run_id: &str, rows: &[SummaryRow]) {
    let header_exit = "Exit";
    println!("Benchmark run: {run_id}");
    println!(
        "{:<24} {:<8} {:<9} {header_exit}",
        "Variant", "Runner", "Status"
    );
    for row in rows {
        println!(
            "{:<24} {:<8} {:<9} {}",
            row.variant, row.runner, row.status, row.exit_code
        );
    }
}

/// Print the T-ux.1 per-spawn progress + ETA line.
///
/// Emitted to stderr immediately after the existing `'<name>' finished:
/// status=..., exit_code=...` line (and after the resume-mode skip notice
/// for jobs in the skip set). The line is suppressed on the final spawn
/// because there is nothing left to estimate.
///
/// Exact shape (ASCII only; pinned by the integration test):
///
/// ```text
/// [runner:<name>] progress: <i>/<total> done | elapsed <H>h <M>m <S>s | ETA ~<H>h <M>m <S>s
/// ```
///
/// `format_hms` collapses the prefix when it is zero, so a sub-minute run
/// reads `47s` rather than `0h 00m 47s`. See
/// `progress_eta::estimate_eta` for the hybrid (nominal + measured
/// overhead) ETA formula.
fn emit_progress_eta(
    runner_name: &str,
    completed: usize,
    total: usize,
    elapsed: Duration,
    nominal_so_far: Duration,
    nominal_remaining: Duration,
) {
    let Some(eta) =
        progress_eta::estimate_eta(elapsed, nominal_so_far, nominal_remaining, completed, total)
    else {
        // Final spawn (or defensive completed==0 case): no ETA to print.
        return;
    };
    eprintln!(
        "[runner:{runner_name}] progress: {completed}/{total} done | elapsed {} | ETA ~{}",
        progress_eta::format_hms(elapsed),
        progress_eta::format_hms(eta),
    );
}

/// Print the post-mortem diagnostic block immediately after a non-success
/// status line.
///
/// The format is deliberately compact and grep-friendly so it survives
/// being collected from several runners into one operator's terminal.
/// Output goes to the runner's own stderr -- same stream as the status
/// line -- via `eprintln!`.
///
/// Block layout:
///
/// ```text
/// [runner:<name>] stderr capture: <abs path>
/// [runner:<name>] jsonl log:      <abs path>           # only if file exists
/// [runner:<name>] ---- stderr tail (last 20 lines) ----
/// <up to 20 lines, last <= 64 KiB of the capture>
/// [runner:<name>] ---- end stderr tail ----
/// ```
///
/// If the capture file is missing on disk (defensive case -- the spawn
/// path always creates it before exec) the tail block is omitted
/// entirely with no notice. If the capture file is empty (the common
/// child-killed-before-flush case the original bug surfaced) a single
/// line replaces the bracketed tail block:
///
/// ```text
/// [runner:<name>] (stderr capture is empty -- child likely killed before writing any output)
/// ```
fn print_failure_diagnostics(
    runner_name: &str,
    effective_name: &str,
    stderr_capture: &std::path::Path,
    log_subdir: &std::path::Path,
    run: &str,
) {
    eprintln!(
        "[runner:{runner_name}] stderr capture: {}",
        stderr_capture.display()
    );

    // JSONL log pointer: print only when the file exists on disk. The
    // schema dictates the filename; the variant may or may not have got
    // far enough to create it. Courtesy pointer, not a guarantee.
    let jsonl = spawn::jsonl_log_path(log_subdir, effective_name, runner_name, run);
    if jsonl.exists() {
        eprintln!("[runner:{runner_name}] jsonl log:      {}", jsonl.display());
    }

    // Stderr tail. Cap the displayed lines at 20 to keep the operator's
    // terminal readable; the byte cap inside the helper handles the
    // pathological-file case.
    const TAIL_LINES: usize = 20;
    match spawn::tail_stderr_file(stderr_capture, TAIL_LINES) {
        Ok(Some(content)) if content.is_empty() => {
            eprintln!(
                "[runner:{runner_name}] (stderr capture is empty -- child likely killed before writing any output)"
            );
        }
        Ok(Some(content)) => {
            eprintln!("[runner:{runner_name}] ---- stderr tail (last {TAIL_LINES} lines) ----");
            // Print the tail content as-is. Use `eprint!` (not `eprintln!`)
            // because the tail already carries its own line breaks; adding
            // another would produce a blank line at the end. If the tail
            // does NOT end with '\n' (child crashed without flushing the
            // final newline) we add one so the closing separator lands on
            // its own line.
            eprint!("{content}");
            if !content.ends_with('\n') {
                eprintln!();
            }
            eprintln!("[runner:{runner_name}] ---- end stderr tail ----");
        }
        Ok(None) => {
            // Capture file is unexpectedly missing on disk. The spawn path
            // creates it before exec, so this is a "should never happen"
            // case. Silently skip rather than print a misleading notice;
            // the stderr-capture path line above already tells the operator
            // where to look.
        }
        Err(e) => {
            eprintln!("[runner:{runner_name}] WARN: failed to read stderr capture for tail: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_initial_sync_complete_passes_when_no_peers_failed() {
        // No peers were listed as failed -- the run must be allowed to
        // proceed (single-runner runs and successful multi-runner runs both
        // hit this path).
        require_initial_sync_complete(&[]).expect("empty failed list must succeed");
    }

    #[test]
    fn require_initial_sync_complete_fails_when_any_peer_has_zero_samples() {
        // One peer with zero samples -> Err. This is the load-bearing T8.5
        // hardening: cross-machine latency without an offset measurement
        // is statistically meaningless and must NOT be silently produced.
        let err = require_initial_sync_complete(&["bob".to_string()])
            .expect_err("zero-sample peer must abort the run");
        let msg = err.to_string();
        assert!(
            msg.contains("initial clock-sync failed"),
            "error message should mention initial sync failure: {msg}"
        );
        assert!(
            msg.contains("bob"),
            "error message should name the failed peer: {msg}"
        );
    }

    #[test]
    fn require_initial_sync_complete_fails_when_any_peer_in_a_set_failed() {
        // Even one failed peer in a larger set is fatal. Mixed success/
        // failure does not "average" -- analysis cannot correct one cross-
        // runner pair while leaving another uncorrected.
        let err = require_initial_sync_complete(&["bob".to_string(), "carol".to_string()])
            .expect_err("any failed peer must abort the run");
        let msg = err.to_string();
        assert!(msg.contains("bob") && msg.contains("carol"), "msg={msg}");
    }

    // -----------------------------------------------------------------
    // T14.8: expand_and_gate_jobs capability gating.
    // -----------------------------------------------------------------

    fn parse_bench_config(toml_str: &str) -> config::BenchConfig {
        let mut cfg: config::BenchConfig = toml::from_str(toml_str).unwrap();
        cfg.resolve_templates().unwrap();
        cfg.validate().unwrap();
        cfg
    }

    #[test]
    fn gating_skips_unsupported_mode_with_eprintln_notice() {
        // Variant supports only `single`; config asks for both modes.
        // The multi spawn must be skipped with the exact contract line
        // and must NOT appear in the returned job list.
        let cfg = parse_bench_config(
            r#"
run = "g"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "single-only"
binary = "./x"
supported_modes = ["single"]
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1
  qos = 1
  threading_modes = ["single", "multi"]
"#,
        );
        let mut lines: Vec<String> = Vec::new();
        let jobs = expand_and_gate_jobs(&cfg, "a", |l| lines.push(l.to_string())).unwrap();

        // Only the single-mode spawn survives.
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].1.effective_name, "single-only-single");
        assert_eq!(jobs[0].1.threading_mode, config::ThreadingMode::Single);

        // The exact contract-pin stderr line for the skipped multi spawn.
        let skip_line = lines
            .iter()
            .find(|l| l.contains("skipping single-only-multi"))
            .expect("skipping notice must be emitted; got lines: {lines:?}");
        assert!(
            skip_line.contains("variant does not support threading_mode=multi"),
            "skip line shape: {skip_line}"
        );
        assert!(
            skip_line.starts_with("[runner:a] "),
            "skip line must carry [runner:<name>] prefix: {skip_line}"
        );
    }

    #[test]
    fn gating_permissive_default_emits_one_time_note() {
        // A variant entry that omits `supported_modes` runs every requested
        // mode and emits a single stderr note (per variant, not per spawn).
        let cfg = parse_bench_config(
            r#"
run = "g"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "permissive"
binary = "./x"
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1
  qos = [1, 2]
  threading_modes = ["single", "multi"]
"#,
        );
        let mut lines: Vec<String> = Vec::new();
        let jobs = expand_and_gate_jobs(&cfg, "a", |l| lines.push(l.to_string())).unwrap();

        // All four spawns survive (no gating).
        assert_eq!(jobs.len(), 4);

        // Exactly one note line, regardless of how many spawns.
        let notes: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("has no supported_modes declared"))
            .collect();
        assert_eq!(
            notes.len(),
            1,
            "expected exactly one permissive-default note, got {}: {lines:?}",
            notes.len()
        );
    }

    #[test]
    fn gating_no_skip_when_variant_supports_all_requested_modes() {
        let cfg = parse_bench_config(
            r#"
run = "g"
runners = ["a"]
default_timeout_secs = 10

[[variant]]
name = "both"
binary = "./x"
supported_modes = ["single", "multi"]
  [variant.common]
  tick_rate_hz = 100
  values_per_tick = 1
  qos = 1
  threading_modes = ["single", "multi"]
"#,
        );
        let mut lines: Vec<String> = Vec::new();
        let jobs = expand_and_gate_jobs(&cfg, "a", |l| lines.push(l.to_string())).unwrap();

        // Both spawns survive.
        assert_eq!(jobs.len(), 2);
        // No skipping notice for declared variants whose requested modes
        // are all supported.
        assert!(
            !lines.iter().any(|l| l.contains("skipping")),
            "must not skip when all modes are supported, got: {lines:?}"
        );
        // No permissive note for declared variants.
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("has no supported_modes declared")),
            "must not emit permissive note when supported_modes is declared, got: {lines:?}"
        );
    }
}
