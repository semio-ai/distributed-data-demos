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
#
# Bumped to "3" by T11.5: the ``connected`` event gained a
# ``threading_mode`` field (E14) which we project into a new
# ``threading_mode`` column. Older shards predating this column would
# fail the schema-equality check; bumping forces a rebuild from the
# source JSONL so all shards carry the new column (null for pre-E14
# connected lines).
#
# Bumped to "4" by T18.4 (E18): the cache pipeline now ingests the
# T18.2 ``.compact.parquet`` source format in addition to legacy JSONL.
# The compact loader's projected output differs from the JSONL-derived
# projection in a few non-load-bearing places -- specifically the
# ``clock_sync`` slot mapping (``offset_ns -> offset_ms`` via the
# polymorphic ``extra_i64`` column) and the EOT field handling (the
# ``eot_id`` / ``wait_ms`` columns are sourced from the compact
# ``extra_i64`` slot, with an unsigned cast). Bumping forces a
# rebuild so caches built from JSONL on a v3 run get re-projected
# through the unified v4 pipeline when a compact file appears.
#
# Bumped to "5" by T19.5 (E19): the ``write`` event gained
# ``leaf_count`` / ``shape`` fields and the analysis pipeline now also
# captures the ``bytes`` payload size. Three new columns join
# ``SHARD_SCHEMA``: ``leaf_count`` (UInt32, default 1 on write rows
# from pre-E19 logs), ``shape`` (Utf8, default ``"scalar"`` on write
# rows from pre-E19 logs), and ``bytes`` (Int64, optional). These are
# additive in the strict Parquet sense (older shards would simply
# lack the columns) but the downstream lazy pipeline references them
# unconditionally -- correlate.py copies ``leaf_count`` / ``shape`` /
# ``bytes`` from writes onto correlated receives, and performance.py
# derives ``leaves_per_sec`` / ``bytes_per_sec``. Bumping forces a
# rebuild so existing caches get the new columns.
SCHEMA_VERSION: str = "5"

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
    # Threading-mode dimension (E14 / T11.5). Populated on ``connected``
    # events from the runner-injected --threading-mode CLI flag. Value
    # is one of ``"single"`` / ``"multi"`` per the contract in
    # ``api-contracts/variant-cli.md``. Optional during E14 rollout:
    # pre-T14.8 logs omit the JSON field, in which case the column is
    # null and the analysis defaults the grouping value to ``"single"``.
    "threading_mode": pl.Utf8,
    # Receive-buffer hint (E14): the --recv-buffer-kb value the runner
    # supplied to the variant on launch. Recorded alongside
    # threading_mode for offline reproducibility; not currently used by
    # the analysis pipeline but kept in-schema so future grouping or
    # plotting work can reach it without a rebuild.
    "recv_buffer_kb": pl.UInt32,
    # Workload-shape dimension (E19 / T19.5). ``leaf_count`` is the
    # number of scalar leaves carried by a WriteOp (1 for scalar-flood;
    # > 1 for block-flood / mixed-types). ``shape`` is one of
    # ``"scalar"`` / ``"array"`` / ``"struct"``. Both are populated only
    # on ``write`` rows; correlate.py propagates them onto matching
    # ``receive`` rows by the (writer, seq, path) key. Legacy JSONL /
    # compact-Parquet pre-E19 default to ``leaf_count = 1`` and
    # ``shape = "scalar"`` per the api-contracts.
    "leaf_count": pl.UInt32,
    "shape": pl.Utf8,
    # Serialized payload size for ``write`` / ``receive`` events. The
    # JSONL contract has always recorded this on the per-event line, but
    # the analysis pipeline previously dropped it. E19 introduces
    # ``bytes_per_sec`` as a headline throughput metric so we now keep
    # the value in-schema. Null on event types that have no payload
    # (phase / connected / resource / etc.).
    "bytes": pl.Int64,
}


# All known event types from ``api-contracts/jsonl-log-schema.md``.
KNOWN_EVENTS: frozenset[str] = frozenset(
    {
        "connected",
        "phase",
        "write",
        "backpressure_skipped",
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
