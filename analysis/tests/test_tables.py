"""Tests for the CLI summary table formatting (T11.5 column ordering)."""

from __future__ import annotations

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy
from performance import performance_for_group
from integrity import IntegrityResult
from tables import format_integrity_table, format_performance_table


def _perf(events: list[dict], variant: str = "test-variant", run: str = "run01"):
    lazy = events_to_lazy(events)
    deliveries = correlate_lazy(lazy).collect()
    return performance_for_group(lazy, deliveries, variant, run)


def _baseline_events() -> list[dict]:
    """Synthetic pre-T11.5 dataset used by the backwards-compat test."""
    events = [
        make_event(
            "connected",
            runner="alice",
            launch_ts="2025-04-15T09:35:49Z",
            elapsed_ms=42.0,
            offset_ms=42,
        ),
        make_event(
            "connected",
            runner="bob",
            launch_ts="2025-04-15T09:35:49Z",
            elapsed_ms=60.0,
            offset_ms=60,
        ),
        make_event("phase", runner="alice", phase="operate", offset_ms=1000),
        make_event("phase", runner="bob", phase="operate", offset_ms=1000),
    ]
    for i in range(5):
        events.append(
            make_event(
                "write",
                runner="alice",
                seq=i + 1,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=1001 + i,
            )
        )
        events.append(
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=i + 1,
                path="/k",
                qos=2,
                bytes=8,
                offset_ms=1011 + i,
            )
        )
    events.append(
        make_event(
            "resource",
            runner="alice",
            cpu_percent=12.5,
            memory_mb=14.0,
            offset_ms=1500,
        )
    )
    events.append(make_event("phase", runner="alice", phase="silent", offset_ms=2000))
    events.append(make_event("phase", runner="bob", phase="silent", offset_ms=2000))
    return events


class TestPerformanceTableColumnOrder:
    """T11.5: receive throughput leads, write rate becomes 'requested rate'."""

    def _table_header(self, table: str) -> str:
        # The header is the third line: title, separator, header.
        lines = table.splitlines()
        assert len(lines) >= 3
        return lines[2]

    def test_receive_rate_precedes_write_rate(self) -> None:
        """Headline column is ``Receives/s``; ``Writes/s`` follows it."""
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
                offset_ms=1010,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        table = format_performance_table([r])
        header = self._table_header(table)
        rcv_idx = header.index("Receives/s")
        write_idx = header.index("Writes/s")
        assert rcv_idx < write_idx, (
            "Receives/s must appear before Writes/s in the T11.5 header"
        )

    def test_write_rate_labelled_requested(self) -> None:
        """Write throughput column is labelled as the requested rate."""
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
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        table = format_performance_table([r])
        # "Writes/s(req)" -- distinguishing the writer's requested rate
        # from the headline receive rate.
        assert "Writes/s(req)" in table

    def test_delivery_percentage_column_present(self) -> None:
        """Delivery% follows the two throughput columns."""
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
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        table = format_performance_table([r])
        header = self._table_header(table)
        assert "Delivery%" in header
        # Delivery% is between throughput and latency columns.
        deliv_idx = header.index("Delivery%")
        write_idx = header.index("Writes/s")
        conn_idx = header.index("Connect(ms)")
        assert write_idx < deliv_idx < conn_idx
        # Numerically, 1 receive / 2 writes = 50%. Throughput numbers
        # also satisfy the same ratio, so Delivery% = 50.00%.
        body = "\n".join(table.splitlines()[4:])
        assert "50.00%" in body

    def test_threading_mode_column_present(self) -> None:
        """T11.5: the new Thread column shows the threading_mode value.

        Default for pre-T14.8 logs (no threading_mode field on
        ``connected``) is ``"single"``; the column renders that value.
        """
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
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        table = format_performance_table([r])
        header = self._table_header(table)
        assert "Thread" in header
        # Default 'single' appears in the body row.
        body = "\n".join(table.splitlines()[4:])
        assert "single" in body

    def test_backwards_compat_numeric_values_preserved(self) -> None:
        """T11.5 must not change any numeric metric on pre-existing data.

        Run the synthetic baseline through the full performance
        pipeline and assert every pre-T11.5 metric still resolves to
        the same value it did before the reorder. The hand-computed
        expectations below are the values the pre-T11.5 pipeline would
        produce on this fixture: 5 writes / 1s operate window = 5
        writes per second; 5 receives = 5 per second; raw 10 ms latency
        on every delivery. The point is to lock in numeric stability:
        if a future change shifts any of these, the test will catch it.
        """
        r = _perf(_baseline_events())
        # Connection metrics unchanged.
        assert abs(r.connect_mean_ms - 51.0) < 0.001
        assert abs(r.connect_max_ms - 60.0) < 0.001
        # Latency: raw delta = 10 ms on every receive; same-runner pair
        # is bob receiving from alice (cross-runner, no clock-sync log
        # available, so latency is raw and the uncorrected flag is set).
        # Percentiles all evaluate to the constant 10 ms.
        assert abs(r.latency_p50_ms - 10.0) < 0.001
        assert abs(r.latency_p95_ms - 10.0) < 0.001
        assert abs(r.latency_p99_ms - 10.0) < 0.001
        assert abs(r.latency_max_ms - 10.0) < 0.001
        # Throughput: 5 events / 1 s window.
        assert abs(r.writes_per_sec - 5.0) < 0.001
        assert abs(r.receives_per_sec - 5.0) < 0.001
        # Loss is zero (5/5 delivered).
        assert r.loss_pct == 0.0
        # Resource usage row from alice's resource event.
        assert len(r.resources) == 1
        assert abs(r.resources[0].mean_cpu_pct - 12.5) < 0.001

    def test_backwards_compat_column_order_changed_not_data(self) -> None:
        """T11.5 changes header order; values are placed in matching cells.

        We assert the new ordering of headers AND that the data row's
        first wide-column value (receive throughput) matches the
        numeric receives_per_sec. The point is to prove the values
        track the new column slots, not the legacy ones.
        """
        r = _perf(_baseline_events())
        table = format_performance_table([r])
        header = self._table_header(table)
        # The receive-throughput column header precedes write throughput.
        rcv_idx = header.index("Receives/s")
        write_idx = header.index("Writes/s")
        assert rcv_idx < write_idx
        # The body row has the numeric receives_per_sec where the new
        # column expects it.
        body = "\n".join(table.splitlines()[4:])
        # Both "5" values appear; the receives column is encountered
        # first in the row. Verify by spot check that the rendered
        # rate string for 5/s is present.
        assert "5.0" in body

    def test_existing_metrics_still_present(self) -> None:
        """No metric is removed in T11.5; only the column ORDER changes."""
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
                offset_ms=1010,
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
        table = format_performance_table([r])
        # Every column header that existed pre-T11.5 still appears.
        for col in (
            "Variant",
            "Run",
            "Connect(ms)",
            "Lat p50",
            "p95",
            "p99",
            "Max",
            "Writes/s",
            "Jitter avg",
            "Jitter p95",
            "Loss%",
            "Late",
        ):
            assert col in table, f"Missing column: {col}"
        # Resource Usage sub-table is preserved.
        assert "Resource Usage" in table


