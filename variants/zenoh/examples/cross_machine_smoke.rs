//! T16.20 — Minimal Zenoh pub/sub cross-machine smoke binary.
//!
//! Standalone diagnostic: no bench plumbing, no JSONL, no variant-base. The
//! goal is a ~200-line program that exposes the same Zenoh primitives the
//! `variant-zenoh` benchmark uses (per-publisher CongestionControl cache,
//! `bench/**`-style wildcard subscriber, the `--multicast-interface`
//! scouting pin, the `transport/link` queue / buffer sizes) so we can
//! bisect WHERE cross-machine traffic is dying:
//!   - the bench's subscription / encoding setup
//!   - Zenoh's own cross-machine networking on WiFi
//!   - the AP / firewall layer underneath
//!
//! Build:
//!   cargo build --release -p variant-zenoh --example cross_machine_smoke
//!
//! See `examples/CROSS_MACHINE_SMOKE.md` for the user-facing two-machine
//! procedure and `examples/run_cross_machine_smoke.ps1` for the wrapper.
//!
//! NB on the QoS mapping: the bench variant only ever calls
//! `.congestion_control(...)` on the publisher builder; it does NOT call
//! `.reliability(...)`. `Reliability` in `zenoh::qos` is gated behind
//! the `unstable` feature (not enabled on the variant's stable
//! dependency), so the smoke matches the bench exactly: only
//! CongestionControl is configured.
//!
//! The T16.20 task spec lists a (Reliability, CC) mapping per QoS level,
//! but the bench-variant's actual mapping (see
//! `variants/zenoh/src/zenoh.rs` around lines 1442-1452 + the
//! `publishers_drop` / `publishers_block` caches) is:
//!   QoS 1 (BestEffort)   -> CongestionControl::Drop
//!   QoS 2 (LatestValue)  -> CongestionControl::Drop
//!   QoS 3 (ReliableUdp)  -> CongestionControl::Block
//!   QoS 4 (ReliableTcp)  -> CongestionControl::Block
//! The task instructs the worker to MATCH the variant's mapping rather
//! than the spec's literal one (which diverges from the variant), so the
//! smoke uses that. Spec mapping is reproduced in the startup banner as
//! a reference.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use zenoh::pubsub::Publisher;
use zenoh::qos::CongestionControl;
use zenoh::Config;

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mode {
    Pub,
    Sub,
    Both,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum PeerMode {
    Peer,
    Client,
}

impl PeerMode {
    fn as_str(self) -> &'static str {
        match self {
            PeerMode::Peer => "peer",
            PeerMode::Client => "client",
        }
    }
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "cross_machine_smoke",
    about = "T16.20 minimal Zenoh pub/sub diagnostic"
)]
struct Args {
    /// Which side(s) to run.
    #[arg(long, value_enum)]
    mode: Mode,
    /// Zenoh session mode.
    #[arg(long, value_enum, default_value_t = PeerMode::Peer)]
    peer_mode: PeerMode,
    /// Pin scouting/multicast/interface to this local IPv4 (matches the
    /// variant's `--multicast-interface`). When unset, Zenoh defaults to
    /// `"auto"`.
    #[arg(long)]
    multicast_interface: Option<Ipv4Addr>,
    /// Diagnostic QoS knob (matches the variant's mapping):
    ///   1 (BestEffort)   -> CongestionControl::Drop
    ///   2 (LatestValue)  -> CongestionControl::Drop
    ///   3 (ReliableUdp)  -> CongestionControl::Block
    ///   4 (ReliableTcp)  -> CongestionControl::Block
    #[arg(long, default_value_t = 1)]
    qos: u8,
    /// Key prefix. The publisher emits on `<key>/0..N-1`; the subscriber
    /// declares `<key>/**`. Keep this DIFFERENT from `bench/` so the
    /// smoke can run alongside a bench matrix without colliding.
    #[arg(long, default_value = "smoke/test")]
    key: String,
    /// Publisher ticks per second. 0 means "as fast as possible".
    #[arg(long, default_value_t = 100)]
    rate_hz: u32,
    /// Number of distinct keys (per tick the publisher emits one sample
    /// for each of `<key>/0..N-1`).
    #[arg(long, default_value_t = 1000)]
    values_per_tick: u32,
    /// How long to run the publish loop.
    #[arg(long, default_value_t = 30)]
    duration_secs: u32,
    /// Raw payload byte length per sample.
    #[arg(long, default_value_t = 16)]
    payload_size_bytes: usize,
    /// Optional explicit connect endpoints (e.g. `tcp/192.168.1.77:7447`).
    /// When set, bypass multicast scouting entirely. Repeatable.
    #[arg(long)]
    connect: Vec<String>,
    /// Optional explicit listen endpoints. Repeatable.
    #[arg(long)]
    listen: Vec<String>,
    /// In `pub` and `both` modes, before starting to publish, wait until
    /// the session reports at least this many connected peers (or
    /// `--wait-peers-timeout-secs` elapses). 0 disables the wait
    /// (default; matches the bench variant's behaviour). Set to 1 for
    /// the localhost-gate two-process smoke so the first ~50 publishes
    /// don't race ahead of multicast scouting and silently drop on
    /// CongestionControl::Drop paths.
    #[arg(long, default_value_t = 0)]
    wait_peers: usize,
    /// Max time `--wait-peers` will block startup before giving up and
    /// publishing anyway.
    #[arg(long, default_value_t = 10)]
    wait_peers_timeout_secs: u32,
}

