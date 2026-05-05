//! NTP-style application-level clock synchronization.
//!
//! See `metak-shared/api-contracts/clock-sync.md` for the protocol contract
//! and `runner/CUSTOM.md` for the architectural rationale.
//!
//! The engine sends `N` `ProbeRequest` messages per peer with a small
//! inter-sample delay, collects matching `ProbeResponse`s within a per-sample
//! timeout, and selects the sample with the smallest RTT. The selected
//! sample's `(t1, t2, t3, t4)` are turned into an `OffsetMeasurement` with
//! `offset_ms = ((t2 - t1) + (t3 - t4)) / 2` and
//! `rtt_ms = (t4 - t1) - (t3 - t2)`.
//!
//! While probing a peer, the engine still answers any inbound
//! `ProbeRequest` addressed to it — peers run the algorithm symmetrically
//! and would otherwise time out waiting for our response.

use crate::message::Message;
use anyhow::Result;
use chrono::{DateTime, Utc};
use socket2::Socket;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Process-wide verbose-tracing toggle. Turned on by the `--verbose-clock-sync`
/// CLI flag in `main.rs`. When `true`, the engine and the coordinator emit
/// detailed per-datagram traces to stderr while measuring offsets so an
/// operator can diagnose silent-failure modes (e.g. probe responses being
/// dropped by the wrong-`to` filter, or `is_single_runner()` taking the wrong
/// branch on a peer machine).
///
/// Reads are `Relaxed` because tracing is best-effort observability — we never
/// gate behavior on this flag.
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Enable verbose clock-sync tracing process-wide. Idempotent.
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// Whether verbose clock-sync tracing is currently enabled.
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Number of samples per peer. The contract says 32; this is the default the
/// runner uses unless an override is passed to `measure_offsets`.
pub const DEFAULT_SAMPLES: usize = 32;

/// Inter-sample delay between successive `ProbeRequest`s to the same peer.
const INTER_SAMPLE_DELAY: Duration = Duration::from_millis(5);

/// Per-sample timeout. If no `ProbeResponse` for the matching id arrives
/// within this window, the sample is dropped.
const PER_SAMPLE_TIMEOUT: Duration = Duration::from_millis(100);

/// Result of a single peer's clock-offset measurement.
///
/// `offset_ms` is `peer.clock − self.clock` in milliseconds: ADD this number
/// to a timestamp logged by `peer` to get the equivalent reading on `self`'s
/// clock. (Equivalently, SUBTRACT it from a timestamp logged by `self` to
/// express it in `peer`'s frame.)
///
/// `rtt_ms` corresponds to the chosen sample. `samples`, `min_rtt_ms`, and
/// `max_rtt_ms` are diagnostic fields kept for the JSONL log; analysis only
/// consumes `offset_ms` and `rtt_ms`.
///
/// `raw_samples` retains every collected `(t1,t2,t3,t4)` and its derived
/// `(offset_ms, rtt_ms)` for debug logging. Production code reads only the
/// summary fields above; the diagnostic file produced by `clock_sync_log` is
/// the only consumer of `raw_samples`.
#[derive(Debug, Clone, PartialEq)]
pub struct OffsetMeasurement {
    pub offset_ms: f64,
    pub rtt_ms: f64,
    pub samples: usize,
    pub min_rtt_ms: f64,
    pub max_rtt_ms: f64,
    /// Per-sample diagnostic trace. Populated by `pick_best` from the raw
    /// `Sample` collection. Order matches the order samples were collected.
    pub raw_samples: Vec<RawSample>,
    /// Whether `pick_best` rejected its initial min-RTT choice in favor of the
    /// median-of-three-lowest-RTT fallback because the chosen offset deviated
    /// from the cohort by more than `OUTLIER_STDDEV_THRESHOLD` standard
    /// deviations. Surface only for diagnostics; analysis ignores it.
    pub outlier_rejected: bool,
}

/// Outcome of one probe-attempt as recorded in the per-sample debug log.
///
/// Every `ProbeRequest` the engine sends produces exactly one `ProbeAttempt`
/// row regardless of whether a response arrived. The `result` field records
/// what happened so that an empty cohort still leaves a diagnostic trail —
/// previously, only successful samples were logged, which gave zero signal
/// in the cross-machine failure mode observed in T8.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResult {
    /// A matching `ProbeResponse` arrived in time and the sample's
    /// `(t1, t2, t3, t4)` are all populated.
    Ok,
    /// No matching `ProbeResponse` arrived within `PER_SAMPLE_TIMEOUT`.
    /// `t1_ns`/`t4_ns` may still be present; `t2_ns`/`t3_ns` are 0.
    Timeout,
    /// One or more datagrams arrived but were filtered out because their
    /// `(from, to, id)` did not match the in-flight probe. Recorded only
    /// when the probe also ultimately timed out (otherwise we would have
    /// fallen through to `Ok`).
    RejectedFilter,
    /// A `ProbeResponse` matched on `(from, to, id)` but its echoed `t1`
    /// string did not match the request's `t1` (defense-in-depth check).
    RejectedT1,
    /// One or more datagrams arrived during the wait window but failed to
    /// parse as a `Message`. Recorded only if no successful response was
    /// also received.
    ParseError,
}

impl ProbeResult {
    /// Stable string representation written to the debug JSONL `result` field.
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeResult::Ok => "ok",
            ProbeResult::Timeout => "timeout",
            ProbeResult::RejectedFilter => "rejected_filter",
            ProbeResult::RejectedT1 => "rejected_t1",
            ProbeResult::ParseError => "parse_error",
        }
    }
}

