"""Tests for the streaming JSONL line-to-row projection."""

from __future__ import annotations

import io
import json
from pathlib import Path

from parse import COLUMN_ORDER, iter_rows, parse_timestamp_ns, project_line
from schema import SHARD_SCHEMA


class TestParseTimestampNs:
    def test_nanosecond_precision_preserved(self) -> None:
        ns = parse_timestamp_ns("2026-04-15T09:35:50.000178400Z")
        # Compute the expected value: epoch seconds + 178400 ns.
        # We don't hardcode the epoch -- just check that the last 9 digits
        # encode the nanosecond fractional part exactly.
        assert ns is not None
        assert ns % 1_000_000_000 == 178_400

    def test_utc_z_suffix(self) -> None:
        ns = parse_timestamp_ns("2026-04-15T09:35:50.123456789Z")
        assert ns is not None
        assert ns % 1_000_000_000 == 123_456_789

    def test_no_fractional_seconds(self) -> None:
        ns = parse_timestamp_ns("2026-04-15T09:35:50Z")
        assert ns is not None
        assert ns % 1_000_000_000 == 0

    def test_short_fractional(self) -> None:
        ns = parse_timestamp_ns("2026-04-15T09:35:50.5Z")
        assert ns is not None
        assert ns % 1_000_000_000 == 500_000_000

    def test_invalid_returns_none(self) -> None:
        assert parse_timestamp_ns("") is None
        assert parse_timestamp_ns("not-a-date") is None


class TestProjectLine:
    def test_columns_match_schema(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.003424900Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "local-test-01",
                "event": "write",
                "seq": 1,
                "path": "/bench/0",
                "qos": 1,
                "bytes": 8,
            }
        )
        row = project_line(line)
        assert row is not None
        assert set(row.keys()) == set(SHARD_SCHEMA.keys())
        # Required common fields
        assert row["variant"] == "custom-udp"
        assert row["runner"] == "alice"
        assert row["run"] == "local-test-01"
        assert row["event"] == "write"
        # Write-specific fields
        assert row["seq"] == 1
        assert row["path"] == "/bench/0"
        assert row["qos"] == 1
        # Receive-only fields are null
        assert row["writer"] is None
        # ts is encoded as nanoseconds since epoch
        assert isinstance(row["ts"], int)

    def test_receive_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.003611700Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "local-test-01",
                "event": "receive",
                "writer": "bob",
                "seq": 1,
                "path": "/bench/0",
                "qos": 2,
                "bytes": 8,
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["event"] == "receive"
        assert row["writer"] == "bob"
        assert row["qos"] == 2

    def test_phase_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:50.000Z",
                "variant": "v",
                "runner": "a",
                "run": "r",
                "event": "phase",
                "phase": "operate",
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["phase"] == "operate"

    def test_resource_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.128Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "r",
                "event": "resource",
                "cpu_percent": 1.39,
                "memory_mb": 11.38,
            }
        )
        row = project_line(line)
        assert row is not None
        assert abs(row["cpu_percent"] - 1.39) < 0.01
        assert abs(row["memory_mb"] - 11.38) < 0.01

    def test_connected_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:50.002908800Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "r",
                "event": "connected",
                "launch_ts": "2026-04-15T09:35:49.946206400Z",
                "elapsed_ms": 56.6997,
            }
        )
        row = project_line(line)
        assert row is not None
        assert abs(row["elapsed_ms"] - 56.6997) < 0.001

    def test_clock_sync_event(self) -> None:
        """``clock_sync`` lines populate peer/offset_ms/rtt_ms columnar fields.

        The diagnostic-only fields ``samples``/``min_rtt_ms``/``max_rtt_ms``
        live in the JSONL line for debugging but are not part of
        ``SHARD_SCHEMA``. ``project_line`` should accept them silently
        without surfacing them as columns.
        """
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:50.000Z",
                "variant": "",  # initial sync, before any variant
                "runner": "alice",
                "run": "udp-all",
                "event": "clock_sync",
                "peer": "bob",
                "offset_ms": 50.123,
                "rtt_ms": 0.42,
                "samples": 32,
                "min_rtt_ms": 0.42,
                "max_rtt_ms": 1.4,
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["event"] == "clock_sync"
        assert row["variant"] == ""
        assert row["runner"] == "alice"
        assert row["peer"] == "bob"
        assert abs(row["offset_ms"] - 50.123) < 1e-9
        assert abs(row["rtt_ms"] - 0.42) < 1e-9
        # Required-shape contract: every SHARD_SCHEMA column is present
        # and unrelated fields are null.
        assert set(row.keys()) == set(SHARD_SCHEMA.keys())
        assert row["seq"] is None
        assert row["path"] is None
        assert row["writer"] is None
        # Diagnostic-only fields are silently ignored.
        assert "samples" not in row
        assert "min_rtt_ms" not in row
        assert "max_rtt_ms" not in row

    def test_clock_sync_per_variant_resync(self) -> None:
        """Per-variant resync rows carry the variant about to start."""
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:55.000Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "udp-all",
                "event": "clock_sync",
                "peer": "bob",
                "offset_ms": 51.0,
                "rtt_ms": 0.5,
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["variant"] == "custom-udp"
        assert row["peer"] == "bob"
        assert row["offset_ms"] == 51.0

    def test_gap_events(self) -> None:
        gd = project_line(
            json.dumps(
                {
                    "ts": "2026-04-15T09:35:51.000Z",
                    "variant": "v",
                    "runner": "a",
                    "run": "r",
                    "event": "gap_detected",
                    "writer": "bob",
                    "missing_seq": 7,
                }
            )
        )
        assert gd is not None
        assert gd["missing_seq"] == 7

        gf = project_line(
            json.dumps(
                {
                    "ts": "2026-04-15T09:35:51.000Z",
                    "variant": "v",
                    "runner": "a",
                    "run": "r",
                    "event": "gap_filled",
                    "writer": "bob",
                    "recovered_seq": 7,
                }
            )
        )
        assert gf is not None
        assert gf["recovered_seq"] == 7

    def test_empty_line(self) -> None:
        assert project_line("") is None
        assert project_line("   ") is None

    def test_invalid_json(self) -> None:
        assert project_line("not json") is None

    def test_missing_required_field(self) -> None:
        assert project_line(json.dumps({"ts": "2026-04-15T09:35:50Z"})) is None


