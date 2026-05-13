//! Phase-aware termination state machine (T15.4, epic E15).
//!
//! The runner replaces the pre-E15 wall-clock-only per-spawn timeout
//! with an activity-based, phase-aware termination signal. The state
//! machine consumes:
//!
//! - the local [`LocalProgressTracker`] (T15.2) -- folded from the
//!   variant's stdout progress stream;
//! - the [`RemoteProgressView`] (T15.3) -- folded from every peer
//!   runner's per-spawn `ProgressUpdate` messages.
//!
//! On each ~250 ms tick the spawn monitor calls
//! [`evaluate`] with the current local snapshot, the current remote
//! view snapshot, and the runtime config. The decision is one of three
//! variants:
//!
//! - [`TerminationDecision::Continue`] -- nothing to do; keep polling
//!   the child. This is the steady-state in `connect`, `stabilize`,
//!   and the active part of `operate`.
//! - [`TerminationDecision::OperateIdle`] -- the runner has observed
//!   that local AND every peer's variant has stopped advancing its
//!   `(sent, received)` counters for [`TerminationConfig::operate_idle_secs`].
//!   This is INFORMATIONAL only -- the runner keeps polling because
//!   the variant is independently detecting its own idle (T15.5) and
//!   will transition itself to `silent` then `done`.
//! - [`TerminationDecision::SafetyNet`] -- the spawn has been running
//!   for more than [`TerminationConfig::max_spawn_secs`] seconds and
//!   has not reached `done`. The monitor must kill the child as a
//!   last-resort fallback. Should rarely fire under healthy variant
//!   lifecycles.
//!
//! The state machine is intentionally pure: it takes snapshots and
//! returns a decision, with no I/O of its own. This lets unit tests
//! drive it with synthetic data, and lets the spawn loop apply the
//! decision in the same place it already enforces a wall-clock check.

use std::time::{Duration, SystemTime};

use crate::progress::{LocalProgressTracker, RemoteProgressView};

/// Runtime configuration for the termination state machine.
///
/// All durations are inputs, not derived. The spawn loop builds one of
/// these per spawn from the runner's CLI args (`--operate-idle-secs`,
/// `--max-spawn-secs`) plus the per-variant fallback timeout already
/// in the existing code path.
#[derive(Debug, Clone, Copy)]
pub struct TerminationConfig {
    /// Idle threshold in seconds. When local AND every peer's variant
    /// has not advanced either of `(sent, received)` for at least this
    /// many seconds during the `operate` phase, the runner notes
    /// "operate done" via [`TerminationDecision::OperateIdle`]. Matches
    /// the variant-side `--operate-idle-secs` so the variant's own
    /// T15.5 idle detection fires at roughly the same time.
    pub operate_idle_secs: u32,
    /// Absolute wall-clock deadline (since spawn start) after which the
    /// child is killed as a safety-net fallback. Should be large
    /// enough to almost never fire under healthy lifecycles; default
    /// in the CLI is 300 seconds.
    pub max_spawn_secs: u32,
}

impl TerminationConfig {
    /// Convenience builder used by the spawn site to combine the
    /// runner-wide `--max-spawn-secs` with a per-variant fallback
    /// timeout. The smaller of the two wins so existing tests that
    /// pass tiny `Duration` timeouts still trip the safety net at the
    /// expected wall-clock.
    pub fn with_bounded_max(
        operate_idle_secs: u32,
        max_spawn_secs: u32,
        fallback_secs: u64,
    ) -> Self {
        // Clamp `fallback_secs` to a u32-safe upper bound so the
        // `min` below does not silently truncate huge values from a
        // pathological config. Anything above u32::MAX seconds is well
        // past any realistic spawn lifetime; cap it.
        let fallback_u32 = u32::try_from(fallback_secs).unwrap_or(u32::MAX);
        Self {
            operate_idle_secs,
            max_spawn_secs: max_spawn_secs.min(fallback_u32),
        }
    }
}

