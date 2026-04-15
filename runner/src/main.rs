mod cli_args;
mod config;
mod message;
mod protocol;
mod spawn;

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

    // Create coordinator and run discovery.
    let coordinator = protocol::Coordinator::new(
        cli.name.clone(),
        &bench_config.runners,
        config_hash,
        cli.port,
    )?;

    eprintln!("[runner:{}] starting discovery...", cli.name);
    coordinator.discover()?;
    eprintln!("[runner:{}] discovery complete", cli.name);

    // Generate a single UTC timestamp for the run so all variants share the same log subfolder.
    let run_ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let log_subdir = format!("{}-{}", bench_config.run, run_ts);

    eprintln!("[runner:{}] log subfolder: {}", cli.name, log_subdir);

    // Track results for summary table.
    let mut summary: Vec<SummaryRow> = Vec::new();

    // Execute each variant in config order.
    for variant in &bench_config.variant {
        let timeout_secs = variant.effective_timeout(bench_config.default_timeout_secs);

        eprintln!(
            "[runner:{}] ready barrier for variant '{}'",
            cli.name, variant.name
        );
        coordinator.ready_barrier(&variant.name)?;

        // Build CLI args (launch_ts is a placeholder here; spawn_and_monitor records the real one).
        // We build args with a placeholder, then replace launch_ts after getting the real one from spawn.
        // Actually, spawn_and_monitor records its own launch_ts, but we need to pass it as an arg
        // BEFORE spawning. So we compute it inside the arg builder flow.
        let launch_ts = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.9fZ")
            .to_string();

        // Resolve the log directory: if the variant config has a log_dir, append the run subfolder.
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
        );

        eprintln!(
            "[runner:{}] spawning variant '{}' (timeout: {}s)",
            cli.name, variant.name, timeout_secs
        );

        let outcome =
            spawn::spawn_and_monitor(&variant.binary, &args, Duration::from_secs(timeout_secs))?;

        let status = outcome.status_str();
        let exit_code = outcome.exit_code();

        eprintln!(
            "[runner:{}] variant '{}' finished: status={}, exit_code={}",
            cli.name, variant.name, status, exit_code
        );

        // Done barrier.
        let done_results = coordinator.done_barrier(&variant.name, status, exit_code)?;

        // Collect summary rows for all runners for this variant.
        for (runner_name, (s, c)) in &done_results {
            summary.push(SummaryRow {
                variant: variant.name.clone(),
                runner: runner_name.clone(),
                status: s.clone(),
                exit_code: *c,
            });
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
