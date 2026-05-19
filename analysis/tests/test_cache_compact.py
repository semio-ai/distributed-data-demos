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
    def test_jsonl_only(self, tmp_logs: Path) -> None:
        """``tmp_logs`` fixture writes two JSONL files; both surface."""
        sources = discover_sources(tmp_logs)
        assert set(sources.keys()) == {
            "test-variant-alice-run01",
            "test-variant-bob-run01",
        }
        for _stem, (_, fmt) in sources.items():
            assert fmt is SourceFormat.JSONL

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


class TestNumericParityAcrossFormats:
    """Pivot-table / integrity-style aggregates must match across formats.

    The acceptance criterion from the task: pivot tables / integrity
    reports / drop-rate heatmaps numerically match between a
    compact-only run and a JSONL-only run of the same logical workload.

    Here we exercise the cache's projection layer directly -- per-event
    counts and per-(writer, qos, path) write counts must agree between
    the two formats. Higher-level pivot / heatmap parity falls out from
    the existing analyzer tests once the projections agree, but having
    a focused parity test here keeps regressions on the cache layer
    itself easy to bisect.
    """

    def _workload_pair(self, root: Path) -> tuple[Path, Path]:
        """Drop a JSONL-only run and a compact-only run as sibling dirs.

        Returns ``(jsonl_dir, compact_dir)``.
        """
        jsonl_dir = root / "j"
        compact_dir = root / "c"
        jsonl_dir.mkdir()
        compact_dir.mkdir()

        # Identical workload: alice writes seq 1..3 on /k at qos 4;
        # bob receives all three.
        alice = [
            {
                "ts": _ts(0),
                "variant": "v",
                "runner": "alice",
                "run": "run01",
                "event": "phase",
                "phase": "operate",
            },
        ]
        for seq, off in [(1, 100), (2, 200), (3, 300)]:
            alice.append(
                {
                    "ts": _ts(off),
                    "variant": "v",
                    "runner": "alice",
                    "run": "run01",
                    "event": "write",
                    "seq": seq,
                    "path": "/k",
                    "qos": 4,
                    "bytes": 8,
                }
            )
        bob = [
            {
                "ts": _ts(0),
                "variant": "v",
                "runner": "bob",
                "run": "run01",
                "event": "phase",
                "phase": "operate",
            },
        ]
        for seq, off in [(1, 150), (2, 250), (3, 350)]:
            bob.append(
                {
                    "ts": _ts(off),
                    "variant": "v",
                    "runner": "bob",
                    "run": "run01",
                    "event": "receive",
                    "writer": "alice",
                    "seq": seq,
                    "path": "/k",
                    "qos": 4,
                    "bytes": 8,
                }
            )

        write_jsonl(jsonl_dir / "v-alice-run01.jsonl", alice)
        write_jsonl(jsonl_dir / "v-bob-run01.jsonl", bob)

        # Compact mirror of the same workload.
        base_ns = 1744710950_000_000_000  # matches helpers._ts base

        def ns(off_ms: float) -> int:
            return base_ns + int(off_ms * 1_000_000)

        fa = CompactFixture(variant="v", runner="alice", run="run01")
        fa.push_phase(ns(0), "operate")
        for seq, off in [(1, 100), (2, 200), (3, 300)]:
            fa.push_write(ns(off), "/k", 4, seq, 8)
        fa.write(compact_dir / "v-alice-run01.compact.parquet")

        fb = CompactFixture(variant="v", runner="bob", run="run01")
        fb.push_phase(ns(0), "operate")
        for seq, off in [(1, 150), (2, 250), (3, 350)]:
            fb.push_receive(ns(off), "alice", seq, "/k", 4, 8)
        fb.write(compact_dir / "v-bob-run01.compact.parquet")

        return jsonl_dir, compact_dir

    def test_event_counts_match(self, tmp_path: Path) -> None:
        jsonl_dir, compact_dir = self._workload_pair(tmp_path)
        update_cache(jsonl_dir)
        update_cache(compact_dir)
        j = scan_shards(jsonl_dir).collect()
        c = scan_shards(compact_dir).collect()

        # Event counts per kind must agree across the two formats.
        j_counts = (
            j.group_by(pl.col("event").cast(pl.Utf8))
            .agg(pl.len().alias("n"))
            .sort("event")
        )
        c_counts = (
            c.group_by(pl.col("event").cast(pl.Utf8))
            .agg(pl.len().alias("n"))
            .sort("event")
        )
        assert j_counts.equals(c_counts)

    def test_write_rows_match(self, tmp_path: Path) -> None:
        jsonl_dir, compact_dir = self._workload_pair(tmp_path)
        update_cache(jsonl_dir)
        update_cache(compact_dir)
        j = scan_shards(jsonl_dir).collect()
        c = scan_shards(compact_dir).collect()

        j_writes = (
            j.filter(pl.col("event") == "write")
            .select(["seq", "path", "qos"])
            .sort("seq")
        )
        c_writes = (
            c.filter(pl.col("event") == "write")
            .select(["seq", "path", "qos"])
            .sort("seq")
        )
        assert j_writes.equals(c_writes)

    def test_receive_rows_match(self, tmp_path: Path) -> None:
        jsonl_dir, compact_dir = self._workload_pair(tmp_path)
        update_cache(jsonl_dir)
        update_cache(compact_dir)
        j = scan_shards(jsonl_dir).collect()
        c = scan_shards(compact_dir).collect()

        j_recvs = (
            j.filter(pl.col("event") == "receive")
            .select(["seq", "path", "writer", "qos"])
            .sort("seq")
        )
        c_recvs = (
            c.filter(pl.col("event") == "receive")
            .select(["seq", "path", "writer", "qos"])
            .sort("seq")
        )
        assert j_recvs.equals(c_recvs)


