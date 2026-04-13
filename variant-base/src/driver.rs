use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::DateTime;

use crate::cli::CliArgs;
use crate::logger::Logger;
use crate::resource::ResourceMonitor;
use crate::seq::SeqGenerator;
use crate::types::{Phase, Qos};
use crate::variant_trait::Variant;
use crate::workload::create_workload;

/// Run the full test protocol: connect, stabilize, operate, silent.
///
/// The driver owns the logger and all support modules. The variant only
/// performs transport-specific operations through the `Variant` trait.
pub fn run_protocol(variant: &mut impl Variant, config: &CliArgs) -> Result<()> {
    let qos = Qos::from_int(config.qos)
        .ok_or_else(|| anyhow::anyhow!("invalid QoS level: {}", config.qos))?;

    let mut logger = Logger::new(
        &config.log_dir,
        &config.variant,
        &config.runner,
        &config.run,
    )?;
    let mut seq_gen = SeqGenerator::new();
    let mut resource_monitor = ResourceMonitor::new();
    let mut workload = create_workload(&config.workload)?;

    // -- Phase 1: Connect --
    logger.log_phase(Phase::Connect, None)?;
    variant.connect()?;

    let launch_ts = DateTime::parse_from_rfc3339(&config.launch_ts)?;
    let now = chrono::Utc::now();
    let elapsed_ms = (now - launch_ts.with_timezone(&chrono::Utc))
        .num_nanoseconds()
        .unwrap_or(0) as f64
        / 1_000_000.0;
    logger.log_connected(&config.launch_ts, elapsed_ms)?;

    // -- Phase 2: Stabilize --
    logger.log_phase(Phase::Stabilize, None)?;
    std::thread::sleep(Duration::from_secs(config.stabilize_secs));

    // -- Phase 3: Operate --
    logger.log_phase(Phase::Operate, Some(&config.workload))?;

    let tick_interval = Duration::from_secs_f64(1.0 / f64::from(config.tick_rate_hz));
    let operate_duration = Duration::from_secs(config.operate_secs);
    let resource_interval = Duration::from_millis(100);

    let operate_start = Instant::now();
    let mut last_resource_sample = Instant::now();
    let mut next_tick = Instant::now();

    while operate_start.elapsed() < operate_duration {
        // Wait for the next tick.
        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += tick_interval;

        // Generate and publish writes.
        let ops = workload.generate(config.values_per_tick);
        for op in &ops {
            let seq = seq_gen.next_seq();
            variant.publish(&op.path, &op.payload, qos, seq)?;
            logger.log_write(seq, &op.path, qos, op.payload.len())?;
        }

        // Drain received updates.
        while let Some(update) = variant.poll_receive()? {
            logger.log_receive(
                &update.writer,
                update.seq,
                &update.path,
                update.qos,
                update.payload.len(),
            )?;
        }

        // Periodic resource sampling.
        if last_resource_sample.elapsed() >= resource_interval {
            let (cpu, mem) = resource_monitor.sample();
            logger.log_resource(cpu, mem)?;
            last_resource_sample = Instant::now();
        }
    }

    // -- Phase 4: Silent (drain + flush) --
    logger.log_phase(Phase::Silent, None)?;

    let silent_duration = Duration::from_secs(config.silent_secs);
    let silent_start = Instant::now();
    while silent_start.elapsed() < silent_duration {
        match variant.poll_receive()? {
            Some(update) => {
                logger.log_receive(
                    &update.writer,
                    update.seq,
                    &update.path,
                    update.qos,
                    update.payload.len(),
                )?;
            }
            None => {
                // No pending updates; sleep briefly to avoid busy-waiting.
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    variant.disconnect()?;
    logger.flush()?;

    Ok(())
}
