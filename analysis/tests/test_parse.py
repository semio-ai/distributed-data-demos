"""Tests for the JSONL parsing module."""

from __future__ import annotations

import json
from datetime import timezone
from pathlib import Path

from parse import parse_file, parse_line, parse_timestamp


class TestParseTimestamp:
    def test_nanosecond_precision_truncated(self) -> None:
        ts = parse_timestamp("2026-04-15T09:35:50.000178400Z")
        assert ts.year == 2026
        assert ts.month == 4
        assert ts.day == 15
        assert ts.hour == 9
        assert ts.minute == 35
        assert ts.second == 50
        assert ts.microsecond == 178  # truncated from 000178400
        assert ts.tzinfo is not None

    def test_utc_z_suffix(self) -> None:
        ts = parse_timestamp("2026-04-15T09:35:50.123456789Z")
        assert ts.tzinfo == timezone.utc

    def test_no_fractional_seconds(self) -> None:
        ts = parse_timestamp("2026-04-15T09:35:50Z")
        assert ts.second == 50
        assert ts.microsecond == 0

    def test_microsecond_precision(self) -> None:
        ts = parse_timestamp("2026-04-15T09:35:50.123456Z")
        assert ts.microsecond == 123456


class TestParseLine:
    def test_valid_write_event(self) -> None:
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
        ev = parse_line(line)
        assert ev is not None
        assert ev.event == "write"
        assert ev.variant == "custom-udp"
        assert ev.runner == "alice"
        assert ev.run == "local-test-01"
        assert ev.data["seq"] == 1
        assert ev.data["path"] == "/bench/0"
        assert ev.data["qos"] == 1

    def test_valid_receive_event(self) -> None:
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
                "qos": 1,
                "bytes": 8,
            }
        )
        ev = parse_line(line)
        assert ev is not None
        assert ev.event == "receive"
        assert ev.data["writer"] == "bob"

    def test_valid_connected_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:50.002908800Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "local-test-01",
                "event": "connected",
                "launch_ts": "2026-04-15T09:35:49.946206400Z",
                "elapsed_ms": 56.6997,
            }
        )
        ev = parse_line(line)
        assert ev is not None
        assert ev.event == "connected"
        assert ev.data["elapsed_ms"] == 56.6997

    def test_valid_resource_event(self) -> None:
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.128359800Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "local-test-01",
                "event": "resource",
                "cpu_percent": 1.39,
                "memory_mb": 11.38,
            }
        )
        ev = parse_line(line)
        assert ev is not None
        assert ev.event == "resource"
        assert abs(ev.data["cpu_percent"] - 1.39) < 0.01

    def test_empty_line(self) -> None:
        assert parse_line("") is None
        assert parse_line("   ") is None

    def test_invalid_json(self) -> None:
        assert parse_line("not json") is None

    def test_missing_required_field(self) -> None:
        line = json.dumps({"ts": "2026-04-15T09:35:50Z", "variant": "x"})
        assert parse_line(line) is None


class TestParseFile:
    def test_parse_real_file(self, tmp_path: Path) -> None:
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
        path = tmp_path / "test.jsonl"
        with open(path, "w") as f:
            for ev in events:
                f.write(json.dumps(ev) + "\n")

        parsed = parse_file(path)
        assert len(parsed) == 2
        assert parsed[0].event == "phase"
        assert parsed[1].event == "write"

    def test_skips_invalid_lines(self, tmp_path: Path) -> None:
        path = tmp_path / "mixed.jsonl"
        with open(path, "w") as f:
            f.write("not json\n")
            f.write(
                json.dumps(
                    {
                        "ts": "2026-04-15T09:35:50.000Z",
                        "variant": "t",
                        "runner": "a",
                        "run": "r",
                        "event": "phase",
                        "phase": "connect",
                    }
                )
                + "\n"
            )
            f.write("\n")  # blank line

        parsed = parse_file(path)
        assert len(parsed) == 1
