"""Integration tests using real JSONL log files from logs/."""

from __future__ import annotations

from pathlib import Path

import pytest

from cache import load_and_update
from helpers import TWO_RUNNER_LOGS
from correlate import correlate
from integrity import verify_integrity
from performance import compute_performance
from tables import format_integrity_table, format_performance_table


# Skip if real log files are not available
pytestmark = pytest.mark.skipif(
    not TWO_RUNNER_LOGS.is_dir()
    or not list(TWO_RUNNER_LOGS.glob("*.jsonl")),
    reason="Real log files not available at logs/",
)


class TestRealLogParsing:
    def test_loads_all_files(self, tmp_path: Path) -> None:
        """Copy real logs to tmp and verify caching pipeline loads them."""
        import shutil

        jsonl_files = list(TWO_RUNNER_LOGS.glob("*.jsonl"))
        for f in jsonl_files:
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        assert len(cache.files) == len(jsonl_files)

        events = cache.all_events()
        assert len(events) > 0

    def test_event_types(self, tmp_path: Path) -> None:
        """Verify expected event types are present."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        event_types = {ev.event for ev in events}
        assert "phase" in event_types
        assert "connected" in event_types
        assert "write" in event_types


class TestRealLogCorrelation:
    def test_correlates_writes_and_receives(self, tmp_path: Path) -> None:
        """Verify correlation produces delivery records from real data."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)

        assert len(records) > 0
        # Every record must have valid fields
        for r in records:
            assert r.writer
            assert r.receiver
            assert r.seq > 0
            assert r.path


class TestRealLogIntegrity:
    def test_integrity_produces_results(self, tmp_path: Path) -> None:
        """Integrity verification runs without errors on real data."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)
        results = verify_integrity(events, records)

        assert len(results) > 0
        for r in results:
            assert 0.0 <= r.delivery_pct <= 100.0
            assert r.out_of_order >= 0
            assert r.duplicates >= 0


class TestRealLogPerformance:
    def test_performance_produces_results(self, tmp_path: Path) -> None:
        """Performance analysis runs and produces metrics on real data."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)
        results = compute_performance(events, records)

        assert len(results) > 0
        for r in results:
            assert r.writes_per_sec >= 0
            assert r.latency_p50_ms >= 0 or r.latency_p50_ms >= -1.0
            assert r.jitter_p95_ms >= 0


class TestRealLogTables:
    def test_end_to_end_tables(self, tmp_path: Path) -> None:
        """Full pipeline: parse, correlate, verify, compute, format."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)

        integrity_results = verify_integrity(events, records)
        performance_results = compute_performance(events, records)

        integrity_table = format_integrity_table(integrity_results)
        performance_table = format_performance_table(performance_results)

        assert "Integrity Report" in integrity_table
        assert "Performance Report" in performance_table
        assert "Jitter p95" in performance_table

        # Print for manual inspection
        print()
        print(integrity_table)
        print(performance_table)