/// One probe exchange's raw timestamps + derived offset/rtt. Public so
/// `clock_sync_log` can serialize it to the diagnostic JSONL.
///
/// `t*_ns` are nanoseconds since the Unix epoch. `t1`/`t4` are on `self`'s
/// clock, `t2`/`t3` are on `peer`'s clock. `offset_ms`/`rtt_ms` are derived
/// per the NTP formulas. `accepted` is `true` if this is the sample whose
/// numbers landed in the parent `OffsetMeasurement`'s `offset_ms`/`rtt_ms`
/// fields. `result` records why the sample landed where it did — see
/// `ProbeResult` — so an empty/skipped cohort still leaves a diagnostic
/// trail in the debug JSONL.
///
/// For `result != Ok`, `t2_ns`/`t3_ns` are 0 and `offset_ms`/`rtt_ms` are
/// `f64::NAN` to signal "no measurement". Consumers reading the debug
/// JSONL must check `result` before trusting the numeric fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawSample {
    pub t1_ns: i64,
    pub t2_ns: i64,
    pub t3_ns: i64,
    pub t4_ns: i64,
    pub offset_ms: f64,
    pub rtt_ms: f64,
    pub accepted: bool,
    pub result: ProbeResult,
}

/// Per-peer outcome of `ClockSyncEngine::measure_one` /
/// `measure_offsets`.
///
/// `measurement` is the canonical `OffsetMeasurement` analysis consumes.
/// `attempts` is one `RawSample` per probe SENT — including timeouts and
/// filter rejections — so `clock_sync_log` can always write at least one
/// debug row per attempt, even when zero responses arrived. This is the
/// hardening required by T8.5: previously, an empty cohort produced a
/// 0-byte debug file, which gave operators no signal to diagnose.
#[derive(Debug, Clone)]
pub struct PeerMeasurement {
    pub measurement: Option<OffsetMeasurement>,
    pub attempts: Vec<RawSample>,
}

/// Build a `RawSample` placeholder for a probe that did not produce a
/// usable `(t1, t2, t3, t4)` quad. `t1_ns` is preserved (we know when the
/// request was sent); the other timestamps are zeroed and the derived
/// numbers are NaN. Consumers must check `result` before reading the
/// numeric fields.
fn timeout_row(t1_ns: i64, result: ProbeResult) -> RawSample {
    RawSample {
        t1_ns,
        t2_ns: 0,
        t3_ns: 0,
        t4_ns: 0,
        offset_ms: f64::NAN,
        rtt_ms: f64::NAN,
        accepted: false,
        result,
    }
}

/// After `pick_best` selects a sample, flip the matching attempt row's
/// `accepted` flag so the per-sample debug JSONL agrees with the canonical
/// summary.
fn mark_accepted(attempts: &mut [RawSample], chosen: &OffsetMeasurement) {
    if chosen.outlier_rejected {
        // The outlier path synthesises an offset from the median of three
        // samples; no single attempt is "the" accepted one.
        return;
    }
    for a in attempts.iter_mut() {
        if a.result == ProbeResult::Ok
            && (a.rtt_ms - chosen.rtt_ms).abs() < 1e-9
            && (a.offset_ms - chosen.offset_ms).abs() < 1e-9
        {
            a.accepted = true;
            return;
        }
    }
}

/// Threshold (in standard deviations) above which `pick_best` treats the
/// min-RTT sample's offset as an outlier and falls back to the median of the
/// three lowest-RTT samples' offsets.
///
/// Rationale: on a quiet LAN the cohort of 32 samples' offsets clusters
/// tightly (sub-millisecond standard deviation). A sample whose offset sits
/// hundreds of milliseconds away from that cluster — even with a small RTT —
/// is far more likely a clock-quantization or transient time-jump artefact
/// than a true measurement. Five sigma on a Gaussian cohort has p ≈ 6e-7,
/// so this is conservative enough to never fire on legitimate jitter.
///
/// See `metak-shared/api-contracts/clock-sync.md` for context.
pub const OUTLIER_STDDEV_THRESHOLD: f64 = 5.0;

/// A single (t1, t2, t3, t4) sample collected from one probe exchange.
///
/// All fields are nanoseconds since the Unix epoch. `t1` and `t4` are on the
/// initiator's clock; `t2` and `t3` are on the peer's clock.
#[derive(Debug, Clone, Copy)]
struct Sample {
    t1_ns: i64,
    t2_ns: i64,
    t3_ns: i64,
    t4_ns: i64,
}

impl Sample {
    /// `rtt = (t4 − t1) − (t3 − t2)` in nanoseconds.
    fn rtt_ns(&self) -> i64 {
        (self.t4_ns - self.t1_ns) - (self.t3_ns - self.t2_ns)
    }

    /// `offset = ((t2 − t1) + (t3 − t4)) / 2` in nanoseconds. peer − self.
    fn offset_ns(&self) -> i64 {
        ((self.t2_ns - self.t1_ns) + (self.t3_ns - self.t4_ns)) / 2
    }
}

/// Mean and population standard deviation of a slice of f64 values.
fn mean_stddev(xs: &[f64]) -> (f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    (mean, var.sqrt())
}