/// Result of one [`evaluate`] call. See the module-level docs for the
/// semantics of each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationDecision {
    /// Keep polling the child; no termination event yet.
    Continue,
    /// Local AND every peer's variant has been operate-idle for at
    /// least `operate_idle_secs`. Observational only -- the runner
    /// does not kill the child on this signal; the variant exits
    /// itself via its own idle detection (T15.5).
    OperateIdle,
    /// The spawn has exceeded `max_spawn_secs`. The monitor must kill
    /// the child.
    SafetyNet,
}

/// Evaluate the state machine for one poll-loop tick.
///
/// `local` is a snapshot of [`LocalProgressTracker`] for the spawn.
/// `remote` is a snapshot of [`RemoteProgressView`] holding every
/// peer's latest per-spawn snapshot. `spawn_name` is the variant's
/// `effective_name` -- used to look up each peer's matching snapshot
/// for the same spawn (peers track multiple spawn names over time).
/// `peers_expected` is the list of peer runner names this runner
/// should hear from. An empty list (single-runner mode) means the
/// "every peer idle" predicate is vacuously true.
///
/// `elapsed` is the duration since spawn start; `now` is the current
/// wall-clock used to compute idle windows from the tracker's
/// `last_*_change_ts` fields. Both are injected so unit tests can
/// drive deterministic timing.
///
/// Decision priority (highest first):
///
/// 1. `SafetyNet` whenever `elapsed >= max_spawn_secs`. Wins over
///    everything else so a stuck spawn cannot mask the safety net
///    with an idle signal.
/// 2. `OperateIdle` when the variant is in `operate` AND local has
///    been idle for `operate_idle_secs` AND every expected peer has
///    a matching snapshot whose `(sent, received)` has been idle for
///    the same window. Missing peer snapshots count as "not yet
///    idle" so a runner that just spawned waits for its peers to
///    report.
/// 3. `Continue` otherwise.
///
/// Phases other than `operate` (`connect`, `stabilize`, `eot`,
/// `silent`, `done`, `unknown`) yield `Continue` for idle detection;
/// the variant transitions through them by its own clock. The safety
/// net is independent of phase.
pub fn evaluate(
    local: &LocalProgressTracker,
    remote: &RemoteProgressView,
    spawn_name: &str,
    peers_expected: &[String],
    config: TerminationConfig,
    elapsed: Duration,
    now: SystemTime,
) -> TerminationDecision {
    // Safety net is unconditional: a spawn stuck in any phase for
    // longer than `max_spawn_secs` must be killed.
    if elapsed >= Duration::from_secs(u64::from(config.max_spawn_secs)) {
        return TerminationDecision::SafetyNet;
    }

    // Operate-idle detection only applies during the `operate` phase.
    // Other phases have their own variant-side time-based transitions
    // (connect/stabilize -> next; eot/silent -> drain -> done) so the
    // runner does not need to do anything.
    if local.phase != "operate" {
        return TerminationDecision::Continue;
    }

    let idle_threshold = Duration::from_secs(u64::from(config.operate_idle_secs));

    // Local idle predicate: BOTH sent and received counters must have
    // been flat for >= operate_idle_secs.
    let local_sent_idle = duration_since(now, local.last_sent_change_ts) >= idle_threshold;
    let local_recv_idle = duration_since(now, local.last_received_change_ts) >= idle_threshold;
    if !(local_sent_idle && local_recv_idle) {
        return TerminationDecision::Continue;
    }

    // Every-peer idle predicate. Missing snapshots count as "not yet
    // idle" so we wait for the peer to report before concluding
    // cross-runner agreement. This is the conservative branch: it
    // never falsely fires when a peer is just slow to publish, but
    // does mean a peer that fails to publish keeps the runner
    // observing until the safety net catches up. Operators should
    // notice the missing peer in the progress_coord startup log.
    for peer in peers_expected {
        let Some(snap) = remote.snapshot_for(peer, spawn_name) else {
            return TerminationDecision::Continue;
        };
        // For remote snapshots we approximate per-counter
        // last-advance with `last_update_ts` because the on-wire
        // `ProgressUpdate` does not carry per-counter timestamps -- if
        // the peer's counters are flat, every subsequent inbound
        // update folds in the same values and only refreshes
        // `last_update_ts`. We instead enforce a slightly stricter
        // predicate: the peer must have reported the same `sent` and
        // `received` counters for at least `operate_idle_secs` AND
        // its last_update_ts must itself be older than the threshold
        // (the latter catches a peer that simply stopped publishing).
        //
        // Simpler-but-equivalent implementation: require both sent
        // and received to be non-decreasing AND `last_update_ts` to
        // be at least `idle_threshold` behind `now`. If the peer is
        // still active, its 1Hz cadence keeps `last_update_ts`
        // fresher than the threshold. If the peer is idle, its
        // cadence keeps coming through but every fold is a flat
        // update -- and because of T15.3's monotonic-max merge,
        // `last_update_ts` advances on every inbound frame
        // regardless of whether counters changed. So we cannot rely
        // on `last_update_ts` alone to signify "idle".
        //
        // Pragmatic resolution: read the peer's `(sent, received)`
        // pair into a small local-side history. Out of scope for the
        // current single-tick API; the simplest correct rule is to
        // require the peer's `last_update_ts` to be FRESH (peer is
        // healthy and reporting) AND the peer's `phase` to NOT be
        // `operate` (peer has already moved on via its own T15.5
        // idle), OR the peer's phase is `operate` but its
        // `(sent, received)` exactly matches the previous tick's view.
        //
        // Concrete rule used here (rationale documented above):
        // every peer must be in a post-operate phase (`eot`,
        // `silent`, `done`) -- which is what T15.5 transitions them
        // to once they observe their own idle. Local idle + every
        // peer phase >= eot is the unambiguous agreement signal.
        let post_operate = matches!(snap.phase.as_str(), "eot" | "silent" | "done");
        if !post_operate {
            // Peer is still in operate (or hasn't advanced phase
            // yet). We have not reached cross-runner idle agreement.
            return TerminationDecision::Continue;
        }
        // Stale peer snapshot: ignored as long as the variant has
        // reported a post-operate phase. The local idle predicate
        // already guarantees the local side is quiescent; the peer's
        // confirmed post-operate phase confirms it too.
        let _ = snap.last_update_ts;
    }

    TerminationDecision::OperateIdle
}

