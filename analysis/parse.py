"""JSONL parsing and data model for benchmark log analysis."""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path


@dataclass
class Event:
    """A single parsed log event from a JSONL line."""

    ts: datetime
    variant: str
    runner: str
    run: str
    event: str
    data: dict = field(default_factory=dict)


@dataclass
class DeliveryRecord:
    """A correlated write-receive pair representing one delivery."""

    variant: str
    run: str
    path: str
    seq: int
    qos: int
    writer: str
    receiver: str
    write_ts: datetime
    receive_ts: datetime
    latency_ms: float


def parse_timestamp(ts_str: str) -> datetime:
    """Parse an RFC 3339 timestamp with nanosecond precision.

    Python's datetime only supports microsecond precision, so we truncate
    nanoseconds to microseconds.
    """
    # Handle nanosecond precision by truncating to microseconds.
    # Format: 2026-04-15T09:35:50.000178400Z
    # We need to handle the fractional seconds carefully.
    if "." in ts_str:
        base, frac_and_tz = ts_str.split(".", 1)
        # Separate fractional seconds from timezone suffix
        frac = ""
        tz_suffix = ""
        for i, ch in enumerate(frac_and_tz):
            if ch in ("Z", "+", "-"):
                frac = frac_and_tz[:i]
                tz_suffix = frac_and_tz[i:]
                break
        else:
            frac = frac_and_tz
            tz_suffix = ""

        # Truncate to 6 digits (microseconds)
        frac = frac[:6].ljust(6, "0")
        ts_str = f"{base}.{frac}{tz_suffix}"

    # Normalize Z to +00:00
    if ts_str.endswith("Z"):
        ts_str = ts_str[:-1] + "+00:00"

    return datetime.fromisoformat(ts_str)


def parse_line(line: str) -> Event | None:
    """Parse a single JSONL line into an Event.

    Returns None if the line is empty or cannot be parsed.
    """
    line = line.strip()
    if not line:
        return None

    try:
        obj = json.loads(line)
    except json.JSONDecodeError:
        return None

    # Required fields
    required = ("ts", "variant", "runner", "run", "event")
    for key in required:
        if key not in obj:
            return None

    ts = parse_timestamp(obj["ts"])
    event_type = obj["event"]

    # Collect event-specific data (everything except the common fields)
    common_keys = {"ts", "variant", "runner", "run", "event"}
    data = {k: v for k, v in obj.items() if k not in common_keys}

    return Event(
        ts=ts,
        variant=obj["variant"],
        runner=obj["runner"],
        run=obj["run"],
        event=event_type,
        data=data,
    )


def parse_file(path: Path) -> list[Event]:
    """Parse all events from a JSONL file.

    Skips lines that fail to parse.
    """
    events: list[Event] = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            event = parse_line(line)
            if event is not None:
                events.append(event)
    return events
