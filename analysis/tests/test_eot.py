"""Tests for end-of-test (EOT) wiring in the analysis pipeline.

Covers:

* ``parse.project_line`` projection of ``eot_sent``, ``eot_received``,
  and ``eot_timeout`` events into the columnar shard schema.
* Operate-window scoping in ``performance``: per-writer windows
  bounded by ``eot_sent_ts`` when present, falling back to
  ``silent_start`` for legacy logs.
* The new ``late_receives`` metric: counts post-EOT pre-silent
  receives, ``None`` for legacy logs.

See ``metak-shared/api-contracts/eot-protocol.md`` for the contract.
"""

from __future__ import annotations

import json

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy
from parse import project_line
from performance import performance_for_group
from schema import KNOWN_EVENTS, SHARD_SCHEMA
from tables import format_performance_table


def _perf(events: list[dict], variant: str = "test-variant", run: str = "run01"):
    """Run the performance pipeline on a synthetic event list."""
    lazy = events_to_lazy(events)
    deliveries = correlate_lazy(lazy).collect()
    return performance_for_group(lazy, deliveries, variant, run)


class TestEotSchema:
    def test_eot_events_in_known_events(self) -> None:
        assert "eot_sent" in KNOWN_EVENTS
        assert "eot_received" in KNOWN_EVENTS
        assert "eot_timeout" in KNOWN_EVENTS

    def test_eot_columns_in_schema(self) -> None:
        assert "eot_id" in SHARD_SCHEMA
        assert "eot_missing" in SHARD_SCHEMA
        assert "wait_ms" in SHARD_SCHEMA


class TestEotParse:
    def test_eot_sent_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.000Z",
                "variant": "v",
                "runner": "alice",
                "run": "r",
                "event": "eot_sent",
                "eot_id": 1234567890123456789,
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["event"] == "eot_sent"
        assert row["eot_id"] == 1234567890123456789
        assert row["eot_missing"] is None
        assert row["wait_ms"] is None
        # All schema columns present.
        assert set(row.keys()) == set(SHARD_SCHEMA.keys())

    def test_eot_received_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.000Z",
                "variant": "v",
                "runner": "bob",
                "run": "r",
                "event": "eot_received",
                "writer": "alice",
                "eot_id": 42,
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["event"] == "eot_received"
        assert row["writer"] == "alice"
        assert row["eot_id"] == 42

    def test_eot_timeout_event_round_trip(self) -> None:
        """``eot_timeout.missing`` is stored as JSON-string in the column.

        Round-tripping the JSON-encoded ``eot_missing`` column should
        give back the original list of peer names.
        """
        missing = ["bob", "carol", "dave"]
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:55.000Z",
                "variant": "v",
                "runner": "alice",
                "run": "r",
                "event": "eot_timeout",
                "missing": missing,
                "wait_ms": 5000,
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["event"] == "eot_timeout"
        assert row["wait_ms"] == 5000
        assert row["eot_missing"] is not None
        decoded = json.loads(row["eot_missing"])
        assert decoded == missing

    def test_eot_timeout_empty_missing(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:55.000Z",
                "variant": "v",
                "runner": "alice",
                "run": "r",
                "event": "eot_timeout",
                "missing": [],
                "wait_ms": 100,
            }
        )
        row = project_line(line)
        assert row is not None
        assert json.loads(row["eot_missing"]) == []
        assert row["wait_ms"] == 100

    def test_eot_id_zero_preserved(self) -> None:
        """``eot_id == 0`` is the no-op default for variants without
        EOT support; the column must preserve it (not coerce to None)."""
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.000Z",
                "variant": "v",
                "runner": "alice",
                "run": "r",
                "event": "eot_sent",
                "eot_id": 0,
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["eot_id"] == 0


