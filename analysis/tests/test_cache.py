"""Tests for the per-shard Parquet caching pipeline."""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from unittest.mock import patch

import polars as pl

from cache import (
    CACHE_DIRNAME,
    GLOBAL_SENTINEL_NAME,
    LEGACY_PICKLE_NAME,
    _is_clocksync_shard,
    cache_dir,
    discover_groups,
    scan_shards,
    shard_paths,
    update_cache,
)
from schema import SCHEMA_VERSION


class TestUpdateCache:
    def test_builds_shards_for_each_jsonl(self, tmp_logs: Path) -> None:
        metas = update_cache(tmp_logs)
        cdir = cache_dir(tmp_logs)
        # Two source JSONLs in tmp_logs -> two shards + sidecars + sentinel.
        assert cdir.is_dir()
        parquet_files = list(cdir.glob("*.parquet"))
        meta_files = list(cdir.glob("*.meta.json"))
        assert len(parquet_files) == 2
        assert len(meta_files) == 2
        assert (cdir / GLOBAL_SENTINEL_NAME).exists()
        assert len(metas) == 2

    def test_sidecar_round_trip(self, tmp_logs: Path) -> None:
        metas = update_cache(tmp_logs)
        for stem, meta in metas.items():
            _, meta_path = shard_paths(tmp_logs, stem)
            with open(meta_path, encoding="utf-8") as f:
                obj = json.load(f)
            assert obj["schema_version"] == SCHEMA_VERSION
            assert obj["row_count"] == meta.row_count
            assert obj["mtime"] == meta.mtime

    def test_no_rebuild_on_unchanged(self, tmp_logs: Path) -> None:
        metas1 = update_cache(tmp_logs)
        # Touch shard files to detect a rebuild via mtime
        cdir = cache_dir(tmp_logs)
        shard_mtimes = {p.name: p.stat().st_mtime for p in cdir.glob("*.parquet")}
        # Sleep just enough to ensure any new write would have a higher mtime.
        time.sleep(0.05)

        metas2 = update_cache(tmp_logs)
        for p in cdir.glob("*.parquet"):
            assert shard_mtimes[p.name] == p.stat().st_mtime, (
                f"shard {p.name} was rewritten"
            )
        assert metas1 == metas2

    def test_rebuild_on_jsonl_mtime_drift(self, tmp_logs: Path) -> None:
        metas1 = update_cache(tmp_logs)
        # Bump mtime of one of the JSONL files into the future.
        jsonls = sorted(tmp_logs.glob("*.jsonl"))
        target = jsonls[0]
        future = time.time() + 60
        os.utime(target, (future, future))

        metas2 = update_cache(tmp_logs)
        # The targeted shard's meta must reflect the new mtime
        assert metas2[target.stem].mtime != metas1[target.stem].mtime

    def test_rebuild_on_schema_version_mismatch(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)
        cdir = cache_dir(tmp_logs)
        # Tamper with the global sentinel.
        with open(cdir / GLOBAL_SENTINEL_NAME, "w", encoding="utf-8") as f:
            json.dump({"schema_version": "0-old"}, f)

        # Cache should be wiped + rebuilt.
        update_cache(tmp_logs)
        with open(cdir / GLOBAL_SENTINEL_NAME, encoding="utf-8") as f:
            assert json.load(f)["schema_version"] == SCHEMA_VERSION

    def test_rebuild_on_meta_schema_mismatch(self, tmp_logs: Path) -> None:
        metas1 = update_cache(tmp_logs)
        # Corrupt one sidecar's schema_version.
        stem = next(iter(metas1.keys()))
        _, meta_path = shard_paths(tmp_logs, stem)
        with open(meta_path, "w", encoding="utf-8") as f:
            json.dump(
                {
                    "mtime": 0.0,
                    "row_count": 0,
                    "schema_version": "different",
                },
                f,
            )

        metas2 = update_cache(tmp_logs)
        assert metas2[stem].schema_version == SCHEMA_VERSION
        assert metas2[stem].row_count > 0

    def test_orphan_shards_removed(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)
        cdir = cache_dir(tmp_logs)

        # Create a fake orphan shard with no matching JSONL.
        orphan = cdir / "orphan-stem.parquet"
        pl.DataFrame({"x": [1]}).write_parquet(orphan)
        orphan_meta = cdir / "orphan-stem.meta.json"
        with open(orphan_meta, "w", encoding="utf-8") as f:
            json.dump(
                {"mtime": 0, "row_count": 0, "schema_version": SCHEMA_VERSION},
                f,
            )

        update_cache(tmp_logs)
        assert not orphan.exists()
        assert not orphan_meta.exists()

    def test_clear_removes_cache_dir(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)
        cdir = cache_dir(tmp_logs)
        assert cdir.is_dir()
        assert any(cdir.iterdir())

        update_cache(tmp_logs, clear=True)
        # After clear+rebuild, the directory exists with fresh contents.
        assert cdir.is_dir()
        # Sentinel must be the current version.
        with open(cdir / GLOBAL_SENTINEL_NAME, encoding="utf-8") as f:
            assert json.load(f)["schema_version"] == SCHEMA_VERSION

    def test_legacy_pickle_removed(self, tmp_logs: Path) -> None:
        # Drop a stub legacy pickle.
        legacy = tmp_logs / LEGACY_PICKLE_NAME
        legacy.write_bytes(b"\x80\x05K\x00.")
        assert legacy.exists()

        update_cache(tmp_logs)
        assert not legacy.exists()


