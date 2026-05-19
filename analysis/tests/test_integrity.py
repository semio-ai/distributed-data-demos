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
    """QoS 2 (latest-value datagram): loss-tolerant, no ordering guarantee.

    Pre-T14.17-follow-up the qos2 ordering check fired ``[FAIL: ordering]``
    on out-of-order receives. The 2026-05-14 follow-up makes the check
    QoS-aware: qos1 and qos2 are unreliable/latest-value datagram-style
    QoS levels with no ordering contract by design (the WebRTC qos2
    implementation uses an unreliable/unordered SCTP channel and
    relies on the receiver's latest-value semantics, so out-of-order
    receives are a normal protocol feature). The ``[FAIL: ordering]``
    annotation is reserved for qos3/qos4 from this point on.
    """

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

    def test_out_of_order_not_flagged(self) -> None:
        """qos2 out-of-order receives are counted but NOT flagged.

        Post-2026-05-14 ``[FAIL: ordering]`` only fires for qos3 and
        qos4. The ``out_of_order`` field still records the count so
        operators can see the absolute number; the ``ordering_error``
        boolean stays False.
        """
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
        # qos2 has no ordering guarantee -- the count is recorded but
        # the boolean error flag stays False.
        assert not r.ordering_error


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


def _out_of_order_pair(qos: int) -> list[dict]:
    """Build a write+receive event sequence whose receives are out of order.

    Used by the QoS-aware ordering tests: at every QoS level we want
    the same input shape so the only variable is the ``qos`` field
    -- the integrity rule is what changes per level.
    """
    return [
        make_event(
            "write",
            runner="alice",
            seq=1,
            path="/k",
            qos=qos,
            bytes=8,
            offset_ms=100,
        ),
        make_event(
            "write",
            runner="alice",
            seq=2,
            path="/k",
            qos=qos,
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
            qos=qos,
            bytes=8,
            offset_ms=109,
        ),
        make_event(
            "receive",
            runner="bob",
            writer="alice",
            seq=1,
            path="/k",
            qos=qos,
            bytes=8,
            offset_ms=110,
        ),
    ]


class TestOrderingQoSAware:
    """T14.17 follow-up (2026-05-14): the ordering check is QoS-aware.

    qos1 (best-effort) and qos2 (latest-value) are datagram-style QoS
    levels with no ordering guarantee by design. The WebRTC qos1/qos2
    implementations rely on the underlying transport's
    unreliable/unordered datagram channel, so out-of-order receives
    are a normal protocol feature -- the ``[FAIL: ordering]``
    annotation must NOT fire. Only qos3 (reliable-ordered) and qos4
    (reliable-tcp) carry an ordering contract and continue to flag.
    """

    def test_qos1_out_of_order_not_flagged(self) -> None:
        results = _verify(_out_of_order_pair(qos=1))
        r = results[0]
        # The count is still recorded so operators can see the absolute
        # number; only the boolean error flag is suppressed.
        assert r.out_of_order > 0
        assert not r.ordering_error

    def test_qos2_out_of_order_not_flagged(self) -> None:
        results = _verify(_out_of_order_pair(qos=2))
        r = results[0]
        assert r.out_of_order > 0
        assert not r.ordering_error

    def test_qos3_out_of_order_still_flagged(self) -> None:
        results = _verify(_out_of_order_pair(qos=3))
        r = results[0]
        assert r.out_of_order > 0
        assert r.ordering_error

    def test_qos4_out_of_order_still_flagged(self) -> None:
        results = _verify(_out_of_order_pair(qos=4))
        r = results[0]
        assert r.out_of_order > 0
        assert r.ordering_error


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


class TestIntegritySkipAtReliable:
    """T17.9: ``backpressure_skipped`` at QoS 3/4 is a contract violation.

    Per ``DESIGN.md`` § 6.5 (Strict No-Skip Contract for QoS 3/4) and
    ``api-contracts/jsonl-log-schema.md``, the variant MUST block the
    publish call at QoS 3/4 rather than skip. Any
    ``backpressure_skipped`` row with ``qos >= 3`` is a regression
    against E17 -- the integrity row must flip ``skip_at_reliable_error``
    on (-> ``[FAIL: skip-at-reliable]`` annotation in the table).
    """

    def _events_with_skip(self, qos: int) -> list[dict]:
        return [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=qos,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "backpressure_skipped",
                runner="alice",
                path="/k",
                qos=qos,
                offset_ms=101,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=qos,
                bytes=8,
                offset_ms=110,
            ),
        ]

    def test_qos3_skip_flags_violation(self) -> None:
        results = _verify(self._events_with_skip(qos=3))
        r = results[0]
        assert r.skip_at_reliable_count == 1
        assert r.skip_at_reliable_error

    def test_qos4_skip_flags_violation(self) -> None:
        results = _verify(self._events_with_skip(qos=4))
        r = results[0]
        assert r.skip_at_reliable_count == 1
        assert r.skip_at_reliable_error

    def test_qos1_skip_is_not_a_violation(self) -> None:
        """``backpressure_skipped`` is contractual at QoS 1."""
        results = _verify(self._events_with_skip(qos=1))
        r = results[0]
        # Aggregate counter still records the skip event for stats...
        assert r.backpressure_skipped_count == 1
        # ...but the contract-violation specific counter stays 0 and
        # the error flag stays False -- QoS 1 is loss-tolerant by
        # design (DESIGN.md § 6.5).
        assert r.skip_at_reliable_count == 0
        assert not r.skip_at_reliable_error

    def test_qos2_skip_is_not_a_violation(self) -> None:
        """``backpressure_skipped`` is contractual at QoS 2 too."""
        results = _verify(self._events_with_skip(qos=2))
        r = results[0]
        assert r.backpressure_skipped_count == 1
        assert r.skip_at_reliable_count == 0
        assert not r.skip_at_reliable_error

    def test_no_skip_events_yields_zero(self) -> None:
        """Healthy QoS 3 stream produces zero violation count + flag."""
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
        ]
        results = _verify(events)
        r = results[0]
        assert r.skip_at_reliable_count == 0
        assert not r.skip_at_reliable_error

    def test_skip_count_attaches_only_to_matching_qos_row(self) -> None:
        """Per-(writer, qos) keying: a qos3 skip count does not bleed
        onto a qos1 row written by the same writer.

        Synthetic shape: alice writes to bob (qos1) and to carol
        (qos3), and the driver emits a single ``backpressure_skipped``
        at qos3. Only the alice->carol qos3 row should carry the
        violation; the alice->bob qos1 row stays clean.
        """
        events = [
            # qos1 path: one write, one receive (bob).
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k1",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k1",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
            # qos3 path: one write, one receive (carol), one violation skip.
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k3",
                qos=3,
                bytes=8,
                offset_ms=200,
            ),
            make_event(
                "backpressure_skipped",
                runner="alice",
                path="/k3",
                qos=3,
                offset_ms=201,
            ),
            make_event(
                "receive",
                runner="carol",
                writer="alice",
                seq=1,
                path="/k3",
                qos=3,
                bytes=8,
                offset_ms=210,
            ),
        ]
        results = _verify(events)
        by_receiver = {r.receiver: r for r in results}
        # alice -> bob (qos1): clean, no contract violation.
        bob = by_receiver["bob"]
        assert bob.qos == 1
        assert bob.skip_at_reliable_count == 0
        assert not bob.skip_at_reliable_error
        # alice -> carol (qos3): contract violation flagged.
        carol = by_receiver["carol"]
        assert carol.qos == 3
        assert carol.skip_at_reliable_count == 1
        assert carol.skip_at_reliable_error