def _baseline_two_runner_events(
    *,
    write_count: int = 5,
    receive_offset_ms: int = 5,
    extra_alice: list[dict] | None = None,
    extra_bob: list[dict] | None = None,
    silent_offset_ms: int = 2000,
) -> list[dict]:
    """Synthetic two-runner event list shared by the operate-window tests.

    Alice writes ``seq=1..write_count`` at ``offset_ms 1000+i``.
    Bob receives them at ``offset_ms 1000+i+receive_offset_ms``.
    A ``phase=operate`` at offset 1000 and a ``phase=silent`` at
    ``silent_offset_ms`` bracket the operate window. Extra events
    can be appended with ``extra_alice`` / ``extra_bob``.
    """
    alice: list[dict] = [
        make_event("phase", runner="alice", phase="operate", offset_ms=1000),
    ]
    bob: list[dict] = [
        make_event("phase", runner="bob", phase="operate", offset_ms=1000),
    ]
    for i in range(1, write_count + 1):
        alice.append(
            make_event(
                "write",
                runner="alice",
                seq=i,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1000 + i,
            )
        )
        bob.append(
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=i,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1000 + i + receive_offset_ms,
            )
        )
    if extra_alice:
        alice.extend(extra_alice)
    if extra_bob:
        bob.extend(extra_bob)
    alice.append(
        make_event("phase", runner="alice", phase="silent", offset_ms=silent_offset_ms)
    )
    bob.append(
        make_event("phase", runner="bob", phase="silent", offset_ms=silent_offset_ms)
    )
    return alice + bob


