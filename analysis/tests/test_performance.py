"""Tests for the polars-based performance metrics module."""

from __future__ import annotations

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy
from performance import _percentile, performance_for_group


def _perf(events: list[dict], variant: str = "test-variant", run: str = "run01"):
    lazy = events_to_lazy(events)
    deliveries = correlate_lazy(lazy).collect()
    return performance_for_group(lazy, deliveries, variant, run)


class TestLateTailStats:
    """T11.5: late-receive-tail metric (latencies > 10 * p99)."""

    def test_hand_computed_example(self) -> None:
        """Spec example: p99=10ms, latencies [1,5,99,150,200] -> 2 / 40%.

        Hand-computation: threshold = 10 * 10 = 100 ms. Latencies 150
        and 200 ms both exceed the threshold; 99 is below. Count = 2,
        percentage = 2 / 5 = 40.0%.

        We feed the latencies via synthetic receive timestamps so the
        full pipeline (correlate + performance) processes them; the
        p99 of [1,5,99,150,200] is ~199.04 with linear interpolation,
        so we pass the desired p99 directly to ``_late_tail_stats``
        to keep the test focused on the threshold + percentage math.
        """
        from performance import _late_tail_stats

        # Build a delivery DataFrame with the given latencies.
        import polars as pl

        deliveries = pl.DataFrame(
            {"latency_ms": [1.0, 5.0, 99.0, 150.0, 200.0]},
        )
        count, pct = _late_tail_stats(deliveries, p99_ms=10.0)
        assert count == 2
        assert pct == 40.0

    def test_no_outliers(self) -> None:
        """All latencies under the threshold yield zero late-tail."""
        from performance import _late_tail_stats

        import polars as pl

        deliveries = pl.DataFrame(
            {"latency_ms": [1.0, 2.0, 3.0, 4.0, 5.0]},
        )
        count, pct = _late_tail_stats(deliveries, p99_ms=10.0)
        assert count == 0
        assert pct == 0.0

    def test_empty_deliveries(self) -> None:
        """Empty input returns (0, 0.0) without crashing."""
        from performance import _late_tail_stats

        import polars as pl

        empty = pl.DataFrame({"latency_ms": []}, schema={"latency_ms": pl.Float64})
        count, pct = _late_tail_stats(empty, p99_ms=5.0)
        assert count == 0
        assert pct == 0.0

    def test_attached_to_performance_result(self) -> None:
        """The metric is exposed on PerformanceResult."""
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1001,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1002,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        # The fields exist with sane defaults.
        assert r.late_receives_tail_count == 0
        assert r.late_receives_tail_pct == 0.0