/// Median of a slice of f64 values. Sorts a copy so the caller's order is
/// preserved.
fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Compute an `OffsetMeasurement` from a non-empty list of samples.
///
/// Strategy:
/// 1. The default pick is the sample with the smallest RTT (standard NTP
///    heuristic — least asymmetric queueing delay → least biased offset).
/// 2. If that sample's offset is more than `OUTLIER_STDDEV_THRESHOLD` standard
///    deviations away from the median offset of the whole cohort, treat it as
///    an outlier (clock quantization, transient time jump, network anomaly)
///    and fall back to the median offset of the three lowest-RTT samples.
///    The reported `rtt_ms` in that case is the median of those three samples'
///    RTTs, so analysis still sees a representative quality metric.
///
/// `raw_samples` is always populated with every input sample, regardless of
/// which path was taken.
fn pick_best(samples: &[Sample]) -> Option<OffsetMeasurement> {
    if samples.is_empty() {
        return None;
    }

    let mut min_rtt = i64::MAX;
    let mut max_rtt = i64::MIN;
    let mut offsets_ms: Vec<f64> = Vec::with_capacity(samples.len());
    let mut by_rtt: Vec<(i64, i64)> = Vec::with_capacity(samples.len()); // (rtt_ns, offset_ns)
    for s in samples {
        let r = s.rtt_ns();
        if r < min_rtt {
            min_rtt = r;
        }
        if r > max_rtt {
            max_rtt = r;
        }
        offsets_ms.push(s.offset_ns() as f64 / 1_000_000.0);
        by_rtt.push((r, s.offset_ns()));
    }
    by_rtt.sort_by_key(|&(r, _)| r);
    let (best_rtt_ns, best_off_ns) = by_rtt[0];
    let best_off_ms = best_off_ns as f64 / 1_000_000.0;
    let best_rtt_ms = best_rtt_ns as f64 / 1_000_000.0;

    // Outlier check on the min-RTT sample's offset.
    let (_, sd) = mean_stddev(&offsets_ms);
    let med = median(&offsets_ms);
    let outlier =
        samples.len() >= 3 && sd > 0.0 && (best_off_ms - med).abs() > OUTLIER_STDDEV_THRESHOLD * sd;

    let (chosen_off_ms, chosen_rtt_ms) = if outlier {
        // Fallback: median of the three lowest-RTT samples' offsets/rtts.
        let take = by_rtt.len().min(3);
        let off_subset: Vec<f64> = by_rtt
            .iter()
            .take(take)
            .map(|&(_, o)| o as f64 / 1_000_000.0)
            .collect();
        let rtt_subset: Vec<f64> = by_rtt
            .iter()
            .take(take)
            .map(|&(r, _)| r as f64 / 1_000_000.0)
            .collect();
        (median(&off_subset), median(&rtt_subset))
    } else {
        (best_off_ms, best_rtt_ms)
    };

    let raw_samples: Vec<RawSample> = samples
        .iter()
        .map(|s| {
            let s_off_ns = s.offset_ns();
            let s_rtt_ns = s.rtt_ns();
            // A sample is "accepted" if its numbers match the min-RTT pick
            // AND we did NOT fall back to the median heuristic. When the
            // outlier path fires, no single sample is "accepted" — the
            // reported offset is a synthesised median of three samples.
            let accepted = !outlier && s_rtt_ns == best_rtt_ns && s_off_ns == best_off_ns;
            RawSample {
                t1_ns: s.t1_ns,
                t2_ns: s.t2_ns,
                t3_ns: s.t3_ns,
                t4_ns: s.t4_ns,
                offset_ms: s_off_ns as f64 / 1_000_000.0,
                rtt_ms: s_rtt_ns as f64 / 1_000_000.0,
                accepted,
                // Every `Sample` reaching `pick_best` came back from
                // `wait_for_response` with all four timestamps populated.
                result: ProbeResult::Ok,
            }
        })
        .collect();

    Some(OffsetMeasurement {
        offset_ms: chosen_off_ms,
        rtt_ms: chosen_rtt_ms,
        samples: samples.len(),
        min_rtt_ms: min_rtt as f64 / 1_000_000.0,
        max_rtt_ms: max_rtt as f64 / 1_000_000.0,
        raw_samples,
        outlier_rejected: outlier,
    })
}

/// Format a wall-clock timestamp as the canonical RFC 3339 nanosecond string
/// used everywhere in the JSONL/coordination protocol.
pub fn format_ts(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
}

/// Parse an RFC 3339 nanosecond string back into nanoseconds since the Unix
/// epoch. Returns `None` on parse failure or if the timestamp is outside the
/// range expressible in `i64` nanoseconds (year ~1677 to ~2262).
fn parse_ns(s: &str) -> Option<i64> {
    let dt = DateTime::parse_from_rfc3339(s).ok()?;
    dt.with_timezone(&Utc).timestamp_nanos_opt()
}

/// Send a `ProbeResponse` answering the given `ProbeRequest`. This is invoked
/// both from the engine's send-loop (when a peer probes us mid-measurement)
/// and from the coordinator's barrier loops (always-respond rule). The
/// `(t2, t3)` pair is captured here; `t1` is echoed from the request.
pub fn respond_to_probe(
    socket: &Socket,
    peer_addrs: &[SocketAddr],
    self_name: &str,
    from: &str,
    id: u64,
    t1: &str,
) -> Result<()> {
    let t2 = format_ts(Utc::now());
    // Build the response and immediately stamp t3. t2 vs t3 captures the
    // peer-side processing time, which the rtt formula subtracts out.
    let t3 = format_ts(Utc::now());
    let response = Message::ProbeResponse {
        from: self_name.to_string(),
        to: from.to_string(),
        id,
        t1: t1.to_string(),
        t2,
        t3,
    };
    let data = response.to_bytes();
    for addr in peer_addrs {
        let _ = socket.send_to(&data, &(*addr).into());
    }
    Ok(())
}

