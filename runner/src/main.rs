mod cli_args;
mod clock_sync;
mod clock_sync_log;
mod config;
mod local_addrs;
mod message;
mod progress;
mod progress_coord;
mod protocol;
mod resume;
mod spawn;
mod spawn_job;

use anyhow::{bail, Result};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

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

fn main() -> Result<()> {
    let cli = Cli::parse();

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
    // Any other anyhow::Error propagates back to the runtime and aborts
    // with the standard non-zero exit (which the wrappers do NOT retry).
    match run(&cli) {
        Ok(()) => Ok(()),
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
            Err(e)
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
    // generation and resume-mode latest-folder selection need it. Variants
    // may declare their own `log_dir` in `[variant.common]`; we use the first
    // one we find as the canonical run directory (this matches the existing
    // behavior further down in the loop). Fallback to `./logs` so single-
    // runner runs without a configured log_dir still work.
    let base_log_dir = bench_config
        .variant
        .iter()
        .find_map(|v| v.common.get("log_dir"))
        .map(cli_args::toml_value_to_string)
        .unwrap_or_else(|| "./logs".to_string());

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

    // Track results for summary table.
    let mut summary: Vec<SummaryRow> = Vec::new();

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

        // Resolve the log directory: if the variant config has a log_dir,
        // append the run subfolder.
        let log_dir_resolved = variant.common.get("log_dir").map(|log_dir_val| {
            let base = cli_args::toml_value_to_string(log_dir_val);
            format!("{}/{}", base, log_subdir)
        });

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
        );

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

    // Print summary table.
    print_summary(&bench_config.run, &summary);

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
