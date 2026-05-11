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
