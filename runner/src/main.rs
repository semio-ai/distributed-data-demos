mod cli_args;
mod clock_sync;
mod clock_sync_log;
mod config;
mod local_addrs;
mod message;
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
}

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

    // Wire the process-wide clock-sync verbose toggle so the engine and
    // coordinator emit per-datagram traces while diagnosing field issues.
    clock_sync::set_verbose(cli.verbose_clock_sync);

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
    let mut all_jobs: Vec<(usize, spawn_job::SpawnJob)> = Vec::new();
    for (idx, variant) in bench_config.variant.iter().enumerate() {
        for job in spawn_job::expand_variant(idx, variant)? {
            all_jobs.push((idx, job));
        }
    }
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
        eprintln!(
            "[runner:{}] resume: local manifest has {} complete job(s)",
            cli.name,
            local.complete_jobs.len()
        );

        // Exchange manifests with peers. Single-runner mode short-circuits
        // and returns just our own.
        let manifests = coordinator
            .exchange_resume_manifest(local.complete_jobs.clone())
            .map_err(|e| anyhow::anyhow!("resume manifest exchange failed: {e:#}"))?;

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
        coordinator.ready_barrier(&job.effective_name)?;

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

        let outcome =
            spawn::spawn_and_monitor(&variant.binary, &args, Duration::from_secs(timeout_secs))?;

        let status = outcome.status_str();
        let exit_code = outcome.exit_code();

        eprintln!(
            "[runner:{}] '{}' finished: status={}, exit_code={}",
            cli.name, job.effective_name, status, exit_code
        );

        // Done barrier identified by the effective spawn name.
        let done_results = coordinator.done_barrier(&job.effective_name, status, exit_code)?;

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
}
