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

# Lifecycle events live in the JSONL stream since the E19 cleanup
# (T19.10c). Per-event observations (``write`` / ``receive`` / etc.)
# now flow exclusively through the compact-Parquet sibling file. Any
# event type listed here is also valid in JSONL.
_LIFECYCLE_EVENTS: frozenset[str] = frozenset(
    {
        "phase",
        "connected",
        "eot_sent",
        "eot_received",
        "eot_timeout",
        "resource",
        "clock_sync",
    }
)

# Per-event observations -- compact-Parquet only since the E19 cleanup.
_COMPACT_EVENTS: frozenset[str] = frozenset(
    {
        "write",
        "receive",
        "backpressure_skipped",
        "gap_detected",
        "gap_filled",
    }
)


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


def _ts_string_to_ns(ts: str) -> int:
    """Convert an RFC 3339 timestamp string to nanoseconds since epoch.

    Used by :func:`write_spawn_pair` to bridge the JSONL-shaped event
    dicts (where ``ts`` is a string) to the compact-Parquet writer
    (which keeps ``ts_ns`` as an integer).
    """
    from parse import parse_timestamp_ns

    ns = parse_timestamp_ns(ts)
    if ns is None:
        raise ValueError(f"could not parse RFC 3339 timestamp: {ts!r}")
    return ns