class TestScanShards:
    def test_scan_returns_lazy_frame(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)
        lazy = scan_shards(tmp_logs)
        assert isinstance(lazy, pl.LazyFrame)
        df = lazy.collect()
        assert df.height > 0
        assert "ts" in df.columns
        assert "event" in df.columns

    def test_event_types_present(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)
        lazy = scan_shards(tmp_logs)
        events = (
            lazy.select(pl.col("event").cast(pl.Utf8))
            .unique()
            .collect()
            .get_column("event")
            .to_list()
        )
        assert "phase" in events
        assert "write" in events
        assert "receive" in events


class TestCacheDir:
    def test_cache_dir_path(self, tmp_path: Path) -> None:
        assert cache_dir(tmp_path) == tmp_path / CACHE_DIRNAME


class TestWarmCacheShortCircuit:
    """Verify the global-sentinel index short-circuits the warm path.

    On a cold run the per-sidecar JSON sidecars are read once. On the
    immediate next warm run the sentinel index covers every stem, so
    ``_read_meta`` should NOT be called -- this is the optimisation
    that takes the 40 GB warm wall-time below the 30 s target.
    """

    def test_warm_run_skips_per_sidecar_reads(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)

        # Patch the per-sidecar reader and confirm the second update
        # never reaches it because the global index already covers
        # every stem.
        with patch("cache._read_meta") as mocked:
            mocked.return_value = None
            update_cache(tmp_logs)
            assert mocked.call_count == 0, (
                "warm update_cache must not open per-shard sidecars when "
                "the global index already covers every stem"
            )

    def test_global_sentinel_carries_shard_index(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)
        sentinel_path = cache_dir(tmp_logs) / GLOBAL_SENTINEL_NAME
        with open(sentinel_path, encoding="utf-8") as f:
            payload = json.load(f)
        assert payload["schema_version"] == SCHEMA_VERSION
        assert "shards" in payload, "warm-path index missing from sentinel"
        # Each tmp_logs JSONL must have a shard index entry with
        # variant + run filled in.
        jsonl_stems = {p.stem for p in tmp_logs.glob("*.jsonl")}
        assert set(payload["shards"].keys()) == jsonl_stems
        for entry in payload["shards"].values():
            assert entry["schema_version"] == SCHEMA_VERSION
            assert entry.get("variant") is not None
            assert entry.get("run") is not None

    def test_legacy_sentinel_without_index_still_works(self, tmp_logs: Path) -> None:
        """Caches built before T11.2 only carry ``schema_version`` in the
        sentinel; ``update_cache`` must fall back to the per-sidecar
        read path without forcing a rebuild."""
        update_cache(tmp_logs)
        cdir = cache_dir(tmp_logs)
        # Rewrite the sentinel to the legacy version-only shape.
        with open(cdir / GLOBAL_SENTINEL_NAME, "w", encoding="utf-8") as f:
            json.dump({"schema_version": SCHEMA_VERSION}, f)

        # Capture pre-call shard mtimes.
        shard_mtimes = {p.name: p.stat().st_mtime for p in cdir.glob("*.parquet")}

        # On the next call, sidecars must be consulted (no rebuild).
        with patch("cache._read_meta", wraps=__import__("cache")._read_meta) as mocked:
            update_cache(tmp_logs)
            assert mocked.call_count >= len(shard_mtimes), (
                "legacy sentinel path must fall back to per-sidecar reads"
            )

        # No shard was rewritten.
        for p in cdir.glob("*.parquet"):
            assert shard_mtimes[p.name] == p.stat().st_mtime, (
                f"shard {p.name} unexpectedly rebuilt under legacy sentinel"
            )


