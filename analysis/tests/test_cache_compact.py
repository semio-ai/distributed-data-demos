"""Cache integration tests for the compact-parquet source format (T18.4).

These exercise the path where ``update_cache`` ingests a directory
containing ``.compact.parquet`` source files (or a mix of compact +
JSONL). The cache must produce shards with the same ``SHARD_SCHEMA``
projection as the JSONL-only path so the downstream pivot / integrity
pipeline is format-transparent.
"""

from __future__ import annotations

import json
from pathlib import Path

import polars as pl

from cache import (
    cache_dir,
    discover_groups,
    discover_jsonl,
    discover_sources,
    scan_shards,
    shard_paths,
    update_cache,
)
from compact_fixture import CompactFixture
from helpers import _ts, write_jsonl
from parse import SourceFormat
from schema import SCHEMA_VERSION


# ----- discover_sources -----


class TestDiscoverSources:
    def test_canonical_pair_surfaces_compact(self, tmp_logs: Path) -> None:
        """``tmp_logs`` writes a JSONL + compact-Parquet pair per spawn.

        Post-E19 (T19.10c) the canonical layout is the per-spawn file
        pair: lifecycle-only JSONL alongside a compact-Parquet sibling
        for per-event observations. ``discover_sources`` collapses each
        stem to a single source -- the compact file wins.
        """
        sources = discover_sources(tmp_logs)
        assert set(sources.keys()) == {
            "test-variant-alice-run01",
            "test-variant-bob-run01",
        }
        for _stem, (_, fmt) in sources.items():
            assert fmt is SourceFormat.COMPACT

    def test_compact_only(self, tmp_path: Path) -> None:
        fx = CompactFixture(variant="v", runner="alice", run="run01")
        base_ns = 1_700_000_000_000_000_000
        fx.push_phase(base_ns + 0, "operate")
        fx.push_write(base_ns + 1_000_000, "/k", 1, 1, 8)
        fx.write(tmp_path / "v-alice-run01.compact.parquet")

        sources = discover_sources(tmp_path)
        assert set(sources.keys()) == {"v-alice-run01"}
        path, fmt = sources["v-alice-run01"]
        assert fmt is SourceFormat.COMPACT
        assert path.name == "v-alice-run01.compact.parquet"

    def test_compact_wins_when_both_exist(self, tmp_path: Path) -> None:
        """When both formats are present, compact wins."""
        # Drop a legacy JSONL.
        write_jsonl(
            tmp_path / "v-alice-run01.jsonl",
            [
                {
                    "ts": _ts(0),
                    "variant": "v",
                    "runner": "alice",
                    "run": "run01",
                    "event": "phase",
                    "phase": "operate",
                }
            ],
        )
        # And a compact file for the same stem.
        fx = CompactFixture(variant="v", runner="alice", run="run01")
        base_ns = 1_700_000_000_000_000_000
        fx.push_phase(base_ns + 0, "operate")
        fx.write(tmp_path / "v-alice-run01.compact.parquet")

        sources = discover_sources(tmp_path)
        assert set(sources.keys()) == {"v-alice-run01"}
        _, fmt = sources["v-alice-run01"]
        assert fmt is SourceFormat.COMPACT

    def test_back_compat_discover_jsonl(self, tmp_path: Path) -> None:
        """The legacy ``discover_jsonl`` alias still surfaces JSONLs."""
        write_jsonl(
            tmp_path / "v-alice-run01.jsonl",
            [
                {
                    "ts": _ts(0),
                    "variant": "v",
                    "runner": "alice",
                    "run": "run01",
                    "event": "phase",
                    "phase": "operate",
                }
            ],
        )
        # Add a compact for a different stem -- discover_jsonl should
        # ignore it.
        fx = CompactFixture(variant="v", runner="alice", run="run02")
        base_ns = 1_700_000_000_000_000_000
        fx.push_phase(base_ns, "operate")
        fx.write(tmp_path / "v-alice-run02.compact.parquet")

        jsonl_paths = discover_jsonl(tmp_path)
        assert [p.name for p in jsonl_paths] == ["v-alice-run01.jsonl"]


# ----- update_cache against compact-only directory -----


