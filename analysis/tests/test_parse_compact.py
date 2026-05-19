"""Tests for the compact-parquet loader (T18.4 / E18).

Three layers of coverage:

1. Format detection (``parse.detect_source_format`` /
   ``parse.source_stem``) so the cache picks the right loader.
2. Per-kind projection round-trip: build a small compact-parquet
   fixture via :class:`compact_fixture.CompactFixture`, feed it through
   ``parse_compact.read_compact_parquet``, assert the projected
   ``SHARD_SCHEMA`` rows carry the expected values for each of the
   12 ``kind`` values.
3. Byte-equivalence with the legacy JSONL parser: build the same
   workload twice (once as JSONL, once as compact) and confirm the
   projected DataFrames match on the columns that matter (event
   counts, per-row column values for write/receive/etc.). This is the
   acceptance gate from the task spec.
"""

from __future__ import annotations

import json
from pathlib import Path

import polars as pl
import pytest

from compact_fixture import CompactFixture
from helpers import _ts, events_to_lazy
from parse import SourceFormat, detect_source_format, source_stem
from parse_compact import (
    CompactLoadError,
    compact_stem,
    is_compact_parquet,
    read_compact_metadata,
    read_compact_parquet,
)
from schema import SHARD_SCHEMA


# ----- Format detector -----


class TestDetectSourceFormat:
    def test_jsonl_is_jsonl(self, tmp_path: Path) -> None:
        path = tmp_path / "v-a-r.jsonl"
        path.write_text("")
        assert detect_source_format(path) is SourceFormat.JSONL

    def test_compact_parquet_is_compact(self, tmp_path: Path) -> None:
        path = tmp_path / "v-a-r.compact.parquet"
        path.write_text("")
        assert detect_source_format(path) is SourceFormat.COMPACT

    def test_plain_parquet_is_none(self, tmp_path: Path) -> None:
        # Cache-internal shards use plain `.parquet` -- they must NOT
        # be picked up as source files.
        path = tmp_path / "v-a-r.parquet"
        path.write_text("")
        assert detect_source_format(path) is None

    def test_unrelated_extension_is_none(self, tmp_path: Path) -> None:
        assert detect_source_format(tmp_path / "x.txt") is None
        assert detect_source_format(tmp_path / "x") is None


class TestSourceStem:
    def test_jsonl_stem(self, tmp_path: Path) -> None:
        assert source_stem(tmp_path / "v-a-r.jsonl") == "v-a-r"

    def test_compact_stem(self, tmp_path: Path) -> None:
        # source_stem must strip the full ``.compact.parquet`` suffix
        # so the stem matches the JSONL equivalent.
        assert source_stem(tmp_path / "v-a-r.compact.parquet") == "v-a-r"

    def test_compact_helper_matches(self, tmp_path: Path) -> None:
        p = tmp_path / "v-a-r.compact.parquet"
        assert compact_stem(p) == source_stem(p)


class TestIsCompactParquet:
    def test_compact_path(self, tmp_path: Path) -> None:
        assert is_compact_parquet(tmp_path / "x.compact.parquet")

    def test_plain_parquet(self, tmp_path: Path) -> None:
        assert not is_compact_parquet(tmp_path / "x.parquet")


# ----- Per-kind projection round-trip -----


