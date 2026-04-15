"""Integration tests using real JSONL log files from two-runner-logs/."""

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
    not TWO_RUNNER_LOGS.is_dir(),
    reason="Real log files not available at two-runner-logs/",
)


class TestRealLogParsing:
    def test_loads_all_files(self, tmp_path: Path) -> None:
        """Copy real logs to tmp and verify caching pipeline loads them."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        assert len(cache.files) == 3  # alice, bob, dummy

        events = cache.all_events()
        # 540 lines * 3 files = 1620 total
        assert len(events) == 1620

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
        assert "receive" in event_types
        assert "resource" in event_types


class TestRealLogCorrelation:
    def test_correlates_custom_udp(self, tmp_path: Path) -> None:
        """custom-udp: alice writes, bob receives and vice versa."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("custom-udp-*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)

        # Each runner writes 255 events, each received by the other
        # alice->bob: 255, bob->alice: 255
        assert len(records) == 510

        writers = {r.writer for r in records}
        receivers = {r.receiver for r in records}
        assert writers == {"alice", "bob"}
        assert receivers == {"alice", "bob"}

        # Latencies should be close to zero; small negative values are
        # expected due to clock granularity on same-machine runs.
        for r in records:
            assert r.latency_ms >= -1.0, f"Latency too negative: {r.latency_ms}ms"

    def test_correlates_dummy_loopback(self, tmp_path: Path) -> None:
        """dummy: alice writes and receives her own data (loopback)."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("dummy-*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)

        # 255 writes, 255 receives (all from alice to alice)
        assert len(records) == 255

        for r in records:
            assert r.writer == "alice"
            assert r.receiver == "alice"
            assert r.latency_ms >= 0


class TestRealLogIntegrity:
    def test_custom_udp_integrity(self, tmp_path: Path) -> None:
        """custom-udp should have 100% delivery, no errors."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("custom-udp-*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)
        results = verify_integrity(events, records)

        # Two pairs: alice->bob and bob->alice
        assert len(results) == 2

        for r in results:
            assert r.delivery_pct == 100.0
            assert r.out_of_order == 0
            assert r.duplicates == 0
            assert not r.completeness_error
            assert not r.ordering_error

    def test_dummy_integrity(self, tmp_path: Path) -> None:
        """dummy loopback should have 100% delivery."""
        import shutil

        for f in TWO_RUNNER_LOGS.glob("dummy-*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)
        results = verify_integrity(events, records)

        assert len(results) == 1
        r = results[0]
        assert r.delivery_pct == 100.0
        assert r.writer == "alice"
        assert r.receiver == "alice"


class TestRealLogPerformance:
    def test_custom_udp_performance(self, tmp_path: Path) -> None:
        import shutil

        for f in TWO_RUNNER_LOGS.glob("custom-udp-*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)
        results = compute_performance(events, records)

        assert len(results) == 1
        r = results[0]
        assert r.variant == "custom-udp"
        assert r.connect_mean_ms > 0
        assert r.latency_p50_ms >= 0
        assert r.writes_per_sec > 0
        assert r.loss_pct == 0.0
        assert len(r.resources) == 2  # alice and bob

    def test_dummy_performance(self, tmp_path: Path) -> None:
        import shutil

        for f in TWO_RUNNER_LOGS.glob("dummy-*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        cache = load_and_update(tmp_path)
        events = cache.all_events()
        records = correlate(events)
        results = compute_performance(events, records)

        assert len(results) == 1
        r = results[0]
        assert r.variant == "dummy"
        assert r.latency_p50_ms >= 0  # near-zero for loopback


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

        # Tables should contain expected content
        assert "Integrity Report" in integrity_table
        assert "Performance Report" in performance_table
        assert "custom-udp" in integrity_table
        assert "dummy" in integrity_table
        assert "custom-udp" in performance_table
        assert "dummy" in performance_table

        # Print for manual inspection
        print()
        print(integrity_table)
        print(performance_table)