class TestUpdateCacheCompactOnly:
    @staticmethod
    def _populate(tmp_path: Path) -> None:
        fx = CompactFixture(
            variant="test-variant",
            runner="alice",
            run="run01",
            threading_mode="single",
            recv_buffer_kb=4096,
        )
        base_ns = 1_700_000_000_000_000_000
        fx.push_phase(base_ns + 0, "operate")
        fx.push_write(base_ns + 1_000_000, "/k", 1, 1, 8)
        fx.push_write(base_ns + 2_000_000, "/k", 1, 2, 8)
        fx.push_receive(base_ns + 3_000_000, "bob", 1, "/k", 1, 8)
        fx.push_receive(base_ns + 4_000_000, "bob", 2, "/k", 1, 8)
        fx.push_phase(base_ns + 5_000_000, "silent")
        fx.write(tmp_path / "test-variant-alice-run01.compact.parquet")

    def test_builds_shard_from_compact(self, tmp_path: Path) -> None:
        self._populate(tmp_path)
        metas = update_cache(tmp_path)
        assert "test-variant-alice-run01" in metas
        meta = metas["test-variant-alice-run01"]
        # 2 phases + 2 writes + 2 receives = 6 rows.
        assert meta.row_count == 6
        assert meta.variant == "test-variant"
        assert meta.run == "run01"
        assert meta.schema_version == SCHEMA_VERSION
        assert meta.is_clocksync is False

    def test_shard_columns_match_schema(self, tmp_path: Path) -> None:
        self._populate(tmp_path)
        update_cache(tmp_path)
        parquet_path, _ = shard_paths(tmp_path, "test-variant-alice-run01")
        df = pl.read_parquet(parquet_path)
        # All SHARD_SCHEMA columns are present in the cached shard.
        from schema import SHARD_SCHEMA

        assert set(df.columns) == set(SHARD_SCHEMA.keys())

    def test_discover_groups_finds_compact_spawn(self, tmp_path: Path) -> None:
        self._populate(tmp_path)
        update_cache(tmp_path)
        groups = discover_groups(tmp_path)
        assert len(groups) == 1
        variant, run, paths = groups[0]
        assert variant == "test-variant"
        assert run == "run01"
        assert len(paths) == 1

    def test_scan_shards_reads_compact_derived_rows(self, tmp_path: Path) -> None:
        self._populate(tmp_path)
        update_cache(tmp_path)
        lf = scan_shards(tmp_path)
        df = lf.collect()
        events = sorted(set(df.get_column("event").cast(pl.Utf8).to_list()))
        assert events == ["phase", "receive", "write"]

    def test_rebuild_on_compact_mtime_drift(self, tmp_path: Path) -> None:
        """A touched compact file triggers a shard rebuild."""
        import os
        import time

        self._populate(tmp_path)
        metas1 = update_cache(tmp_path)
        time.sleep(0.05)

        target = tmp_path / "test-variant-alice-run01.compact.parquet"
        future = time.time() + 60
        os.utime(target, (future, future))

        metas2 = update_cache(tmp_path)
        assert (
            metas2["test-variant-alice-run01"].mtime
            != metas1["test-variant-alice-run01"].mtime
        )


# ----- update_cache against a mixed directory -----