def _build_full_compact(tmp_path: Path) -> Path:
    """Build a compact-parquet fixture exercising every event kind."""
    fx = CompactFixture(
        variant="test-variant",
        runner="alice",
        run="run01",
        threading_mode="multi",
        recv_buffer_kb=8192,
    )
    base_ns = 1_700_000_000_000_000_000  # arbitrary 2023-ish epoch ns
    # Lifecycle prologue
    fx.push_phase(base_ns + 0, "connect")
    fx.push_connected(base_ns + 1_000_000, "bob", 12.5, "multi")
    fx.push_phase(base_ns + 2_000_000, "operate")
    # Hot-path events
    fx.push_write(base_ns + 3_000_000, "/bench/0", 4, 1, 128)
    fx.push_write(base_ns + 4_000_000, "/bench/1", 4, 2, 128)
    fx.push_receive(base_ns + 5_000_000, "bob", 1, "/bench/0", 4, 128)
    fx.push_receive(base_ns + 6_000_000, "bob", 2, "/bench/1", 4, 128)
    fx.push_backpressure_skipped(base_ns + 7_000_000, "/bench/2", 1)
    fx.push_gap_detected(base_ns + 8_000_000, "bob", 999)
    fx.push_gap_filled(base_ns + 9_000_000, "bob", 999)
    # Resource sample
    fx.push_resource(base_ns + 10_000_000, 45.5, 256.0)
    # EOT handshake
    fx.push_eot_sent(base_ns + 11_000_000, 42)
    fx.push_eot_received(base_ns + 12_000_000, "bob", 7)
    fx.push_eot_timeout(base_ns + 13_000_000, 5000, json.dumps(["bob"]))
    # Clock-sync (reserved for E8; column mapping still tested here)
    fx.push_clock_sync(base_ns + 14_000_000, "bob", -1_234_000, 0.75)
    # Lifecycle epilogue
    fx.push_phase(base_ns + 15_000_000, "silent")

    out = tmp_path / "test-variant-alice-run01.compact.parquet"
    fx.write(out)
    return out


