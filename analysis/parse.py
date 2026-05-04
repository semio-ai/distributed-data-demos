"""Streaming JSONL line-to-row projection for the per-shard Parquet cache.

Replaces the Phase 1 ``Event`` dataclass: there are no in-memory event
objects in the new pipeline. Each line is projected directly into a
columnar row dict matching ``schema.SHARD_SCHEMA`` and accumulated in a
batch buffer; the buffer is flushed as a polars ``DataFrame`` row group
to disk by ``cache.py``.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from typing import Iterator, TextIO

from schema import SHARD_SCHEMA

# Column order is the dict-insertion order of SHARD_SCHEMA; we precompute
# it once and reuse it everywhere we materialize batches as DataFrames.
COLUMN_ORDER: tuple[str, ...] = tuple(SHARD_SCHEMA.keys())

# Common required fields on every JSONL line.
_REQUIRED_FIELDS: tuple[str, ...] = ("ts", "variant", "runner", "run", "event")


def parse_timestamp_ns(ts_str: str) -> int | None:
    """Parse an RFC 3339 timestamp into a UTC nanosecond integer.

    Returns ``None`` if the string cannot be parsed.

    Polars ``Datetime("ns", "UTC")`` columns are backed by 64-bit
    nanoseconds-since-epoch integers, so we project directly into that
    representation rather than going through Python's ``datetime`` (which
    only supports microsecond precision).
    """
    # Format: 2026-04-15T09:35:50.000178400Z (or with +HH:MM offset)
    if not ts_str:
        return None

    # Find the timezone suffix
    tz_offset_seconds = 0
    s = ts_str
    if s.endswith("Z"):
        s = s[:-1]
    else:
        # Look for trailing +HH:MM or -HH:MM. Skip the leading sign-or-T
        # date dashes by scanning from the right.
        for i in range(len(s) - 1, max(len(s) - 7, 0), -1):
            ch = s[i]
            if ch in ("+", "-") and i >= 10:  # date is YYYY-MM-DD = 10 chars
                tz_part = s[i:]
                s = s[:i]
                # tz_part like "+05:30" or "-08:00"
                try:
                    sign = 1 if tz_part[0] == "+" else -1
                    hh, mm = tz_part[1:].split(":")
                    tz_offset_seconds = sign * (int(hh) * 3600 + int(mm) * 60)
                except (ValueError, IndexError):
                    return None
                break

    # s is now "YYYY-MM-DDTHH:MM:SS" optionally followed by ".fractional"
    if "." in s:
        date_time, frac = s.split(".", 1)
    else:
        date_time, frac = s, ""

    # Parse the date+time portion
    try:
        dt = datetime.strptime(date_time, "%Y-%m-%dT%H:%M:%S").replace(
            tzinfo=timezone.utc
        )
    except ValueError:
        return None

    epoch_seconds = int(dt.timestamp()) - tz_offset_seconds

    # Build nanosecond fraction. Pad/truncate to exactly 9 digits.
    if frac:
        frac = frac[:9].ljust(9, "0")
        try:
            nanos = int(frac)
        except ValueError:
            return None
    else:
        nanos = 0

    return epoch_seconds * 1_000_000_000 + nanos


def project_line(line: str) -> dict | None:
    """Project a single JSONL line into a row dict matching SHARD_SCHEMA.

    Returns ``None`` if the line is empty, malformed, or missing any of
    the required common fields.

    The returned dict has every column in ``SHARD_SCHEMA`` present, with
    ``None`` for columns that don't apply to the line's event type. This
    keeps ``polars.DataFrame`` construction predictable across batches.
    """
    line = line.strip()
    if not line:
        return None

    try:
        obj = json.loads(line)
    except json.JSONDecodeError:
        return None

    for key in _REQUIRED_FIELDS:
        if key not in obj:
            return None

    ts_ns = parse_timestamp_ns(obj["ts"])
    if ts_ns is None:
        return None

    event_type = obj["event"]

    # qos is logged as an int 1..4 but our column dtype is Int8.
    qos_raw = obj.get("qos")
    qos: int | None = int(qos_raw) if qos_raw is not None else None

    # path is sometimes serialized as something non-string; coerce.
    path_raw = obj.get("path")
    path: str | None = str(path_raw) if path_raw is not None else None

    # writer is a string. seq is an int.
    writer_raw = obj.get("writer")
    writer: str | None = str(writer_raw) if writer_raw is not None else None

    seq_raw = obj.get("seq")
    seq: int | None = int(seq_raw) if seq_raw is not None else None

    elapsed_ms_raw = obj.get("elapsed_ms")
    elapsed_ms: float | None = (
        float(elapsed_ms_raw) if elapsed_ms_raw is not None else None
    )

    phase_raw = obj.get("phase") if event_type == "phase" else None
    phase: str | None = str(phase_raw) if phase_raw is not None else None

    missing_seq_raw = obj.get("missing_seq")
    missing_seq: int | None = (
        int(missing_seq_raw) if missing_seq_raw is not None else None
    )

    recovered_seq_raw = obj.get("recovered_seq")
    recovered_seq: int | None = (
        int(recovered_seq_raw) if recovered_seq_raw is not None else None
    )

    cpu_raw = obj.get("cpu_percent")
    cpu_percent: float | None = float(cpu_raw) if cpu_raw is not None else None

    mem_raw = obj.get("memory_mb")
    memory_mb: float | None = float(mem_raw) if mem_raw is not None else None

    peer_raw = obj.get("peer")
    peer: str | None = str(peer_raw) if peer_raw is not None else None

    offset_ms_raw = obj.get("offset_ms")
    offset_ms: float | None = (
        float(offset_ms_raw) if offset_ms_raw is not None else None
    )

    rtt_ms_raw = obj.get("rtt_ms")
    rtt_ms: float | None = float(rtt_ms_raw) if rtt_ms_raw is not None else None

    # EOT (E12). ``eot_id`` is present on ``eot_sent`` and ``eot_received``;
    # ``eot_timeout`` carries a ``missing`` array (re-encoded as JSON
    # string for the columnar shard) and a ``wait_ms`` integer.
    eot_id_raw = obj.get("eot_id")
    eot_id: int | None
    if eot_id_raw is None:
        eot_id = None
    else:
        try:
            eot_id = int(eot_id_raw)
            if eot_id < 0:
                eot_id = None
        except (TypeError, ValueError):
            eot_id = None

    eot_missing: str | None
    if event_type == "eot_timeout":
        missing_arr = obj.get("missing")
        if isinstance(missing_arr, list):
            try:
                eot_missing = json.dumps(
                    [str(x) for x in missing_arr], separators=(",", ":")
                )
            except (TypeError, ValueError):
                eot_missing = None
        else:
            eot_missing = None
    else:
        eot_missing = None

    wait_ms_raw = obj.get("wait_ms")
    wait_ms: int | None
    if wait_ms_raw is None:
        wait_ms = None
    else:
        try:
            wait_ms = int(wait_ms_raw)
            if wait_ms < 0:
                wait_ms = None
        except (TypeError, ValueError):
            wait_ms = None

    return {
        "ts": ts_ns,
        "variant": obj["variant"],
        "runner": obj["runner"],
        "run": obj["run"],
        "event": event_type,
        "seq": seq,
        "path": path,
        "writer": writer,
        "qos": qos,
        "elapsed_ms": elapsed_ms,
        "phase": phase,
        "missing_seq": missing_seq,
        "recovered_seq": recovered_seq,
        "cpu_percent": cpu_percent,
        "memory_mb": memory_mb,
        "peer": peer,
        "offset_ms": offset_ms,
        "rtt_ms": rtt_ms,
        "eot_id": eot_id,
        "eot_missing": eot_missing,
        "wait_ms": wait_ms,
    }


def iter_rows(stream: TextIO) -> Iterator[dict]:
    """Yield projected row dicts from an open JSONL text stream.

    Skips lines that fail to parse.
    """
    for line in stream:
        row = project_line(line)
        if row is not None:
            yield row