class TestUpdateCacheMixedFormats:
    """Mix compact + JSONL across different stems -- both are cached.

    Also covers the "compact wins for the same stem" override: when a
    stem has both formats, only one shard is built and it comes from
    the compact source.
    """

    def test_one_jsonl_one_compact_distinct_stems(self, tmp_path: Path) -> None:
        # JSONL spawn under stem A.
        write_jsonl(
            tmp_path / "v1-alice-run01.jsonl",
            [
                {
                    "ts": _ts(0),
                    "variant": "v1",
                    "runner": "alice",
                    "run": "run01",
                    "event": "phase",
                    "phase": "operate",
                },
                {
                    "ts": _ts(1),
                    "variant": "v1",
                    "runner": "alice",
                    "run": "run01",
                    "event": "write",
                    "seq": 1,
                    "path": "/k",
                    "qos": 1,
                    "bytes": 8,
                },
            ],
        )
        # Compact spawn under stem B.
        fx = CompactFixture(variant="v2", runner="bob", run="run01")
        base_ns = 1_700_000_000_000_000_000
        fx.push_phase(base_ns, "operate")
        fx.push_write(base_ns + 1_000_000, "/k", 1, 1, 8)
        fx.write(tmp_path / "v2-bob-run01.compact.parquet")

        metas = update_cache(tmp_path)
        assert set(metas.keys()) == {"v1-alice-run01", "v2-bob-run01"}
        # Both shards land with the bumped schema version.
        for meta in metas.values():
            assert meta.schema_version == SCHEMA_VERSION

        groups = discover_groups(tmp_path)
        seen = {(v, r) for v, r, _ in groups}
        assert seen == {("v1", "run01"), ("v2", "run01")}

    def test_same_stem_both_formats_uses_compact(self, tmp_path: Path) -> None:
        """Same stem; compact wins; shard reflects compact contents."""
        # Decoy JSONL with a single phase row.
        write_jsonl(
            tmp_path / "v-alice-run01.jsonl",
            [
                {
                    "ts": _ts(0),
                    "variant": "v",
                    "runner": "alice",
                    "run": "run01",
                    "event": "phase",
                    "phase": "this-should-not-make-it",
                }
            ],
        )
        # Compact with TWO phase events; if cache picks compact the
        # shard will carry two rows, not one.
        fx = CompactFixture(variant="v", runner="alice", run="run01")
        base_ns = 1_700_000_000_000_000_000
        fx.push_phase(base_ns, "connect")
        fx.push_phase(base_ns + 1_000_000, "operate")
        fx.write(tmp_path / "v-alice-run01.compact.parquet")

        metas = update_cache(tmp_path)
        assert metas["v-alice-run01"].row_count == 2

        parquet_path, _ = shard_paths(tmp_path, "v-alice-run01")
        df = pl.read_parquet(parquet_path)
        phases = sorted(
            df.filter(pl.col("event") == "phase").get_column("phase").to_list()
        )
        assert phases == ["connect", "operate"]


# ----- Numeric parity: JSONL-only vs compact-only of the same workload -----
#
# Removed by T19.10c. Post-E19 cleanup, the JSONL stream is
# lifecycle-only; "JSONL-only with per-event rows" is no longer a
# supported source shape (``parse.iter_rows`` warns once and skips
# per-event JSONL rows when it encounters them). The remaining
# format-transparency surface is the compact-Parquet path, exercised
# directly by ``TestUpdateCacheCompactOnly`` above. End-to-end pivot
# parity for the canonical per-spawn file pair is exercised by the
# ``tmp_logs`` fixture in ``test_analyze`` / ``test_cache``.


# ----- Schema version bump -----


class TestSchemaVersionBump:
    def test_schema_version_bumped_to_6(self) -> None:
        """T19.10c bumped the cache schema version to 6 (E19 cleanup).

        Version history relevant to this layer:
        - v3: T11.5 added ``threading_mode``.
        - v4: T18.4 added compact-parquet ingest.
        - v5: T19.5 added ``leaf_count`` / ``shape`` / ``bytes`` columns.
        - v6: T19.10c stripped per-event rows out of the JSONL parser.
          Columns are unchanged, but any v5 cache built from a pre-T18.2
          JSONL with per-event rows contains rows that the post-cleanup
          analyzer would have dropped -- bumping forces a rebuild so
          those phantom rows are recomputed under the lifecycle-only
          rule.
        """
        from schema import SCHEMA_VERSION as v

        assert v == "6"

    def test_legacy_v3_cache_is_invalidated(self, tmp_path: Path) -> None:
        """A cache with an older schema version triggers a wipe.

        The previous test name pinned v3 -> bumped-version invalidation;
        the same behaviour applies to any older version (v3 / v4 / v5)
        being invalidated by the T19.10c bump to v6.
        """
        # Drop a fake legacy cache with the v3 sentinel and an orphan
        # shard. The next update_cache call should wipe it because the
        # version no longer matches.
        cdir = cache_dir(tmp_path)
        cdir.mkdir(parents=True)
        with open(cdir / "_cache_schema_version.json", "w") as f:
            json.dump({"schema_version": "3"}, f)
        # Orphan shard file -- if the legacy invalidation works it'll
        # be removed by the wipe.
        (cdir / "stale.parquet").write_text("not real parquet")

        # Drop a fresh compact source so update_cache has something to do.
        fx = CompactFixture(variant="v", runner="alice", run="run01")
        fx.push_phase(1_700_000_000_000_000_000, "operate")
        fx.write(tmp_path / "v-alice-run01.compact.parquet")

        metas = update_cache(tmp_path)
        assert "v-alice-run01" in metas
        # The pre-existing stale file should have been wiped.
        assert not (cdir / "stale.parquet").exists()
        with open(cdir / "_cache_schema_version.json") as f:
            assert json.load(f)["schema_version"] == SCHEMA_VERSION
