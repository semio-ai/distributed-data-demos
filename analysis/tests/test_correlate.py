"""Tests for the write-receive correlation module."""

from __future__ import annotations

from helpers import make_event
from correlate import correlate
from parse import parse_line

import json


def _ev(d: dict) -> object:
    """Parse a dict into an Event."""
    return parse_line(json.dumps(d))


class TestCorrelate:
    def test_basic_correlation(self) -> None:
        """A write from alice and receive on bob should produce one delivery record."""
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=100,
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
                    offset_ms=110,
                )
            ),
        ]
        records = correlate(events)
        assert len(records) == 1
        rec = records[0]
        assert rec.writer == "alice"
        assert rec.receiver == "bob"
        assert rec.seq == 1
        assert rec.path == "/k"
        assert rec.latency_ms > 0

    def test_loopback_correlation(self) -> None:
        """Single-runner loopback: writer == receiver."""
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=100,
                )
            ),
            _ev(
                make_event(
                    "receive",
                    runner="alice",
                    writer="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=100.01,
                )
            ),
        ]
        records = correlate(events)
        assert len(records) == 1
        assert records[0].writer == "alice"
        assert records[0].receiver == "alice"
        assert records[0].latency_ms >= 0

    def test_no_matching_receive(self) -> None:
        """A write with no matching receive produces no record."""
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=100,
                )
            ),
        ]
        records = correlate(events)
        assert len(records) == 0

    def test_multiple_paths(self) -> None:
        """Different paths with same seq should produce separate records."""
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/a",
                    qos=1,
                    bytes=8,
                    offset_ms=100,
                )
            ),
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/b",
                    qos=1,
                    bytes=8,
                    offset_ms=101,
                )
            ),
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=1,
                    path="/a",
                    qos=1,
                    bytes=8,
                    offset_ms=110,
                )
            ),
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=1,
                    path="/b",
                    qos=1,
                    bytes=8,
                    offset_ms=111,
                )
            ),
        ]
        records = correlate(events)
        assert len(records) == 2
        paths = {r.path for r in records}
        assert paths == {"/a", "/b"}

    def test_non_write_receive_events_ignored(self) -> None:
        """Phase and resource events should not produce records."""
        events = [
            _ev(make_event("phase", phase="connect", offset_ms=0)),
            _ev(
                make_event(
                    "connected", launch_ts="2026-04-15T09:35:49Z", elapsed_ms=50.0
                )
            ),
            _ev(make_event("resource", cpu_percent=5.0, memory_mb=10.0, offset_ms=100)),
        ]
        records = correlate(events)
        assert len(records) == 0

    def test_two_runner_bidirectional(self) -> None:
        """Both alice->bob and bob->alice deliveries should be found."""
        events = [
            # alice writes
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=100,
                )
            ),
            # bob writes
            _ev(
                make_event(
                    "write",
                    runner="bob",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=100,
                )
            ),
            # bob receives from alice
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=110,
                )
            ),
            # alice receives from bob
            _ev(
                make_event(
                    "receive",
                    runner="alice",
                    writer="bob",
                    seq=1,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=110,
                )
            ),
        ]
        records = correlate(events)
        assert len(records) == 2
        writers = {r.writer for r in records}
        assert writers == {"alice", "bob"}