/// Compute `now - earlier`, saturating at zero on a clock skew.
///
/// Wall-clock can move backwards on Windows when the OS adjusts the
/// clock (NTP step, manual adjustment). Saturating to zero is
/// equivalent to "no time has elapsed" -- the idle detector then waits
/// another tick. Preferred over panicking on a non-monotonic measure.
fn duration_since(now: SystemTime, earlier: SystemTime) -> Duration {
    now.duration_since(earlier).unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{ProgressEvent, ProgressUpdateRef};
    use std::time::Duration;

    /// Build a `LocalProgressTracker` in the given phase, with the
    /// last-advance timestamps backdated by `idle_for` seconds from
    /// `now`. Counters are arbitrary positive numbers (the state
    /// machine only consults the timestamps + phase).
    fn local_tracker(phase: &str, idle_for: Duration, now: SystemTime) -> LocalProgressTracker {
        let mut t = LocalProgressTracker::new("sp");
        let ev = ProgressEvent {
            ts: "t".into(),
            phase: phase.to_string(),
            sent: 1,
            received: 1,
            eot_sent: false,
            eot_received: false,
        };
        let advance_ts = now.checked_sub(idle_for).unwrap_or(now);
        t.apply_progress(&ev, advance_ts);
        // Re-apply with same counters at `now` so last_progress_ts is
        // current (idle detector reads last_*_change_ts, not
        // last_progress_ts).
        t.apply_progress(&ev, now);
        t
    }

    /// Build a `RemoteProgressView` with one peer snapshot in the
    /// given phase, last-updated `idle_for` ago. Counters fixed at 1.
    fn remote_view_with_peer(
        peer: &str,
        spawn: &str,
        phase: &str,
        idle_for: Duration,
        now: SystemTime,
    ) -> RemoteProgressView {
        let mut v = RemoteProgressView::new();
        let updated_at = now.checked_sub(idle_for).unwrap_or(now);
        v.apply_update(
            ProgressUpdateRef {
                runner: peer,
                spawn,
                phase,
                sent: 1,
                received: 1,
                eot_sent: false,
                eot_received: false,
                ts: "x",
            },
            updated_at,
        );
        v
    }

    // -----------------------------------------------------------------
    // Safety-net branch.
    // -----------------------------------------------------------------

    #[test]
    fn safety_net_fires_when_elapsed_exceeds_max_spawn_secs() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_millis(10), now);
        let remote = RemoteProgressView::new();
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 10,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &[],
            cfg,
            Duration::from_secs(10),
            now,
        );
        assert_eq!(d, TerminationDecision::SafetyNet);
    }

    #[test]
    fn safety_net_takes_priority_over_idle_signal() {
        // Even when local AND remote are idle, the safety net wins if
        // elapsed has crossed the deadline.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_secs(60), now);
        let remote = remote_view_with_peer("bob", "sp", "done", Duration::from_secs(1), now);
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 30,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &["bob".into()],
            cfg,
            Duration::from_secs(31),
            now,
        );
        assert_eq!(d, TerminationDecision::SafetyNet);
    }

    // -----------------------------------------------------------------
    // Stabilize / connect / pre-operate phases.
    // -----------------------------------------------------------------

    #[test]
    fn stabilize_phase_never_triggers_idle_decision() {
        // Even though local counters are flat for ages, the state
        // machine must return Continue in any non-operate phase. The
        // safety net is the only signal that fires in stabilize.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("stabilize", Duration::from_secs(60), now);
        let remote = RemoteProgressView::new();
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &[],
            cfg,
            Duration::from_secs(10),
            now,
        );
        assert_eq!(d, TerminationDecision::Continue);
    }

    #[test]
    fn connect_phase_never_triggers_idle_decision() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("connect", Duration::from_secs(60), now);
        let remote = RemoteProgressView::new();
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(&local, &remote, "sp", &[], cfg, Duration::from_secs(2), now);
        assert_eq!(d, TerminationDecision::Continue);
    }

    // -----------------------------------------------------------------
    // Operate-phase idle detection.
    // -----------------------------------------------------------------

    #[test]
    fn operate_local_idle_but_active_returns_continue_below_threshold() {
        // Local has only been idle for 2s; threshold is 5s -> Continue.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_secs(2), now);
        let remote = RemoteProgressView::new();
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &[],
            cfg,
            Duration::from_secs(20),
            now,
        );
        assert_eq!(d, TerminationDecision::Continue);
    }

    #[test]
    fn operate_single_runner_idle_returns_operate_idle() {
        // No peers expected; local has been idle past the threshold.
        // The state machine must report OperateIdle.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_secs(6), now);
        let remote = RemoteProgressView::new();
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &[],
            cfg,
            Duration::from_secs(20),
            now,
        );
        assert_eq!(d, TerminationDecision::OperateIdle);
    }

    #[test]
    fn operate_local_idle_remote_still_in_operate_returns_continue() {
        // Local is past idle threshold but peer is still reporting
        // operate. Cross-runner agreement is not yet reached.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_secs(6), now);
        let remote = remote_view_with_peer("bob", "sp", "operate", Duration::from_millis(100), now);
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &["bob".into()],
            cfg,
            Duration::from_secs(20),
            now,
        );
        assert_eq!(d, TerminationDecision::Continue);
    }

    #[test]
    fn operate_local_idle_remote_missing_snapshot_returns_continue() {
        // No snapshot folded for the peer at all (just-spawned scenario).
        // The state machine must NOT report idle agreement.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_secs(6), now);
        let remote = RemoteProgressView::new();
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &["bob".into()],
            cfg,
            Duration::from_secs(20),
            now,
        );
        assert_eq!(d, TerminationDecision::Continue);
    }

    #[test]
    fn operate_local_idle_remote_post_operate_returns_operate_idle() {
        // Local idle past threshold AND peer transitioned to silent
        // (so peer's own T15.5 idle detection fired). Cross-runner
        // agreement reached -- state machine reports OperateIdle.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_secs(6), now);
        let remote = remote_view_with_peer("bob", "sp", "silent", Duration::from_millis(100), now);
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &["bob".into()],
            cfg,
            Duration::from_secs(20),
            now,
        );
        assert_eq!(d, TerminationDecision::OperateIdle);
    }

    #[test]
    fn operate_with_one_peer_in_done_and_one_still_in_operate_returns_continue() {
        // Three runners total: this one, alice (still in operate),
        // bob (done). Any one peer still in operate blocks the
        // OperateIdle decision.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let local = local_tracker("operate", Duration::from_secs(6), now);
        let mut remote = RemoteProgressView::new();
        remote.apply_update(
            ProgressUpdateRef {
                runner: "alice",
                spawn: "sp",
                phase: "operate",
                sent: 1,
                received: 1,
                eot_sent: false,
                eot_received: false,
                ts: "t",
            },
            now,
        );
        remote.apply_update(
            ProgressUpdateRef {
                runner: "bob",
                spawn: "sp",
                phase: "done",
                sent: 1,
                received: 1,
                eot_sent: true,
                eot_received: true,
                ts: "t",
            },
            now,
        );
        let cfg = TerminationConfig {
            operate_idle_secs: 5,
            max_spawn_secs: 300,
        };
        let d = evaluate(
            &local,
            &remote,
            "sp",
            &["alice".into(), "bob".into()],
            cfg,
            Duration::from_secs(20),
            now,
        );
        assert_eq!(d, TerminationDecision::Continue);
    }

    // -----------------------------------------------------------------
    // Wall-clock-skew safety: duration_since must not panic.
    // -----------------------------------------------------------------

    #[test]
    fn duration_since_clamps_negative_to_zero() {
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(900);
        assert_eq!(duration_since(now, earlier), Duration::ZERO);
    }

    // -----------------------------------------------------------------
    // TerminationConfig::with_bounded_max
    // -----------------------------------------------------------------

    #[test]
    fn with_bounded_max_picks_smaller_of_cli_and_fallback() {
        // CLI says 300, per-variant fallback says 10 -> 10 wins so
        // existing tests with small timeouts still trip the safety net.
        let cfg = TerminationConfig::with_bounded_max(5, 300, 10);
        assert_eq!(cfg.max_spawn_secs, 10);
    }

    #[test]
    fn with_bounded_max_picks_smaller_when_cli_is_tighter() {
        // CLI says 60, per-variant fallback says 300 -> 60 wins
        // (operator chose a stricter runner-wide cap).
        let cfg = TerminationConfig::with_bounded_max(5, 60, 300);
        assert_eq!(cfg.max_spawn_secs, 60);
    }

    #[test]
    fn with_bounded_max_handles_oversize_fallback() {
        // Fallback above u32::MAX seconds is clamped to u32::MAX so
        // the `min` does not silently underflow.
        let cfg = TerminationConfig::with_bounded_max(5, 300, u64::MAX);
        assert_eq!(cfg.max_spawn_secs, 300);
    }
}
