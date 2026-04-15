"""Tests for the integrity verification module."""

from __future__ import annotations

import json

from helpers import make_event
from correlate import correlate
from integrity import verify_integrity
from parse import parse_line


def _ev(d: dict) -> object:
    return parse_line(json.dumps(d))


class TestIntegrityQoS1:
    """QoS 1: no ordering check, no completeness error, loss-tolerant."""

    def test_full_delivery(self) -> None:
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
                    "write",
                    runner="alice",
                    seq=2,
                    path="/k",
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
                    path="/k",
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
                    seq=2,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=111,
                )
            ),
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        assert len(results) == 1
        r = results[0]
        assert r.delivery_pct == 100.0
        assert not r.completeness_error
        assert not r.ordering_error

    def test_partial_delivery_no_error(self) -> None:
        """QoS 1: partial delivery is not an error."""
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
                    "write",
                    runner="alice",
                    seq=2,
                    path="/k",
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
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=110,
                )
            ),
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        assert len(results) == 1
        r = results[0]
        assert r.delivery_pct == 50.0
        assert not r.completeness_error


class TestIntegrityQoS2:
    """QoS 2: ordering checked, loss-tolerant."""

    def test_in_order(self) -> None:
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=2,
                    bytes=8,
                    offset_ms=100,
                )
            ),
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=2,
                    path="/k",
                    qos=2,
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
                    path="/k",
                    qos=2,
                    bytes=8,
                    offset_ms=110,
                )
            ),
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=2,
                    path="/k",
                    qos=2,
                    bytes=8,
                    offset_ms=111,
                )
            ),
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        r = results[0]
        assert r.out_of_order == 0
        assert not r.ordering_error

    def test_out_of_order_flagged(self) -> None:
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=2,
                    bytes=8,
                    offset_ms=100,
                )
            ),
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=2,
                    path="/k",
                    qos=2,
                    bytes=8,
                    offset_ms=101,
                )
            ),
            # Received out of order (seq 2 before seq 1)
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=2,
                    path="/k",
                    qos=2,
                    bytes=8,
                    offset_ms=109,
                )
            ),
            _ev(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=1,
                    path="/k",
                    qos=2,
                    bytes=8,
                    offset_ms=110,
                )
            ),
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        r = results[0]
        assert r.out_of_order > 0
        assert r.ordering_error


class TestIntegrityQoS3:
    """QoS 3: 100% delivery, strict ordering, no duplicates, gap recovery."""

    def test_missing_delivery_flagged(self) -> None:
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=3,
                    bytes=8,
                    offset_ms=100,
                )
            ),
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=2,
                    path="/k",
                    qos=3,
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
                    path="/k",
                    qos=3,
                    bytes=8,
                    offset_ms=110,
                )
            ),
            # seq 2 not received
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        r = results[0]
        assert r.delivery_pct == 50.0
        assert r.completeness_error

    def test_duplicate_flagged(self) -> None:
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=3,
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
                    qos=3,
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
                    path="/k",
                    qos=3,
                    bytes=8,
                    offset_ms=111,
                )
            ),
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        r = results[0]
        assert r.duplicates > 0
        assert r.duplicate_error

    def test_gap_detected_and_filled(self) -> None:
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=3,
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
                    qos=3,
                    bytes=8,
                    offset_ms=110,
                )
            ),
            _ev(
                make_event(
                    "gap_detected",
                    runner="bob",
                    writer="alice",
                    missing_seq=2,
                    offset_ms=115,
                )
            ),
            _ev(
                make_event(
                    "gap_filled",
                    runner="bob",
                    writer="alice",
                    recovered_seq=2,
                    offset_ms=120,
                )
            ),
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        r = results[0]
        assert r.unresolved_gaps == 0
        assert not r.gap_error

    def test_unresolved_gap_flagged(self) -> None:
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=3,
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
                    qos=3,
                    bytes=8,
                    offset_ms=110,
                )
            ),
            _ev(
                make_event(
                    "gap_detected",
                    runner="bob",
                    writer="alice",
                    missing_seq=2,
                    offset_ms=115,
                )
            ),
            # No gap_filled
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        r = results[0]
        assert r.unresolved_gaps == 1
        assert r.gap_error


class TestIntegrityQoS4:
    """QoS 4: same as QoS 3 but no gap checking."""

    def test_missing_delivery_flagged(self) -> None:
        events = [
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=4,
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
                    qos=4,
                    bytes=8,
                    offset_ms=110,
                )
            ),
            _ev(
                make_event(
                    "write",
                    runner="alice",
                    seq=2,
                    path="/k",
                    qos=4,
                    bytes=8,
                    offset_ms=101,
                )
            ),
            # seq 2 not received
        ]
        records = correlate(events)
        results = verify_integrity(events, records)
        r = results[0]
        assert r.completeness_error
        assert r.unresolved_gaps is None  # gap checking not applicable
