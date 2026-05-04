"""Tests for the polars-based write-receive correlation."""

from __future__ import annotations

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy, deliveries_to_records


def make_clock_sync(
    runner: str = "alice",
    peer: str = "bob",
    variant: str = "",
    offset_ms: float = 0.0,
    rtt_ms: float = 0.5,
    ts_offset_ms: float = 0.0,
    **extra: object,
) -> dict:
    """Build a clock_sync JSONL event dict for correlate tests.

    Mirrors ``test_clock_offsets.make_clock_sync``: ``ts_offset_ms``
    drives the synthetic timestamp generator while ``offset_ms`` is
    the JSONL offset payload.
    """
    ev = make_event(
        "clock_sync",
        runner=runner,
        variant=variant,
        offset_ms=ts_offset_ms,
        **extra,
    )
    ev["peer"] = peer
    ev["offset_ms"] = offset_ms
    ev["rtt_ms"] = rtt_ms
    return ev


class TestCorrelate:
    def test_basic_correlation(self) -> None:
        """A write from alice and receive on bob should produce one delivery record."""
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
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
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
                runner="alice",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100.01,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
        assert len(records) == 1
        assert records[0].writer == "alice"
        assert records[0].receiver == "alice"
        assert records[0].latency_ms >= 0

    def test_no_matching_receive(self) -> None:
        """A write with no matching receive produces no record."""
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
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        assert deliveries.is_empty()

    def test_multiple_paths(self) -> None:
        """Different paths with same seq should produce separate records."""
        events = [
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/a",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/b",
                qos=1,
                bytes=8,
                offset_ms=101,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/a",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/b",
                qos=1,
                bytes=8,
                offset_ms=111,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        assert deliveries.height == 2
        paths = set(deliveries.get_column("path").to_list())
        assert paths == {"/a", "/b"}

    def test_non_write_receive_events_ignored(self) -> None:
        """Phase and resource events should not produce records."""
        events = [
            make_event("phase", phase="connect", offset_ms=0),
            make_event(
                "connected",
                launch_ts="2026-04-15T09:35:49Z",
                elapsed_ms=50.0,
            ),
            make_event("resource", cpu_percent=5.0, memory_mb=10.0, offset_ms=100),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        assert deliveries.is_empty()

    def test_two_runner_bidirectional(self) -> None:
        """Both alice->bob and bob->alice deliveries should be found."""
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
                runner="bob",
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
            make_event(
                "receive",
                runner="alice",
                writer="bob",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        assert deliveries.height == 2
        writers = set(deliveries.get_column("writer").to_list())
        assert writers == {"alice", "bob"}


class TestParityWithPhase1:
    """Synthetic-fixture parity check vs the Phase 1 dict-based correlator.

    The Phase 1 correlator (``Event``-list -> ``DeliveryRecord``-list)
    lived in the old ``correlate.correlate(events: list[Event])``. We
    re-implement that exact algorithm inline as a ground-truth oracle
    and verify the polars implementation produces the same set of
    delivery records (modulo row order).
    """

    @staticmethod
    def _phase1_correlate(events: list[dict]) -> list[tuple]:
        """Phase 1 dict-based correlator, returning a tuple-key set."""
        writes: dict[tuple, dict] = {}
        receives: list[dict] = []
        for ev in events:
            if ev["event"] == "write":
                seq = ev.get("seq")
                path = ev.get("path")
                if seq is not None and path is not None:
                    writes[
                        (
                            ev["variant"],
                            ev["run"],
                            ev["runner"],
                            int(seq),
                            str(path),
                        )
                    ] = ev
            elif ev["event"] == "receive":
                receives.append(ev)

        out: list[tuple] = []
        for r in receives:
            writer = r.get("writer")
            seq = r.get("seq")
            path = r.get("path")
            if writer is None or seq is None or path is None:
                continue
            key = (
                r["variant"],
                r["run"],
                str(writer),
                int(seq),
                str(path),
            )
            w = writes.get(key)
            if w is None:
                continue
            out.append(
                (
                    r["variant"],
                    r["run"],
                    str(writer),
                    r["runner"],
                    int(seq),
                    str(path),
                    int(r.get("qos", 1)),
                )
            )
        return out

    def test_parity_synthetic_qos1(self) -> None:
        events: list[dict] = []
        # Two writers, two receivers, multiple paths and seqs.
        for seq in range(1, 11):
            events.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=seq,
                    path="/a",
                    qos=1,
                    bytes=8,
                    offset_ms=seq,
                )
            )
            events.append(
                make_event(
                    "write",
                    runner="bob",
                    seq=seq,
                    path="/b",
                    qos=1,
                    bytes=8,
                    offset_ms=seq,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=seq,
                    path="/a",
                    qos=1,
                    bytes=8,
                    offset_ms=seq + 5,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="alice",
                    writer="bob",
                    seq=seq,
                    path="/b",
                    qos=1,
                    bytes=8,
                    offset_ms=seq + 5,
                )
            )

        ground_truth = sorted(self._phase1_correlate(events))

        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        polars_keys = sorted(
            (
                row["variant"],
                row["run"],
                row["writer"],
                row["receiver"],
                int(row["seq"]),
                row["path"],
                int(row["qos"]),
            )
            for row in deliveries.iter_rows(named=True)
        )

        assert polars_keys == ground_truth


class TestOffsetApplication:
    """Clock-skew correction via ``join_asof`` (E8)."""

    def test_no_clock_sync_marks_cross_runner_uncorrected(self) -> None:
        """Without any clock_sync rows, cross-runner deliveries are
        flagged ``offset_applied=False`` and their latency is left
        uncorrected."""
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
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
        assert len(records) == 1
        rec = records[0]
        assert rec.offset_applied is False
        assert rec.offset_ms is None
        # Raw latency = 10 ms (no correction applied).
        assert abs(rec.latency_ms - 10.0) < 0.01

    def test_same_runner_offset_zero_applied_true(self) -> None:
        """Same-runner deliveries are forced to offset 0, applied=True
        regardless of any clock_sync entries in the group."""
        events = [
            make_clock_sync(
                runner="alice",
                peer="alice",
                variant="",
                offset_ms=99.0,
                ts_offset_ms=0,
            ),
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
                runner="alice",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=110,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
        assert len(records) == 1
        rec = records[0]
        assert rec.writer == rec.receiver == "alice"
        assert rec.offset_applied is True
        assert rec.offset_ms == 0.0
        # Same-runner: latency stays at the raw 10 ms.
        assert abs(rec.latency_ms - 10.0) < 0.01

    def test_initial_sync_corrects_cross_runner_latency(self) -> None:
        """An initial-sync (variant="") row with offset=+50 ms applied
        on the receiver subtracts 50 ms of skew from the observed
        latency. (The math: writer's ts is 50 ms behind receiver's, so
        receive_ts - write_ts = real_latency + 50; correction adds the
        offset of writer - receiver = -50 ms.)

        Test setup: receiver is bob, writer is alice. clock_sync row
        on bob, peer=alice, offset_ms = -50 means alice's clock is
        50 ms behind bob's. raw delta = 150 ms; corrected = 100 ms.
        """
        events = [
            # Initial sync recorded by bob: alice is 50 ms behind bob.
            make_clock_sync(
                runner="bob",
                peer="alice",
                variant="",
                offset_ms=-50.0,
                ts_offset_ms=0,
            ),
            # Alice writes at her t=100 ms.
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            # Bob receives at his t=250 ms (raw delta = 150 ms, real
            # latency = 100 ms + 50 ms clock skew).
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=250,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
        assert len(records) == 1
        rec = records[0]
        assert rec.offset_applied is True
        assert rec.offset_ms == -50.0
        assert abs(rec.latency_ms - 100.0) < 0.01

    def test_per_variant_resync_preferred_over_initial(self) -> None:
        """If both an initial sync and a matching per-variant resync
        exist, the per-variant one wins."""
        events = [
            # Initial sync says alice is 50 ms behind bob (stale).
            make_clock_sync(
                runner="bob",
                peer="alice",
                variant="",
                offset_ms=-50.0,
                ts_offset_ms=0,
            ),
            # Per-variant resync (drift-corrected) says alice is now 70 ms
            # behind. This is the value we expect to win.
            make_clock_sync(
                runner="bob",
                peer="alice",
                variant="test-variant",
                offset_ms=-70.0,
                ts_offset_ms=80,
            ),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=100,
            ),
            # Raw delta = 270 ms; with -70 correction => 200 ms.
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=370,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
        assert len(records) == 1
        rec = records[0]
        assert rec.offset_applied is True
        assert rec.offset_ms == -70.0
        assert abs(rec.latency_ms - 200.0) < 0.01

    def test_initial_fallback_when_per_variant_missing(self) -> None:
        """Variant has no per-variant resync entry but an initial sync
        is available -- the initial sync is used.

        ``correlate_lazy`` is called on a per-(variant, run) group so we
        only feed rows for the current variant. The ``other-variant``
        entry below is intentionally NOT included to simulate the
        per-variant absence; the initial sync (variant="") is the
        only viable offset source for the group.
        """
        events = [
            make_clock_sync(
                runner="bob",
                peer="alice",
                variant="",
                offset_ms=-50.0,
                ts_offset_ms=0,
            ),
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
                offset_ms=250,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
        assert len(records) == 1
        rec = records[0]
        assert rec.offset_applied is True
        assert rec.offset_ms == -50.0
        # Raw 150 ms + (-50 ms) = 100 ms corrected.
        assert abs(rec.latency_ms - 100.0) < 0.01

    def test_missing_offset_for_one_pair_other_pair_corrected(self) -> None:
        """One cross-runner pair has an offset, the other does not.

        - alice -> bob: clock_sync(bob, peer=alice) is present -> corrected.
        - bob -> alice: no clock_sync(alice, peer=bob) -> uncorrected.
        """
        events = [
            make_clock_sync(
                runner="bob",
                peer="alice",
                variant="",
                offset_ms=-50.0,
                ts_offset_ms=0,
            ),
            # alice -> bob delivery
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
                offset_ms=250,
            ),
            # bob -> alice delivery (no offset on alice side)
            make_event(
                "write",
                runner="bob",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=300,
            ),
            make_event(
                "receive",
                runner="alice",
                writer="bob",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=320,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = sorted(deliveries_to_records(deliveries), key=lambda r: r.seq)
        assert len(records) == 2

        a2b = records[0]
        assert a2b.writer == "alice" and a2b.receiver == "bob"
        assert a2b.offset_applied is True
        assert a2b.offset_ms == -50.0
        assert abs(a2b.latency_ms - 100.0) < 0.01

        b2a = records[1]
        assert b2a.writer == "bob" and b2a.receiver == "alice"
        assert b2a.offset_applied is False
        assert b2a.offset_ms is None
        # Raw 20 ms is preserved.
        assert abs(b2a.latency_ms - 20.0) < 0.01

    def test_fixture_plus_50ms_skew_corrected(self) -> None:
        """End-to-end: a +50 ms receiver skew, correction yields ~100 ms.

        Setup: writer alice writes at her t=100 ms. Receiver bob's
        clock is +50 ms ahead of alice's (so a write_ts of t=100 ms
        arrives on bob's wall clock at bob's t=250 ms when the real
        network latency is 100 ms). Without correction the analysis
        would report 150 ms; with correction (offset = peer - self
        = alice - bob = -50 ms applied as +offset) reports 100 ms.
        """
        events = [
            make_clock_sync(
                runner="bob",
                peer="alice",
                variant="",
                offset_ms=-50.0,
                ts_offset_ms=0,
            ),
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
                offset_ms=250,
            ),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = deliveries_to_records(deliveries)
        assert len(records) == 1
        rec = records[0]
        # Raw latency would have been 150 ms; corrected is ~100 ms.
        assert abs(rec.latency_ms - 100.0) < 0.01
        assert rec.offset_applied is True