class TestOperateWindowScoping:
    def test_legacy_no_eot_falls_back_to_silent_start(self) -> None:
        """Without any ``eot_sent`` events, the operate window ends at
        ``silent_start`` (pre-E12 behaviour). All 5 writes land before
        silent_start so the loss% stays 0 and late_receives is ``None``.
        """
        events = _baseline_two_runner_events(write_count=5)
        r = _perf(events)
        # All 5 writes / 5 receives count toward loss%.
        assert r.loss_pct == 0.0
        # No EOT means no meaningful late metric.
        assert r.late_receives is None

    def test_eot_present_bounds_window_at_eot_sent(self) -> None:
        """With an ``eot_sent`` from alice between the last receive and
        ``silent_start``, the operate window ends at ``eot_sent.ts``.
        Loss% should still be 0 (all receives landed before EOT) and
        late_receives should be 0.
        """
        # Alice's last receive lands at ts=1010 (write at 1005,
        # receive_offset_ms=5). EOT is at 1500, silent at 2000.
        events = _baseline_two_runner_events(
            write_count=5,
            receive_offset_ms=5,
            extra_alice=[
                make_event(
                    "eot_sent",
                    runner="alice",
                    eot_id=42,
                    offset_ms=1500,
                )
            ],
            silent_offset_ms=2000,
        )
        r = _perf(events)
        assert r.loss_pct == 0.0
        assert r.late_receives == 0

    def test_late_receives_counted_between_eot_and_silent(self) -> None:
        """Receives that arrive AFTER alice's eot_sent_ts but at or
        before silent_start are counted as late_receives. They must NOT
        contribute to the loss% denominator's matched receives -- a
        write+receive that straddles eot_sent_ts is considered
        unmatched in the operate window (write within, receive late).
        """
        # alice writes seq 1..3 at offset 1001..1003 (within window).
        # alice writes seq 4..5 at offset 1100..1101 -- ALSO within
        # the operate window because eot_sent is at 1200.
        # bob receives seq 1..3 at 1006..1008 (within window).
        # bob receives seq 4..5 at 1300..1301 -- AFTER alice's eot_sent
        # at 1200 but before silent at 2000 -> late_receives.
        alice = [
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
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1002,
            ),
            make_event(
                "write",
                runner="alice",
                seq=3,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1003,
            ),
            make_event(
                "write",
                runner="alice",
                seq=4,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1100,
            ),
            make_event(
                "write",
                runner="alice",
                seq=5,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1101,
            ),
            make_event("eot_sent", runner="alice", eot_id=99, offset_ms=1200),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        bob = [
            make_event("phase", runner="bob", phase="operate", offset_ms=1000),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1006,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1007,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=3,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1008,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=4,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1300,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=5,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1301,
            ),
            make_event(
                "eot_received",
                runner="bob",
                writer="alice",
                eot_id=99,
                offset_ms=1250,
            ),
            make_event("phase", runner="bob", phase="silent", offset_ms=2000),
        ]
        r = _perf(alice + bob)
        # Two receives landed in (eot_sent_ts, silent_start].
        assert r.late_receives == 2
        # 5 writes within the operate window, 3 in-window receives ->
        # loss% = (1 - 3/5) * 100 = 40%.
        assert abs(r.loss_pct - 40.0) < 1e-6

    def test_receives_before_eot_not_counted_as_late(self) -> None:
        """Receives strictly before alice's eot_sent_ts are NOT late."""
        events = _baseline_two_runner_events(
            write_count=3,
            receive_offset_ms=5,
            extra_alice=[
                make_event("eot_sent", runner="alice", eot_id=7, offset_ms=1500)
            ],
            silent_offset_ms=2000,
        )
        r = _perf(events)
        assert r.late_receives == 0
        assert r.loss_pct == 0.0

    def test_late_receives_per_writer_aggregated(self) -> None:
        """Two writers, each with their own EOT: late_receives sums
        across both writer windows."""
        # alice: writes seq=1 in window, bob receives at 1300 (late).
        # bob: writes seq=2 in window, alice receives at 1400 (late).
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1010,
            ),
            make_event("eot_sent", runner="alice", eot_id=1, offset_ms=1200),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
            # alice receives bob's write LATE (after bob's eot at 1250).
            make_event(
                "receive",
                runner="alice",
                writer="bob",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1400,
            ),
            make_event("phase", runner="bob", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="bob",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1010,
            ),
            make_event("eot_sent", runner="bob", eot_id=2, offset_ms=1250),
            # bob receives alice's write LATE (after alice's eot at 1200).
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1300,
            ),
            make_event("phase", runner="bob", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        # Both deliveries are late (one per writer window).
        assert r.late_receives == 2

    def test_late_receives_table_dash_for_legacy(self) -> None:
        """Table renders ``-`` when ``late_receives`` is ``None``."""
        events = _baseline_two_runner_events(write_count=2)
        r = _perf(events)
        assert r.late_receives is None
        out = format_performance_table([r])
        # The Late column header is present.
        assert "Late" in out
        # And the row has a "-" for the late count (followed by a
        # newline or end of string).
        assert " -\n" in out or out.rstrip().endswith(" -")

    def test_late_receives_table_count_with_eot(self) -> None:
        events = _baseline_two_runner_events(
            write_count=2,
            receive_offset_ms=5,
            extra_alice=[
                make_event("eot_sent", runner="alice", eot_id=1, offset_ms=1500)
            ],
            silent_offset_ms=2000,
        )
        r = _perf(events)
        assert r.late_receives == 0
        out = format_performance_table([r])
        assert "Late" in out


class TestEotTimeoutParsing:
    def test_eot_timeout_in_lazy_frame(self) -> None:
        """``eot_timeout`` events round-trip through the lazy-frame
        construction used by the analysis pipeline."""
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "eot_timeout",
                runner="alice",
                missing=["bob", "carol"],
                wait_ms=5000,
                offset_ms=1500,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        lazy = events_to_lazy(events)
        df = lazy.collect()
        timeout_rows = df.filter(df.get_column("event") == "eot_timeout")
        assert timeout_rows.height == 1
        row = timeout_rows.row(0, named=True)
        assert row["wait_ms"] == 5000
        assert json.loads(row["eot_missing"]) == ["bob", "carol"]