def write_spawn_pair(
    logs_dir: Path,
    *,
    variant: str,
    runner: str,
    run: str,
    events: list[dict],
    threading_mode: str = "single",
    recv_buffer_kb: int = 4096,
) -> tuple[Path, Path]:
    """Write a post-E19 per-spawn file pair into ``logs_dir``.

    Splits ``events`` by event type and emits two files matching the
    on-disk contract:

    - ``<variant>-<runner>-<run>.jsonl`` -- lifecycle events only
      (``phase`` / ``connected`` / ``eot_*`` / ``resource`` /
      ``clock_sync``).
    - ``<variant>-<runner>-<run>.compact.parquet`` -- per-event rows
      (``write`` / ``receive`` / ``backpressure_skipped`` /
      ``gap_detected`` / ``gap_filled``), built via
      :class:`compact_fixture.CompactFixture`.

    The compact-Parquet file is written even when no per-event events
    are present; this matches variant-base's behaviour (the digest
    phase always flushes a Parquet file at spawn end, regardless of
    whether per-event rows were observed). Returns the
    ``(jsonl_path, compact_path)`` pair.

    Used by tests that previously wrote synthetic per-event JSONL --
    after the E19 cleanup that path no longer feeds the analyzer.
    """
    # Import lazily so helpers.py stays importable from tests that
    # don't need the compact fixture builder.
    from compact_fixture import CompactFixture

    jsonl_events: list[dict] = []
    fx = CompactFixture(
        variant=variant,
        runner=runner,
        run=run,
        threading_mode=threading_mode,
        recv_buffer_kb=recv_buffer_kb,
    )
    for ev in events:
        kind = ev["event"]
        if kind in _LIFECYCLE_EVENTS:
            jsonl_events.append(ev)
            # Variant-base T18.2b mirrors lifecycle rows into the
            # compact-Parquet sibling so the digest is self-contained;
            # mirror that here. The compact-Parquet wins for shard
            # derivation when both formats are present, so the
            # analyzer needs lifecycle rows on the compact side.
            ts_ns = _ts_string_to_ns(ev["ts"])
            if kind == "phase":
                fx.push_phase(ts_ns, str(ev.get("phase", "")))
            elif kind == "connected":
                fx.push_connected(
                    ts_ns=ts_ns,
                    peer=None,
                    elapsed_ms=float(ev.get("elapsed_ms", 0.0)),
                    threading_mode=str(ev.get("threading_mode", threading_mode)),
                )
            elif kind == "eot_sent":
                fx.push_eot_sent(ts_ns, int(ev.get("eot_id", 0)))
            elif kind == "eot_received":
                fx.push_eot_received(
                    ts_ns,
                    str(ev.get("writer", "")),
                    int(ev.get("eot_id", 0)),
                )
            elif kind == "eot_timeout":
                missing = ev.get("missing", [])
                fx.push_eot_timeout(
                    ts_ns,
                    int(ev.get("wait_ms", 0)),
                    json.dumps(missing) if isinstance(missing, list) else str(missing),
                )
            elif kind == "resource":
                fx.push_resource(
                    ts_ns,
                    float(ev.get("cpu_percent", 0.0)),
                    float(ev.get("memory_mb", 0.0)),
                )
            elif kind == "clock_sync":
                fx.push_clock_sync(
                    ts_ns,
                    str(ev.get("peer", "")),
                    int(float(ev.get("offset_ms", 0.0)) * 1_000_000),
                    float(ev.get("rtt_ms", 0.0)),
                )
            continue
        if kind not in _COMPACT_EVENTS:
            # Unknown event -- let the JSONL path carry it for
            # forward-compat; the analyzer treats unknown events as
            # opaque rows.
            jsonl_events.append(ev)
            continue
        ts_ns = _ts_string_to_ns(ev["ts"])
        if kind == "write":
            fx.push_write(
                ts_ns=ts_ns,
                path=str(ev.get("path", "/")),
                qos=int(ev.get("qos", 0)),
                seq=int(ev.get("seq", 0)),
                bytes_n=int(ev.get("bytes", 0)),
            )
            # E19: encode leaf_count / shape onto the latest row via
            # post-construction columns -- the public push_write API
            # doesn't take them, so we mutate after the fact.
            leaf_count = int(ev.get("leaf_count", 1))
            shape = str(ev.get("shape", "scalar"))
            # Stash them on the fixture; the writer below merges them
            # into the Parquet output.
            fx_extra_leaf_count = getattr(fx, "_t19_10c_leaf_count", None)
            if fx_extra_leaf_count is None:
                fx_extra_leaf_count = []
                fx._t19_10c_leaf_count = fx_extra_leaf_count  # type: ignore[attr-defined]
            fx_extra_shape = getattr(fx, "_t19_10c_shape", None)
            if fx_extra_shape is None:
                fx_extra_shape = []
                fx._t19_10c_shape = fx_extra_shape  # type: ignore[attr-defined]
            # Align lengths -- one entry per row already pushed.
            while len(fx_extra_leaf_count) < len(fx.kind) - 1:
                fx_extra_leaf_count.append(None)
                fx_extra_shape.append(None)
            fx_extra_leaf_count.append(leaf_count)
            fx_extra_shape.append(shape)
        elif kind == "receive":
            fx.push_receive(
                ts_ns=ts_ns,
                writer=str(ev.get("writer", "")),
                seq=int(ev.get("seq", 0)),
                path=str(ev.get("path", "/")),
                qos=int(ev.get("qos", 0)),
                bytes_n=int(ev.get("bytes", 0)),
            )
        elif kind == "backpressure_skipped":
            fx.push_backpressure_skipped(
                ts_ns=ts_ns,
                path=str(ev.get("path", "/")),
                qos=int(ev.get("qos", 0)),
            )
        elif kind == "gap_detected":
            fx.push_gap_detected(
                ts_ns=ts_ns,
                writer=str(ev.get("writer", "")),
                missing_seq=int(ev.get("missing_seq", 0)),
            )
        elif kind == "gap_filled":
            fx.push_gap_filled(
                ts_ns=ts_ns,
                writer=str(ev.get("writer", "")),
                recovered_seq=int(ev.get("recovered_seq", 0)),
            )

    jsonl_path = logs_dir / f"{variant}-{runner}-{run}.jsonl"
    compact_path = logs_dir / f"{variant}-{runner}-{run}.compact.parquet"
    write_jsonl(jsonl_path, jsonl_events)

    # Pad any unfilled leaf_count / shape entries before writing.
    fx_extra_leaf_count = getattr(fx, "_t19_10c_leaf_count", None)
    fx_extra_shape = getattr(fx, "_t19_10c_shape", None)
    if fx_extra_leaf_count is not None:
        while len(fx_extra_leaf_count) < len(fx.kind):
            fx_extra_leaf_count.append(None)
            fx_extra_shape.append(None)

    fx.write(compact_path)

    # Merge in leaf_count / shape_idx columns + shape_intern metadata
    # so the loader sees the E19 fields. Mirrors how
    # ``test_workload_shape.test_compact_parquet_with_leaf_count_and_shape``
    # synthesises the columns after-the-fact.
    if fx_extra_leaf_count is not None and any(
        v is not None for v in fx_extra_leaf_count
    ):
        shape_intern: list[str] = []
        intern_lookup: dict[str, int] = {}
        shape_idxs: list[int | None] = []
        for s in fx_extra_shape:  # type: ignore[union-attr]
            if s is None:
                shape_idxs.append(None)
                continue
            idx = intern_lookup.get(s)
            if idx is None:
                idx = len(shape_intern)
                shape_intern.append(s)
                intern_lookup[s] = idx
            shape_idxs.append(idx)

        raw = pl.read_parquet(str(compact_path))
        augmented = raw.with_columns(
            pl.Series("leaf_count", fx_extra_leaf_count, dtype=pl.UInt32),
            pl.Series("shape_idx", shape_idxs, dtype=pl.UInt32),
        )
        metadata = {
            "schema_version": str(fx.schema_version),
            "paths": json.dumps(fx.paths),
            "peers": json.dumps(fx.peers),
            "variant": fx.variant,
            "runner": fx.runner,
            "run": fx.run,
            "threading_mode": fx.threading_mode,
            "recv_buffer_kb": str(fx.recv_buffer_kb),
            "shape_intern": json.dumps(shape_intern),
        }
        augmented.write_parquet(compact_path, compression="snappy", metadata=metadata)

    return jsonl_path, compact_path
