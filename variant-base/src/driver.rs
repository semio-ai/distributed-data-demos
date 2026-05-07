use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::DateTime;

use crate::cli::{parse_peer_names_from_extra, CliArgs};
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
    // drains. See T-fairness.1.
    //
    // Defaults: drain at most `2 * values_per_tick` messages (so a fair
    // drain still keeps up with a peer publishing at our rate), and at
    // most 1ms of wallclock per outer iteration. Whichever trips first
    // breaks out and lets the next publish tick run; remaining queued
    // messages stay in the variant's internal buffer.
    let drain_msg_budget = (config.values_per_tick as usize).saturating_mul(2).max(1);
    let drain_time_budget = Duration::from_millis(1);

    while operate_start.elapsed() < operate_duration {
        // In max-throughput mode, skip the tick sleep entirely.
        if !max_throughput {
            let now = Instant::now();
            if now < next_tick {
                std::thread::sleep(next_tick - now);
            }
            next_tick += tick_interval;
        }

        // Generate and publish writes.
        let ops = workload.generate(config.values_per_tick);
        for op in &ops {
            let seq = seq_gen.next_seq();
            variant.publish(&op.path, &op.payload, qos, seq)?;
            logger.log_write(seq, &op.path, qos, op.payload.len())?;
        }

        // Drain received updates, bounded by both a message-count and a
        // wallclock budget. Whichever trips first ends this drain pass;
        // any remaining queued messages drain on subsequent iterations.
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

    let expected: HashSet<String> = parse_peer_names_from_extra(&config.extra)
        .into_iter()
        .filter(|name| name != &config.runner)
        .collect();

    let eot_timeout_secs = config
        .eot_timeout_secs
        .unwrap_or_else(|| std::cmp::max(config.operate_secs, 5));
    let eot_timeout = Duration::from_secs(eot_timeout_secs);

    let my_eot_id = variant.signal_end_of_test()?;
    logger.log_eot_sent(my_eot_id)?;

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

        // Drain any in-flight data updates while waiting. Bound each
        // pass with the same two-budget pattern as the operate phase so a
        // peer that keeps publishing cannot starve the EOT poll loop.
        // Overall EOT semantics are unchanged: the outer loop keeps
        // iterating until every expected peer is seen or the timeout
        // expires, so total time spent draining can still exceed 1ms.
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
                    drained += 1;
                    if drain_start.elapsed() >= drain_time_budget {
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::{BufRead, BufReader};
    use std::path::Path;
    use std::time::Duration;

    use anyhow::Result;
    use tempfile::TempDir;

    use crate::cli::CliArgs;
    use crate::driver::run_protocol;
    use crate::types::{Qos, ReceivedUpdate};
    use crate::variant_trait::{PeerEot, Variant};

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
        fn connect(&mut self) -> Result<()> {
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
        fn connect(&mut self) -> Result<()> {
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
        fn connect(&mut self) -> Result<()> {
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
}
