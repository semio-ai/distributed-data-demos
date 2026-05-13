use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::DateTime;

use crate::cli::{parse_peer_names_from_extra, CliArgs};
use crate::logger::{Logger, LoggerHandle};
use crate::progress_emitter::ProgressEmitter;
use crate::resource::ResourceMonitor;
use crate::seq::SeqGenerator;
use crate::types::{Phase, Qos, ThreadingMode};
use crate::variant_trait::Variant;
use crate::workload::create_workload;

/// Thin proxy that exposes the `Logger` event API on top of a
/// [`LoggerHandle`]. Used by the driver so existing call sites can keep
/// the historical `logger.log_*` shape after the underlying logger was
/// moved behind `Arc<Mutex<Logger>>` for T14.10. Each call locks the
/// mutex briefly, emits the event, then releases.
struct LoggerProxy<'a> {
    handle: &'a LoggerHandle,
}

impl<'a> LoggerProxy<'a> {
    fn new(handle: &'a LoggerHandle) -> Self {
        Self { handle }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Logger>> {
        self.handle
            .inner()
            .lock()
            .map_err(|_| anyhow::anyhow!("LoggerHandle mutex poisoned"))
    }

    fn log_phase(&mut self, phase: Phase, profile: Option<&str>) -> Result<()> {
        self.lock()?.log_phase(phase, profile)
    }

    fn log_connected(
        &mut self,
        launch_ts: &str,
        elapsed_ms: f64,
        threading_mode: ThreadingMode,
        recv_buffer_kb: u32,
    ) -> Result<()> {
        self.lock()?
            .log_connected(launch_ts, elapsed_ms, threading_mode, recv_buffer_kb)
    }

    fn log_write(&mut self, seq: u64, path: &str, qos: Qos, bytes: usize) -> Result<()> {
        self.lock()?.log_write(seq, path, qos, bytes)
    }

    fn log_backpressure_skipped(&mut self, path: &str, qos: Qos) -> Result<()> {
        self.lock()?.log_backpressure_skipped(path, qos)
    }

    fn log_receive(
        &mut self,
        writer: &str,
        seq: u64,
        path: &str,
        qos: Qos,
        bytes: usize,
    ) -> Result<()> {
        self.lock()?.log_receive(writer, seq, path, qos, bytes)
    }

    fn log_eot_sent(&mut self, eot_id: u64) -> Result<()> {
        self.lock()?.log_eot_sent(eot_id)
    }

    fn log_eot_received(&mut self, writer: &str, eot_id: u64) -> Result<()> {
        self.lock()?.log_eot_received(writer, eot_id)
    }

    fn log_eot_timeout(&mut self, missing: &[String], wait_ms: u64) -> Result<()> {
        self.lock()?.log_eot_timeout(missing, wait_ms)
    }

    fn log_resource(&mut self, cpu_percent: f64, memory_mb: f64) -> Result<()> {
        self.lock()?.log_resource(cpu_percent, memory_mb)
    }

    fn flush(&mut self) -> Result<()> {
        self.lock()?.flush()
    }
}

/// Minimum EOT-phase drain budget in seconds when no explicit override is
/// provided. Replaces the previous 5-second floor: even short fixture runs
/// (e.g. `operate_secs = 1..=10`) get a meaningful 30-second drain window
/// for late-arriving messages on hybrid TCP/UDP transports.
pub const MIN_DEFAULT_EOT_TIMEOUT_SECS: u64 = 30;

/// Multiplier applied to `operate_secs` when computing the default EOT
/// timeout. At 100 K writes/s the in-flight backlog can take roughly the
/// operate-phase wall-clock to drain on hybrid transports; the factor of 3
/// gives headroom for late deliveries while still being bounded.
pub const DEFAULT_EOT_TIMEOUT_OPERATE_MULTIPLIER: u64 = 3;

/// Compute the default EOT timeout in seconds from `operate_secs`.
///
/// Formula: `max(3 * operate_secs, 30)`. Used by the driver when
/// `--eot-timeout-secs` is not provided on the CLI.
#[inline]
pub fn default_eot_timeout_secs(operate_secs: u64) -> u64 {
    std::cmp::max(
        operate_secs.saturating_mul(DEFAULT_EOT_TIMEOUT_OPERATE_MULTIPLIER),
        MIN_DEFAULT_EOT_TIMEOUT_SECS,
    )
}

/// Minimum operate-phase drain wallclock budget. Applies to both
/// workload profiles -- the budget is never allowed below this floor.
pub const MIN_OPERATE_DRAIN_TIME_BUDGET: Duration = Duration::from_millis(1);

/// Safety margin subtracted from `next_tick - now` to leave room for the
/// next publish phase. Only used in the scalar-flood path.
pub const OPERATE_DRAIN_SAFETY_MARGIN: Duration = Duration::from_millis(1);

/// Flat wallclock cap for the max-throughput operate-phase drain loop.
/// There is no tick boundary in max-throughput, but the drain still must
/// not be unbounded -- the publisher needs to run again. Empirically
/// tuned to absorb a substantial fraction of the recv buffer without
/// starving the publish path.
pub const MAX_THROUGHPUT_DRAIN_TIME_BUDGET: Duration = Duration::from_millis(5);

/// Compute the wallclock drain budget for one operate-phase iteration
/// (T-impl.10).
///
/// - **max-throughput**: returns a flat `MAX_THROUGHPUT_DRAIN_TIME_BUDGET`
///   (5 ms). No tick boundary to respect, but the drain must remain
///   bounded.
/// - **scalar-flood**: returns `(next_tick - now) - OPERATE_DRAIN_SAFETY_MARGIN`,
///   floored at `MIN_OPERATE_DRAIN_TIME_BUDGET` (1 ms). If we are already
///   behind on the publish phase (`next_tick - now <= safety_margin`),
///   falls back to the 1 ms floor so we don't compound the lateness.
///
/// Motivation: a hardcoded 1 ms drain cap is too tight when per-message
/// receive cost is high (e.g. websocket frame parsing). The recv buffer
/// then grows monotonically each tick at high symmetric rates until one
/// side's TCP window collapses -- see the 2026-05-11 diagnostic incident
/// referenced in `variant-base/CUSTOM.md`.
#[inline]
pub fn compute_operate_drain_time_budget(
    max_throughput: bool,
    next_tick: Instant,
    now: Instant,
) -> Duration {
    if max_throughput {
        MAX_THROUGHPUT_DRAIN_TIME_BUDGET
    } else if next_tick <= now {
        MIN_OPERATE_DRAIN_TIME_BUDGET
    } else {
        let remaining = next_tick - now;
        if remaining <= OPERATE_DRAIN_SAFETY_MARGIN {
            MIN_OPERATE_DRAIN_TIME_BUDGET
        } else {
            std::cmp::max(
                remaining - OPERATE_DRAIN_SAFETY_MARGIN,
                MIN_OPERATE_DRAIN_TIME_BUDGET,
            )
        }
    }
}

/// Run the full test protocol: connect, stabilize, operate, silent.
///
/// The driver owns the logger and all support modules. The variant only
/// performs transport-specific operations through the `Variant` trait.
pub fn run_protocol(variant: &mut impl Variant, config: &CliArgs) -> Result<()> {
    let qos = Qos::from_int(config.qos)
        .ok_or_else(|| anyhow::anyhow!("invalid QoS level: {}", config.qos))?;

    let owned_logger = Logger::new(
        &config.log_dir,
        &config.variant,
        &config.runner,
        &config.run,
    )?;
    // Wrap the logger in a thread-safe handle (T14.10) so variants
    // whose reader threads emit `receive` events directly can clone it
    // into the spawned thread. Driver-side event emission goes through
    // a `LoggerProxy` that locks the mutex per event -- the proxy keeps
    // the historical `logger.log_*` call shape unchanged at all sites.
    let logger_handle = LoggerHandle::new(owned_logger);
    let mut logger = LoggerProxy::new(&logger_handle);
    let mut seq_gen = SeqGenerator::new();
    let mut resource_monitor = ResourceMonitor::new();
    let mut workload = create_workload(&config.workload)?;

    // Stdout progress emitter (E15 / T15.1).
    //
    // The emitter spawns a background thread that writes one JSON
    // progress line to stdout every `progress_stdout_interval_ms`. When
    // the CLI arg is `0` the emitter is fully disabled: no thread is
    // spawned and no stdout writes happen. The driver still calls the
    // setter methods unconditionally so the in-process counter state
    // remains correct (the setters short-circuit to atomic updates on
    // the disabled path).
    //
    // The initial phase is `Connect`, matching the very first
    // `log_phase` call a few lines below; any background tick that
    // fires before the driver advances the phase will see `connect`.
    let mut progress = ProgressEmitter::new(config.progress_stdout_interval_ms, Phase::Connect);

    // -- Phase 1: Connect --
    //
    // The threading mode is passed through to the variant so it can
    // branch its connect-time setup. After `connect` returns Ok, the
    // driver calls `start_reader_threads(mode)` to spawn any per-peer
    // reader threads the variant declared support for; the default
    // impl is a no-op for Single-only variants. See E14 / T14.1.
    logger.log_phase(Phase::Connect, None)?;
    progress.set_phase(Phase::Connect);
    variant.connect(config.threading_mode)?;
    // Hand the variant a thread-safe logger handle BEFORE reader
    // threads are spawned so they capture it at spawn time. The default
    // trait impl is a no-op for variants that route all logging
    // through the driver thread.
    variant.attach_logger(logger_handle.clone());
    variant.start_reader_threads(config.threading_mode)?;

    let launch_ts = DateTime::parse_from_rfc3339(&config.launch_ts)?;
    let now = chrono::Utc::now();
    let elapsed_ms = (now - launch_ts.with_timezone(&chrono::Utc))
        .num_nanoseconds()
        .unwrap_or(0) as f64
        / 1_000_000.0;
    logger.log_connected(
        &config.launch_ts,
        elapsed_ms,
        config.threading_mode,
        config.recv_buffer_kb,
    )?;

    // -- Phase 2: Stabilize --
    logger.log_phase(Phase::Stabilize, None)?;
    progress.set_phase(Phase::Stabilize);
    std::thread::sleep(Duration::from_secs(config.stabilize_secs));

    // -- Phase 3: Operate --
    logger.log_phase(Phase::Operate, Some(&config.workload))?;
    progress.set_phase(Phase::Operate);

    let max_throughput = config.workload == "max-throughput";
    let tick_interval = Duration::from_secs_f64(1.0 / f64::from(config.tick_rate_hz));
    let operate_duration = Duration::from_secs(config.operate_secs);
    let resource_interval = Duration::from_millis(100);

    let operate_start = Instant::now();
    let mut last_resource_sample = Instant::now();
    let mut next_tick = Instant::now();

    // Bound the receive-drain per outer iteration by both a message-count
    // budget and a wallclock budget. Without this, an unbounded
    // `while let Some(update) = variant.poll_receive()? { ... }` starves
    // `publish` whenever a peer publishes faster than the local variant
    // drains. See T-fairness.1 (original bound) and T-impl.10 (the
    // tick-aware widening below).
    //
    // Message-count budget (operate phase): `4 * values_per_tick`, floor
    // at 1. The doubled cap (was `2 *`) costs nothing when the buffer is
    // small -- the `Ok(None)` early-exit fires immediately -- and gives
    // breathing room at high symmetric rates where transports with
    // expensive per-message receive cost otherwise build up backlog.
    //
    // Wallclock budget (operate phase): computed per-iteration inside the
    // loop below. See `compute_operate_drain_time_budget` for the
    // tick-aware formula motivated by the 2026-05-11 websocket-1000x100hz
    // diagnostic incident.
    //
    // The EOT-phase drain (further below) retains the pre-T-impl.10
    // budgets (2 * vpt, 1 ms wallclock) -- the failure mode the new
    // formula addresses only manifests during operate-phase pacing.
    let drain_msg_budget = (config.values_per_tick as usize).saturating_mul(4).max(1);
    let eot_drain_msg_budget = (config.values_per_tick as usize).saturating_mul(2).max(1);
    let eot_drain_time_budget = Duration::from_millis(1);

    // Two-tier back-off counter for the max-throughput loop (T-impl.8).
    // Local to this protocol run -- only relevant under MaxThroughput;
    // unused under ScalarFlood where the inter-tick sleep already paces
    // the writer.
    //
    // Tiers (max-throughput only):
    //   - 1st consecutive Ok(false): yield_now() -- release timeslice.
    //   - 2nd+ consecutive Ok(false): sleep(1ms) -- substantial back-off
    //     (~15ms on Windows due to timer granularity, ~1ms on Linux).
    //   - Ok(true) resets the counter to 0.
    let mut consecutive_skipped: u32 = 0;

    // Variant-side idle detection (E15 / T15.5).
    //
    // When both `sent` and `received` counters have not advanced for
    // `operate_idle_secs`, the variant transitions directly to the
    // `silent` phase, bypassing the on-wire EOT exchange. The
    // bookkeeping below is unconditional (cheap atomic snapshots) but
    // the trigger check is gated on `operate_idle_secs > 0`: `0`
    // disables idle detection entirely (pre-E15 behaviour, only the
    // time-based `operate_secs` transition fires).
    //
    // We compare against the emitter's snapshot rather than mirror
    // counters locally because the emitter is the source of truth for
    // both: every successful `try_publish` calls `inc_sent`, and every
    // `poll_receive` -> `Some(...)` calls `inc_received`. Tracking
    // `last_*_value` rather than a "saw any work this iteration"
    // boolean is robust to drain iterations that happen to publish
    // zero values per tick (e.g. very small `values_per_tick`).
    let idle_threshold = Duration::from_secs(u64::from(config.operate_idle_secs));
    let idle_enabled = config.operate_idle_secs > 0;
    let mut last_sent_value: u64 = 0;
    let mut last_received_value: u64 = 0;
    let mut last_sent_change_at = Instant::now();
    let mut last_received_change_at = Instant::now();
    let mut idle_triggered = false;

    while operate_start.elapsed() < operate_duration {
        // In max-throughput mode, skip the tick sleep entirely.
        if !max_throughput {
            let now = Instant::now();
            if now < next_tick {
                std::thread::sleep(next_tick - now);
            }
            next_tick += tick_interval;
        }

        // Generate writes and offer them to the transport via
        // `try_publish`. If the transport reports backpressure
        // (`Ok(false)`) we log a `backpressure_skipped` event and move
        // on to the next value -- the value is NOT delivered, NOT
        // retried within the same tick, and NOT recorded as a `write`.
        // Real errors still propagate. See T-impl.6 and
        // `metak-shared/api-contracts/jsonl-log-schema.md`.
        //
        // Under max-throughput, Ok(false) also triggers the two-tier
        // self-pacing back-off (yield then sleep) -- see T-impl.8 and
        // `variant-base/CUSTOM.md`. Under scalar-flood the inter-tick
        // sleep already paces the writer, so no extra back-off here.
        let ops = workload.generate(config.values_per_tick);
        for op in &ops {
            let seq = seq_gen.next_seq();
            if variant.try_publish(&op.path, &op.payload, qos, seq)? {
                logger.log_write(seq, &op.path, qos, op.payload.len())?;
                progress.inc_sent();
                if max_throughput {
                    consecutive_skipped = 0;
                }
            } else {
                logger.log_backpressure_skipped(&op.path, qos)?;
                if max_throughput {
                    consecutive_skipped = consecutive_skipped.saturating_add(1);
                    if consecutive_skipped == 1 {
                        // First skip since the last successful publish:
                        // yield the timeslice so the receiver thread may
                        // be scheduled, but do not sleep -- a yield costs
                        // <100us on Windows and may suffice.
                        std::thread::yield_now();
                    } else {
                        // Second or later consecutive skip: take a real
                        // sleep. On Windows this actually sleeps ~15ms
                        // (timer granularity); on Linux it's ~1ms.
                        // Either way it's substantially longer than a
                        // yield -- a deliberate back-off.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }

        // Drain received updates, bounded by both a message-count and a
        // wallclock budget. Whichever trips first ends this drain pass;
        // any remaining queued messages drain on subsequent iterations.
        //
        // The wallclock budget is recomputed each iteration (T-impl.10).
        // - scalar-flood: scales with the time-to-next-tick so we
        //   actually use the slack between bursts; floored at 1 ms when
        //   the publish phase already overran the tick.
        // - max-throughput: flat 5 ms -- no tick boundary, but the drain
        //   must not become unbounded.
        let drain_time_budget =
            compute_operate_drain_time_budget(max_throughput, next_tick, Instant::now());
        let drain_start = Instant::now();
        let mut drained = 0usize;
        while drained < drain_msg_budget {
            match variant.poll_receive()? {
                Some(update) => {
                    logger.log_receive(
                        &update.writer,
                        update.seq,
                        &update.path,
                        update.qos,
                        update.payload.len(),
                    )?;
                    progress.inc_received();
                    drained += 1;
                    if drain_start.elapsed() >= drain_time_budget {
                        break;
                    }
                }
                None => break,
            }
        }

        // Periodic resource sampling.
        if last_resource_sample.elapsed() >= resource_interval {
            let (cpu, mem) = resource_monitor.sample();
            logger.log_resource(cpu, mem)?;
            last_resource_sample = Instant::now();
        }

        // Variant-side idle detection (T15.5). Refresh the per-counter
        // "last change" timestamps from the emitter's atomic snapshot,
        // then -- if enabled -- check whether BOTH counters have been
        // flat for at least `operate_idle_secs`. The check is
        // unconditional bookkeeping followed by a single gated
        // comparison, so disabling it via `operate_idle_secs = 0` adds
        // no measurable overhead.
        let snap = progress.snapshot();
        let now_idle = Instant::now();
        if snap.sent > last_sent_value {
            last_sent_value = snap.sent;
            last_sent_change_at = now_idle;
        }
        if snap.received > last_received_value {
            last_received_value = snap.received;
            last_received_change_at = now_idle;
        }
        if idle_enabled
            && now_idle.duration_since(last_sent_change_at) >= idle_threshold
            && now_idle.duration_since(last_received_change_at) >= idle_threshold
        {
            idle_triggered = true;
            break;
        }
    }

    // Variant-side idle short-circuit (T15.5).
    //
    // When the operate loop exited via idle detection (rather than
    // operate_secs expiring), emit the `eot_sent` JSONL event the
    // analysis pipeline expects (T11.5, T14.17) and flip the progress
    // flags so the next stdout tick reflects `eot_sent: true`. We do
    // NOT engage the on-wire EOT exchange in this path -- the runner
    // (E15 / T15.4) is the authoritative observer of cross-peer
    // agreement, so the variant can mark `eot_received` optimistically
    // and proceed straight to `silent`. The on-wire path further below
    // stays in place for back-compat (T15.8 removes it later).
    if idle_triggered {
        // Optimistic eot_id of 0 -- the on-wire exchange is bypassed so
        // there is no peer-supplied id to record. The eot_id field on
        // the JSONL event is informational; analysis (T11.5) only uses
        // the `ts`.
        logger.log_eot_sent(0)?;
        progress.mark_eot_sent();
        progress.mark_eot_received();

        // -- Phase 5: Silent (drain + flush) -- (idle-triggered path) --
        //
        // We re-implement the same drain loop the on-wire path below
        // uses so the silent-phase semantics are preserved regardless
        // of which exit fires. Duplicating ~10 lines keeps the
        // pre-T15.5 code path totally untouched on the time-based
        // exit, which is the back-compat property the task spec calls
        // out.
        logger.log_phase(Phase::Silent, None)?;
        progress.set_phase(Phase::Silent);

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
                    progress.inc_received();
                }
                None => {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }

        variant.stop_reader_threads()?;
        variant.disconnect()?;
        logger.flush()?;

        progress.set_done();
        progress.stop();

        return Ok(());
    }

    // -- Phase 4: EOT (end-of-test handshake) --
    //
    // Per `metak-shared/api-contracts/eot-protocol.md`: the writer
    // signals EOT once, then waits (bounded by `--eot-timeout-secs`)
    // for every expected peer to signal EOT back. While waiting, any
    // in-flight `receive` events are still drained. Variants that do
    // not override `signal_end_of_test` / `poll_peer_eots` see an
    // `eot_timeout` event after the timeout (with the full peer set
    // as `missing`) but the spawn does NOT abort.
    logger.log_phase(Phase::Eot, None)?;
    progress.set_phase(Phase::Eot);

    let expected: HashSet<String> = parse_peer_names_from_extra(&config.extra)
        .into_iter()
        .filter(|name| name != &config.runner)
        .collect();
    // If there are no expected peers (single-runner self-loopback or
    // a misconfigured peer list), the variant has effectively
    // "received" every expected EOT before the wait loop starts. Set
    // the flag eagerly so the first progress tick in EOT phase
    // reflects that.
    if expected.is_empty() {
        progress.mark_eot_received();
    }

    let eot_timeout_secs = config
        .eot_timeout_secs
        .unwrap_or_else(|| default_eot_timeout_secs(config.operate_secs));
    let eot_timeout = Duration::from_secs(eot_timeout_secs);

    let my_eot_id = variant.signal_end_of_test()?;
    logger.log_eot_sent(my_eot_id)?;
    progress.mark_eot_sent();

    let eot_start = Instant::now();
    let deadline = eot_start + eot_timeout;
    let mut seen: HashSet<String> = HashSet::new();

    while seen != expected && Instant::now() < deadline {
        let new_eots = variant.poll_peer_eots()?;
        let mut got_any_new = false;
        for eot in new_eots {
            // Defensive dedup-by-writer: variant is the source of truth
            // but we backstop on our side too.
            if seen.insert(eot.writer.clone()) {
                logger.log_eot_received(&eot.writer, eot.eot_id)?;
                got_any_new = true;
            }
        }
        // Flip the progress flag once the expected-peer set has been
        // fully observed. Sticky for the remainder of the spawn.
        if !expected.is_empty() && expected.is_subset(&seen) {
            progress.mark_eot_received();
        }

        // Drain any in-flight data updates while waiting. Bound each
        // pass with the same two-budget pattern as the operate phase so a
        // peer that keeps publishing cannot starve the EOT poll loop.
        // Overall EOT semantics are unchanged: the outer loop keeps
        // iterating until every expected peer is seen or the timeout
        // expires, so total time spent draining can still exceed 1ms.
        //
        // EOT uses the pre-T-impl.10 budgets (2 * vpt, 1 ms wallclock)
        // deliberately -- the tick-aware widening is operate-only.
        let drain_start = Instant::now();
        let mut drained = 0usize;
        while drained < eot_drain_msg_budget {
            match variant.poll_receive()? {
                Some(update) => {
                    logger.log_receive(
                        &update.writer,
                        update.seq,
                        &update.path,
                        update.qos,
                        update.payload.len(),
                    )?;
                    progress.inc_received();
                    drained += 1;
                    if drain_start.elapsed() >= eot_drain_time_budget {
                        break;
                    }
                }
                None => break,
            }
        }

        if !got_any_new {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    if !expected.is_subset(&seen) {
        let mut missing: Vec<String> = expected.difference(&seen).cloned().collect();
        missing.sort();
        let wait_ms = eot_start.elapsed().as_millis() as u64;
        logger.log_eot_timeout(&missing, wait_ms)?;
    }

    // -- Phase 5: Silent (drain + flush) --
    logger.log_phase(Phase::Silent, None)?;
    progress.set_phase(Phase::Silent);

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
                progress.inc_received();
            }
            None => {
                // No pending updates; sleep briefly to avoid busy-waiting.
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    // Tear down reader threads (if any) BEFORE the variant's own
    // `disconnect` runs, so any in-flight receives those threads were
    // about to deliver can drain cleanly. The default impl is a no-op.
    // See E14 / T14.1.
    variant.stop_reader_threads()?;
    variant.disconnect()?;
    logger.flush()?;

    // Final progress transition: `done`. The emitter thread may emit
    // one more line in this state before we join it on `stop()`. We
    // explicitly stop here so the thread is joined before `Ok(())`
    // propagates out and the binary exits.
    progress.set_done();
    progress.stop();

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::{BufRead, BufReader};
    use std::path::Path;
    use std::time::Duration;

    use anyhow::Result;
    use tempfile::TempDir;

    use crate::cli::{CliArgs, DEFAULT_RECV_BUFFER_KB};
    use crate::driver::{
        default_eot_timeout_secs, run_protocol, DEFAULT_EOT_TIMEOUT_OPERATE_MULTIPLIER,
        MIN_DEFAULT_EOT_TIMEOUT_SECS,
    };
    use crate::types::{Qos, ReceivedUpdate, ThreadingMode};
    use crate::variant_trait::{PeerEot, Variant};

    #[test]
    fn default_eot_timeout_secs_uses_30s_floor_for_short_operate() {
        // operate_secs = 5 -> 3*5 = 15, floor at 30 -> 30.
        assert_eq!(default_eot_timeout_secs(5), 30);
        // operate_secs = 0 -> floor applies.
        assert_eq!(default_eot_timeout_secs(0), 30);
        // operate_secs = 10 -> 3*10 = 30, exactly at the floor.
        assert_eq!(default_eot_timeout_secs(10), 30);
    }

    #[test]
    fn default_eot_timeout_secs_scales_with_operate_secs() {
        // operate_secs = 30 -> 3*30 = 90, well above the 30s floor.
        assert_eq!(default_eot_timeout_secs(30), 90);
        // operate_secs = 60 -> 3*60 = 180.
        assert_eq!(default_eot_timeout_secs(60), 180);
    }

    #[test]
    fn default_eot_timeout_secs_saturates_on_overflow() {
        // u64::MAX as operate_secs would overflow 3 * operate_secs;
        // saturating_mul keeps the result at u64::MAX rather than panicking.
        assert_eq!(default_eot_timeout_secs(u64::MAX), u64::MAX);
    }

    #[test]
    fn default_eot_timeout_constants_match_documented_values() {
        assert_eq!(MIN_DEFAULT_EOT_TIMEOUT_SECS, 30);
        assert_eq!(DEFAULT_EOT_TIMEOUT_OPERATE_MULTIPLIER, 3);
    }

    #[test]
    fn driver_uses_default_when_eot_timeout_secs_is_none() {
        // We cannot directly observe the computed duration without spawning
        // a 90 s wait, so we verify the computation lives in
        // `default_eot_timeout_secs` and matches the documented examples.
        // operate_secs = 30, override = None -> 90
        assert_eq!(default_eot_timeout_secs(30), 90);
        // operate_secs = 5, override = None -> 30 (floor)
        assert_eq!(default_eot_timeout_secs(5), 30);
        // Override-wins path: driver applies the exact override unchanged,
        // ignoring the default-computation helper.
        fn pick(override_value: Option<u64>, operate_secs: u64) -> u64 {
            override_value.unwrap_or_else(|| default_eot_timeout_secs(operate_secs))
        }
        assert_eq!(pick(Some(5), 30), 5, "explicit override wins over default");
        assert_eq!(
            pick(None, 30),
            90,
            "default formula applies when override is None"
        );
        assert_eq!(
            pick(None, 5),
            30,
            "floor applies when override is None and operate is short"
        );
    }

    /// Variant that does NOT override the EOT trait methods, used to
    /// exercise the default-impl fallback path.
    struct StubVariant {
        name: &'static str,
    }

    impl StubVariant {
        fn new(name: &'static str) -> Self {
            Self { name }
        }
    }

    impl Variant for StubVariant {
        fn name(&self) -> &str {
            self.name
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            Ok(())
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// A second stub that DOES override the EOT methods so we can verify
    /// the driver's logging and dedup paths.
    struct EotStubVariant {
        name: &'static str,
        my_eot_id: u64,
        signal_calls: u32,
        scripted_eots: VecDeque<Vec<PeerEot>>,
    }

    impl EotStubVariant {
        fn new(name: &'static str, my_eot_id: u64, scripted: Vec<Vec<PeerEot>>) -> Self {
            Self {
                name,
                my_eot_id,
                signal_calls: 0,
                scripted_eots: scripted.into(),
            }
        }
    }

    impl Variant for EotStubVariant {
        fn name(&self) -> &str {
            self.name
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            Ok(())
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
        fn signal_end_of_test(&mut self) -> Result<u64> {
            self.signal_calls += 1;
            Ok(self.my_eot_id)
        }
        fn poll_peer_eots(&mut self) -> Result<Vec<PeerEot>> {
            Ok(self.scripted_eots.pop_front().unwrap_or_default())
        }
    }

    fn base_args(log_dir: &str, runner: &str, peers: &str, eot_timeout_secs: u64) -> CliArgs {
        CliArgs {
            tick_rate_hz: 100,
            stabilize_secs: 0,
            operate_secs: 0,
            silent_secs: 0,
            eot_timeout_secs: Some(eot_timeout_secs),
            workload: "scalar-flood".to_string(),
            values_per_tick: 1,
            qos: 1,
            log_dir: log_dir.to_string(),
            launch_ts: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.9fZ")
                .to_string(),
            variant: "test".to_string(),
            runner: runner.to_string(),
            run: "run01".to_string(),
            threading_mode: ThreadingMode::Single,
            recv_buffer_kb: DEFAULT_RECV_BUFFER_KB,
            // Disable stdout progress in driver unit tests so they
            // never touch the real process stdout.
            progress_stdout_interval_ms: 0,
            // Disable variant-side idle detection in driver unit tests
            // by default. Tests that exercise the new T15.5 path
            // override this explicitly.
            operate_idle_secs: 0,
            extra: vec!["--peers".to_string(), peers.to_string()],
        }
    }

    fn read_log(log_dir: &Path, runner: &str) -> Vec<serde_json::Value> {
        let path = log_dir.join(format!("test-{runner}-run01.jsonl"));
        let file = File::open(&path).expect("log file should exist");
        BufReader::new(file)
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn test_trait_defaults_return_zero_and_empty_vec() {
        let mut v = StubVariant::new("a");
        // Default impls are accessible via the trait.
        assert_eq!(v.signal_end_of_test().unwrap(), 0);
        assert!(v.poll_peer_eots().unwrap().is_empty());
    }

    #[test]
    fn test_eot_phase_emits_timeout_for_no_override_variant() {
        let dir = TempDir::new().unwrap();
        let args = base_args(
            dir.path().to_str().unwrap(),
            "alice",
            "alice=127.0.0.1,bob=127.0.0.1",
            1,
        );
        let mut variant = StubVariant::new("stub");
        run_protocol(&mut variant, &args).expect("protocol completes");

        let lines = read_log(dir.path(), "alice");
        let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();

        // phase=eot must appear and `eot_sent` with eot_id 0 (default impl).
        let eot_sent_lines: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "eot_sent").collect();
        assert_eq!(eot_sent_lines.len(), 1);
        assert_eq!(eot_sent_lines[0]["eot_id"], 0);

        // The driver must emit a single `eot_timeout` listing `bob` as missing.
        let timeout_lines: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "eot_timeout")
            .collect();
        assert_eq!(timeout_lines.len(), 1);
        let missing = timeout_lines[0]["missing"].as_array().unwrap();
        let names: Vec<&str> = missing.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(names, vec!["bob"]);
        assert!(timeout_lines[0]["wait_ms"].as_u64().unwrap() > 0);

        // Phase ordering: operate -> eot -> silent
        let phase_seq: Vec<&str> = lines
            .iter()
            .filter(|l| l["event"] == "phase")
            .map(|l| l["phase"].as_str().unwrap())
            .collect();
        assert_eq!(
            phase_seq,
            vec!["connect", "stabilize", "operate", "eot", "silent"]
        );

        // Existence of phase=eot in the event stream.
        assert!(events.contains(&"phase"));
    }

    #[test]
    fn test_eot_phase_logs_eot_received_and_no_timeout_when_all_peers_seen() {
        let dir = TempDir::new().unwrap();
        let args = base_args(
            dir.path().to_str().unwrap(),
            "alice",
            "alice=127.0.0.1,bob=127.0.0.1,carol=127.0.0.1",
            5,
        );

        // First poll returns nothing (test the sleep path), second returns
        // bob and carol.
        let mut variant = EotStubVariant::new(
            "stub",
            123,
            vec![
                vec![],
                vec![
                    PeerEot {
                        writer: "bob".into(),
                        eot_id: 11,
                    },
                    PeerEot {
                        writer: "carol".into(),
                        eot_id: 22,
                    },
                ],
            ],
        );
        run_protocol(&mut variant, &args).expect("protocol completes");
        assert_eq!(variant.signal_calls, 1, "signal_end_of_test called once");

        let lines = read_log(dir.path(), "alice");

        let eot_sent_lines: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "eot_sent").collect();
        assert_eq!(eot_sent_lines.len(), 1);
        assert_eq!(eot_sent_lines[0]["eot_id"], 123);

        let received_lines: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "eot_received")
            .collect();
        assert_eq!(received_lines.len(), 2);
        let writers: std::collections::HashSet<&str> = received_lines
            .iter()
            .map(|l| l["writer"].as_str().unwrap())
            .collect();
        assert!(writers.contains("bob"));
        assert!(writers.contains("carol"));

        let timeout_lines: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "eot_timeout")
            .collect();
        assert!(
            timeout_lines.is_empty(),
            "no eot_timeout when every peer EOT is seen"
        );
    }

    #[test]
    fn test_eot_phase_dedupes_repeated_writer() {
        let dir = TempDir::new().unwrap();
        let args = base_args(
            dir.path().to_str().unwrap(),
            "alice",
            "alice=127.0.0.1,bob=127.0.0.1",
            5,
        );

        // Variant returns bob twice (defensive dedup test on the driver
        // side).
        let mut variant = EotStubVariant::new(
            "stub",
            7,
            vec![
                vec![PeerEot {
                    writer: "bob".into(),
                    eot_id: 99,
                }],
                vec![PeerEot {
                    writer: "bob".into(),
                    eot_id: 99,
                }],
            ],
        );
        run_protocol(&mut variant, &args).expect("protocol completes");

        let lines = read_log(dir.path(), "alice");
        let received_lines: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "eot_received")
            .collect();
        assert_eq!(
            received_lines.len(),
            1,
            "driver must dedupe by-writer even if variant emits the same writer twice"
        );
        assert_eq!(received_lines[0]["writer"], "bob");
    }

    /// A variant whose `poll_receive` returns `Some` forever — modelling
    /// a peer that publishes faster than we can drain. Used to verify
    /// that the driver's bounded receive-drain still gives `publish` a
    /// chance to run (T-fairness.1).
    struct AlwaysReceiveVariant {
        publish_calls: u64,
    }

    impl AlwaysReceiveVariant {
        fn new() -> Self {
            Self { publish_calls: 0 }
        }
    }

    impl Variant for AlwaysReceiveVariant {
        fn name(&self) -> &str {
            "always-receive"
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            self.publish_calls += 1;
            Ok(())
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            // Always return Some — simulates an unbounded incoming firehose.
            Ok(Some(ReceivedUpdate {
                writer: "peer".to_string(),
                seq: 0,
                path: "/firehose".to_string(),
                qos: Qos::BestEffort,
                payload: vec![0u8; 8],
            }))
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_operate_loop_bounds_receive_drain() {
        // With an always-`Some` peer feed, the unbounded `while let
        // Some(_)` from before T-fairness.1 would never let `publish`
        // run more than once. With the bounded drain (default 1ms
        // wallclock budget), `publish` must be invoked many times
        // across a 1-second operate window.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(
            dir.path().to_str().unwrap(),
            "alice",
            // Single-runner so the EOT phase exits immediately.
            "alice=127.0.0.1",
            1,
        );
        // Max-throughput skips the tick sleep, so the outer loop is
        // dominated by the drain budget itself — easiest to measure.
        args.workload = "max-throughput".to_string();
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.values_per_tick = 1;

        let mut variant = AlwaysReceiveVariant::new();
        run_protocol(&mut variant, &args).expect("protocol completes");

        // 1ms drain budget over 1s operate -> conservatively expect at
        // least ~50 publishes (allows for scheduler jitter and slow CI).
        // The pre-fix code would publish exactly once per tick — i.e.
        // it would never get past the first iteration, so `publish_calls`
        // would equal `values_per_tick = 1`.
        assert!(
            variant.publish_calls >= 50,
            "publish should be called at least once per drain budget; got {}",
            variant.publish_calls
        );
    }

    #[test]
    fn test_eot_phase_terminates_immediately_when_expected_set_is_empty() {
        let dir = TempDir::new().unwrap();
        // Single-runner config: only this runner in --peers.
        let args = base_args(
            dir.path().to_str().unwrap(),
            "solo",
            "solo=127.0.0.1",
            // Set a long timeout to prove the phase exits without hitting it.
            60,
        );

        let mut variant = StubVariant::new("stub");
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();

        // Empty expected set -> the eot wait loop must exit immediately.
        // Ten seconds is far below the 60-second timeout but well above
        // any plausible scheduler jitter, so a true wait would clearly
        // exceed it.
        assert!(
            elapsed < Duration::from_secs(10),
            "EOT phase should not wait when expected set is empty (took {:?})",
            elapsed
        );

        let lines = read_log(dir.path(), "solo");
        let timeout_lines: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "eot_timeout")
            .collect();
        assert!(
            timeout_lines.is_empty(),
            "single-runner case must not emit eot_timeout"
        );

        // `eot_sent` is still emitted (default impl returns id 0).
        let eot_sent_lines: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "eot_sent").collect();
        assert_eq!(eot_sent_lines.len(), 1);

        // `eot_received` is NOT emitted (no peers).
        let received: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "eot_received")
            .collect();
        assert!(received.is_empty());
    }

    /// A variant whose `try_publish` always reports backpressure.
    /// Used to verify that the driver logs `backpressure_skipped`
    /// instead of `write` and never calls the underlying `publish`.
    struct AlwaysBackpressuredVariant {
        publish_calls: u64,
        try_publish_calls: u64,
    }

    impl AlwaysBackpressuredVariant {
        fn new() -> Self {
            Self {
                publish_calls: 0,
                try_publish_calls: 0,
            }
        }
    }

    impl Variant for AlwaysBackpressuredVariant {
        fn name(&self) -> &str {
            "always-backpressured"
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            // Track stray calls so the assertion can prove try_publish
            // did NOT fall through to publish on the Ok(false) path.
            self.publish_calls += 1;
            Ok(())
        }
        fn try_publish(
            &mut self,
            _path: &str,
            _payload: &[u8],
            _qos: Qos,
            _seq: u64,
        ) -> Result<bool> {
            self.try_publish_calls += 1;
            Ok(false)
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_backpressured_variant_logs_skipped_not_write() {
        // Short config: 1s operate, 10 Hz tick, 5 values per tick.
        // Expected: ~10 ticks * 5 values = ~50 backpressure_skipped
        // events, zero write events, and publish() never called.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(
            dir.path().to_str().unwrap(),
            "alice",
            // Single-runner self-loopback so EOT exits immediately.
            "alice=127.0.0.1",
            1,
        );
        args.tick_rate_hz = 10;
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.values_per_tick = 5;

        let mut variant = AlwaysBackpressuredVariant::new();
        run_protocol(&mut variant, &args).expect("protocol completes");

        let lines = read_log(dir.path(), "alice");
        let write_events: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "write").collect();
        let skip_events: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "backpressure_skipped")
            .collect();

        assert_eq!(
            write_events.len(),
            0,
            "no `write` events should be emitted when try_publish returns Ok(false)"
        );
        assert!(
            !skip_events.is_empty(),
            "expected at least one `backpressure_skipped` event over a 1s operate phase"
        );
        // Every skip event must carry path and qos and the common fields.
        for ev in &skip_events {
            assert!(ev.get("path").is_some(), "skip event missing path");
            assert!(ev.get("qos").is_some(), "skip event missing qos");
            assert!(ev.get("ts").is_some(), "skip event missing ts");
            assert_eq!(ev["runner"], "alice");
            assert_eq!(ev["variant"], "test");
            assert_eq!(ev["run"], "run01");
        }
        // The default impl was bypassed -- the override saw every call.
        assert_eq!(
            variant.publish_calls, 0,
            "publish() should not be called when try_publish is overridden to return Ok(false)"
        );
        assert!(
            variant.try_publish_calls > 0,
            "try_publish() should be called for every intended value"
        );
        // Sanity: there were as many try_publish calls as skip events.
        assert_eq!(
            variant.try_publish_calls as usize,
            skip_events.len(),
            "every Ok(false) call should produce exactly one `backpressure_skipped` event"
        );
    }

    /// A variant that does NOT override `try_publish`. Used to verify
    /// that the trait's default impl falls through to `publish` and
    /// the driver continues to emit `write` events (no
    /// `backpressure_skipped` events) -- preserving pre-T-impl.6
    /// behaviour.
    struct CountingPublishVariant {
        publish_calls: u64,
    }

    impl CountingPublishVariant {
        fn new() -> Self {
            Self { publish_calls: 0 }
        }
    }

    impl Variant for CountingPublishVariant {
        fn name(&self) -> &str {
            "counting-publish"
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            self.publish_calls += 1;
            Ok(())
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// A variant whose `try_publish` returns `Ok(false)` exactly once
    /// and `Ok(true)` forever after. Used to verify that the
    /// max-throughput loop reacts to the first backpressure with a
    /// cheap `yield_now()` (no sleep) and resumes immediately.
    struct OnceBackpressuredVariant {
        try_publish_calls: u64,
    }

    impl OnceBackpressuredVariant {
        fn new() -> Self {
            Self {
                try_publish_calls: 0,
            }
        }
    }

    impl Variant for OnceBackpressuredVariant {
        fn name(&self) -> &str {
            "once-backpressured"
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            Ok(())
        }
        fn try_publish(
            &mut self,
            _path: &str,
            _payload: &[u8],
            _qos: Qos,
            _seq: u64,
        ) -> Result<bool> {
            self.try_publish_calls += 1;
            // First call returns Ok(false); everything after is Ok(true).
            Ok(self.try_publish_calls != 1)
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// A variant whose `try_publish` alternates `Ok(false), Ok(true),
    /// Ok(false), Ok(true), ...` indefinitely. Used to verify that the
    /// max-throughput back-off counter resets on every successful
    /// publish, so each `false` triggers a cheap yield rather than the
    /// 1ms sleep path.
    struct AlternatingBackpressuredVariant {
        try_publish_calls: u64,
    }

    impl AlternatingBackpressuredVariant {
        fn new() -> Self {
            Self {
                try_publish_calls: 0,
            }
        }
    }

    impl Variant for AlternatingBackpressuredVariant {
        fn name(&self) -> &str {
            "alternating-backpressured"
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            Ok(())
        }
        fn try_publish(
            &mut self,
            _path: &str,
            _payload: &[u8],
            _qos: Qos,
            _seq: u64,
        ) -> Result<bool> {
            self.try_publish_calls += 1;
            // Odd calls (1st, 3rd, ...) -> Ok(false); even calls -> Ok(true).
            Ok(self.try_publish_calls.is_multiple_of(2))
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn max_throughput_yields_on_first_backpressure_does_not_sleep() {
        // Stub returns Ok(false) once, then Ok(true) forever. Under
        // max-throughput the first false should trigger yield_now() and
        // every subsequent true should produce a `write` event AND
        // reset the consecutive-skipped counter, so the sleep(1ms) path
        // is never taken on this variant.
        //
        // Direct timing assertion: the FIRST try_publish (false) wraps
        // a yield, and every subsequent try_publish is Ok(true). We
        // measure the wall-clock between the variant's first call and
        // the second call (via the publish counter). Yield should keep
        // that delta well under one Windows scheduler tick.
        //
        // We can't observe the variant's call timestamps directly here,
        // so we instead use the event count as a proxy: if a sleep had
        // happened on the first skip, the operate loop would still
        // accumulate millions of writes over 1s, but the test must
        // observe a HIGH write-rate -- which proves no per-skip sleep.
        // The simpler signal: with sleep(1ms) we would expect maybe
        // 1000-66000 writes/sec; without sleep (yield-only) the rate
        // is bounded by libc/log I/O at millions/sec.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 5);
        args.workload = "max-throughput".to_string();
        args.operate_secs = 1; // smallest non-zero value the CLI supports
        args.silent_secs = 0;
        args.values_per_tick = 1;

        let mut variant = OnceBackpressuredVariant::new();
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();

        let lines = read_log(dir.path(), "alice");
        let write_events: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "write").collect();
        let skip_events: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "backpressure_skipped")
            .collect();

        eprintln!(
            "max_throughput_yields_on_first_backpressure: elapsed={:?}, writes={}, skips={}",
            elapsed,
            write_events.len(),
            skip_events.len()
        );

        assert_eq!(
            skip_events.len(),
            1,
            "exactly one backpressure_skipped event expected; got {}",
            skip_events.len()
        );
        // With a 1s operate window and only the FIRST call returning
        // Ok(false), we should accumulate thousands of `write` events.
        // A sleep(1ms) on the first skip would NOT prevent this -- but
        // would push the total wall-clock above 1s by ~15ms on Windows
        // (negligible). The strong evidence here is the write count.
        assert!(
            write_events.len() > 100,
            "expected many `write` events after the single skip; got {}",
            write_events.len()
        );
        // Sanity-bound the wall-clock: 1s operate + EOT no-op +
        // a few ms of stabilize/silent/logger I/O. If something
        // accidentally inserted a long sleep, total time would balloon.
        // 3s gives generous slack for CI noise on Windows.
        assert!(
            elapsed < Duration::from_secs(3),
            "operate phase ran long -- did the yield path accidentally sleep? got {:?}",
            elapsed
        );
    }

    #[test]
    fn max_throughput_sleeps_after_consecutive_backpressure_does_rate_limit() {
        // Stub returns Ok(false) forever. Under max-throughput the
        // first false yields, every subsequent consecutive false hits
        // the 1ms sleep path. Over a 1-second operate window (the
        // minimum the CliArgs `operate_secs: u64` supports) the skip
        // count must be PACED -- bounded by the sleep granularity, not
        // free-spinning at millions/sec.
        //
        // Platform expectations (always-false, ~1s operate, vpt=1):
        //   Linux  (~1ms sleep): ~1000 skips
        //   Windows (~15ms sleep): ~66 skips
        // Free-spin (no back-off) would produce millions of skips per
        // second. We assert (well) below that.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(
            dir.path().to_str().unwrap(),
            "alice",
            "alice=127.0.0.1",
            5, // small EOT timeout, but empty expected-peer set exits immediately
        );
        args.workload = "max-throughput".to_string();
        args.operate_secs = 1; // smallest non-zero value the CLI supports
        args.silent_secs = 0;
        args.tick_rate_hz = 1;
        args.values_per_tick = 1; // one try_publish per outer iter -> each iter triggers back-off

        let mut variant = AlwaysBackpressuredVariant::new();
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();

        let lines = read_log(dir.path(), "alice");
        let skip_events: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "backpressure_skipped")
            .collect();

        eprintln!(
            "max_throughput_sleeps_after_consecutive: elapsed={:?}, skips={}, try_publish_calls={}",
            elapsed,
            skip_events.len(),
            variant.try_publish_calls
        );

        // Lower bound: pacing should still let SOME skips happen
        // (at least more than one per ~15ms tick over 1s on Windows).
        assert!(
            skip_events.len() >= 5,
            "expected at least 5 backpressure_skipped events over 1s; got {}",
            skip_events.len()
        );
        // Upper bound: the key assertion -- the loop is paced by the
        // sleep granularity, NOT free-spinning. Free-spin would push
        // skip counts into the millions in 1s. We accept any value up
        // to a few thousand to absorb fast Linux scheduling, but
        // anything above that means the back-off didn't fire.
        assert!(
            skip_events.len() < 5000,
            "max-throughput should be paced (not free-spinning); got {} skips in {:?}",
            skip_events.len(),
            elapsed
        );
    }

    #[test]
    fn max_throughput_resets_on_successful_publish() {
        // Stub returns alternating Ok(false), Ok(true), Ok(false), ...
        // Every false should be paired with a yield (since the previous
        // iteration's true reset the counter). NO false should ever hit
        // the sleep path -- which is the assertion this test enforces
        // via skip-rate: if the sleep path had fired, we would observe
        // a skip count throttled by sleep granularity. With pure yield
        // we should accumulate many thousands of skip+write pairs over
        // a 1-second operate window.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 5);
        args.workload = "max-throughput".to_string();
        args.operate_secs = 1; // smallest non-zero value
        args.silent_secs = 0;
        args.tick_rate_hz = 1;
        args.values_per_tick = 1; // simplest pattern: each outer iter is exactly one call

        let mut variant = AlternatingBackpressuredVariant::new();
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();

        let lines = read_log(dir.path(), "alice");
        let write_events: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "write").collect();
        let skip_events: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "backpressure_skipped")
            .collect();

        eprintln!(
            "max_throughput_resets_on_success: elapsed={:?}, writes={}, skips={}",
            elapsed,
            write_events.len(),
            skip_events.len()
        );

        // Both event types should be present and roughly equal (the
        // pattern alternates 1:1).
        assert!(
            !write_events.is_empty(),
            "expected `write` events from the alternating pattern"
        );
        assert!(
            !skip_events.is_empty(),
            "expected `backpressure_skipped` events from the alternating pattern"
        );
        // 1:1 ratio within a small slack (off-by-one if loop ends on a
        // false).
        let diff = write_events.len().abs_diff(skip_events.len());
        assert!(
            diff <= 1,
            "expected ~equal write and skip counts in alternating pattern; got writes={}, skips={}",
            write_events.len(),
            skip_events.len()
        );

        // KEY assertion: if every false had triggered sleep(1ms), in
        // 1 second we would have observed at most ~1000 skip+write
        // pairs on Linux (~66 on Windows). With the counter resetting
        // to 0 on each Ok(true), every false instead takes the yield
        // path, and we should observe MANY MORE pairs than the
        // sleep-bound -- proving the reset works.
        //
        // We require a count well above the Windows sleep ceiling
        // (~66/s). 1000 skips in 1s is achievable even with sleep on
        // Linux, so we set the bar at 5000+ to unambiguously rule out
        // the sleep path on either platform.
        assert!(
            skip_events.len() > 5000,
            "reset-on-success should bypass sleep; got only {} skips in {:?} (expected >5000, indicating yield-only path)",
            skip_events.len(),
            elapsed
        );
    }

    #[test]
    fn scalar_flood_max_throughput_path_unchanged() {
        // Stub returns Ok(false) forever under the scalar-flood profile.
        // The driver must NOT apply the max-throughput yield/sleep
        // back-off here -- the inter-tick sleep is the sole pacing.
        // We expect exactly tick_rate_hz * operate_secs * vpt skipped
        // events (one per intended value), and the wall-clock should
        // be roughly operate_secs (no extra back-off was added).
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 1);
        args.workload = "scalar-flood".to_string();
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.tick_rate_hz = 10;
        args.values_per_tick = 5;

        let mut variant = AlwaysBackpressuredVariant::new();
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();

        let lines = read_log(dir.path(), "alice");
        let skip_events: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "backpressure_skipped")
            .collect();
        let write_events: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "write").collect();

        eprintln!(
            "scalar_flood_unchanged: elapsed={:?}, skips={}, writes={}",
            elapsed,
            skip_events.len(),
            write_events.len()
        );

        // Expected = tick_rate_hz * operate_secs * vpt = 10 * 1 * 5 = 50.
        // Allow small slack (one or two ticks of timing jitter).
        let expected = (args.tick_rate_hz as usize)
            * (args.operate_secs as usize)
            * (args.values_per_tick as usize);
        let low = expected.saturating_sub(args.values_per_tick as usize * 2);
        let high = expected + args.values_per_tick as usize * 2;
        assert!(
            (low..=high).contains(&skip_events.len()),
            "scalar-flood skip count should equal ticks*vpt (~{expected}); got {} (range {}..={})",
            skip_events.len(),
            low,
            high
        );
        assert!(
            write_events.is_empty(),
            "always-backpressured variant should produce no `write` events"
        );

        // Wall-clock: roughly operate_secs (1s) with no extra back-off.
        // The new yield/sleep code path is gated on max-throughput so
        // it must not fire here. Allow generous slack for stabilize=0,
        // silent=0, EOT (empty expected set exits immediately) and CI
        // jitter.
        assert!(
            elapsed < Duration::from_millis(2500),
            "scalar-flood should not gain new yield/sleep back-off; got {:?}",
            elapsed
        );
    }

    #[test]
    fn test_default_try_publish_falls_through_to_publish() {
        // A variant that does not override try_publish must behave
        // identically to today: every value -> one `write` event, zero
        // `backpressure_skipped` events.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 1);
        args.tick_rate_hz = 10;
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.values_per_tick = 5;

        let mut variant = CountingPublishVariant::new();
        run_protocol(&mut variant, &args).expect("protocol completes");

        let lines = read_log(dir.path(), "alice");
        let write_events: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l["event"] == "write").collect();
        let skip_events: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["event"] == "backpressure_skipped")
            .collect();

        assert!(
            !write_events.is_empty(),
            "default try_publish should produce at least one `write` event over a 1s operate phase"
        );
        assert!(
            skip_events.is_empty(),
            "default try_publish must not emit any `backpressure_skipped` events"
        );
        // Every write event corresponds to one publish() call.
        assert_eq!(
            variant.publish_calls as usize,
            write_events.len(),
            "publish() call count should match `write` event count"
        );
    }

    // ----- T-impl.10: operate-loop drain budget tests -----

    /// Variant whose `poll_receive` always returns `Some` (unbounded
    /// inbound) and records each call's timestamp segmented by drain
    /// phase. A new drain phase begins on every `try_publish` (or
    /// `publish` fallthrough) call. Lets a test measure the wall-clock
    /// duration of each drain phase independently.
    struct InstrumentedReceiveVariant {
        // Per drain phase: vector of `Instant`s when `poll_receive` was
        // called during that phase. A new entry is appended on every
        // call to `try_publish` (or its fallback `publish`).
        drain_phases: Vec<Vec<std::time::Instant>>,
    }

    impl InstrumentedReceiveVariant {
        fn new() -> Self {
            Self {
                drain_phases: vec![Vec::new()],
            }
        }

        fn begin_drain_phase(&mut self) {
            // Only start a fresh phase if the current one already has
            // entries; otherwise the test setup (back-to-back publishes
            // without an intervening receive) would create a flurry of
            // empty phases and break per-phase counting.
            if self.drain_phases.last().is_some_and(|p| !p.is_empty()) {
                self.drain_phases.push(Vec::new());
            }
        }
    }

    impl Variant for InstrumentedReceiveVariant {
        fn name(&self) -> &str {
            "instrumented-receive"
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            self.begin_drain_phase();
            Ok(())
        }
        fn try_publish(
            &mut self,
            _path: &str,
            _payload: &[u8],
            _qos: Qos,
            _seq: u64,
        ) -> Result<bool> {
            // Always accept. The trait default would fall through to
            // `publish` but we override to also record the phase boundary
            // on the success path without bumping `publish` counters.
            self.begin_drain_phase();
            Ok(true)
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            self.drain_phases
                .last_mut()
                .expect("at least one phase exists")
                .push(std::time::Instant::now());
            Ok(Some(ReceivedUpdate {
                writer: "peer".to_string(),
                seq: 0,
                path: "/firehose".to_string(),
                qos: Qos::BestEffort,
                payload: vec![0u8; 8],
            }))
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scalar_flood_drain_msg_budget_is_four_x_vpt() {
        // Scenario: low tick rate (100 Hz -> 10 ms tick) and small
        // `values_per_tick` (10). The publish phase finishes fast, so
        // most of the tick is available for the drain. With the
        // pre-T-impl.10 1 ms wallclock cap the drain would terminate
        // well below the message budget; with the new tick-aware
        // formula the message budget (`4 * vpt = 40`) is the operative
        // limit.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 1);
        args.workload = "scalar-flood".to_string();
        args.tick_rate_hz = 100;
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.values_per_tick = 10;

        let mut variant = InstrumentedReceiveVariant::new();
        run_protocol(&mut variant, &args).expect("protocol completes");

        // Inspect drain phases that occurred AFTER a publish. The
        // very first phase (index 0) is the pre-first-publish window
        // which may be empty; meaningful phases are non-empty.
        let nonempty_phases: Vec<&Vec<std::time::Instant>> = variant
            .drain_phases
            .iter()
            .filter(|p| !p.is_empty())
            .collect();
        eprintln!(
            "scalar_flood_drain_msg_budget: phases={}, vpt={}",
            nonempty_phases.len(),
            args.values_per_tick
        );
        assert!(
            nonempty_phases.len() >= 10,
            "expected many drain phases over a 1s operate at 100 Hz; got {}",
            nonempty_phases.len()
        );

        // Each drain phase must be capped at `4 * vpt = 40` receives.
        let budget = (args.values_per_tick as usize) * 4;
        for (idx, phase) in nonempty_phases.iter().enumerate() {
            assert!(
                phase.len() <= budget,
                "drain phase {idx} exceeded msg budget: got {} receives, budget {}",
                phase.len(),
                budget,
            );
        }

        // The TYPICAL drain phase should hit the message budget --
        // the wallclock cap is now slack-aware (and at 100 Hz / vpt=10
        // there is several ms of slack each tick). Allow some phases to
        // come in short (jitter, scheduler) but require that the median
        // phase saturates the message budget.
        let mut lens: Vec<usize> = nonempty_phases.iter().map(|p| p.len()).collect();
        lens.sort_unstable();
        let median = lens[lens.len() / 2];
        assert_eq!(
            median, budget,
            "median drain-phase size should saturate `4 * vpt = {budget}` (the message budget is operative); got {median}. Full distribution: {lens:?}",
        );
    }

    #[test]
    fn scalar_flood_drain_does_not_overrun_tick() {
        // Tight tick: 1000 Hz -> 1 ms tick. Publishing 1000 values per
        // tick will saturate (or overrun) the tick. The drain budget
        // formula must NOT add measurable overrun on top of whatever
        // overrun publishing already incurs. We assert the total
        // operate-phase wall-clock stays within `operate_secs + 50 ms`
        // over a 1-second run.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 1);
        args.workload = "scalar-flood".to_string();
        args.tick_rate_hz = 1000;
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.values_per_tick = 1000;

        let mut variant = InstrumentedReceiveVariant::new();
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();
        eprintln!("scalar_flood_drain_does_not_overrun_tick: elapsed={elapsed:?}");

        // The driver entered the protocol BEFORE the operate phase
        // (connect, stabilize) and exits AFTER (eot, silent). With
        // stabilize=0, silent=0, and a single-runner empty expected-peer
        // EOT, those overheads are minimal. The dominant wall-clock is
        // the operate phase itself plus per-iteration drain overhead.
        // 1 s operate + 50 ms slack absorbs scheduler jitter, log I/O,
        // and the floor-at-1ms fallback when the publish phase overran
        // the tick. Anything beyond that means the drain compounded the
        // lateness, which is exactly what the formula must prevent.
        assert!(
            elapsed < Duration::from_millis(1000 + 50),
            "operate phase should not slip more than 50 ms beyond operate_secs; got {elapsed:?}",
        );
    }

    #[test]
    fn max_throughput_drain_bounded_to_five_ms() {
        // Stub variant with unbounded inbound + max-throughput profile.
        // Each drain phase must be bounded by ~5 ms (the
        // MAX_THROUGHPUT_DRAIN_TIME_BUDGET). Without bounding, the
        // first drain phase would never exit and the test would hang
        // forever. We use a tolerant ceiling (<25 ms) to absorb
        // Windows scheduler jitter and the cost of logging each
        // receive event to disk.
        //
        // `values_per_tick=1_000_000` makes the message budget
        // effectively unreachable (4 million per phase) so the
        // wallclock cap is the operative limit.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 1);
        args.workload = "max-throughput".to_string();
        args.tick_rate_hz = 1; // ignored under max-throughput
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.values_per_tick = 1_000_000;

        let mut variant = InstrumentedReceiveVariant::new();
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();

        let nonempty_phases: Vec<&Vec<std::time::Instant>> = variant
            .drain_phases
            .iter()
            .filter(|p| p.len() >= 2)
            .collect();
        eprintln!(
            "max_throughput_drain_bounded: elapsed={elapsed:?}, phases_with_2plus={}",
            nonempty_phases.len()
        );
        assert!(
            !nonempty_phases.is_empty(),
            "expected at least one drain phase with >=2 receives over 1s max-throughput",
        );

        // Per-phase wall-clock: from first to last receive in the phase.
        // 25 ms tolerance for Windows scheduler / log I/O noise on the
        // 5 ms target.
        for (idx, phase) in nonempty_phases.iter().enumerate() {
            let phase_dur = *phase.last().unwrap() - *phase.first().unwrap();
            assert!(
                phase_dur < Duration::from_millis(25),
                "max-throughput drain phase {idx} too long: {phase_dur:?} (cap is 5ms; tolerance 25ms)",
            );
        }
    }

    /// Variant whose `poll_receive` always returns `Ok(None)` (empty
    /// queue) and counts every call so the test can verify the early-
    /// exit fired regardless of the wallclock budget.
    struct EmptyReceiveCountingVariant {
        poll_receive_calls: u64,
    }

    impl EmptyReceiveCountingVariant {
        fn new() -> Self {
            Self {
                poll_receive_calls: 0,
            }
        }
    }

    impl Variant for EmptyReceiveCountingVariant {
        fn name(&self) -> &str {
            "empty-receive-counting"
        }
        fn connect(&mut self, _threading_mode: ThreadingMode) -> Result<()> {
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            Ok(())
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            self.poll_receive_calls += 1;
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn empty_queue_drain_still_early_exits() {
        // The drain inner loop's `None => break` arm must fire
        // immediately when the queue is empty, regardless of how
        // generous the wallclock budget is. We assert that over a 1 s
        // operate window at 100 Hz the tick cadence is preserved
        // (~100 outer iterations -> ~100 `poll_receive` calls in the
        // operate phase, plus a few from the EOT/silent paths) and
        // that the operate phase completes in roughly `operate_secs`.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "alice", "alice=127.0.0.1", 1);
        args.workload = "scalar-flood".to_string();
        args.tick_rate_hz = 100;
        args.operate_secs = 1;
        args.silent_secs = 0;
        args.values_per_tick = 1;

        let mut variant = EmptyReceiveCountingVariant::new();
        let start = std::time::Instant::now();
        run_protocol(&mut variant, &args).expect("protocol completes");
        let elapsed = start.elapsed();
        eprintln!(
            "empty_queue_drain_still_early_exits: elapsed={elapsed:?}, poll_receive_calls={}",
            variant.poll_receive_calls
        );

        // Wall-clock: ~1 s of operate (+ EOT immediate exit + 0 silent).
        // 250 ms slack absorbs scheduler jitter on CI.
        assert!(
            elapsed < Duration::from_millis(1250),
            "empty-queue drain should not add measurable overhead; got {elapsed:?}",
        );
        // Each outer iteration does exactly ONE `poll_receive` (because
        // the very first call returns None and breaks). At 100 Hz over
        // 1 s that's ~100 calls; the EOT phase exits immediately (empty
        // expected set) so it contributes none. Bound loosely to absorb
        // jitter.
        assert!(
            variant.poll_receive_calls >= 50 && variant.poll_receive_calls <= 500,
            "expected ~100 poll_receive calls (1s at 100Hz, one per iter); got {}",
            variant.poll_receive_calls,
        );
    }

    // ----- T14.1: threading-mode hook ordering tests -----

    /// Variant that records the order in which its lifecycle methods
    /// are called and the `ThreadingMode` it received at `connect` /
    /// `start_reader_threads`. Used to verify the driver invokes the
    /// new reader-thread hooks in the documented order.
    struct LifecycleRecordingVariant {
        connect_mode: Option<ThreadingMode>,
        start_mode: Option<ThreadingMode>,
        events: Vec<&'static str>,
    }

    impl LifecycleRecordingVariant {
        fn new() -> Self {
            Self {
                connect_mode: None,
                start_mode: None,
                events: Vec::new(),
            }
        }
    }

    impl Variant for LifecycleRecordingVariant {
        fn name(&self) -> &str {
            "lifecycle-recording"
        }
        fn supported_threading_modes(&self) -> &'static [ThreadingMode] {
            &[ThreadingMode::Single, ThreadingMode::Multi]
        }
        fn connect(&mut self, threading_mode: ThreadingMode) -> Result<()> {
            self.connect_mode = Some(threading_mode);
            self.events.push("connect");
            Ok(())
        }
        fn start_reader_threads(&mut self, mode: ThreadingMode) -> Result<()> {
            self.start_mode = Some(mode);
            self.events.push("start_reader_threads");
            Ok(())
        }
        fn stop_reader_threads(&mut self) -> Result<()> {
            self.events.push("stop_reader_threads");
            Ok(())
        }
        fn publish(&mut self, _path: &str, _payload: &[u8], _qos: Qos, _seq: u64) -> Result<()> {
            Ok(())
        }
        fn poll_receive(&mut self) -> Result<Option<ReceivedUpdate>> {
            Ok(None)
        }
        fn disconnect(&mut self) -> Result<()> {
            self.events.push("disconnect");
            Ok(())
        }
    }

    #[test]
    fn default_supported_threading_modes_is_single_only() {
        // StubVariant does not override `supported_threading_modes`, so
        // the trait default must return `[Single]`.
        let stub = StubVariant::new("stub");
        assert_eq!(
            stub.supported_threading_modes(),
            &[ThreadingMode::Single],
            "default trait impl must report Single-only"
        );
    }

    #[test]
    fn default_start_and_stop_reader_threads_are_noops_returning_ok() {
        // StubVariant does not override either hook, so the trait
        // default must accept any mode and return Ok(()).
        let mut stub = StubVariant::new("stub");
        assert!(stub.start_reader_threads(ThreadingMode::Single).is_ok());
        assert!(stub.start_reader_threads(ThreadingMode::Multi).is_ok());
        assert!(stub.stop_reader_threads().is_ok());
    }

    #[test]
    fn driver_calls_reader_thread_hooks_in_order_around_connect_disconnect() {
        // Ordering contract from T14.1:
        //   connect -> start_reader_threads -> ... -> stop_reader_threads -> disconnect
        let dir = TempDir::new().unwrap();
        let mut args = base_args(
            dir.path().to_str().unwrap(),
            "solo",
            // Single-runner self-loopback -> empty expected EOT peers.
            "solo=127.0.0.1",
            1,
        );
        args.operate_secs = 0;
        args.silent_secs = 0;
        args.threading_mode = ThreadingMode::Multi;

        let mut variant = LifecycleRecordingVariant::new();
        run_protocol(&mut variant, &args).expect("protocol completes");

        // `connect` saw Multi mode.
        assert_eq!(variant.connect_mode, Some(ThreadingMode::Multi));
        // `start_reader_threads` saw the same Multi mode.
        assert_eq!(variant.start_mode, Some(ThreadingMode::Multi));
        // The hooks fired in the documented order.
        assert_eq!(
            variant.events,
            vec![
                "connect",
                "start_reader_threads",
                "stop_reader_threads",
                "disconnect",
            ]
        );
    }

    #[test]
    fn driver_passes_threading_mode_single_through_to_connect_and_start() {
        // Same shape as the Multi test but verifies the Single path so
        // we know the driver doesn't accidentally hard-code one mode.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "solo", "solo=127.0.0.1", 1);
        args.operate_secs = 0;
        args.silent_secs = 0;
        args.threading_mode = ThreadingMode::Single;

        let mut variant = LifecycleRecordingVariant::new();
        run_protocol(&mut variant, &args).expect("protocol completes");

        assert_eq!(variant.connect_mode, Some(ThreadingMode::Single));
        assert_eq!(variant.start_mode, Some(ThreadingMode::Single));
    }

    #[test]
    fn connected_event_records_threading_mode_and_recv_buffer_kb() {
        // The driver must emit a `connected` event tagged with the
        // exact threading mode and recv-buffer-kb the spawn ran under.
        let dir = TempDir::new().unwrap();
        let mut args = base_args(dir.path().to_str().unwrap(), "solo", "solo=127.0.0.1", 1);
        args.operate_secs = 0;
        args.silent_secs = 0;
        args.threading_mode = ThreadingMode::Multi;
        args.recv_buffer_kb = 8192;

        let mut variant = LifecycleRecordingVariant::new();
        run_protocol(&mut variant, &args).expect("protocol completes");

        let lines = read_log(dir.path(), "solo");
        let connected: &serde_json::Value = lines
            .iter()
            .find(|l| l["event"] == "connected")
            .expect("connected event must be present");
        assert_eq!(connected["threading_mode"], "multi");
        assert_eq!(connected["recv_buffer_kb"], 8192);
    }
}
