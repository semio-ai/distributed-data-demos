"""Tests for the performance metrics module."""

from __future__ import annotations

import json

from helpers import make_event
from correlate import correlate
from parse import parse_line
from performance import _percentile, compute_performance


def _ev(d: dict) -> object:
    return parse_line(json.dumps(d))


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


class TestComputePerformance:
    def test_basic_metrics(self) -> None:
        """Compute performance for a simple two-runner scenario."""
        events = [
            _ev(make_event("phase", runner="alice", phase="connect", offset_ms=0)),
            _ev(
                make_event(
                    "connected",
                    runner="alice",
                    launch_ts="2025-04-15T09:35:49Z",
                    elapsed_ms=50.0,
                    offset_ms=50,
                )
            ),
            _ev(make_event("phase", runner="alice", phase="stabilize", offset_ms=51)),
            _ev(
                make_event(
                    "phase",
                    runner="alice",
                    phase="operate",
                    profile="scalar-flood",
                    offset_ms=1000,
                )
            ),
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1001,
                )
            ),
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=2,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1002,
                )
            ),
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1010,
                )
            ),
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=2,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1011,
                )
            ),
            _ev(
                make_event(
                    "resource",
                    runner="alice",
                    cpu_percent=5.0,
                    memory_mb=10.0,
                    offset_ms=1100,
                )
            ),
            _ev(make_event("phase", runner="alice", phase="silent", offset_ms=2000)),
        ]

        records = correlate(events)
        results = compute_performance(events, records)
        assert len(results) == 1

        r = results[0]
        assert r.variant == "test-variant"
        assert r.connect_mean_ms == 50.0
        assert r.latency_p50_ms > 0
        assert r.writes_per_sec > 0
        assert r.loss_pct == 0.0
        assert len(r.resources) == 1
        assert r.resources[0].mean_cpu_pct == 5.0

    def test_no_events(self) -> None:
        results = compute_performance([], [])
        assert results == []

    def test_connection_time_from_connected_event(self) -> None:
        """Connection time should use elapsed_ms from connected events."""
        events = [
            _ev(
                make_event(
                    "connected",
                    runner="alice",
                    launch_ts="2025-04-15T09:35:49Z",
                    elapsed_ms=42.5,
                    offset_ms=42,
                )
            ),
            _ev(
                make_event(
                    "connected",
                    runner="bob",
                    launch_ts="2025-04-15T09:35:49Z",
                    elapsed_ms=60.0,
                    offset_ms=60,
                )
            ),
        ]
        results = compute_performance(events, [])
        assert len(results) == 1
        r = results[0]
        assert abs(r.connect_mean_ms - 51.25) < 0.01
        assert r.connect_max_ms == 60.0