class TestDiscoverGroupsIndexed:
    """``discover_groups`` should read ``(variant, run)`` from the
    sentinel index rather than opening a Parquet first row per shard."""

    def test_no_parquet_reads_when_index_complete(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)

        with patch("cache.pl.read_parquet") as mocked:
            groups = discover_groups(tmp_logs)
            assert mocked.call_count == 0, (
                "discover_groups must not open Parquet shards when the "
                "global index already carries (variant, run) for every stem"
            )
        # Sanity: at least one group surfaced.
        assert len(groups) >= 1

    def test_falls_back_to_parquet_when_index_lacks_entry(self, tmp_logs: Path) -> None:
        update_cache(tmp_logs)
        # Strip the index from the sentinel so the function must
        # recover (variant, run) by reading the Parquet first row.
        cdir = cache_dir(tmp_logs)
        with open(cdir / GLOBAL_SENTINEL_NAME, "w", encoding="utf-8") as f:
            json.dump({"schema_version": SCHEMA_VERSION}, f)

        with patch(
            "cache.pl.read_parquet",
            wraps=pl.read_parquet,
        ) as mocked:
            groups = discover_groups(tmp_logs)
            assert mocked.call_count >= 1
        assert len(groups) >= 1


class TestClockSyncShardHandling:
    """Cache treatment of clock-sync sibling logs (E8).

    A ``<runner>-clock-sync-<run>.jsonl`` file is picked up by the
    same ``*.jsonl`` glob as the variant logs. Its shard's first row
    is a ``clock_sync`` event with ``variant=""``; ``discover_groups``
    must broadcast it into every variant group of the same run rather
    than create a stand-alone ``("", run)`` group.
    """

    def _write_clocksync_run(self, tmp_path: Path) -> Path:
        """Build a synthetic two-runner run with a clock-sync sibling file."""
        from helpers import _ts, write_jsonl

        run = "csync-run01"
        # Variant log for runner alice with one write.
        alice_variant = [
            {
                "ts": _ts(0),
                "variant": "test-variant",
                "runner": "alice",
                "run": run,
                "event": "phase",
                "phase": "operate",
            },
            {
                "ts": _ts(100),
                "variant": "test-variant",
                "runner": "alice",
                "run": run,
                "event": "write",
                "seq": 1,
                "path": "/k",
                "qos": 1,
                "bytes": 8,
            },
        ]
        # Variant log for runner bob with one matching receive.
        bob_variant = [
            {
                "ts": _ts(0),
                "variant": "test-variant",
                "runner": "bob",
                "run": run,
                "event": "phase",
                "phase": "operate",
            },
            {
                "ts": _ts(200),
                "variant": "test-variant",
                "runner": "bob",
                "run": run,
                "event": "receive",
                "writer": "alice",
                "seq": 1,
                "path": "/k",
                "qos": 1,
                "bytes": 8,
            },
        ]
        # Bob's clock-sync sibling log: initial sync + per-variant resync.
        bob_clocksync = [
            {
                "ts": _ts(-50),
                "variant": "",
                "runner": "bob",
                "run": run,
                "event": "clock_sync",
                "peer": "alice",
                "offset_ms": 0.0,
                "rtt_ms": 0.4,
                "samples": 32,
                "min_rtt_ms": 0.4,
                "max_rtt_ms": 0.6,
            },
            {
                "ts": _ts(50),
                "variant": "test-variant",
                "runner": "bob",
                "run": run,
                "event": "clock_sync",
                "peer": "alice",
                "offset_ms": 0.0,
                "rtt_ms": 0.4,
            },
        ]

        write_jsonl(tmp_path / f"test-variant-alice-{run}.jsonl", alice_variant)
        write_jsonl(tmp_path / f"test-variant-bob-{run}.jsonl", bob_variant)
        write_jsonl(tmp_path / f"bob-clock-sync-{run}.jsonl", bob_clocksync)
        return tmp_path

    def test_clocksync_log_picked_up(self, tmp_path: Path) -> None:
        """``update_cache`` builds a shard for the clock-sync sibling file."""
        logs = self._write_clocksync_run(tmp_path)
        metas = update_cache(logs)
        # Three source JSONLs -> three shards.
        assert any(stem.endswith("clock-sync-csync-run01") for stem in metas)

    def test_clocksync_shard_broadcast_into_variant_group(self, tmp_path: Path) -> None:
        """Clock-sync shards appear in every variant group sharing the run."""
        logs = self._write_clocksync_run(tmp_path)
        update_cache(logs)
        groups = discover_groups(logs)
        # Only one (variant, run) group surfaces -- the clock-sync shard
        # is NOT a separate group.
        assert len(groups) == 1
        variant, run, paths = groups[0]
        assert variant == "test-variant"
        assert run == "csync-run01"
        # The variant group includes the two variant shards plus the
        # clock-sync shard.
        stems = sorted(p.stem for p in paths)
        assert any("clock-sync" in s for s in stems)
        assert sum(1 for s in stems if "clock-sync" not in s) == 2