class TestThreadingMode:
    """T11.5: threading_mode grouping dimension with single-default fallback."""

    def test_defaults_to_single_when_absent(self) -> None:
        """Pre-T14.8 logs omit threading_mode -> grouping value is 'single'."""
        events = [
            make_event(
                "connected",
                runner="alice",
                launch_ts="2025-04-15T09:35:49Z",
                elapsed_ms=42.0,
                offset_ms=42,
            ),
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1001,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1010,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        assert r.threading_mode == "single"

    def test_reads_explicit_single(self) -> None:
        """T14.8 logs with threading_mode='single' surface unchanged."""
        events = [
            make_event(
                "connected",
                runner="alice",
                launch_ts="2025-04-15T09:35:49Z",
                elapsed_ms=42.0,
                threading_mode="single",
                offset_ms=42,
            ),
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        assert r.threading_mode == "single"

    def test_reads_explicit_multi(self) -> None:
        """T14.8 logs with threading_mode='multi' surface unchanged."""
        events = [
            make_event(
                "connected",
                runner="alice",
                launch_ts="2025-04-15T09:35:49Z",
                elapsed_ms=42.0,
                threading_mode="multi",
                offset_ms=42,
            ),
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        assert r.threading_mode == "multi"

    def test_no_connected_events_yields_single(self) -> None:
        """Empty / connected-less groups default to 'single' too."""
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        assert r.threading_mode == "single"


class TestPercentile:
    def test_empty(self) -> None:
        assert _percentile([], 50) == 0.0

    def test_single_value(self) -> None:
        assert _percentile([5.0], 50) == 5.0
        assert _percentile([5.0], 99) == 5.0

    def test_two_values(self) -> None:
        result = _percentile([1.0, 3.0], 50)
        assert abs(result - 2.0) < 0.01

    def test_p99_close_to_max(self) -> None:
        data = list(range(100))
        p99 = _percentile([float(x) for x in data], 99)
        assert p99 >= 97.0


class TestPerformanceForGroup:
    def test_basic_metrics(self) -> None:
        events = [
            make_event("phase", runner="alice", phase="connect", offset_ms=0),
            make_event(
                "connected",
                runner="alice",
                launch_ts="2025-04-15T09:35:49Z",
                elapsed_ms=50.0,
                offset_ms=50,
            ),
            make_event("phase", runner="alice", phase="stabilize", offset_ms=51),
            make_event(
                "phase",
                runner="alice",
                phase="operate",
                profile="scalar-flood",
                offset_ms=1000,
            ),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1001,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1002,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1010,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1011,
            ),
            make_event(
                "resource",
                runner="alice",
                cpu_percent=5.0,
                memory_mb=10.0,
                offset_ms=1100,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        assert r.variant == "test-variant"
        assert r.connect_mean_ms == 50.0
        assert r.latency_p50_ms > 0
        assert r.writes_per_sec > 0
        assert r.loss_pct == 0.0
        assert len(r.resources) == 1
        assert r.resources[0].mean_cpu_pct == 5.0

    def test_no_events(self) -> None:
        # Empty group still returns a result, with zero metrics.
        r = _perf([])
        assert r.connect_mean_ms == 0.0
        assert r.latency_p50_ms == 0.0
        assert r.writes_per_sec == 0.0

    def test_connection_time_from_connected_event(self) -> None:
        events = [
            make_event(
                "connected",
                runner="alice",
                launch_ts="2025-04-15T09:35:49Z",
                elapsed_ms=42.5,
                offset_ms=42,
            ),
            make_event(
                "connected",
                runner="bob",
                launch_ts="2025-04-15T09:35:49Z",
                elapsed_ms=60.0,
                offset_ms=60,
            ),
        ]
        r = _perf(events)
        assert abs(r.connect_mean_ms - 51.25) < 0.01
        assert r.connect_max_ms == 60.0


class TestLossCrossClockAccounting:
    """T16.16: loss% accounting must use writer-clock-only boundaries.

    The pre-T16.16 bug filtered receives by ``receive_ts`` (receiver's
    local clock) against the writer's ``eot_sent_ts`` (writer's clock).
    On cross-machine runs with unsynced OS clocks this systematically
    excluded legitimate receives whose corresponding writes were
    in-window, manifesting as a spurious ~1% loss baseline on every
    transport at QoS 4 low rates (see
    ``logs/two-machines-all-variants-01-20260515_143007``).

    The fix derives ``receive_count`` from correlated deliveries and
    uses each delivery's ``write_ts`` (writer's clock) as the in-window
    test. These tests pin the new accounting so the bug cannot regress.
    """

    @staticmethod
    def _writer_and_receiver_events(
        *,
        write_count: int,
        clock_drift_ms: int,
        in_flight_ms: float,
        skip_receive_seqs: tuple[int, ...] = (),
        eot_offset_ms: int = 10_000,
        silent_offset_ms: int = 12_000,
    ) -> list[dict]:
        """Build a synthetic two-runner spawn with controllable clock drift.

        Alice writes ``seq=1..write_count`` over ``[1000, 10_000]`` ms in
        her own clock (one write every ~9 ms). Each write's
        corresponding receive on bob lands at
        ``alice_write_ts + in_flight_ms + clock_drift_ms`` in bob's
        clock -- a fixed receiver-side drift simulates two unsynced
        OS clocks. Alice's ``eot_sent`` at ``eot_offset_ms`` (her
        clock) closes the writer window; ``phase=silent`` at
        ``silent_offset_ms`` closes the group. Receives for sequences
        in ``skip_receive_seqs`` are omitted so the test can pin the
        ``loss_pct = 1/N`` arithmetic.
        """
        alice: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
        ]
        bob: list[dict] = [
            make_event("phase", runner="bob", phase="operate", offset_ms=1000),
        ]
        # Spread writes evenly across [1000, eot_offset_ms - 1].
        span = (eot_offset_ms - 1) - 1000
        step = span / max(write_count - 1, 1)
        for i in range(1, write_count + 1):
            write_offset = 1000 + step * (i - 1)
            alice.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=i,
                    path="/k",
                    qos=4,
                    bytes=8,
                    offset_ms=write_offset,
                )
            )
            if i in skip_receive_seqs:
                continue
            bob.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=i,
                    path="/k",
                    qos=4,
                    bytes=8,
                    offset_ms=write_offset + in_flight_ms + clock_drift_ms,
                )
            )
        alice.append(
            make_event("eot_sent", runner="alice", eot_id=1, offset_ms=eot_offset_ms)
        )
        alice.append(
            make_event(
                "phase", runner="alice", phase="silent", offset_ms=silent_offset_ms
            )
        )
        bob.append(
            make_event(
                "phase", runner="bob", phase="silent", offset_ms=silent_offset_ms
            )
        )
        return alice + bob

    def test_cross_clock_drift_yields_zero_loss(self) -> None:
        """Every write has a matching receive; receiver's clock runs
        ahead by 500 ms so half the receives have ``receive_ts >
        alice's eot_sent_ts``. Pre-T16.16 this would report ~50% loss
        (and would have reported ~1% on the validation dataset where
        drift was small relative to the run length). Post-T16.16 the
        loss must be 0% because every write_ts is in-window and every
        write has a matched delivery.
        """
        events = self._writer_and_receiver_events(
            write_count=100,
            clock_drift_ms=500,
            in_flight_ms=1.0,
        )
        r = _perf(events)
        assert r.loss_pct == 0.0

    def test_cross_clock_drift_with_one_missing(self) -> None:
        """Same setup as above but drop a single receive on the wire.
        The reported loss% must be exactly ``1/N * 100`` -- the cross-
        clock drift does NOT add phantom losses on top of the real one.
        """
        events = self._writer_and_receiver_events(
            write_count=100,
            clock_drift_ms=500,
            in_flight_ms=1.0,
            skip_receive_seqs=(50,),
        )
        r = _perf(events)
        # 99/100 delivered -> 1% loss.
        assert abs(r.loss_pct - 1.0) < 1e-6

    def test_legacy_no_eot_falls_back_to_silent_start(self) -> None:
        """Legacy logs without any ``eot_sent`` event must still produce
        sensible loss% by falling back to ``silent_start`` as the writer
        window end. The writer-clock accounting still applies: a
        delivery is "in-window" if its ``write_ts <= silent_start``.

        Expected behaviour for legacy logs:
        - ``late_receives`` is ``None`` (no EOT to define the late
          boundary -- documented in ``_late_receives_count``).
        - ``loss_pct`` is computed exactly as for the EOT path, just
          with ``silent_start`` substituted for ``eot_sent_ts``.
        """
        events = self._writer_and_receiver_events(
            write_count=50,
            clock_drift_ms=200,
            in_flight_ms=1.0,
            eot_offset_ms=10_000,
            silent_offset_ms=11_000,
        )
        # Strip the eot_sent event injected by the helper so this run
        # exercises the legacy fallback path.
        events = [e for e in events if e.get("event") != "eot_sent"]
        r = _perf(events)
        assert r.late_receives is None
        assert r.loss_pct == 0.0

    def test_late_receives_metric_independent_of_loss_fix(self) -> None:
        """The late_receives observability metric is unchanged by the
        T16.16 fix: it still counts receives in
        ``(eot_sent_ts, silent_start]`` on the receiver's clock. Even
        when loss_pct = 0 (every write is matched), late_receives can
        be non-zero -- the two metrics surface different things.
        """
        # Drift = 600 ms; writes evenly spaced; in-flight 1 ms; the
        # last few receives will land after alice's eot_sent_ts in
        # bob's clock and so will count as late_receives.
        events = self._writer_and_receiver_events(
            write_count=20,
            clock_drift_ms=600,
            in_flight_ms=1.0,
            eot_offset_ms=10_000,
            silent_offset_ms=12_000,
        )
        r = _perf(events)
        # Loss% is 0 because every write_ts is in-window and every
        # write has a matched delivery (writer-clock accounting).
        assert r.loss_pct == 0.0
        # Late_receives is > 0 because some receives crossed
        # alice's eot_sent_ts in bob's clock (drift > in-flight).
        assert r.late_receives is not None
        assert r.late_receives > 0
