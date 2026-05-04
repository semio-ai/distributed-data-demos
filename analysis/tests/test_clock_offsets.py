"""Tests for ``clock_offsets.build_offset_table``."""

from __future__ import annotations

from helpers import events_to_lazy, make_event

from clock_offsets import OFFSET_COLUMNS, build_offset_table


def make_clock_sync(
    runner: str = "alice",
    peer: str = "bob",
    variant: str = "",
    offset_ms: float = 0.0,
    rtt_ms: float = 0.5,
    ts_offset_ms: float = 0.0,
    **extra: object,
) -> dict:
    """Build a clock_sync JSONL event dict.

    ``ts_offset_ms`` is the millisecond shift applied to the synthetic
    timestamp generator (cf. ``helpers.make_event``'s ``offset_ms``);
    we rename it here so it does not collide with the
    JSONL ``offset_ms`` payload field.
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


class TestBuildOffsetTable:
    def test_empty_when_no_clock_sync_events(self) -> None:
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
        offsets = build_offset_table(lazy)
        assert offsets.is_empty()
        # Schema is still well-formed so callers can count on the columns.
        assert set(offsets.columns) == set(OFFSET_COLUMNS)

    def test_single_initial_sync_row(self) -> None:
        events = [
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="",
                offset_ms=50.0,
                rtt_ms=0.4,
                ts_offset_ms=100,
                samples=32,
                min_rtt_ms=0.4,
                max_rtt_ms=1.2,
            ),
        ]
        lazy = events_to_lazy(events)
        offsets = build_offset_table(lazy)
        assert offsets.height == 1
        row = offsets.row(0, named=True)
        assert row["runner"] == "alice"
        assert row["peer"] == "bob"
        assert row["variant"] == ""
        assert row["offset_ms"] == 50.0

    def test_per_variant_resync_rows(self) -> None:
        events = [
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="",
                offset_ms=50.0,
                ts_offset_ms=100,
            ),
            # Per-variant resync entry for variant "custom-udp".
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="custom-udp",
                offset_ms=51.0,
                ts_offset_ms=2000,
            ),
        ]
        lazy = events_to_lazy(events)
        offsets = build_offset_table(lazy)
        assert offsets.height == 2
        variants = sorted(offsets.get_column("variant").to_list())
        assert variants == ["", "custom-udp"]

    def test_sorted_by_runner_peer_variant_ts(self) -> None:
        events = [
            make_clock_sync(
                runner="bob",
                peer="alice",
                variant="custom-udp",
                offset_ms=-50.0,
                ts_offset_ms=200,
            ),
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="",
                offset_ms=50.0,
                ts_offset_ms=100,
            ),
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="custom-udp",
                offset_ms=51.0,
                ts_offset_ms=300,
            ),
        ]
        lazy = events_to_lazy(events)
        offsets = build_offset_table(lazy)
        runners = offsets.get_column("runner").to_list()
        assert runners == ["alice", "alice", "bob"]
        # Among alice rows, variants come in lexicographic order ("" < "custom-udp").
        alice_variants = [
            v
            for runner, v in zip(runners, offsets.get_column("variant").to_list())
            if runner == "alice"
        ]
        assert alice_variants == ["", "custom-udp"]

    def test_diagnostic_fields_ignored(self) -> None:
        """``samples``/``min_rtt_ms``/``max_rtt_ms`` are JSONL-only."""
        events = [
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="",
                offset_ms=50.0,
                samples=32,
                min_rtt_ms=0.4,
                max_rtt_ms=1.2,
            ),
        ]
        lazy = events_to_lazy(events)
        offsets = build_offset_table(lazy)
        assert set(offsets.columns) == set(OFFSET_COLUMNS)
        # No diagnostic-only fields leak into the offset table.
        for diag in ("samples", "min_rtt_ms", "max_rtt_ms"):
            assert diag not in offsets.columns

    def test_drops_rows_missing_required_fields(self) -> None:
        """A clock_sync line with no peer or no offset_ms is unusable."""
        # Valid baseline.
        events = [
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="",
                offset_ms=50.0,
                ts_offset_ms=100,
            ),
        ]
        # Missing peer.
        malformed_no_peer = make_event(
            "clock_sync",
            runner="alice",
            variant="",
            offset_ms=200,
        )
        malformed_no_peer["offset_ms"] = 99.0
        malformed_no_peer["rtt_ms"] = 0.4

        # Missing offset_ms.
        malformed_no_offset = make_event(
            "clock_sync",
            runner="alice",
            variant="",
            offset_ms=300,
        )
        malformed_no_offset["peer"] = "bob"
        malformed_no_offset["rtt_ms"] = 0.4

        events.extend([malformed_no_peer, malformed_no_offset])

        lazy = events_to_lazy(events)
        offsets = build_offset_table(lazy)
        assert offsets.height == 1
        row = offsets.row(0, named=True)
        assert row["offset_ms"] == 50.0

    def test_writes_and_receives_filtered_out(self) -> None:
        """Only ``clock_sync`` event rows make it to the offset table."""
        events = [
            make_clock_sync(
                runner="alice",
                peer="bob",
                variant="",
                offset_ms=50.0,
                ts_offset_ms=100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=200,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=210,
            ),
        ]
        lazy = events_to_lazy(events)
        offsets = build_offset_table(lazy)
        assert offsets.height == 1
        row = offsets.row(0, named=True)
        assert row["peer"] == "bob"