class TestIsClocksyncShard:
    """Unit tests for ``_is_clocksync_shard``.

    Both checks (event-name match AND empty-variant fallback) are
    exercised here so future regressions surface immediately. See the
    docstring on ``_is_clocksync_shard`` for why both are needed.
    """

    @staticmethod
    def _write_shard(path: Path, *, event: str, variant: str) -> None:
        """Write a one-row Parquet shard matching the cache's columnar layout.

        Uses ``events_to_lazy`` so the ``SHARD_SCHEMA`` typing (including
        the ``Datetime`` column for ``ts``) is handled via the same
        ``parse.project_line`` path the production cache uses.
        """
        from helpers import events_to_lazy, make_event

        ev = make_event(event=event, variant=variant)
        lf = events_to_lazy([ev])
        lf.collect().write_parquet(path, compression="snappy")

    def test_clock_sync_event_returns_true(self, tmp_path: Path) -> None:
        shard = tmp_path / "alice-clock-sync-run01.parquet"
        self._write_shard(shard, event="clock_sync", variant="")
        assert _is_clocksync_shard(shard) is True

    def test_clock_sync_sample_event_returns_true(self, tmp_path: Path) -> None:
        # The debug clock-sync shards (T-analysis.1) emit
        # ``clock_sync_sample`` rather than ``clock_sync``.
        shard = tmp_path / "alice-clock-sync-debug-run01.parquet"
        self._write_shard(shard, event="clock_sync_sample", variant="")
        assert _is_clocksync_shard(shard) is True

    def test_regular_write_event_returns_false(self, tmp_path: Path) -> None:
        shard = tmp_path / "test-variant-alice-run01.parquet"
        self._write_shard(shard, event="write", variant="test-variant")
        assert _is_clocksync_shard(shard) is False

    def test_empty_variant_returns_true(self, tmp_path: Path) -> None:
        # Defence-in-depth: even if a future broadcast log uses an
        # event name we don't yet recognise, an empty variant still
        # marks it as a sibling log so it never becomes its own group.
        shard = tmp_path / "alice-broadcast-run01.parquet"
        self._write_shard(shard, event="some_future_event", variant="")
        assert _is_clocksync_shard(shard) is True
