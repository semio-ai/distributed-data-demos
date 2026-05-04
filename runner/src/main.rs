mod cli_args;
mod clock_sync;
mod clock_sync_log;
mod config;
mod local_addrs;
mod message;
mod protocol;
mod spawn;
mod spawn_job;

use anyhow::{bail, Result};
use clap::Parser;
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

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

    // Generate a proposed log subfolder name before discovery so it can be
    // negotiated with other runners. The leader (first in the runners list)
    // decides the final name so all runners use the same subfolder.
    let run_ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let proposed_log_subdir = format!("{}-{}", bench_config.run, run_ts);

    // Create coordinator and run discovery.
    let coordinator = protocol::Coordinator::new(
        cli.name.clone(),
        &bench_config.runners,
        config_hash,
        cli.port,
        proposed_log_subdir,
        bench_config.run.clone(),
    )?;

    eprintln!("[runner:{}] starting discovery...", cli.name);
    let log_subdir = coordinator.discover()?;
    eprintln!("[runner:{}] discovery complete", cli.name);

    eprintln!("[runner:{}] log subfolder: {}", cli.name, log_subdir);

    // Snapshot the peer host map captured during discovery. This is passed
    // to every variant spawn as `--peers name=host,...` (sorted by name).
    let peer_hosts = coordinator.peer_hosts();
    eprintln!("[runner:{}] peer_hosts: {:?}", cli.name, peer_hosts);

    // Resolve the per-run log directory used for the clock-sync JSONL file.
    // Variants may declare their own `log_dir` in `[variant.common]`; we use
    // the first one we find as the canonical run directory. Fallback to
    // `./logs` so single-runner mode without a configured log_dir still has
    // a sensible default.
    let base_log_dir = bench_config
        .variant
        .iter()
        .find_map(|v| v.common.get("log_dir"))
        .map(cli_args::toml_value_to_string)
        .unwrap_or_else(|| "./logs".to_string());
    let run_log_dir: PathBuf = PathBuf::from(format!("{base_log_dir}/{log_subdir}"));

    // Open the clock-sync log file (skipped in single-runner mode -- no peers
    // means no sync events would ever be written, and the contract permits
    // an absent file in that case).
    let mut clock_sync_log = if !coordinator.is_single_runner() {
        std::fs::create_dir_all(&run_log_dir).ok();
        match clock_sync_log::open_clock_sync_log(&run_log_dir, &cli.name, &bench_config.run) {
            Ok(l) => Some(l),
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

    // Phase 1.5: initial clock sync. Logged with `variant=""`.
    let peer_names: Vec<String> = bench_config
        .runners
        .iter()
        .filter(|n| *n != &cli.name)
        .cloned()
        .collect();
    if let (Some(engine), Some(log)) = (clock_sync_engine.as_ref(), clock_sync_log.as_mut()) {
        if !peer_names.is_empty() {
            eprintln!(
                "[runner:{}] initial clock sync against {} peer(s)...",
                cli.name,
                peer_names.len()
            );
            let measurements = engine.measure_offsets(&peer_names, clock_sync::DEFAULT_SAMPLES);
            for peer in &peer_names {
                if let Some(m) = measurements.get(peer) {
                    if let Err(e) = log.write("", peer, m) {
                        eprintln!(
                            "[runner:{}] WARN: clock-sync log write failed: {e:#}",
                            cli.name
                        );
                    }
                    eprintln!(
                        "[runner:{}] clock_sync (initial) peer={peer} offset_ms={:.3} rtt_ms={:.3}",
                        cli.name, m.offset_ms, m.rtt_ms
                    );
                } else {
                    eprintln!(
                        "[runner:{}] WARN: no clock-sync samples received from peer={peer}",
                        cli.name
                    );
                }
            }
        }
    }

    let inter_qos_grace = Duration::from_millis(bench_config.inter_qos_grace_ms());

    // Track results for summary table.
    let mut summary: Vec<SummaryRow> = Vec::new();

    // Execute each variant in config order. Each [[variant]] expands into
    // one or more spawn jobs (one per concrete QoS level). Jobs from one
    // entry run sequentially in ascending QoS order, with a small inter-job
    // grace period to let TCP/UDP sockets release before the next spawn.
    for (idx, variant) in bench_config.variant.iter().enumerate() {
        let timeout_secs = variant.effective_timeout(bench_config.default_timeout_secs);
        let jobs = spawn_job::expand_variant(idx, variant)?;

        for (job_idx, job) in jobs.iter().enumerate() {
            eprintln!(
                "[runner:{}] ready barrier for spawn '{}' (qos={})",
                cli.name, job.effective_name, job.qos
            );
            coordinator.ready_barrier(&job.effective_name)?;

            // Per-variant clock resync: catches drift across the run. Logged
            // with the spawn's effective name so analysis joins the latest
            // measurement preceding the variant's writes. No-op in
            // single-runner mode (engine/log are None).
            if let (Some(engine), Some(log)) = (clock_sync_engine.as_ref(), clock_sync_log.as_mut())
            {
                if !peer_names.is_empty() {
                    let measurements =
                        engine.measure_offsets(&peer_names, clock_sync::DEFAULT_SAMPLES);
                    for peer in &peer_names {
                        if let Some(m) = measurements.get(peer) {
                            if let Err(e) = log.write(&job.effective_name, peer, m) {
                                eprintln!(
                                    "[runner:{}] WARN: clock-sync log write failed: {e:#}",
                                    cli.name
                                );
                            }
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
                &peer_hosts,
            );

            eprintln!(
                "[runner:{}] spawning '{}' (qos={}, timeout: {}s)",
                cli.name, job.effective_name, job.qos, timeout_secs
            );

            let outcome = spawn::spawn_and_monitor(
                &variant.binary,
                &args,
                Duration::from_secs(timeout_secs),
            )?;

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

            // Inter-job grace period: skip after the last job in this entry
            // (the next entry's ready barrier already provides a natural
            // boundary). Only sleep if there is another job ahead.
            let more_jobs_in_entry = job_idx + 1 < jobs.len();
            if more_jobs_in_entry && !inter_qos_grace.is_zero() {
                std::thread::sleep(inter_qos_grace);
            }
        }
    }

    // Print summary table.
    print_summary(&bench_config.run, &summary);

    // Exit non-zero if any variant failed.
    let any_failure = summary.iter().any(|r| r.status != "success");
    if any_failure {
        std::process::exit(1);
    }

    Ok(())
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
