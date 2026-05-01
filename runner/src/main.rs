mod cli_args;
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