class TestReadCompactMetadata:
    def test_decodes_spawn_identity(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        meta = read_compact_metadata(path)
        assert meta.variant == "test-variant"
        assert meta.runner == "alice"
        assert meta.run == "run01"
        assert meta.threading_mode == "multi"
        assert meta.recv_buffer_kb == 8192
        assert meta.schema_version == 1

    def test_decodes_intern_dicts(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        meta = read_compact_metadata(path)
        # Three distinct paths -- /bench/0, /bench/1, /bench/2.
        assert sorted(meta.paths) == ["/bench/0", "/bench/1", "/bench/2"]
        # One peer (bob).
        assert meta.peers == ["bob"]


class TestReadCompactParquet:
    def test_columns_match_shard_schema(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        # Every SHARD_SCHEMA column is present with the canonical dtype.
        assert list(df.columns) == list(SHARD_SCHEMA.keys())
        for name, dtype in SHARD_SCHEMA.items():
            assert df.schema[name] == dtype, (
                f"column {name} expected {dtype}, got {df.schema[name]}"
            )

    def test_event_counts_per_kind(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        counts = df.group_by("event").agg(pl.len().alias("n")).sort("event")
        out = {row["event"]: row["n"] for row in counts.iter_rows(named=True)}
        # phase: connect / operate / silent.
        assert out["phase"] == 3
        # write: two events.
        assert out["write"] == 2
        # receive: two events.
        assert out["receive"] == 2
        # one of each of the remaining kinds.
        assert out["backpressure_skipped"] == 1
        assert out["gap_detected"] == 1
        assert out["gap_filled"] == 1
        assert out["resource"] == 1
        assert out["eot_sent"] == 1
        assert out["eot_received"] == 1
        assert out["eot_timeout"] == 1
        assert out["connected"] == 1
        assert out["clock_sync"] == 1

    def test_write_projection(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        writes = df.filter(pl.col("event") == "write").sort("seq")
        assert writes.height == 2
        row0 = writes.row(0, named=True)
        row1 = writes.row(1, named=True)
        assert row0["seq"] == 1
        assert row0["path"] == "/bench/0"
        assert row0["qos"] == 4
        assert row0["writer"] is None  # write has no `writer`
        assert row1["seq"] == 2
        assert row1["path"] == "/bench/1"

    def test_receive_projection(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        recvs = df.filter(pl.col("event") == "receive").sort("seq")
        assert recvs.height == 2
        for row in recvs.iter_rows(named=True):
            assert row["writer"] == "bob"
            assert row["qos"] == 4

    def test_phase_projection(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        phases = (
            df.filter(pl.col("event") == "phase")
            .sort("ts")
            .get_column("phase")
            .to_list()
        )
        assert phases == ["connect", "operate", "silent"]

    def test_gap_events_use_extra_i64(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        gd = df.filter(pl.col("event") == "gap_detected").row(0, named=True)
        gf = df.filter(pl.col("event") == "gap_filled").row(0, named=True)
        # Compact ``extra_i64`` carries the missing/recovered seq.
        assert gd["missing_seq"] == 999
        assert gd["writer"] == "bob"
        assert gf["recovered_seq"] == 999
        assert gf["writer"] == "bob"

    def test_resource_projection(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        r = df.filter(pl.col("event") == "resource").row(0, named=True)
        assert abs(r["cpu_percent"] - 45.5) < 1e-3
        assert abs(r["memory_mb"] - 256.0) < 1e-3

    def test_connected_projection_uses_metadata_recv_buffer(
        self, tmp_path: Path
    ) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        c = df.filter(pl.col("event") == "connected").row(0, named=True)
        assert c["peer"] == "bob"
        assert abs(c["elapsed_ms"] - 12.5) < 1e-3
        # Per-row extra_utf8 carries the threading mode; falls back to
        # the spawn-level metadata when null.
        assert c["threading_mode"] == "multi"
        assert c["recv_buffer_kb"] == 8192

    def test_eot_projection(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        sent = df.filter(pl.col("event") == "eot_sent").row(0, named=True)
        recv = df.filter(pl.col("event") == "eot_received").row(0, named=True)
        to = df.filter(pl.col("event") == "eot_timeout").row(0, named=True)
        assert sent["eot_id"] == 42
        assert recv["eot_id"] == 7
        assert recv["writer"] == "bob"
        assert to["wait_ms"] == 5000
        # JSON missing list is propagated verbatim.
        assert to["eot_missing"] == json.dumps(["bob"])

    def test_clock_sync_offset_ns_to_offset_ms(self, tmp_path: Path) -> None:
        """``ClockSync`` row's ``offset_ns`` is exposed as ``offset_ms``.

        ``SHARD_SCHEMA`` carries ``offset_ms`` (matching the legacy
        JSONL field name). The compact format stores ``offset_ns``;
        the loader converts on the fly.
        """
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        cs = df.filter(pl.col("event") == "clock_sync").row(0, named=True)
        assert cs["peer"] == "bob"
        # -1_234_000 ns = -1.234 ms.
        assert abs(cs["offset_ms"] - (-1.234)) < 1e-6
        assert abs(cs["rtt_ms"] - 0.75) < 1e-3

    def test_rows_sorted_by_ts(self, tmp_path: Path) -> None:
        path = _build_full_compact(tmp_path)
        df = read_compact_parquet(path)
        ts_list = df.get_column("ts").to_list()
        assert ts_list == sorted(ts_list)

    def test_empty_buffers_round_trip(self, tmp_path: Path) -> None:
        fx = CompactFixture(variant="empty-v", runner="alice", run="run01")
        out = tmp_path / "empty-v-alice-run01.compact.parquet"
        fx.write(out)
        df = read_compact_parquet(out)
        assert df.height == 0
        assert list(df.columns) == list(SHARD_SCHEMA.keys())

    def test_missing_spawn_identity_raises(self, tmp_path: Path) -> None:
        """A compact-parquet missing variant/runner/run is rejected."""
        # Build a file with the right shape but stripped metadata.
        df = pl.DataFrame(
            {
                "ts_ns": [1],
                "kind": [0],
                "seq": [0],
                "path_idx": [0],
                "peer_idx": [255],
                "qos": [0],
                "bytes": [0],
                "extra_f32": [None],
                "extra_f32_b": [None],
                "extra_i64": [None],
                "extra_utf8": [None],
            },
            schema={
                "ts_ns": pl.Int64,
                "kind": pl.Int32,
                "seq": pl.Int64,
                "path_idx": pl.Int32,
                "peer_idx": pl.Int32,
                "qos": pl.Int32,
                "bytes": pl.Int32,
                "extra_f32": pl.Float32,
                "extra_f32_b": pl.Float32,
                "extra_i64": pl.Int64,
                "extra_utf8": pl.Utf8,
            },
        )
        out = tmp_path / "no-meta.compact.parquet"
        df.write_parquet(out, metadata={"schema_version": "1"})
        with pytest.raises(CompactLoadError):
            read_compact_parquet(out)


# ----- Cross-format byte-equivalence -----
#
# Build the same workload twice -- once as JSONL through the streaming
# parser, once as compact through the compact loader -- and assert the
# projected SHARD_SCHEMA frames match. The compact format does NOT
# carry the ``bytes`` payload size (which the legacy JSONL parser also
# drops, because the schema has no ``bytes`` column), so equivalence is
# checked on the columns that matter to the downstream pipeline.


def _build_equivalent_workload(
    tmp_path: Path,
) -> tuple[pl.DataFrame, pl.DataFrame]:
    """Build identical synthetic spawns as both JSONL and compact.

    Returns ``(jsonl_df, compact_df)`` -- the projected SHARD_SCHEMA
    DataFrames the two loaders produce for the same logical events.
    """
    # JSONL leg -- reuse the existing helper.
    jsonl_events = [
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
            "elapsed_ms": 7.5,
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
        {
            "ts": _ts(3),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "write",
            "seq": 1,
            "path": "/k",
            "qos": 4,
            "bytes": 8,
        },
        {
            "ts": _ts(4),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "write",
            "seq": 2,
            "path": "/k",
            "qos": 4,
            "bytes": 8,
        },
        {
            "ts": _ts(5),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "receive",
            "writer": "bob",
            "seq": 1,
            "path": "/k",
            "qos": 4,
            "bytes": 8,
        },
        {
            "ts": _ts(6),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "receive",
            "writer": "bob",
            "seq": 2,
            "path": "/k",
            "qos": 4,
            "bytes": 8,
        },
        {
            "ts": _ts(7),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "gap_detected",
            "writer": "bob",
            "missing_seq": 99,
        },
        {
            "ts": _ts(8),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "gap_filled",
            "writer": "bob",
            "recovered_seq": 99,
        },
        {
            "ts": _ts(9),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "resource",
            "cpu_percent": 10.5,
            "memory_mb": 64.0,
        },
        {
            "ts": _ts(10),
            "variant": "v",
            "runner": "alice",
            "run": "run01",
            "event": "phase",
            "phase": "silent",
        },
    ]
    jsonl_df = events_to_lazy(jsonl_events).collect()

    # Compact leg -- mirror the same events using the same wall-clock
    # offsets so the ``ts`` column matches across formats.

    def _ts_ns(offset_ms: float) -> int:
        # Same base epoch as helpers._ts.
        base = 1744710950_000_000_000
        return base + int(offset_ms * 1_000_000)

    fx = CompactFixture(
        variant="v",
        runner="alice",
        run="run01",
        threading_mode="single",
        recv_buffer_kb=4096,
    )
    fx.push_phase(_ts_ns(0), "connect")
    fx.push_connected(_ts_ns(1), "bob", 7.5, "single")
    fx.push_phase(_ts_ns(2), "operate")
    fx.push_write(_ts_ns(3), "/k", 4, 1, 8)
    fx.push_write(_ts_ns(4), "/k", 4, 2, 8)
    fx.push_receive(_ts_ns(5), "bob", 1, "/k", 4, 8)
    fx.push_receive(_ts_ns(6), "bob", 2, "/k", 4, 8)
    fx.push_gap_detected(_ts_ns(7), "bob", 99)
    fx.push_gap_filled(_ts_ns(8), "bob", 99)
    fx.push_resource(_ts_ns(9), 10.5, 64.0)
    fx.push_phase(_ts_ns(10), "silent")

    out = tmp_path / "v-alice-run01.compact.parquet"
    fx.write(out)
    compact_df = read_compact_parquet(out)

    # Polars datetime equality is sensitive to time zone tagging --
    # the helpers._ts path goes through parse_timestamp_ns which
    # produces an Int64 (ns) we then cast to Datetime("ns","UTC"); the
    # compact path casts directly. Both produce the same physical
    # nanosecond integer, so a join on (ts, event) is robust.
    return jsonl_df, compact_df


class TestJsonlCompactByteEquivalence:
    """The two loaders must produce equivalent SHARD_SCHEMA frames for
    the same logical workload. The acceptance gate of T18.4."""

    def test_row_count_matches(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        assert jsonl_df.height == compact_df.height

    def test_event_count_per_kind_matches(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        for ev in (
            "write",
            "receive",
            "phase",
            "gap_detected",
            "gap_filled",
            "resource",
            "connected",
        ):
            j = jsonl_df.filter(pl.col("event") == ev).height
            c = compact_df.filter(pl.col("event") == ev).height
            assert j == c, f"event {ev}: jsonl={j} compact={c}"

    def test_write_rows_match(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        j = (
            jsonl_df.filter(pl.col("event") == "write")
            .select(["seq", "path", "qos"])
            .sort("seq")
        )
        c = (
            compact_df.filter(pl.col("event") == "write")
            .select(["seq", "path", "qos"])
            .sort("seq")
        )
        assert j.equals(c), f"write rows differ:\nJSONL:\n{j}\nCOMPACT:\n{c}"

    def test_receive_rows_match(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        j = (
            jsonl_df.filter(pl.col("event") == "receive")
            .select(["seq", "path", "writer", "qos"])
            .sort("seq")
        )
        c = (
            compact_df.filter(pl.col("event") == "receive")
            .select(["seq", "path", "writer", "qos"])
            .sort("seq")
        )
        assert j.equals(c)

    def test_gap_rows_match(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        for ev, col in (
            ("gap_detected", "missing_seq"),
            ("gap_filled", "recovered_seq"),
        ):
            j = jsonl_df.filter(pl.col("event") == ev).select(["writer", col])
            c = compact_df.filter(pl.col("event") == ev).select(["writer", col])
            assert j.equals(c), f"{ev} rows differ:\n{j}\nvs\n{c}"

    def test_resource_rows_match(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        j = jsonl_df.filter(pl.col("event") == "resource")
        c = compact_df.filter(pl.col("event") == "resource")
        assert j.height == c.height == 1
        jr = j.row(0, named=True)
        cr = c.row(0, named=True)
        assert abs(jr["cpu_percent"] - cr["cpu_percent"]) < 1e-4
        assert abs(jr["memory_mb"] - cr["memory_mb"]) < 1e-4

    def test_connected_rows_match(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        j = jsonl_df.filter(pl.col("event") == "connected").row(0, named=True)
        c = compact_df.filter(pl.col("event") == "connected").row(0, named=True)
        assert abs(j["elapsed_ms"] - c["elapsed_ms"]) < 1e-4
        assert j["threading_mode"] == c["threading_mode"]
        assert j["recv_buffer_kb"] == c["recv_buffer_kb"]

    def test_phase_rows_match(self, tmp_path: Path) -> None:
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        j = (
            jsonl_df.filter(pl.col("event") == "phase")
            .sort("ts")
            .get_column("phase")
            .to_list()
        )
        c = (
            compact_df.filter(pl.col("event") == "phase")
            .sort("ts")
            .get_column("phase")
            .to_list()
        )
        assert j == c

    def test_ts_columns_match(self, tmp_path: Path) -> None:
        """The two loaders must produce the same wall-clock ts values."""
        jsonl_df, compact_df = _build_equivalent_workload(tmp_path)
        # Sort both by (ts, event) so equal-key rows are stable.
        j_ts = jsonl_df.sort(["ts", "event"]).get_column("ts").to_list()
        c_ts = compact_df.sort(["ts", "event"]).get_column("ts").to_list()
        assert j_ts == c_ts
