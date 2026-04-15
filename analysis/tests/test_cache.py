"""Tests for the pickle caching pipeline."""

from __future__ import annotations

from pathlib import Path

from cache import CACHE_FILENAME, Cache, load_and_update, load_cache, save_cache


class TestCacheLoadSave:
    def test_empty_cache_when_no_file(self, tmp_path: Path) -> None:
        cache = load_cache(tmp_path)
        assert len(cache.files) == 0

    def test_round_trip(self, tmp_path: Path) -> None:
        cache = Cache()
        # Simulate a file entry (we won't store real events here)
        from cache import FileEntry
        from parse import Event
        from datetime import datetime, timezone

        ev = Event(
            ts=datetime(2026, 1, 1, tzinfo=timezone.utc),
            variant="v",
            runner="r",
            run="run1",
            event="phase",
            data={"phase": "connect"},
        )
        cache.files["test-file"] = FileEntry(mtime=1000.0, events=[ev])
        save_cache(tmp_path, cache)

        loaded = load_cache(tmp_path)
        assert "test-file" in loaded.files
        assert len(loaded.files["test-file"].events) == 1

    def test_clear_deletes_cache(self, tmp_path: Path) -> None:
        # Write a cache
        cache = Cache()
        save_cache(tmp_path, cache)
        assert (tmp_path / CACHE_FILENAME).exists()

        # Clear should remove it
        loaded = load_cache(tmp_path, clear=True)
        assert not (tmp_path / CACHE_FILENAME).exists()
        assert len(loaded.files) == 0


class TestCacheUpdate:
    def test_detects_new_files(self, tmp_logs: Path) -> None:
        cache = load_and_update(tmp_logs)
        # Should have parsed both JSONL files
        assert len(cache.files) == 2
        events = cache.all_events()
        assert len(events) > 0

    def test_no_reparse_when_unchanged(self, tmp_logs: Path) -> None:
        # First load
        cache1 = load_and_update(tmp_logs)
        n1 = len(cache1.all_events())

        # Second load (no changes)
        cache2 = load_and_update(tmp_logs)
        n2 = len(cache2.all_events())
        assert n1 == n2

    def test_clear_and_rebuild(self, tmp_logs: Path) -> None:
        # First load
        cache1 = load_and_update(tmp_logs)
        n1 = len(cache1.all_events())

        # Clear and rebuild
        cache2 = load_and_update(tmp_logs, clear=True)
        n2 = len(cache2.all_events())
        assert n1 == n2

    def test_removes_stale_entries(self, tmp_logs: Path) -> None:
        cache = load_and_update(tmp_logs)
        assert len(cache.files) == 2

        # Remove one file
        files = list(tmp_logs.glob("*.jsonl"))
        files[0].unlink()

        cache2 = load_and_update(tmp_logs)
        assert len(cache2.files) == 1