/// Measures pairwise clock offsets against listed peers using the existing
/// coordination UDP socket. See module docs for the algorithm.
pub struct ClockSyncEngine {
    /// This runner's name (the `from` field on outbound probes, and the only
    /// `to` value we accept on inbound probe responses).
    self_name: String,
    /// Shared coordination socket. The engine reads/writes the same port the
    /// `Coordinator` uses; the two are invoked sequentially from `main`.
    socket: Arc<Socket>,
    /// Same address fan-out the Coordinator uses (multicast + per-peer
    /// localhost). We broadcast probes to all of them and let the receiver
    /// filter by `to`.
    peer_addrs: Vec<SocketAddr>,
    /// Monotonic id counter for outbound probes. 64-bit so overflow is
    /// effectively impossible during a benchmark run.
    next_id: AtomicU64,
}

impl ClockSyncEngine {
    /// Construct a new engine. Most callers should use
    /// `Coordinator::clock_sync_engine()` instead, which wires the socket and
    /// peer-address fan-out from the existing coordinator.
    pub fn new(self_name: String, socket: Arc<Socket>, peer_addrs: Vec<SocketAddr>) -> Self {
        ClockSyncEngine {
            self_name,
            socket,
            peer_addrs,
            next_id: AtomicU64::new(1),
        }
    }

    /// Measure offsets against every peer in `peers`. The local runner must
    /// be excluded by the caller.
    ///
    /// Returns one entry per peer in `peers`, regardless of whether any
    /// sample succeeded. The value is a `PeerMeasurement` carrying:
    /// - `measurement`: `Some(OffsetMeasurement)` if at least one sample
    ///   round-tripped, else `None` (peer unreachable or 100% probe loss).
    /// - `attempts`: one `RawSample` per probe sent — including timeouts /
    ///   filter rejections — so the per-sample debug JSONL always has rows
    ///   to inspect after a silent failure.
    pub fn measure_offsets(
        &self,
        peers: &[String],
        n_samples: usize,
    ) -> HashMap<String, PeerMeasurement> {
        let mut out = HashMap::new();
        for peer in peers {
            if peer == &self.self_name {
                continue;
            }
            out.insert(peer.clone(), self.measure_one(peer, n_samples));
        }
        out
    }