/// QoS -> CongestionControl per the variant's actual mapping
/// (`publishers_drop` / `publishers_block` caches in
/// `variants/zenoh/src/zenoh.rs`). Also returns the spec's nominal
/// Reliability label for banner-display purposes only -- nothing in
/// this binary acts on it because zenoh-1.9's `Reliability` enum is
/// gated behind the `unstable` feature.
fn qos_to_zenoh(qos: u8) -> Result<(&'static str, CongestionControl)> {
    match qos {
        1 => Ok(("BestEffort", CongestionControl::Drop)),
        2 => Ok(("LatestValue", CongestionControl::Drop)),
        3 => Ok(("ReliableUdp", CongestionControl::Block)),
        4 => Ok(("ReliableTcp", CongestionControl::Block)),
        _ => bail!("--qos must be one of 1, 2, 3, 4 (got {qos})"),
    }
}

fn build_config(args: &Args) -> Result<Config> {
    let mut config = Config::default();
    config
        .insert_json5("mode", &format!("\"{}\"", args.peer_mode.as_str()))
        .map_err(|e| anyhow!("zenoh insert_json5 mode: {e}"))?;

    // Mirror the bench variant's TX queue depth + RX buffer (see
    // variants/zenoh/CUSTOM.md "Transport queue tuning"). Without these
    // the smoke would silently exercise a different transport profile
    // than the bench it is meant to diagnose.
    for prio in [
        "control",
        "real_time",
        "interactive_high",
        "interactive_low",
        "data_high",
        "data",
        "data_low",
        "background",
    ] {
        config
            .insert_json5(&format!("transport/link/tx/queue/size/{prio}"), "16")
            .map_err(|e| anyhow!("zenoh insert_json5 tx queue {prio}: {e}"))?;
    }
    config
        .insert_json5("transport/link/rx/buffer_size", "8388608")
        .map_err(|e| anyhow!("zenoh insert_json5 rx buffer: {e}"))?;

    // T16.10d: deterministic autoconnect (same as the variant). Without
    // this the smoke can establish two redundant routes on peer-mode and
    // mask the very route-establishment race we're chasing.
    config
        .insert_json5("scouting/multicast/autoconnect_strategy", "\"greater-zid\"")
        .map_err(|e| anyhow!("zenoh insert_json5 mc autoconnect: {e}"))?;
    config
        .insert_json5("scouting/gossip/autoconnect_strategy", "\"greater-zid\"")
        .map_err(|e| anyhow!("zenoh insert_json5 gossip autoconnect: {e}"))?;

    if let Some(ip) = args.multicast_interface {
        config
            .insert_json5("scouting/multicast/interface", &format!("\"{}\"", ip))
            .map_err(|e| anyhow!("zenoh insert_json5 multicast interface: {e}"))?;
        eprintln!(
            "[smoke] multicast interface: {} (pinned via --multicast-interface)",
            ip
        );
    } else {
        eprintln!("[smoke] multicast interface: auto");
    }

    if !args.listen.is_empty() {
        let arr = args
            .listen
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(",");
        config
            .insert_json5("listen/endpoints", &format!("[{arr}]"))
            .map_err(|e| anyhow!("zenoh insert_json5 listen: {e}"))?;
    }

    if !args.connect.is_empty() {
        let arr = args
            .connect
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(",");
        config
            .insert_json5("connect/endpoints", &format!("[{arr}]"))
            .map_err(|e| anyhow!("zenoh insert_json5 connect: {e}"))?;
    }

    Ok(config)
}

