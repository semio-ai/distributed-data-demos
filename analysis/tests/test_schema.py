"""Tests for the columnar schema definition."""

from __future__ import annotations

import polars as pl

from schema import KNOWN_EVENTS, SCHEMA_VERSION, SHARD_SCHEMA


class TestSchema:
    def test_schema_version_is_string(self) -> None:
        assert isinstance(SCHEMA_VERSION, str)
        assert SCHEMA_VERSION

    def test_schema_columns_present(self) -> None:
        # Required common columns
        for col in ("ts", "variant", "runner", "run", "event"):
            assert col in SHARD_SCHEMA

        # Per-event-type columns from ANALYSIS.md section 4.1
        for col in (
            "seq",
            "path",
            "writer",
            "qos",
            "elapsed_ms",
            "phase",
            "missing_seq",
            "recovered_seq",
            "cpu_percent",
            "memory_mb",
            "peer",
            "offset_ms",
            "rtt_ms",
        ):
            assert col in SHARD_SCHEMA

    def test_categorical_columns(self) -> None:
        assert SHARD_SCHEMA["variant"] == pl.Categorical
        assert SHARD_SCHEMA["runner"] == pl.Categorical
        assert SHARD_SCHEMA["run"] == pl.Categorical
        assert SHARD_SCHEMA["event"] == pl.Categorical

    def test_timestamp_dtype(self) -> None:
        ts_dtype = SHARD_SCHEMA["ts"]
        assert isinstance(ts_dtype, pl.Datetime)
        assert ts_dtype.time_unit == "ns"
        assert ts_dtype.time_zone == "UTC"

    def test_round_trip_parquet(self, tmp_path) -> None:  # type: ignore[no-untyped-def]
        from datetime import datetime, timezone

        # Build a tiny DataFrame matching the schema and write/read it.
        rows = [
            {
                "ts": datetime(2026, 1, 1, tzinfo=timezone.utc),
                "variant": "v",
                "runner": "r",
                "run": "run01",
                "event": "phase",
                "seq": None,
                "path": None,
                "writer": None,
                "qos": None,
                "elapsed_ms": None,
                "phase": "connect",
                "missing_seq": None,
                "recovered_seq": None,
                "cpu_percent": None,
                "memory_mb": None,
                "peer": None,
                "offset_ms": None,
                "rtt_ms": None,
            },
        ]
        df = pl.DataFrame(rows, schema=SHARD_SCHEMA, orient="row")
        path = tmp_path / "shard.parquet"
        df.write_parquet(path)

        loaded = pl.read_parquet(path)
        assert loaded.height == 1
        assert set(loaded.columns) == set(SHARD_SCHEMA.keys())
        assert loaded.get_column("event")[0] == "phase"

    def test_known_events(self) -> None:
        for ev in (
            "connected",
            "phase",
            "write",
            "receive",
            "gap_detected",
            "gap_filled",
            "resource",
            "clock_sync",
        ):
            assert ev in KNOWN_EVENTS
