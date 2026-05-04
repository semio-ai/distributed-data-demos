"""Tests for the polars-based performance metrics module."""

from __future__ import annotations

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy
from performance import _percentile, performance_for_group


def _perf(events: list[dict], variant: str = "test-variant", run: str = "run01"):
    lazy = events_to_lazy(events)
    deliveries = correlate_lazy(lazy).collect()
    return performance_for_group(lazy, deliveries, variant, run)


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
