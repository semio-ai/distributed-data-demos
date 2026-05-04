"""Shared test helpers for analysis tests."""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

import polars as pl

# Add the analysis package root to sys.path so imports work without install
_ANALYSIS_ROOT = Path(__file__).resolve().parent.parent
if str(_ANALYSIS_ROOT) not in sys.path:
    sys.path.insert(0, str(_ANALYSIS_ROOT))

from parse import project_line  # noqa: E402
from schema import SHARD_SCHEMA  # noqa: E402

TWO_RUNNER_LOGS = Path(__file__).resolve().parent.parent.parent / "logs"


def _ts(offset_ms: float = 0.0) -> str:
    """Generate an RFC 3339 timestamp with a millisecond offset from a base time."""
    base_ns = 1744710950_000_000_000  # 2025-04-15T09:35:50Z approx
    ns = base_ns + int(offset_ms * 1_000_000)
    secs = ns // 1_000_000_000
    frac = ns % 1_000_000_000
    dt = datetime.fromtimestamp(secs, tz=timezone.utc)
    return dt.strftime(f"%Y-%m-%dT%H:%M:%S.{frac:09d}Z")


def make_event(
    event: str,
    runner: str = "alice",
    variant: str = "test-variant",
    run: str = "run01",
    offset_ms: float = 0.0,
    **extra: object,
) -> dict:
    """Build a JSONL event dict."""
    obj: dict = {
        "ts": _ts(offset_ms),
        "variant": variant,
        "runner": runner,
        "run": run,
        "event": event,
    }
    obj.update(extra)
    return obj


def write_jsonl(path: Path, events: list[dict]) -> None:
    """Write a list of event dicts as JSONL to a file."""
    with open(path, "w", encoding="utf-8") as f:
        for ev in events:
            f.write(json.dumps(ev) + "\n")


def events_to_lazy(events: list[dict]) -> pl.LazyFrame:
    """Convert a list of JSONL event dicts to a polars LazyFrame.

    Mirrors what the cache pipeline produces: project each dict via
    ``parse.project_line`` and assemble a ``pl.DataFrame`` typed against
    ``SHARD_SCHEMA``.
    """
    rows = []
    for ev in events:
        line = json.dumps(ev)
        row = project_line(line)
        if row is not None:
            rows.append(row)
    if not rows:
        return pl.DataFrame(schema=SHARD_SCHEMA).lazy()
    return pl.DataFrame(rows, schema=SHARD_SCHEMA, orient="row").lazy()