    /// Measure a single peer. Sends `n_samples` probes one at a time and
    /// waits for each response before moving on.
    ///
    /// The returned `PeerMeasurement` always has `attempts.len() == n_samples`
    /// (modulo timestamp-overflow skips, which are extremely unlikely). The
    /// `measurement` field is `None` if zero valid samples were collected.
    pub fn measure_one(&self, peer: &str, n_samples: usize) -> PeerMeasurement {
        let mut samples: Vec<Sample> = Vec::with_capacity(n_samples);
        let mut attempts: Vec<RawSample> = Vec::with_capacity(n_samples);
        for i in 0..n_samples {
            if i > 0 {
                std::thread::sleep(INTER_SAMPLE_DELAY);
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let t1 = Utc::now();
            let t1_str = format_ts(t1);
            let t1_ns = match t1.timestamp_nanos_opt() {
                Some(v) => v,
                None => continue,
            };
            if self.send_probe_request(peer, id, &t1_str).is_err() {
                attempts.push(timeout_row(t1_ns, ProbeResult::Timeout));
                continue;
            }
            let (sample, result) = self.wait_for_response(peer, id, &t1_str, t1_ns);
            match (sample, result) {
                (Some(s), ProbeResult::Ok) => {
                    let off_ns = s.offset_ns();
                    let rtt_ns = s.rtt_ns();
                    attempts.push(RawSample {
                        t1_ns: s.t1_ns,
                        t2_ns: s.t2_ns,
                        t3_ns: s.t3_ns,
                        t4_ns: s.t4_ns,
                        offset_ms: off_ns as f64 / 1_000_000.0,
                        rtt_ms: rtt_ns as f64 / 1_000_000.0,
                        // `accepted` is decided after pick_best runs across
                        // the cohort, so we leave it false here and let the
                        // caller refresh the flag for the chosen sample.
                        accepted: false,
                        result: ProbeResult::Ok,
                    });
                    samples.push(s);
                }
                (_, other) => {
                    // No usable sample — record why so the debug log has a
                    // row regardless.
                    attempts.push(timeout_row(t1_ns, other));
                }
            }
        }
        let measurement = pick_best(&samples);
        // If pick_best chose a sample, mark the matching attempt as accepted
        // so the debug JSONL agrees with the canonical measurement file.
        if let Some(m) = measurement.as_ref() {
            mark_accepted(&mut attempts, m);
        }
        PeerMeasurement {
            measurement,
            attempts,
        }
    }

    /// Broadcast a `ProbeRequest` to all peer addresses. Peers filter by the
    /// `to` field, so receivers other than `peer` will discard it.
    fn send_probe_request(&self, peer: &str, id: u64, t1: &str) -> Result<()> {
        let req = Message::ProbeRequest {
            from: self.self_name.clone(),
            to: peer.to_string(),
            id,
            t1: t1.to_string(),
        };
        let data = req.to_bytes();
        for addr in &self.peer_addrs {
            let _ = self.socket.send_to(&data, &(*addr).into());
        }
        Ok(())
    }

    /// Wait up to `PER_SAMPLE_TIMEOUT` for a `ProbeResponse` matching this
    /// `(peer, id)`. While waiting, also answer any inbound `ProbeRequest`
    /// addressed to us (the always-respond rule from the contract).
    ///
    /// Returns `(Some(sample), Ok)` on success, or `(None, reason)` on
    /// timeout where `reason` summarises the most informative failure mode
    /// observed during the wait window. `RejectedFilter` is preferred over
    /// `ParseError` is preferred over `Timeout` so that an operator looking
    /// at a debug-row `result="rejected_filter"` immediately knows datagrams
    /// arrived but were addressed to someone else (a strong hint that
    /// per-runner port routing is misconfigured).
    fn wait_for_response(
        &self,
        peer: &str,
        id: u64,
        t1_str: &str,
        t1_ns: i64,
    ) -> (Option<Sample>, ProbeResult) {
        let deadline = Instant::now() + PER_SAMPLE_TIMEOUT;
        let mut worst_reason = ProbeResult::Timeout;
        loop {
            if Instant::now() >= deadline {
                return (None, worst_reason);
            }
            let mut buf = [std::mem::MaybeUninit::uninit(); 4096];
            match self.socket.recv_from(&mut buf) {
                Ok((n, src)) => {
                    let data: Vec<u8> = buf[..n]
                        .iter()
                        .map(|b| unsafe { b.assume_init() })
                        .collect();
                    let msg = match Message::from_bytes(&data) {
                        Some(m) => m,
                        None => {
                            if is_verbose() {
                                eprintln!(
                                    "[clock-sync verbose] {}: received {n}-byte datagram from {:?} that did not parse as Message",
                                    self.self_name,
                                    src.as_socket()
                                );
                            }
                            // Promote only if no stronger reason already.
                            if worst_reason == ProbeResult::Timeout {
                                worst_reason = ProbeResult::ParseError;
                            }
                            continue;
                        }
                    };
                    match msg {
                        Message::ProbeResponse {
                            from,
                            to,
                            id: rid,
                            t1: rt1,
                            t2,
                            t3,
                        } => {
                            if to != self.self_name || from != peer || rid != id {
                                if is_verbose() {
                                    eprintln!(
                                        "[clock-sync verbose] {}: rejected ProbeResponse from={from} to={to} id={rid} (expected from={peer} to={} id={id})",
                                        self.self_name, self.self_name
                                    );
                                }
                                worst_reason = ProbeResult::RejectedFilter;
                                continue;
                            }
                            // Defense-in-depth: even though (from, to, id)
                            // uniquely identifies this exchange, also require
                            // the echoed t1 string to match what we sent.
                            if rt1 != t1_str {
                                if is_verbose() {
                                    eprintln!(
                                        "[clock-sync verbose] {}: rejected ProbeResponse t1 mismatch (got {rt1}, sent {t1_str})",
                                        self.self_name
                                    );
                                }
                                worst_reason = ProbeResult::RejectedT1;
                                continue;
                            }
                            // Use the local t4 we record now, not anything
                            // from the wire.
                            let t4_ns = match Utc::now().timestamp_nanos_opt() {
                                Some(v) => v,
                                None => return (None, worst_reason),
                            };
                            let t2_ns = match parse_ns(&t2) {
                                Some(v) => v,
                                None => {
                                    if worst_reason == ProbeResult::Timeout {
                                        worst_reason = ProbeResult::ParseError;
                                    }
                                    continue;
                                }
                            };
                            let t3_ns = match parse_ns(&t3) {
                                Some(v) => v,
                                None => {
                                    if worst_reason == ProbeResult::Timeout {
                                        worst_reason = ProbeResult::ParseError;
                                    }
                                    continue;
                                }
                            };
                            if is_verbose() {
                                eprintln!(
                                    "[clock-sync verbose] {}: matched ProbeResponse from={from} id={id}",
                                    self.self_name
                                );
                            }
                            return (
                                Some(Sample {
                                    t1_ns,
                                    t2_ns,
                                    t3_ns,
                                    t4_ns,
                                }),
                                ProbeResult::Ok,
                            );
                        }
                        Message::ProbeRequest {
                            from,
                            to,
                            id: rid,
                            t1: rt1,
                        } => {
                            if to == self.self_name {
                                if is_verbose() {
                                    eprintln!(
                                        "[clock-sync verbose] {}: answering inbound ProbeRequest from={from} id={rid}",
                                        self.self_name
                                    );
                                }
                                let _ = respond_to_probe(
                                    &self.socket,
                                    &self.peer_addrs,
                                    &self.self_name,
                                    &from,
                                    rid,
                                    &rt1,
                                );
                            }
                            // Keep waiting for our own response.
                        }
                        // Discover/Ready/Done messages from a fast peer can
                        // arrive here. They will be re-broadcast by the peer
                        // (every BROADCAST_INTERVAL) and re-handled by the
                        // appropriate barrier loop later.
                        _ => {}
                    }
                }
                Err(_) => {
                    // Read timed out (no datagram in this window). Loop and
                    // re-check the per-sample deadline.
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol as SocketProtocol, Type};
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::atomic::AtomicU16;

    /// Allocate unique ports for each test so concurrent tests don't collide.
    fn next_port() -> u16 {
        static C: AtomicU16 = AtomicU16::new(31000);
        C.fetch_add(2, Ordering::Relaxed)
    }

    fn bind_loopback(port: u16) -> Arc<Socket> {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(SocketProtocol::UDP)).unwrap();
        s.set_reuse_address(true).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        s.bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into())
            .unwrap();
        Arc::new(s)
    }

    #[test]
    fn pick_best_picks_smallest_rtt() {
        // We construct three samples with known offsets and RTTs by working
        // backwards from a target peer-clock-skew. For symmetric network
        // delay D and clock skew S (peer ahead of self by S),
        //   t2 = t1 + D + S
        //   t3 = t2 (peer responds instantly on its clock)
        //   t4 = t3 - S + D = t1 + 2D
        // gives rtt = 2D and offset = S.
        //
        // Sample 0: D = 5ms,  S = 50ms  (rtt = 10ms, offset = 50ms)
        // Sample 1: D = 1ms,  S = 1ms   (rtt = 2ms,  offset = 1ms)  <- best
        // Sample 2: D = 25ms, S = 25ms  (rtt = 50ms, offset = 25ms)
        let samples = vec![
            Sample {
                t1_ns: 0,
                t2_ns: 5_000_000 + 50_000_000,
                t3_ns: 5_000_000 + 50_000_000,
                t4_ns: 10_000_000,
            },
            Sample {
                t1_ns: 100_000_000,
                t2_ns: 100_000_000 + 1_000_000 + 1_000_000,
                t3_ns: 100_000_000 + 1_000_000 + 1_000_000,
                t4_ns: 100_000_000 + 2_000_000,
            },
            Sample {
                t1_ns: 200_000_000,
                t2_ns: 200_000_000 + 25_000_000 + 25_000_000,
                t3_ns: 200_000_000 + 25_000_000 + 25_000_000,
                t4_ns: 200_000_000 + 50_000_000,
            },
        ];
        let m = pick_best(&samples).unwrap();
        // Best sample's RTT is 2 ms.
        assert!((m.rtt_ms - 2.0).abs() < 1e-9, "rtt_ms={}", m.rtt_ms);
        // Best sample's offset is 1 ms (peer ahead of self by 1 ms).
        assert!(
            (m.offset_ms - 1.0).abs() < 1e-9,
            "offset_ms={}",
            m.offset_ms
        );
        assert_eq!(m.samples, 3);
        assert!((m.min_rtt_ms - 2.0).abs() < 1e-9);
        assert!((m.max_rtt_ms - 50.0).abs() < 1e-9);
        // Cohort offsets (50, 1, 25) cluster widely enough that the min-RTT
        // pick (1 ms) is within 5 sigma of the median (25 ms): no outlier.
        assert!(!m.outlier_rejected);
        assert_eq!(m.raw_samples.len(), 3);
        // Exactly one sample is marked as accepted (the min-RTT one).
        assert_eq!(m.raw_samples.iter().filter(|s| s.accepted).count(), 1);
    }

    #[test]
    fn pick_best_rejects_offset_outlier_with_low_rtt() {
        // Reproduces the smoke-t94c-20260503_115309 outlier signature.
        // Cohort: 31 samples with offset ~0 and rtt ~0.3 ms; 1 sample with
        // offset = -387 ms but the smallest rtt of the bunch (0.18 ms).
        // The min-RTT sample MUST be rejected and the chosen offset MUST
        // come from the median-of-three-lowest-RTT fallback.
        let mut samples: Vec<Sample> = Vec::with_capacity(32);
        // 31 well-behaved samples around offset 0, rtt ~0.3 ms.
        for i in 0..31 {
            // rtt = 300_000 ns (0.3 ms), offset = 0 ns. Vary the base t1 so
            // they're distinct samples (not strictly required for the math,
            // but mimics reality).
            let t1 = 1_000_000_000_i64 + i as i64 * 5_000_000;
            let t4 = t1 + 300_000;
            let t2 = t1 + 150_000;
            let t3 = t2;
            samples.push(Sample {
                t1_ns: t1,
                t2_ns: t2,
                t3_ns: t3,
                t4_ns: t4,
            });
        }
        // 1 outlier: rtt = 180_000 ns (0.18 ms, smallest), offset ≈ -387 ms.
        // Build it by setting t2 BEFORE t1 (peer "ahead by -387 ms" in the
        // formula's frame). offset = ((t2 - t1) + (t3 - t4)) / 2.
        // To get -387 ms = -387_000_000 ns, choose t2-t1 = -387_000_000,
        // t3-t4 = -387_000_000, but with rtt = (t4-t1) - (t3-t2) = 180_000.
        // Pick t1 = 0, t4 = 180_000, t3 = -387_000_000 + 180_000, t2 = -387_000_000.
        // Then t3 - t2 = 180_000, rtt = 180_000 - 180_000 = 0. That's
        // degenerate. Instead split D: t4 - t1 = 180_000, t3 - t2 = 0, so
        // both legs of network delay are 90_000 ns. Then t2 = t1 + 90_000 +
        // S where S is the "skew" component. We want offset = ((t2 - t1) +
        // (t3 - t4)) / 2 = -387_000_000. With symmetric D and t3 = t2:
        //   t2 - t1 = D + S; t3 - t4 = (t2) - (t1 + 2D) = S - D.
        //   offset = (D + S + S - D)/2 = S.
        // So set S = -387_000_000.
        let t1 = 1_000_000_000_000_i64;
        let d = 90_000_i64;
        let s_skew = -387_000_000_i64;
        let t4 = t1 + 2 * d;
        let t2 = t1 + d + s_skew;
        let t3 = t2;
        samples.push(Sample {
            t1_ns: t1,
            t2_ns: t2,
            t3_ns: t3,
            t4_ns: t4,
        });

        let m = pick_best(&samples).unwrap();
        // Outlier was detected and rejected.
        assert!(
            m.outlier_rejected,
            "expected outlier rejection, got offset_ms={}",
            m.offset_ms
        );
        // The reported offset is now the median of the three-lowest-RTT
        // samples' offsets. Two of the three lowest-RTT samples have offset
        // 0 (the well-behaved cohort, since outlier had the very lowest
        // RTT, the next two come from the well-behaved set), so median = 0.
        assert!(
            m.offset_ms.abs() < 1.0,
            "expected fallback offset near 0, got {}",
            m.offset_ms
        );
        // min_rtt_ms still reflects the absolute minimum across the cohort.
        assert!((m.min_rtt_ms - 0.18).abs() < 1e-6);
        // raw_samples is the full cohort.
        assert_eq!(m.raw_samples.len(), 32);
    }

    #[test]
    fn pick_best_does_not_reject_when_cohort_is_uniformly_offset() {
        // If every sample reports the same large offset, that's a real clock
        // skew, not an outlier. pick_best must NOT reject in this case.
        let mut samples: Vec<Sample> = Vec::with_capacity(8);
        for i in 0..8 {
            let t1 = 1_000_000_000_i64 + i as i64 * 5_000_000;
            // Peer ahead by 100 ms uniformly, rtt 0.3 ms.
            let d = 150_000_i64;
            let s = 100_000_000_i64;
            let t4 = t1 + 2 * d;
            let t2 = t1 + d + s;
            let t3 = t2;
            samples.push(Sample {
                t1_ns: t1,
                t2_ns: t2,
                t3_ns: t3,
                t4_ns: t4,
            });
        }
        let m = pick_best(&samples).unwrap();
        assert!(
            !m.outlier_rejected,
            "uniform-offset cohort must not be flagged"
        );
        assert!((m.offset_ms - 100.0).abs() < 1.0);
    }

    #[test]
    fn pick_best_small_cohort_skips_outlier_check() {
        // With only two samples we cannot compute a meaningful stddev; the
        // outlier check is skipped (the implementation requires len >= 3).
        let samples = vec![
            Sample {
                t1_ns: 0,
                t2_ns: 100,
                t3_ns: 100,
                t4_ns: 200,
            },
            Sample {
                t1_ns: 1_000_000_000,
                // Wildly different offset, low rtt.
                t2_ns: 1_000_000_000 + 50 - 500_000_000,
                t3_ns: 1_000_000_000 + 50 - 500_000_000,
                t4_ns: 1_000_000_000 + 100,
            },
        ];
        let m = pick_best(&samples).unwrap();
        assert!(
            !m.outlier_rejected,
            "outlier check requires >= 3 samples; got {} samples",
            samples.len()
        );
    }

    #[test]
    fn offset_math_zero_when_clocks_aligned() {
        // Identical clocks, 4ms RTT, perfectly symmetric.
        let s = Sample {
            t1_ns: 0,
            t2_ns: 2_000_000,
            t3_ns: 2_000_000,
            t4_ns: 4_000_000,
        };
        // offset = ((t2 - t1) + (t3 - t4)) / 2 = (2_000_000 + (-2_000_000)) / 2 = 0
        assert_eq!(s.offset_ns(), 0);
        // rtt = (t4 - t1) - (t3 - t2) = 4_000_000 - 0 = 4_000_000
        assert_eq!(s.rtt_ns(), 4_000_000);
    }

    #[test]
    fn offset_math_peer_ahead_by_known_amount() {
        // Peer's clock is +50ms ahead of self. Network is symmetric, RTT
        // 10ms. So self sends at t1, peer receives at t1 + 5ms (network) +
        // 50ms (clock skew), replies at the same instant on its clock,
        // self receives at t1 + 10ms.
        let t1 = 1_000_000_000_i64;
        let t4 = t1 + 10_000_000;
        let t2 = t1 + 5_000_000 + 50_000_000;
        let t3 = t2;
        let s = Sample {
            t1_ns: t1,
            t2_ns: t2,
            t3_ns: t3,
            t4_ns: t4,
        };
        assert_eq!(s.offset_ns(), 50_000_000); // 50ms
        assert_eq!(s.rtt_ns(), 10_000_000); // 10ms
    }

    #[test]
    fn parse_ns_roundtrip() {
        let now = Utc::now();
        let s = format_ts(now);
        let ns = parse_ns(&s).unwrap();
        let expected = now.timestamp_nanos_opt().unwrap();
        assert_eq!(ns, expected);
    }

    #[test]
    fn parse_ns_invalid_returns_none() {
        assert!(parse_ns("not a timestamp").is_none());
    }

    #[test]
    fn pick_best_empty_returns_none() {
        let empty: Vec<Sample> = vec![];
        assert!(pick_best(&empty).is_none());
    }

    /// Two engines on the same machine talk to each other over loopback.
    /// True clock offset is 0 since they share `Utc::now()`. Verifies
    /// `|offset_ms| < 1.0` and `rtt_ms > 0`.
    #[test]
    fn two_engines_localhost_offset_near_zero() {
        let port_a = next_port();
        let port_b = port_a + 1;

        // Each engine needs its own bound socket. Both fan-out points at the
        // other's port.
        let sock_a = bind_loopback(port_a);
        let sock_b = bind_loopback(port_b);

        let addrs_a_to_b = vec![SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            port_b,
        ))];
        let addrs_b_to_a = vec![SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            port_a,
        ))];

        let engine_a = ClockSyncEngine::new("a".into(), Arc::clone(&sock_a), addrs_a_to_b);
        let engine_b = ClockSyncEngine::new("b".into(), Arc::clone(&sock_b), addrs_b_to_a);

        // Both threads probe each other simultaneously so both sides exercise
        // the always-respond path while waiting for their own responses.
        let t_a = std::thread::spawn(move || engine_a.measure_one("b", 8));
        let t_b = std::thread::spawn(move || engine_b.measure_one("a", 8));

        let pm_a = t_a.join().unwrap();
        let pm_b = t_b.join().unwrap();
        let m_a = pm_a.measurement.expect("engine a got at least one sample");
        let m_b = pm_b.measurement.expect("engine b got at least one sample");

        assert!(
            m_a.offset_ms.abs() < 1.0,
            "engine a |offset_ms| should be < 1.0, got {}",
            m_a.offset_ms
        );
        assert!(
            m_b.offset_ms.abs() < 1.0,
            "engine b |offset_ms| should be < 1.0, got {}",
            m_b.offset_ms
        );
        assert!(m_a.rtt_ms > 0.0, "engine a rtt_ms should be > 0.0");
        assert!(m_b.rtt_ms > 0.0, "engine b rtt_ms should be > 0.0");
        assert!(m_a.samples > 0);
        assert!(m_b.samples > 0);
        // Every probe sent must produce exactly one attempt row, regardless
        // of outcome. With 8 samples each side records 8 attempts.
        assert_eq!(pm_a.attempts.len(), 8, "engine a attempts");
        assert_eq!(pm_b.attempts.len(), 8, "engine b attempts");
    }

    #[test]
    fn measure_one_records_timeout_attempts_when_peer_is_silent() {
        // No peer is bound on `port_b`, so every probe will time out.
        // Even so, `measure_one` must produce one attempt row per sample with
        // `result == Timeout`, so the debug JSONL has rows even on total
        // failure (the silent-failure regression from T8.5).
        let port_a = next_port();
        let port_b = port_a + 1;
        let sock_a = bind_loopback(port_a);

        let addrs_a_to_b = vec![SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            port_b,
        ))];
        let engine_a = ClockSyncEngine::new("a".into(), Arc::clone(&sock_a), addrs_a_to_b);

        // Use a small N; per-sample timeout is 100 ms so 4 samples ≈ 0.4s.
        let pm = engine_a.measure_one("b", 4);
        assert!(pm.measurement.is_none(), "no peer => no measurement");
        assert_eq!(pm.attempts.len(), 4, "one attempt per probe");
        for a in &pm.attempts {
            assert_eq!(
                a.result,
                ProbeResult::Timeout,
                "no datagram should ever arrive; got {:?}",
                a.result
            );
            assert!(!a.accepted, "timeout rows are never accepted");
            assert!(a.offset_ms.is_nan(), "timeout rows have NaN offset");
            assert!(a.rtt_ms.is_nan(), "timeout rows have NaN rtt");
            assert_eq!(a.t2_ns, 0);
            assert_eq!(a.t3_ns, 0);
            assert!(a.t1_ns > 0, "t1 should still be recorded for timeouts");
        }
    }

    #[test]
    fn measure_one_marks_chosen_attempt_accepted_on_success() {
        // Sanity: when a peer answers, exactly one of the attempt rows in
        // the returned PeerMeasurement carries `accepted = true`, and that
        // row's offset/rtt match the canonical OffsetMeasurement.
        let port_a = next_port();
        let port_b = port_a + 1;
        let sock_a = bind_loopback(port_a);
        let sock_b = bind_loopback(port_b);

        let addrs_a_to_b = vec![SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            port_b,
        ))];
        let addrs_b_to_a = vec![SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            port_a,
        ))];

        let engine_a = ClockSyncEngine::new("a".into(), Arc::clone(&sock_a), addrs_a_to_b);
        let engine_b = ClockSyncEngine::new("b".into(), Arc::clone(&sock_b), addrs_b_to_a);

        let t_a = std::thread::spawn(move || engine_a.measure_one("b", 4));
        let _t_b = std::thread::spawn(move || engine_b.measure_one("a", 4));

        let pm_a = t_a.join().unwrap();
        let m_a = pm_a.measurement.expect("a got at least one sample");
        if !m_a.outlier_rejected {
            // Exactly one accepted row.
            let accepted: Vec<_> = pm_a.attempts.iter().filter(|a| a.accepted).collect();
            assert_eq!(accepted.len(), 1, "exactly one accepted attempt");
            let row = accepted[0];
            assert!((row.offset_ms - m_a.offset_ms).abs() < 1e-9);
            assert!((row.rtt_ms - m_a.rtt_ms).abs() < 1e-9);
            assert_eq!(row.result, ProbeResult::Ok);
        }
    }

    #[test]
    fn probe_result_as_str_is_stable() {
        // Locked: the JSONL `result` field must stay stable since downstream
        // tooling (post-mortem scripts in LEARNED.md) keys off these values.
        assert_eq!(ProbeResult::Ok.as_str(), "ok");
        assert_eq!(ProbeResult::Timeout.as_str(), "timeout");
        assert_eq!(ProbeResult::RejectedFilter.as_str(), "rejected_filter");
        assert_eq!(ProbeResult::RejectedT1.as_str(), "rejected_t1");
        assert_eq!(ProbeResult::ParseError.as_str(), "parse_error");
    }
}
