"""Columnar schema for the per-shard Parquet cache.

Single source of truth for the columnar layout used by both the
streaming ingester (``parse.py`` / ``cache.py``) and the analysis
readers (``correlate.py`` / ``integrity.py`` / ``performance.py``).

See ``metak-shared/ANALYSIS.md`` section 4.1 for the design.
"""

from __future__ import annotations

import polars as pl

# Bump this string when ``SHARD_SCHEMA`` changes in a non-additive way.
# Bumping forces every cached shard to be rebuilt on the next run via the
# global ``_cache_schema_version.json`` sentinel.
SCHEMA_VERSION: str = "2"

# Flat columnar event schema. One row per JSONL line. Event-specific
# fields share the same row; columns that don't apply to a given event
# type are null.
#
# Categorical encoding for ``variant``, ``runner``, ``run`` and
# ``event`` is essential -- they are low-cardinality (under ~50 distinct
# values across an entire dataset) and the dictionary encoding shrinks
# them dramatically vs storing them as Utf8.
SHARD_SCHEMA: dict[str, pl.DataType] = {
    "ts": pl.Datetime("ns", "UTC"),
    "variant": pl.Categorical,
    "runner": pl.Categorical,
    "run": pl.Categorical,
    "event": pl.Categorical,
    "seq": pl.Int64,
    "path": pl.Utf8,
    "writer": pl.Utf8,
    "qos": pl.Int8,
    "elapsed_ms": pl.Float64,
    "phase": pl.Utf8,
    "missing_seq": pl.Int64,
    "recovered_seq": pl.Int64,
    "cpu_percent": pl.Float32,
    "memory_mb": pl.Float32,
    # Reserved for clock-sync (E8). Always null in current logs but
    # part of the schema so that landing E8 does not require a rebuild.
    "peer": pl.Utf8,
    "offset_ms": pl.Float64,
    "rtt_ms": pl.Float64,
    # End-of-test (EOT) handshake (E12). ``eot_id`` is populated for both
    # ``eot_sent`` (writer's id) and ``eot_received`` (the writer's id as
    # observed by the receiver). ``eot_missing`` is only populated on
    # ``eot_timeout`` events; the variable-length ``missing`` array from
    # the JSONL line is JSON-encoded into a Utf8 column so it fits the
    # fixed columnar schema. ``wait_ms`` is the wall-clock duration of
    # the wait, only populated for ``eot_timeout``.
    "eot_id": pl.UInt64,
    "eot_missing": pl.Utf8,
    "wait_ms": pl.UInt64,
}


# All known event types from ``api-contracts/jsonl-log-schema.md``.
KNOWN_EVENTS: frozenset[str] = frozenset(
    {
        "connected",
        "phase",
        "write",
        "receive",
        "gap_detected",
        "gap_filled",
        "resource",
        "clock_sync",
        "eot_sent",
        "eot_received",
        "eot_timeout",
    }
)