fn print_startup_banner(args: &Args, qos_label: &str, cc: CongestionControl) {
    eprintln!("[smoke] zenoh git_version : {}", zenoh::GIT_VERSION);
    eprintln!("[smoke] mode              : {:?}", args.mode);
    eprintln!("[smoke] peer_mode         : {}", args.peer_mode.as_str());
    eprintln!(
        "[smoke] qos               : {} ({} + {:?})",
        args.qos, qos_label, cc
    );
    eprintln!("[smoke] key prefix        : {}", args.key);
    eprintln!("[smoke] rate_hz           : {}", args.rate_hz);
    eprintln!("[smoke] values_per_tick   : {}", args.values_per_tick);
    eprintln!("[smoke] duration_secs     : {}", args.duration_secs);
    eprintln!("[smoke] payload_bytes     : {}", args.payload_size_bytes);
    if !args.listen.is_empty() {
        eprintln!("[smoke] listen            : {:?}", args.listen);
    }
    if !args.connect.is_empty() {
        eprintln!("[smoke] connect           : {:?}", args.connect);
    }
}

/// Background task: poll `session.info().peers_zid()` every second and
/// emit a line whenever the set of known peers changes. Idiomatic
/// "least invasive" peer discovery: no LivelinessToken subscription is
/// required (per the T16.20 task spec).
async fn peer_watcher(session: zenoh::Session, stop: Arc<AtomicBool>) {
    let mut last: Vec<String> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let peers_iter = session.info().peers_zid().await;
        let mut current: Vec<String> = peers_iter.map(|z| z.to_string()).collect();
        current.sort();
        if current != last {
            if current.is_empty() {
                eprintln!("[smoke] peers: <none>");
            } else {
                eprintln!("[smoke] peers: {}", current.join(", "));
            }
            last = current;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_peers(session: &zenoh::Session, target: usize, timeout_secs: u32) {
    if target == 0 {
        return;
    }
    eprintln!("[smoke] waiting for >= {target} connected peer(s), timeout {timeout_secs}s...");
    let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);
    loop {
        let peers: Vec<_> = session
            .info()
            .peers_zid()
            .await
            .map(|z| z.to_string())
            .collect();
        if peers.len() >= target {
            eprintln!(
                "[smoke] peer wait satisfied ({} peer(s) visible)",
                peers.len()
            );
            return;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "[smoke] peer wait timed out after {timeout_secs}s with {} peer(s); proceeding anyway",
                peers.len()
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn run_pub(
    session: &zenoh::Session,
    args: &Args,
    cc: CongestionControl,
    stop: Arc<AtomicBool>,
) -> Result<u64> {
    // Pre-declare one Publisher per key. Mirrors the bench variant's
    // `publishers_drop` / `publishers_block` cache pattern: declare once,
    // hold the handle, hot-path is `publisher.put(...).await`.
    eprintln!("[smoke] declaring {} publishers...", args.values_per_tick);
    let t0 = Instant::now();
    let mut set: JoinSet<Result<(u32, Publisher<'static>)>> = JoinSet::new();
    for i in 0..args.values_per_tick {
        let session = session.clone();
        let key = format!("{}/{}", args.key, i);
        set.spawn(async move {
            let p = session
                .declare_publisher(key)
                .congestion_control(cc)
                .await
                .map_err(|e| anyhow!("declare_publisher: {e}"))?;
            Ok((i, p))
        });
    }
    let mut slots: Vec<Option<Publisher<'static>>> =
        (0..args.values_per_tick).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        let (i, p) = joined.context("declare task panicked")??;
        slots[i as usize] = Some(p);
    }
    let publishers: Vec<Publisher<'static>> = slots
        .into_iter()
        .map(|s| s.ok_or_else(|| anyhow!("publisher slot unfilled")))
        .collect::<Result<Vec<_>>>()?;
    eprintln!(
        "[smoke] {} publishers declared in {} ms",
        publishers.len(),
        t0.elapsed().as_millis()
    );

    wait_for_peers(session, args.wait_peers, args.wait_peers_timeout_secs).await;

    // Pre-allocated payload (re-used for every publish, cloned per put
    // because Zenoh consumes the buffer by value).
    let payload: Vec<u8> = vec![0xABu8; args.payload_size_bytes];

    let total = Arc::new(AtomicU64::new(0));
    let total_for_reporter = total.clone();
    let stop_reporter = stop.clone();
    let reporter = tokio::spawn(async move {
        let mut last = 0u64;
        let mut last_t = Instant::now();
        while !stop_reporter.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if stop_reporter.load(Ordering::Relaxed) {
                break;
            }
            let cur = total_for_reporter.load(Ordering::Relaxed);
            let elapsed = last_t.elapsed().as_secs_f64().max(1e-9);
            let last5s = cur - last;
            let rate = (last5s as f64) / elapsed;
            eprintln!("pub: total={} last5s={} rate={:.0} hz", cur, last5s, rate);
            last = cur;
            last_t = Instant::now();
        }
    });

    let deadline = Instant::now() + Duration::from_secs(args.duration_secs as u64);
    let tick_interval = if args.rate_hz > 0 {
        Some(Duration::from_nanos(1_000_000_000u64 / args.rate_hz as u64))
    } else {
        None
    };
    let mut next_tick = Instant::now();

    'outer: while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        for p in publishers.iter() {
            if Instant::now() >= deadline || stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            p.put(payload.clone())
                .await
                .map_err(|e| anyhow!("publisher.put: {e}"))?;
            total.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(interval) = tick_interval {
            next_tick += interval;
            let now = Instant::now();
            if next_tick > now {
                tokio::time::sleep(next_tick - now).await;
            } else {
                // Behind schedule; reset cadence anchor so we don't
                // burst-catchup once the wire drains.
                next_tick = now;
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = reporter.await;
    Ok(total.load(Ordering::Relaxed))
}

async fn run_sub(
    session: &zenoh::Session,
    args: &Args,
    stop: Arc<AtomicBool>,
    drain_grace: Arc<Notify>,
) -> Result<(u64, usize)> {
    let wildcard = format!("{}/**", args.key);
    eprintln!("[smoke] declaring subscriber on {}", wildcard);
    let subscriber = session
        .declare_subscriber(&wildcard)
        .await
        .map_err(|e| anyhow!("declare_subscriber: {e}"))?;

    let total = Arc::new(AtomicU64::new(0));
    let unique_keys = Arc::new(std::sync::Mutex::new(
        std::collections::HashSet::<String>::new(),
    ));

    let total_for_reporter = total.clone();
    let keys_for_reporter = unique_keys.clone();
    let stop_reporter = stop.clone();
    let reporter = tokio::spawn(async move {
        let mut last = 0u64;
        while !stop_reporter.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if stop_reporter.load(Ordering::Relaxed) {
                break;
            }
            let cur = total_for_reporter.load(Ordering::Relaxed);
            let last5s = cur - last;
            let nkeys = keys_for_reporter.lock().unwrap().len();
            eprintln!("sub: total={} last5s={} unique_keys={}", cur, last5s, nkeys);
            last = cur;
        }
    });

    loop {
        tokio::select! {
            _ = drain_grace.notified() => {
                // The publisher side signalled "drained"; allow a small
                // tail-window for in-flight samples then stop. 500 ms is
                // generous for localhost; cross-machine WiFi may need
                // longer but the deadline-task above still bounds it.
                tokio::time::sleep(Duration::from_millis(500)).await;
                break;
            }
            res = subscriber.recv_async() => {
                match res {
                    Ok(sample) => {
                        total.fetch_add(1, Ordering::Relaxed);
                        let key = sample.key_expr().as_str().to_string();
                        unique_keys.lock().unwrap().insert(key);
                    }
                    Err(_) => break,
                }
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = reporter.await;
    let total = total.load(Ordering::Relaxed);
    let nkeys = unique_keys.lock().unwrap().len();
    Ok((total, nkeys))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (qos_label, cc) = qos_to_zenoh(args.qos)?;
    print_startup_banner(&args, qos_label, cc);

    let config = build_config(&args)?;
    let session = zenoh::open(config)
        .await
        .map_err(|e| anyhow!("zenoh::open: {e}"))?;
    let zid = session.info().zid().await;
    eprintln!("[smoke] session zid       : {zid}");

    let stop = Arc::new(AtomicBool::new(false));
    let drain_grace = Arc::new(Notify::new());

    let watcher_stop = stop.clone();
    let session_for_watcher = session.clone();
    let watcher = tokio::spawn(peer_watcher(session_for_watcher, watcher_stop));

    // Hard deadline so sub-only mode terminates and the `both` /
    // sub-side has a guaranteed upper bound even if no samples ever
    // arrive cross-machine. `duration_secs + 5` matches the user's
    // expectation that the smoke "self-completes" without ctrl-c.
    let stop_for_deadline = stop.clone();
    let drain_for_deadline = drain_grace.clone();
    let deadline = Duration::from_secs(args.duration_secs as u64 + 5);
    let deadline_task = tokio::spawn(async move {
        tokio::time::sleep(deadline).await;
        eprintln!("[smoke] hard deadline reached; signalling stop");
        stop_for_deadline.store(true, Ordering::Relaxed);
        drain_for_deadline.notify_waiters();
    });

    let result: Result<()> = match args.mode {
        Mode::Pub => {
            let total = run_pub(&session, &args, cc, stop.clone()).await?;
            eprintln!("[smoke] FINAL pub total = {total}");
            Ok(())
        }
        Mode::Sub => {
            let (total, nkeys) =
                run_sub(&session, &args, stop.clone(), drain_grace.clone()).await?;
            eprintln!("[smoke] FINAL sub total = {total} unique_keys = {nkeys}");
            Ok(())
        }
        Mode::Both => {
            let session_pub = session.clone();
            let session_sub = session.clone();
            let args_pub = args.clone();
            let args_sub = args.clone();
            let stop_pub = stop.clone();
            let stop_sub = stop.clone();
            let drain_for_pub = drain_grace.clone();
            let drain_for_sub = drain_grace.clone();
            let pub_h = tokio::spawn(async move {
                let r = run_pub(&session_pub, &args_pub, cc, stop_pub).await;
                // Publishing done -> tell the subscriber to drain + exit.
                drain_for_pub.notify_waiters();
                r
            });
            let sub_h = tokio::spawn(async move {
                run_sub(&session_sub, &args_sub, stop_sub, drain_for_sub).await
            });
            let pub_total = pub_h.await.context("pub task panicked")??;
            let (sub_total, nkeys) = sub_h.await.context("sub task panicked")??;
            eprintln!(
                "[smoke] FINAL pub total = {pub_total} sub total = {sub_total} unique_keys = {nkeys}"
            );
            Ok(())
        }
    };

    stop.store(true, Ordering::Relaxed);
    drain_grace.notify_waiters();
    deadline_task.abort();
    let _ = watcher.await;
    session
        .close()
        .await
        .map_err(|e| anyhow!("session.close: {e}"))?;
    result
}
