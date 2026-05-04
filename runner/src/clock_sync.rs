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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// One probe exchange's raw timestamps + derived offset/rtt. Public so
/// `clock_sync_log` can serialize it to the diagnostic JSONL.
///
/// `t*_ns` are nanoseconds since the Unix epoch. `t1`/`t4` are on `self`'s
/// clock, `t2`/`t3` are on `peer`'s clock. `offset_ms`/`rtt_ms` are derived
/// per the NTP formulas. `accepted` is `true` if this is the sample whose
/// numbers landed in the parent `OffsetMeasurement`'s `offset_ms`/`rtt_ms`
/// fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawSample {
    pub t1_ns: i64,
    pub t2_ns: i64,
    pub t3_ns: i64,
    pub t4_ns: i64,
    pub offset_ms: f64,
    pub rtt_ms: f64,
    pub accepted: bool,
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
    /// be excluded by the caller. Returns one entry per peer that produced
    /// at least one valid sample. Peers that produced zero samples (e.g.
    /// unreachable, or 100% packet loss within the measurement window) are
    /// omitted from the returned map.
    pub fn measure_offsets(
        &self,
        peers: &[String],
        n_samples: usize,
    ) -> HashMap<String, OffsetMeasurement> {
        let mut out = HashMap::new();
        for peer in peers {
            if peer == &self.self_name {
                continue;
            }
            if let Some(m) = self.measure_one(peer, n_samples) {
                out.insert(peer.clone(), m);
            }
        }
        out
    }

    /// Measure a single peer. Sends `n_samples` probes one at a time and
    /// waits for each response before moving on. Returns `None` if zero
    /// valid samples were collected.
    pub fn measure_one(&self, peer: &str, n_samples: usize) -> Option<OffsetMeasurement> {
        let mut samples: Vec<Sample> = Vec::with_capacity(n_samples);
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
                continue;
            }
            if let Some(s) = self.wait_for_response(peer, id, &t1_str, t1_ns) {
                samples.push(s);
            }
        }
        pick_best(&samples)
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
    fn wait_for_response(&self, peer: &str, id: u64, t1_str: &str, t1_ns: i64) -> Option<Sample> {
        let deadline = Instant::now() + PER_SAMPLE_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            let mut buf = [std::mem::MaybeUninit::uninit(); 4096];
            match self.socket.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    let data: Vec<u8> = buf[..n]
                        .iter()
                        .map(|b| unsafe { b.assume_init() })
                        .collect();
                    let msg = match Message::from_bytes(&data) {
                        Some(m) => m,
                        None => continue,
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
                                // Stale or for someone else; ignore.
                                continue;
                            }
                            // Defense-in-depth: even though (from, to, id)
                            // uniquely identifies this exchange, also require
                            // the echoed t1 string to match what we sent. This
                            // shields the offset math from any conceivable
                            // form of stale response that survives the triple
                            // filter (e.g. a 64-bit id wrap, or a future
                            // protocol change). Mismatch -> drop and keep
                            // waiting.
                            if rt1 != t1_str {
                                continue;
                            }
                            // Use the local t4 we record now, not anything
                            // from the wire.
                            let t4_ns = Utc::now().timestamp_nanos_opt()?;
                            let t2_ns = match parse_ns(&t2) {
                                Some(v) => v,
                                None => continue,
                            };
                            let t3_ns = match parse_ns(&t3) {
                                Some(v) => v,
                                None => continue,
                            };
                            return Some(Sample {
                                t1_ns,
                                t2_ns,
                                t3_ns,
                                t4_ns,
                            });
                        }
                        Message::ProbeRequest {
                            from,
                            to,
                            id: rid,
                            t1: rt1,
                        } => {
                            if to == self.self_name {
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

        let m_a = t_a
            .join()
            .unwrap()
            .expect("engine a got at least one sample");
        let m_b = t_b
            .join()
            .unwrap()
            .expect("engine b got at least one sample");

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
    }
}