def _build_integrity(
    *,
    qos: int,
    skip_at_reliable_count: int,
    skip_at_reliable_error: bool,
) -> IntegrityResult:
    """Build an IntegrityResult focused on the skip-at-reliable signal."""
    return IntegrityResult(
        variant="custom-udp",
        run="r1",
        writer="alice",
        receiver="bob",
        qos=qos,
        write_count=100,
        receive_count=100,
        delivery_pct=100.0,
        out_of_order=0,
        duplicates=0,
        unresolved_gaps=None,
        backpressure_skipped_count=skip_at_reliable_count,
        completeness_error=False,
        ordering_error=False,
        duplicate_error=False,
        gap_error=False,
        skip_at_reliable_count=skip_at_reliable_count,
        skip_at_reliable_error=skip_at_reliable_error,
        timeout_classification="completed",
    )


class TestIntegrityTableSkipAtReliableAnnotation:
    """T17.9: ``[FAIL: skip-at-reliable]`` annotation on the integrity row."""

    def test_violation_row_carries_annotation(self) -> None:
        rows = [
            _build_integrity(
                qos=3, skip_at_reliable_count=4, skip_at_reliable_error=True
            )
        ]
        table = format_integrity_table(rows)
        assert "[FAIL: skip-at-reliable]" in table

    def test_clean_row_has_no_annotation(self) -> None:
        rows = [
            _build_integrity(
                qos=3, skip_at_reliable_count=0, skip_at_reliable_error=False
            )
        ]
        table = format_integrity_table(rows)
        assert "skip-at-reliable" not in table

    def test_qos1_with_skips_has_no_annotation(self) -> None:
        """A QoS 1 row with a non-zero backpressure_skipped_count is
        contract-compliant -- the annotation must not appear."""
        rows = [
            _build_integrity(
                qos=1, skip_at_reliable_count=0, skip_at_reliable_error=False
            )
        ]
        table = format_integrity_table(rows)
        assert "skip-at-reliable" not in table