class TestIntegrityLeavesLost:
    """E19 / T19.6: leaves_lost accounting on the integrity report.

    Locked spec: ``leaves_lost == leaves_written - leaves_received``
    per (writer, receiver, qos) pair. For pre-E19 data where
    ``leaf_count == 1`` everywhere this equals ``ops_lost``; for
    block-flood / mixed-types the leaf total can be many multiples
    larger than the op total.
    """

    def test_legacy_data_leaves_lost_equals_ops_lost(self) -> None:
        """Pre-E19 data (leaf_count defaults to 1) -> leaves_lost == ops_lost."""
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
                "write",
                runner="alice",
                seq=3,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=102,
            ),
            # Only seq 1 + 3 delivered (seq 2 lost in transit).
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
                seq=3,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=112,
            ),
        ]
        results = _verify(events)
        assert len(results) == 1
        r = results[0]
        assert r.write_count == 3
        assert r.receive_count == 2
        assert r.ops_lost == 1
        # leaf_count defaults to 1 on every row -> leaves_lost == ops_lost.
        assert r.leaves_lost == 1
        assert r.leaves_lost == r.ops_lost

    def test_block_flood_leaves_lost_is_ops_lost_times_leaf_count(self) -> None:
        """Block-flood loss: leaves_lost = ops_lost * leaf_count (100 here)."""
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=800,
                leaf_count=100,
                shape="array",
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=800,
                leaf_count=100,
                shape="array",
                offset_ms=101,
            ),
            make_event(
                "write",
                runner="alice",
                seq=3,
                path="/k",
                qos=1,
                bytes=800,
                leaf_count=100,
                shape="array",
                offset_ms=102,
            ),
            # Only seq 1 delivered (seq 2 + 3 lost -> 200 leaves lost).
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=800,
                offset_ms=110,
            ),
        ]
        results = _verify(events)
        assert len(results) == 1
        r = results[0]
        assert r.write_count == 3
        assert r.receive_count == 1
        assert r.ops_lost == 2
        # 2 lost ops * 100 leaves per op = 200 leaves lost.
        assert r.leaves_lost == 200

    def test_full_delivery_zero_leaves_lost(self) -> None:
        """When every op is delivered the leaves_lost column reads 0."""
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=400,
                leaf_count=50,
                shape="array",
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=400,
                leaf_count=50,
                shape="array",
                offset_ms=101,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=400,
                offset_ms=110,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=400,
                offset_ms=111,
            ),
        ]
        results = _verify(events)
        assert len(results) == 1
        r = results[0]
        assert r.write_count == 2
        assert r.receive_count == 2
        assert r.ops_lost == 0
        assert r.leaves_lost == 0

    def test_mixed_leaf_counts_sum_correctly(self) -> None:
        """Mixed-types: per-op leaf_count varies; leaves_lost sums the lost ones."""
        events = [
            # Three writes with varied leaf counts.
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=80,
                leaf_count=10,
                shape="struct",
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=400,
                leaf_count=50,
                shape="struct",
                offset_ms=101,
            ),
            make_event(
                "write",
                runner="alice",
                seq=3,
                path="/k",
                qos=1,
                bytes=240,
                leaf_count=30,
                shape="struct",
                offset_ms=102,
            ),
            # Only seq 1 (10 leaves) delivered; seq 2 (50) + seq 3 (30)
            # lost -> 80 leaves lost total.
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=80,
                offset_ms=110,
            ),
        ]
        results = _verify(events)
        assert len(results) == 1
        r = results[0]
        assert r.write_count == 3
        assert r.receive_count == 1
        assert r.ops_lost == 2
        # 50 + 30 = 80 leaves lost across two unmatched writes.
        assert r.leaves_lost == 80
