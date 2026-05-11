"""Tests for the polars-based integrity verification."""

from __future__ import annotations

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy
from integrity import integrity_for_group


def _verify(events: list[dict]):
    """Run the per-group integrity pipeline against a synthetic event list.

    Tests assume the events are all from a single (variant, run) group.
    """
    lazy = events_to_lazy(events)
    deliveries = correlate_lazy(lazy).collect()
    return integrity_for_group(lazy, deliveries)


class TestIntegrityQoS1:
    """QoS 1: no ordering check, no completeness error, loss-tolerant."""

    def test_full_delivery(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=101,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=111,
            ),
        ]
        results = _verify(events)
        assert len(results) == 1
        r = results[0]
        assert r.delivery_pct == 100.0
        assert not r.completeness_error
        assert not r.ordering_error

    def test_partial_delivery_no_error(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=101,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
        ]
        results = _verify(events)
        assert len(results) == 1
        r = results[0]
        assert r.delivery_pct == 50.0
        assert not r.completeness_error


class TestIntegrityQoS2:
    """QoS 2: ordering checked, loss-tolerant."""

    def test_in_order(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=101,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=111,
            ),
        ]
        results = _verify(events)
        r = results[0]
        assert r.out_of_order == 0
        assert not r.ordering_error

    def test_out_of_order_flagged(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=101,
            ),
            # Received out of order (seq 2 before seq 1)
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=109,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=110,
            ),
        ]
        results = _verify(events)
        r = results[0]
        assert r.out_of_order > 0
        assert r.ordering_error


class TestIntegrityQoS3:
    """QoS 3: 100% delivery, strict ordering, no duplicates, gap recovery."""

    def test_missing_delivery_flagged(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=101,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=110,
            ),
            # seq 2 not received
        ]
        results = _verify(events)
        r = results[0]
        assert r.delivery_pct == 50.0
        assert r.completeness_error

    def test_duplicate_flagged(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=111,
            ),
        ]
        results = _verify(events)
        r = results[0]
        assert r.duplicates > 0
        assert r.duplicate_error

    def test_gap_detected_and_filled(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "gap_detected",
                runner="bob",
                writer="alice",
                missing_seq=2,
                offset_ms=115,
            ),
            make_event(
                "gap_filled",
                runner="bob",
                writer="alice",
                recovered_seq=2,
                offset_ms=120,
            ),
        ]
        results = _verify(events)
        r = results[0]
        assert r.unresolved_gaps == 0
        assert not r.gap_error

    def test_unresolved_gap_flagged(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=3,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "gap_detected",
                runner="bob",
                writer="alice",
                missing_seq=2,
                offset_ms=115,
            ),
            # No gap_filled
        ]
        results = _verify(events)
        r = results[0]
        assert r.unresolved_gaps == 1
        assert r.gap_error


class TestIntegrityQoS4:
    """QoS 4: same as QoS 3 but no gap checking."""

    def test_missing_delivery_flagged(self) -> None:
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=101,
            ),
            # seq 2 not received
        ]
        results = _verify(events)
        r = results[0]
        assert r.completeness_error
        assert r.unresolved_gaps is None  # gap checking not applicable


class TestIntegrityBackpressureSkipped:
    """T-impl.6: per-writer ``backpressure_skipped`` counter on integrity rows."""

    def test_skipped_count_surfaced_on_writer_row(self) -> None:
        # alice writes 2 values, then the driver reports 3 skipped
        # values (transport backpressured). bob receives both writes.
        # Expected: one integrity row alice -> bob with
        # backpressure_skipped_count == 3.
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=101,
            ),
            make_event(
                "backpressure_skipped",
                runner="alice",
                path="/k",
                qos=1,
                offset_ms=102,
            ),
            make_event(
                "backpressure_skipped",
                runner="alice",
                path="/k",
                qos=1,
                offset_ms=103,
            ),
            make_event(
                "backpressure_skipped",
                runner="alice",
                path="/k",
                qos=1,
                offset_ms=104,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=111,
            ),
        ]
        results = _verify(events)
        assert len(results) == 1
        r = results[0]
        assert r.writer == "alice"
        assert r.receiver == "bob"
        assert r.write_count == 2
        assert r.receive_count == 2
        assert r.backpressure_skipped_count == 3

    def test_no_skipped_events_yields_zero(self) -> None:
        # No `backpressure_skipped` events in the log -- the count
        # must default to 0 rather than missing or None. This is the
        # legacy-log compatibility case (pre-T-impl.6).
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
        ]
        results = _verify(events)
        r = results[0]
        assert r.backpressure_skipped_count == 0

    def test_skipped_count_replicated_per_receiver(self) -> None:
        # alice writes to bob and carol; the skip count is per-writer
        # so the same number must appear on both rows.
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "backpressure_skipped",
                runner="alice",
                path="/k",
                qos=1,
                offset_ms=101,
            ),
            make_event(
                "backpressure_skipped",
                runner="alice",
                path="/k",
                qos=1,
                offset_ms=102,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "receive",
                runner="carol",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=111,
            ),
        ]
        results = _verify(events)
        assert len(results) == 2
        for r in results:
            assert r.writer == "alice"
            assert r.backpressure_skipped_count == 2