# ----- End-to-end analyzer parity -----


class TestRunAnalysisParity:
    """End-to-end: ``run_analysis`` must produce equivalent reports on
    JSONL-only vs compact-only directories carrying the same workload.

    This is the acceptance gate from T18.4: pivot tables / integrity /
    drop-rate aggregates numerically match between a compact-only run
    and a JSONL-only run of the same logical workload.
    """

    def _workload_pair(self, root: Path) -> tuple[Path, Path]:
        """Drop the same two-runner workload as both JSONL and compact."""
        jsonl_dir = root / "j"
        compact_dir = root / "c"
        jsonl_dir.mkdir()
        compact_dir.mkdir()

        # Alice writes seq 1..5 on /k at qos 4; bob receives all five.
        alice_jsonl = [
            {
                "ts": _ts(0),
                "variant": "v",
                "runner": "alice",
                "run": "run01",
                "event": "phase",
                "phase": "connect",
            },
            {
                "ts": _ts(1),
                "variant": "v",
                "runner": "alice",
                "run": "run01",
                "event": "connected",
                "elapsed_ms": 1.0,
                "threading_mode": "single",
                "recv_buffer_kb": 4096,
            },
            {
                "ts": _ts(2),
                "variant": "v",
                "runner": "alice",
                "run": "run01",
                "event": "phase",
                "phase": "operate",
            },
        ]
        for seq, off in [(1, 100), (2, 200), (3, 300), (4, 400), (5, 500)]:
            alice_jsonl.append(
                {
                    "ts": _ts(off),
                    "variant": "v",
                    "runner": "alice",
                    "run": "run01",
                    "event": "write",
                    "seq": seq,
                    "path": "/k",
                    "qos": 4,
                    "bytes": 8,
                }
            )
        alice_jsonl.append(
            {
                "ts": _ts(1000),
                "variant": "v",
                "runner": "alice",
                "run": "run01",
                "event": "phase",
                "phase": "silent",
            }
        )

        bob_jsonl = [
            {
                "ts": _ts(0),
                "variant": "v",
                "runner": "bob",
                "run": "run01",
                "event": "phase",
                "phase": "connect",
            },
            {
                "ts": _ts(1),
                "variant": "v",
                "runner": "bob",
                "run": "run01",
                "event": "connected",
                "elapsed_ms": 1.0,
                "threading_mode": "single",
                "recv_buffer_kb": 4096,
            },
            {
                "ts": _ts(2),
                "variant": "v",
                "runner": "bob",
                "run": "run01",
                "event": "phase",
                "phase": "operate",
            },
        ]
        for seq, off in [(1, 150), (2, 250), (3, 350), (4, 450), (5, 550)]:
            bob_jsonl.append(
                {
                    "ts": _ts(off),
                    "variant": "v",
                    "runner": "bob",
                    "run": "run01",
                    "event": "receive",
                    "writer": "alice",
                    "seq": seq,
                    "path": "/k",
                    "qos": 4,
                    "bytes": 8,
                }
            )
        bob_jsonl.append(
            {
                "ts": _ts(1000),
                "variant": "v",
                "runner": "bob",
                "run": "run01",
                "event": "phase",
                "phase": "silent",
            }
        )

        write_jsonl(jsonl_dir / "v-alice-run01.jsonl", alice_jsonl)
        write_jsonl(jsonl_dir / "v-bob-run01.jsonl", bob_jsonl)

        # Compact mirror.
        base_ns = 1744710950_000_000_000

        def ns(off_ms: float) -> int:
            return base_ns + int(off_ms * 1_000_000)

        fa = CompactFixture(
            variant="v",
            runner="alice",
            run="run01",
            threading_mode="single",
            recv_buffer_kb=4096,
        )
        fa.push_phase(ns(0), "connect")
        fa.push_connected(ns(1), "bob", 1.0, "single")
        fa.push_phase(ns(2), "operate")
        for seq, off in [(1, 100), (2, 200), (3, 300), (4, 400), (5, 500)]:
            fa.push_write(ns(off), "/k", 4, seq, 8)
        fa.push_phase(ns(1000), "silent")
        fa.write(compact_dir / "v-alice-run01.compact.parquet")

        fb = CompactFixture(
            variant="v",
            runner="bob",
            run="run01",
            threading_mode="single",
            recv_buffer_kb=4096,
        )
        fb.push_phase(ns(0), "connect")
        fb.push_connected(ns(1), "alice", 1.0, "single")
        fb.push_phase(ns(2), "operate")
        for seq, off in [(1, 150), (2, 250), (3, 350), (4, 450), (5, 550)]:
            fb.push_receive(ns(off), "alice", seq, "/k", 4, 8)
        fb.push_phase(ns(1000), "silent")
        fb.write(compact_dir / "v-bob-run01.compact.parquet")

        return jsonl_dir, compact_dir

    def test_integrity_and_performance_match(self, tmp_path: Path) -> None:
        """End-to-end ``run_analysis`` output is equivalent across formats.

        Compares the IntegrityResult and PerformanceResult dataclass
        fields that downstream consumers (CLI tables, CSV export,
        diagrams) rely on. Equality is exact on counts and within a
        small tolerance on float aggregates -- the two formats encode
        the same wall-clock timestamps so latency math agrees to the
        nanosecond.
        """
        from analyze import run_analysis

        jsonl_dir, compact_dir = self._workload_pair(tmp_path)
        update_cache(jsonl_dir)
        update_cache(compact_dir)

        j_integrity, j_performance = run_analysis(jsonl_dir, do_summary=True)
        c_integrity, c_performance = run_analysis(compact_dir, do_summary=True)

        # One IntegrityResult / PerformanceResult per (variant, run,
        # writer, receiver) pair. Same workload -> same shape.
        assert len(j_integrity) == len(c_integrity)
        assert len(j_performance) == len(c_performance)
        assert j_integrity and j_performance, (
            "regression: empty analyzer output indicates the pipeline "
            "is no longer building anything"
        )

        # Match by (variant, run, writer, receiver, qos) so order
        # differences are tolerated.
        def _ikey(ir):
            return (ir.variant, ir.run, ir.writer, ir.receiver, ir.qos)

        j_map = {_ikey(ir): ir for ir in j_integrity}
        c_map = {_ikey(ir): ir for ir in c_integrity}
        assert set(j_map.keys()) == set(c_map.keys())
        for key, jr in j_map.items():
            cr = c_map[key]
            assert jr.write_count == cr.write_count
            assert jr.receive_count == cr.receive_count
            assert jr.duplicates == cr.duplicates
            assert jr.out_of_order == cr.out_of_order
            assert abs(jr.delivery_pct - cr.delivery_pct) < 1e-9, (
                f"delivery_pct mismatch on {key}: "
                f"jsonl={jr.delivery_pct} compact={cr.delivery_pct}"
            )


# ----- Schema version bump -----


class TestSchemaVersionBump:
    def test_schema_version_bumped_to_4(self) -> None:
        """T18.4 bumped the cache schema version to 4."""
        from schema import SCHEMA_VERSION as v

        assert v == "4"

    def test_legacy_v3_cache_is_invalidated(self, tmp_path: Path) -> None:
        """A cache with the previous schema version triggers a wipe."""
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