class TestIterRows:
    def test_skips_invalid_lines(self) -> None:
        text = "\n".join(
            [
                "not json",
                json.dumps(
                    {
                        "ts": "2026-04-15T09:35:50.000Z",
                        "variant": "t",
                        "runner": "a",
                        "run": "r",
                        "event": "phase",
                        "phase": "connect",
                    }
                ),
                "",
            ]
        )
        rows = list(iter_rows(io.StringIO(text)))
        assert len(rows) == 1
        assert rows[0]["event"] == "phase"

    def test_real_file(self, tmp_path: Path) -> None:
        path = tmp_path / "x.jsonl"
        events = [
            {
                "ts": "2026-04-15T09:35:50.000Z",
                "variant": "test",
                "runner": "a",
                "run": "r1",
                "event": "phase",
                "phase": "connect",
            },
            {
                "ts": "2026-04-15T09:35:51.000Z",
                "variant": "test",
                "runner": "a",
                "run": "r1",
                "event": "write",
                "seq": 1,
                "path": "/k",
                "qos": 1,
                "bytes": 8,
            },
        ]
        with open(path, "w", encoding="utf-8") as f:
            for ev in events:
                f.write(json.dumps(ev) + "\n")
        with open(path, encoding="utf-8") as f:
            rows = list(iter_rows(f))
        assert len(rows) == 2
        assert rows[0]["event"] == "phase"
        assert rows[1]["event"] == "write"
        assert rows[1]["seq"] == 1


class TestColumnOrder:
    def test_column_order_matches_schema(self) -> None:
        assert COLUMN_ORDER == tuple(SHARD_SCHEMA.keys())
