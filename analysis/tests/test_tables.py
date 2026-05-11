"""Tests for the CLI summary table formatting (T11.5 column ordering)."""

from __future__ import annotations

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy
from performance import performance_for_group
from tables import format_performance_table


def _perf(events: list[dict], variant: str = "test-variant", run: str = "run01"):
    lazy = events_to_lazy(events)
    deliveries = correlate_lazy(lazy).collect()
    return performance_for_group(lazy, deliveries, variant, run)


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
