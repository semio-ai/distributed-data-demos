"""Loader for the compact-parquet per-spawn log format (E18).

The compact format ships a single columnar ``compact_events`` table per
spawn (one Parquet file: ``<variant>-<runner>-<run>.compact.parquet``)
plus Parquet file-level key-value metadata carrying the spawn identity
and the path/peer intern dictionaries. See
``metak-shared/api-contracts/compact-log-schema.md`` for the
authoritative schema.

This module reads such a file and expands its tagged-union rows back
into a ``schema.SHARD_SCHEMA``-shaped polars ``DataFrame``, so the
downstream pivot / integrity / performance / plot pipeline continues to
operate on the same row shape it does for JSONL-derived shards.

The mapping from compact ``kind`` to JSONL-equivalent ``event`` plus
the ``extra_*`` slot semantics are pinned by
``metak-shared/api-contracts/compact-log-schema.md`` § Event kinds and
the variant-base writer in ``variant-base/src/compact.rs`` /
``compact_writer.rs``.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

import polars as pl

from schema import SHARD_SCHEMA

# Numeric ``kind`` values match the on-disk Parquet wire format. They
# are pinned by ``variant-base::compact::EventKind`` and the
# compact-log-schema contract.
KIND_WRITE: int = 0
KIND_RECEIVE: int = 1
KIND_BACKPRESSURE_SKIPPED: int = 2
KIND_GAP_DETECTED: int = 3
KIND_GAP_FILLED: int = 4
KIND_PHASE: int = 5
KIND_CONNECTED: int = 6
KIND_EOT_SENT: int = 7
KIND_EOT_RECEIVED: int = 8
KIND_EOT_TIMEOUT: int = 9
KIND_RESOURCE: int = 10
KIND_CLOCK_SYNC: int = 11

# Mapping from compact ``kind`` int to legacy ``event`` string. Stays in
# lockstep with the JSONL ``event`` field so the downstream pipeline
# discriminates by the same identifiers regardless of source format.
KIND_TO_EVENT: dict[int, str] = {
    KIND_WRITE: "write",
    KIND_RECEIVE: "receive",
    KIND_BACKPRESSURE_SKIPPED: "backpressure_skipped",
    KIND_GAP_DETECTED: "gap_detected",
    KIND_GAP_FILLED: "gap_filled",
    KIND_PHASE: "phase",
    KIND_CONNECTED: "connected",
    KIND_EOT_SENT: "eot_sent",
    KIND_EOT_RECEIVED: "eot_received",
    KIND_EOT_TIMEOUT: "eot_timeout",
    KIND_RESOURCE: "resource",
    KIND_CLOCK_SYNC: "clock_sync",
}

# Sentinel ``peer_idx`` value meaning "self / not applicable". Matches
# ``variant-base::compact::PEER_SELF``.
PEER_SELF: int = 255


@dataclass(frozen=True)
class CompactMeta:
    """Decoded Parquet KV metadata for a compact-parquet file.

    The variant writer (see ``variant-base/src/compact_writer.rs``)
    stores spawn identity + intern dictionaries here. We surface only
    the fields the analyzer consumes; missing fields default to ``None``
    or an empty list so older / partial files still load.

    ``shapes`` (E19) is the ``shape_intern`` dictionary the writer
    persists alongside ``paths`` / ``peers``; index = ``shape_idx``.
    Defaults to ``["scalar"]`` so legacy compact files (pre-E19) that
    omit the dictionary read back with the scalar shape -- this matches
    the api-contracts ``compact-log-schema.md`` § E19 additions.
    """

    schema_version: int | None
    variant: str | None
    runner: str | None
    run: str | None
    threading_mode: str | None
    recv_buffer_kb: int | None
    paths: list[str]
    peers: list[str]
    shapes: list[str]


class CompactLoadError(Exception):
    """Raised when a compact-parquet file cannot be decoded.

    Distinct from a generic ``Exception`` so the cache pipeline can
    surface the path of the offending shard with a clear message instead
    of mis-attributing the failure to the downstream polars pipeline.
    """


def is_compact_parquet(path: Path) -> bool:
    """Return True when ``path`` is the ``.compact.parquet`` extension.

    Used by the format detector in ``cache.py`` / ``parse.py`` to pick
    the right loader per spawn. We match on the exact suffix combo
    (``.compact.parquet``) rather than just ``.parquet`` so the
    analyzer's own cache shards (which sit under ``.cache/`` and use
    plain ``.parquet``) are not confused with source-level compact
    spawn files.
    """
    name = path.name
    return name.endswith(".compact.parquet")


def compact_stem(path: Path) -> str:
    """Strip the ``.compact.parquet`` suffix from a path's name.

    Returns the same ``<variant>-<runner>-<run>`` stem that the legacy
    ``<variant>-<runner>-<run>.jsonl`` would carry via ``Path.stem``.
    Keeping the stems equal means the cache layer's per-stem indexing
    works unchanged whether the source is JSONL or compact-parquet.
    """
    name = path.name
    if name.endswith(".compact.parquet"):
        return name[: -len(".compact.parquet")]
    return path.stem


def read_compact_metadata(path: Path) -> CompactMeta:
    """Read the Parquet KV metadata block from ``path``.

    Returns a [`CompactMeta`] with the fields the analyzer cares about
    decoded into native Python types. Unknown / missing keys default to
    ``None`` so a file written by a future variant version with extra
    keys is forwards-compatible.

    Path / peer intern dictionaries are JSON-encoded ``Vec<String>``
    blobs (see the contract); we ``json.loads`` them here and return
    empty lists when absent.
    """
    try:
        raw = pl.read_parquet_metadata(str(path))
    except Exception as exc:  # noqa: BLE001 -- surface as CompactLoadError
        raise CompactLoadError(
            f"failed to read parquet metadata from {path}: {exc}"
        ) from exc

    def _decode_list(key: str) -> list[str]:
        val = raw.get(key)
        if val is None:
            return []
        try:
            decoded = json.loads(val)
        except json.JSONDecodeError as exc:
            raise CompactLoadError(
                f"{path}: KV metadata key {key!r} is not valid JSON: {exc}"
            ) from exc
        if not isinstance(decoded, list):
            raise CompactLoadError(
                f"{path}: KV metadata key {key!r} must decode to a JSON array, "
                f"got {type(decoded).__name__}"
            )
        return [str(x) for x in decoded]

    def _decode_int(key: str) -> int | None:
        val = raw.get(key)
        if val is None:
            return None
        try:
            return int(val)
        except (TypeError, ValueError):
            return None

    def _decode_str(key: str) -> str | None:
        val = raw.get(key)
        return str(val) if val is not None else None

    # E19 ``shape_intern`` dictionary. Defaults to ``["scalar"]`` when
    # the key is absent (pre-E19 files) so any later ``shape_idx`` lookup
    # against index ``0`` recovers the scalar default consistently. We
    # tolerate either the canonical key name (``shape_intern``) or a
    # shorter alias (``shapes``) since the contract is freshly minted
    # and the writer's exact key name lands in T19.2 -- absence of either
    # collapses to the legacy default.
    shapes: list[str]
    if raw.get("shape_intern") is not None:
        shapes = _decode_list("shape_intern")
    elif raw.get("shapes") is not None:
        shapes = _decode_list("shapes")
    else:
        shapes = ["scalar"]
    # Defensive empty-list fallback: a writer that emits an empty
    # dictionary still needs index ``0`` to resolve to ``"scalar"`` for
    # the legacy / unset case.
    if not shapes:
        shapes = ["scalar"]

    return CompactMeta(
        schema_version=_decode_int("schema_version"),
        variant=_decode_str("variant"),
        runner=_decode_str("runner"),
        run=_decode_str("run"),
        threading_mode=_decode_str("threading_mode"),
        recv_buffer_kb=_decode_int("recv_buffer_kb"),
        paths=_decode_list("paths"),
        peers=_decode_list("peers"),
        shapes=shapes,
    )


# ----- per-kind projection helpers -----
#
# Each helper takes the raw compact-events frame (cast to predictable
# dtypes) plus the resolved intern columns and the spawn identity
# fields, filters to a single ``kind``, and returns a DataFrame whose
# columns are a strict subset of ``SHARD_SCHEMA``. The caller stacks
# the per-kind frames into a single output via ``pl.concat`` with
# ``how="diagonal"`` -- which fills missing columns with nulls.
#
# Splitting the projection by kind keeps each branch's column
# population obvious; the trade-off vs a single ``when/then`` chain
# across all 12 kinds is one extra concat at the end, which is cheap
# (one Arrow buffer slice per column) compared with the cost of
# materialising the per-row dispatch logic for hundreds of millions of
# rows in the larger benchmarks.


def _common_cols(
    frame: pl.DataFrame,
    *,
    variant: str,
    runner: str,
    run: str,
) -> pl.DataFrame:
    """Stamp the spawn identity columns + cast ``ts`` to ns-UTC datetime.

    Returns a frame with ``ts``, ``variant``, ``runner``, ``run`` set
    on every row. Per-event slots are filled by the kind-specific
    projector that calls this helper.
    """
    return frame.with_columns(
        pl.col("ts_ns").cast(pl.Datetime("ns", "UTC")).alias("ts"),
        pl.lit(variant, dtype=pl.Categorical).alias("variant"),
        pl.lit(runner, dtype=pl.Categorical).alias("runner"),
        pl.lit(run, dtype=pl.Categorical).alias("run"),
    )


def _project_write(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``Write (0)``: ts / seq / path / qos / bytes / leaf_count / shape.

    ``writer`` is left null on write rows -- the row's runner already
    identifies the source. ``bytes`` was historically dropped by the
    legacy projection; E19 (T19.5) keeps it because it feeds the new
    ``bytes_per_sec`` headline metric. ``leaf_count`` and ``shape`` are
    resolved from the compact ``leaf_count`` column and the
    ``shape_idx`` -> ``shape_intern`` lookup (already attached to the
    frame as ``shape_str``). Both default to ``1`` / ``"scalar"`` for
    legacy / pre-E19 rows.
    """
    sub = frame.filter(pl.col("kind") == KIND_WRITE)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    # ``leaf_count`` column is optional in the compact format; older
    # files don't have it. ``with_columns`` + ``fill_null`` would only
    # work if the column exists, so we branch on column-presence.
    if "leaf_count" in sub.columns:
        leaf_count_expr = (
            pl.col("leaf_count").cast(pl.UInt32, strict=False).fill_null(1)
        )
    else:
        leaf_count_expr = pl.lit(1, dtype=pl.UInt32)
    if "shape_str" in sub.columns:
        shape_expr = pl.col("shape_str").fill_null(pl.lit("scalar"))
    else:
        shape_expr = pl.lit("scalar", dtype=pl.Utf8)
    if "bytes" in sub.columns:
        bytes_expr = pl.col("bytes").cast(pl.Int64, strict=False)
    else:
        bytes_expr = pl.lit(None, dtype=pl.Int64)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("write", dtype=pl.Categorical).alias("event"),
        pl.col("seq").cast(pl.Int64),
        pl.col("path_str").alias("path"),
        pl.col("qos").cast(pl.Int8),
        bytes_expr.alias("bytes"),
        leaf_count_expr.alias("leaf_count"),
        shape_expr.alias("shape"),
    )


def _project_receive(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``Receive (1)``: ts / seq / path / writer / qos / bytes.

    ``leaf_count`` / ``shape`` are NOT carried on receive rows by the
    compact format -- the wire is opaque. The analyzer propagates them
    from the matching write row in ``correlate.py``.
    """
    sub = frame.filter(pl.col("kind") == KIND_RECEIVE)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    if "bytes" in sub.columns:
        bytes_expr = pl.col("bytes").cast(pl.Int64, strict=False)
    else:
        bytes_expr = pl.lit(None, dtype=pl.Int64)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("receive", dtype=pl.Categorical).alias("event"),
        pl.col("seq").cast(pl.Int64),
        pl.col("path_str").alias("path"),
        pl.col("peer_str").alias("writer"),
        pl.col("qos").cast(pl.Int8),
        bytes_expr.alias("bytes"),
    )


def _project_backpressure_skipped(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``BackpressureSkipped (2)``: ts / path / qos."""
    sub = frame.filter(pl.col("kind") == KIND_BACKPRESSURE_SKIPPED)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("backpressure_skipped", dtype=pl.Categorical).alias("event"),
        pl.col("path_str").alias("path"),
        pl.col("qos").cast(pl.Int8),
    )


def _project_gap_detected(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``GapDetected (3)``: ts / writer / missing_seq.

    ``missing_seq`` lives in ``extra_i64`` per the T18.2b contract;
    the legacy ``seq`` column also carries it (for backwards
    compatibility with pre-T18.2b readers), but we project from
    ``extra_i64`` to match the contract's canonical slot.
    """
    sub = frame.filter(pl.col("kind") == KIND_GAP_DETECTED)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("gap_detected", dtype=pl.Categorical).alias("event"),
        pl.col("peer_str").alias("writer"),
        pl.col("extra_i64").alias("missing_seq"),
    )


def _project_gap_filled(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``GapFilled (4)``: ts / writer / recovered_seq (from ``extra_i64``)."""
    sub = frame.filter(pl.col("kind") == KIND_GAP_FILLED)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("gap_filled", dtype=pl.Categorical).alias("event"),
        pl.col("peer_str").alias("writer"),
        pl.col("extra_i64").alias("recovered_seq"),
    )


def _project_phase(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``Phase (5)``: ts / phase (from ``extra_utf8``)."""
    sub = frame.filter(pl.col("kind") == KIND_PHASE)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("phase", dtype=pl.Categorical).alias("event"),
        pl.col("extra_utf8").alias("phase"),
    )


def _project_connected(
    frame: pl.DataFrame,
    *,
    variant: str,
    runner: str,
    run: str,
    threading_mode_meta: str | None,
    recv_buffer_kb_meta: int | None,
) -> pl.DataFrame:
    """``Connected (6)``: ts / peer / elapsed_ms / threading_mode.

    Per the contract, ``extra_utf8`` carries the threading-mode string
    and ``extra_f32`` carries ``elapsed_ms``. ``peer_idx`` resolves to
    the peer string via the intern table when not ``PEER_SELF``.

    The ``recv_buffer_kb`` column in ``SHARD_SCHEMA`` is populated from
    the spawn-level metadata (the legacy JSONL parser also reads it
    from the ``connected`` event payload; in compact format the writer
    stamps it once into the file's KV metadata). We back-fill from the
    metadata onto every ``connected`` row so the downstream grouping
    logic in ``analyze.py`` finds it where it expects.
    """
    sub = frame.filter(pl.col("kind") == KIND_CONNECTED)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    # ``peer_idx == PEER_SELF`` -> null peer name; otherwise the
    # resolved intern string.
    peer_expr = (
        pl.when(pl.col("peer_idx") == PEER_SELF)
        .then(None)
        .otherwise(pl.col("peer_str"))
        .alias("peer")
    )
    # Threading mode: prefer the per-row ``extra_utf8`` (variant
    # actually emitted it); fall back to the spawn-level KV metadata
    # when the row is null. This keeps parity with the legacy JSONL
    # parser, which reads the ``threading_mode`` field directly off the
    # ``connected`` line.
    if threading_mode_meta is not None:
        threading_expr = (
            pl.col("extra_utf8")
            .fill_null(pl.lit(threading_mode_meta))
            .alias("threading_mode")
        )
    else:
        threading_expr = pl.col("extra_utf8").alias("threading_mode")
    recv_buf_expr = pl.lit(recv_buffer_kb_meta, dtype=pl.UInt32).alias("recv_buffer_kb")
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("connected", dtype=pl.Categorical).alias("event"),
        peer_expr,
        pl.col("extra_f32").cast(pl.Float64).alias("elapsed_ms"),
        threading_expr,
        recv_buf_expr,
    )


def _project_eot_sent(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``EotSent (7)``: ts / eot_id (from ``extra_i64``)."""
    sub = frame.filter(pl.col("kind") == KIND_EOT_SENT)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("eot_sent", dtype=pl.Categorical).alias("event"),
        pl.col("extra_i64").cast(pl.UInt64, strict=False).alias("eot_id"),
    )


def _project_eot_received(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``EotReceived (8)``: ts / writer / eot_id."""
    sub = frame.filter(pl.col("kind") == KIND_EOT_RECEIVED)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("eot_received", dtype=pl.Categorical).alias("event"),
        pl.col("peer_str").alias("writer"),
        pl.col("extra_i64").cast(pl.UInt64, strict=False).alias("eot_id"),
    )


def _project_eot_timeout(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``EotTimeout (9)``: ts / wait_ms / eot_missing.

    ``extra_i64`` is the wait-millisecond duration, ``extra_utf8`` is a
    JSON array of missing peer names (already JSON-encoded by the
    writer; the legacy JSONL parser re-encodes the ``missing`` array
    to the same shape).
    """
    sub = frame.filter(pl.col("kind") == KIND_EOT_TIMEOUT)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("eot_timeout", dtype=pl.Categorical).alias("event"),
        pl.col("extra_i64").cast(pl.UInt64, strict=False).alias("wait_ms"),
        pl.col("extra_utf8").alias("eot_missing"),
    )


def _project_resource(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``Resource (10)``: ts / cpu_percent / memory_mb."""
    sub = frame.filter(pl.col("kind") == KIND_RESOURCE)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("resource", dtype=pl.Categorical).alias("event"),
        pl.col("extra_f32").cast(pl.Float32).alias("cpu_percent"),
        pl.col("extra_f32_b").cast(pl.Float32).alias("memory_mb"),
    )


def _project_clock_sync(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``ClockSync (11)``: ts / peer / offset_ms / rtt_ms.

    Reserved for E8 -- no variant currently emits this kind. The
    column mapping is part of the contract so this loader still
    handles it when E8 lands. ``extra_i64`` carries ``offset_ns``;
    we convert to ``offset_ms`` because that is what ``SHARD_SCHEMA``
    holds (and what the legacy JSONL parser reads off the JSON line).
    """
    sub = frame.filter(pl.col("kind") == KIND_CLOCK_SYNC)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("clock_sync", dtype=pl.Categorical).alias("event"),
        pl.col("peer_str").alias("peer"),
        (pl.col("extra_i64").cast(pl.Float64) / 1_000_000.0).alias("offset_ms"),
        pl.col("extra_f32").cast(pl.Float64).alias("rtt_ms"),
    )


def _empty_shard_frame() -> pl.DataFrame:
    """Return an empty DataFrame with every ``SHARD_SCHEMA`` column.

    Used as the result of a per-kind projector when the source frame
    has zero rows of that kind. Keeps the downstream ``pl.concat`` happy
    by guaranteeing the same column-name universe across all branches.
    """
    return pl.DataFrame(schema=SHARD_SCHEMA)


def _validate_meta(path: Path, meta: CompactMeta) -> None:
    """Raise :class:`CompactLoadError` if spawn identity is incomplete."""
    if meta.variant is None or meta.runner is None or meta.run is None:
        raise CompactLoadError(
            f"{path}: compact-parquet KV metadata is missing one of "
            f"variant/runner/run (got variant={meta.variant!r}, "
            f"runner={meta.runner!r}, run={meta.run!r})"
        )


def _intern_lookup_df(
    idx_col: str,
    str_col: str,
    values: list[str],
    *,
    idx_dtype: pl.DataType = pl.Int32,
) -> pl.DataFrame:
    """Build a small in-memory intern-lookup DataFrame.

    ``values`` is the intern dictionary (a Python list of distinct
    strings); the returned frame has two columns -- the index and the
    interned string -- so the per-row resolution can be expressed as
    a left-join on the index.
    """
    return pl.DataFrame(
        {idx_col: list(range(len(values))), str_col: values},
        schema={idx_col: idx_dtype, str_col: pl.Utf8},
    )


def _build_projection_lazyframe(
    source: pl.LazyFrame,
    *,
    meta: CompactMeta,
    sort_by_ts: bool = True,
) -> pl.LazyFrame:
    """Project a compact-events LazyFrame into ``SHARD_SCHEMA`` shape.

    Single-pass lazy projection: every ``SHARD_SCHEMA`` column is
    expressed as one ``when/then`` chain over the compact ``kind``
    column. This avoids materialising 12 separate per-kind DataFrames
    (the legacy implementation) plus the ``how="diagonal"`` concat,
    so peak memory is bounded by the streaming engine's row-group
    buffer rather than by ``2x`` the input file size.

    ``sort_by_ts`` (default ``True``) appends a final ``.sort("ts")``
    so the returned frame matches the JSONL-derived ordering. The sort
    is a barrier for the streaming engine -- it forces a full
    materialisation -- so the streaming cache-build path
    (:func:`stream_compact_to_parquet`) passes ``sort_by_ts=False``
    and relies on the source compact-Parquet already being ts-ordered
    (variant writers emit events in wall-clock order). The eager
    :func:`read_compact_parquet` path keeps ``sort_by_ts=True`` to
    preserve its public contract.

    Caller is responsible for kicking off execution -- either by
    ``collect()`` (legacy ``read_compact_parquet`` path) or
    ``sink_parquet()`` (new streaming cache build path).
    """
    variant = meta.variant
    runner = meta.runner
    run = meta.run
    # Spawn identity is validated by the caller. Mypy needs the
    # narrowing -- the assertions are cheap and document the contract.
    assert variant is not None and runner is not None and run is not None

    # Resolve intern indices via small in-memory lookups joined lazily.
    # The dictionaries are tiny (under ~200 entries on any realistic
    # workload), so building them eagerly and turning them into lazy
    # frames keeps the join cheap.
    paths_lf = _intern_lookup_df(
        "path_idx", "path_str", meta.paths, idx_dtype=pl.Int32
    ).lazy()
    peers_lf = _intern_lookup_df(
        "peer_idx", "peer_str", meta.peers, idx_dtype=pl.Int32
    ).lazy()
    shapes_lf = _intern_lookup_df(
        "shape_idx", "shape_str", meta.shapes, idx_dtype=pl.UInt32
    ).lazy()

    # The variant's Parquet writer encodes physical types: ts_ns:i64,
    # kind/path_idx/peer_idx/qos/bytes:i32, seq:i64,
    # extra_f32/extra_f32_b:f32, extra_i64:i64, extra_utf8:utf8.
    # Cast to canonical Python-side dtypes so downstream filters /
    # arithmetic don't see Int32-vs-Int64 surprises.
    base = source.with_columns(
        pl.col("ts_ns").cast(pl.Int64),
        pl.col("kind").cast(pl.Int32),
        pl.col("seq").cast(pl.Int64),
        pl.col("path_idx").cast(pl.Int32),
        pl.col("peer_idx").cast(pl.Int32),
        pl.col("qos").cast(pl.Int32),
    )

    # The compact format may or may not carry every optional column,
    # depending on the writer version. Polars ``scan_parquet`` exposes
    # the schema lazily via ``collect_schema``; we synthesize any
    # missing column as a null literal so the downstream projection
    # can reference them unconditionally.
    base_schema = base.collect_schema()
    base_columns = set(base_schema.names())
    fillers: list[pl.Expr] = []
    if "bytes" not in base_columns:
        fillers.append(pl.lit(None, dtype=pl.Int64).alias("bytes"))
    if "leaf_count" not in base_columns:
        fillers.append(pl.lit(None, dtype=pl.UInt32).alias("leaf_count"))
    if "shape_idx" not in base_columns:
        fillers.append(pl.lit(None, dtype=pl.UInt32).alias("shape_idx"))
    if "extra_f32" not in base_columns:
        fillers.append(pl.lit(None, dtype=pl.Float32).alias("extra_f32"))
    if "extra_f32_b" not in base_columns:
        fillers.append(pl.lit(None, dtype=pl.Float32).alias("extra_f32_b"))
    if "extra_i64" not in base_columns:
        fillers.append(pl.lit(None, dtype=pl.Int64).alias("extra_i64"))
    if "extra_utf8" not in base_columns:
        fillers.append(pl.lit(None, dtype=pl.Utf8).alias("extra_utf8"))
    if fillers:
        base = base.with_columns(fillers)

    # Now coerce existing physical types where present.
    coercions: list[pl.Expr] = []
    if "bytes" in base_columns:
        coercions.append(pl.col("bytes").cast(pl.Int64, strict=False))
    if "leaf_count" in base_columns:
        coercions.append(pl.col("leaf_count").cast(pl.UInt32, strict=False))
    if "shape_idx" in base_columns:
        coercions.append(pl.col("shape_idx").cast(pl.UInt32, strict=False))
    if coercions:
        base = base.with_columns(coercions)

    # Resolve intern indices to strings via left joins. ``shape_intern``
    # always has at least ``["scalar"]`` (see ``read_compact_metadata``)
    # so the shape join always resolves index 0 -> "scalar".
    enriched = (
        base.join(paths_lf, on="path_idx", how="left")
        .join(peers_lf, on="peer_idx", how="left")
        .join(shapes_lf, on="shape_idx", how="left")
    )

    # ---- Per-kind conditional projection of every SHARD_SCHEMA column. ----
    #
    # Each ``event`` value is determined by the ``kind`` integer per
    # the contract. Each *output* column is one ``when/then/.../otherwise``
    # chain; columns not populated for a given kind default to null.
    # This is the single-pass equivalent of the legacy 12-way concat.
    #
    # ``threading_mode`` is sourced from the per-row ``extra_utf8`` on
    # ``connected`` rows, with the spawn-level metadata as a fallback;
    # ``recv_buffer_kb`` is purely metadata-stamped (the contract puts
    # it on the file rather than the row).
    threading_mode_meta = meta.threading_mode
    recv_buffer_kb_meta = meta.recv_buffer_kb

    # Convenience: which kinds produce a value for each column?
    kw, kr, kbp, kgd, kgf = (
        KIND_WRITE,
        KIND_RECEIVE,
        KIND_BACKPRESSURE_SKIPPED,
        KIND_GAP_DETECTED,
        KIND_GAP_FILLED,
    )
    kph, kc, kes, ker, ket, kres, kcs = (
        KIND_PHASE,
        KIND_CONNECTED,
        KIND_EOT_SENT,
        KIND_EOT_RECEIVED,
        KIND_EOT_TIMEOUT,
        KIND_RESOURCE,
        KIND_CLOCK_SYNC,
    )

    kind = pl.col("kind")
    null_utf8: pl.Expr = pl.lit(None, dtype=pl.Utf8)

    # ``event`` -- mapping from kind int to legacy event string.
    event_expr = (
        pl.when(kind == kw)
        .then(pl.lit("write"))
        .when(kind == kr)
        .then(pl.lit("receive"))
        .when(kind == kbp)
        .then(pl.lit("backpressure_skipped"))
        .when(kind == kgd)
        .then(pl.lit("gap_detected"))
        .when(kind == kgf)
        .then(pl.lit("gap_filled"))
        .when(kind == kph)
        .then(pl.lit("phase"))
        .when(kind == kc)
        .then(pl.lit("connected"))
        .when(kind == kes)
        .then(pl.lit("eot_sent"))
        .when(kind == ker)
        .then(pl.lit("eot_received"))
        .when(kind == ket)
        .then(pl.lit("eot_timeout"))
        .when(kind == kres)
        .then(pl.lit("resource"))
        .when(kind == kcs)
        .then(pl.lit("clock_sync"))
        .otherwise(null_utf8)
        .alias("event")
    )

    # ``seq`` populated on write/receive.
    seq_expr = (
        pl.when(kind.is_in([kw, kr]))
        .then(pl.col("seq"))
        .otherwise(pl.lit(None, dtype=pl.Int64))
        .alias("seq")
    )

    # ``path`` populated on write/receive/backpressure_skipped.
    path_expr = (
        pl.when(kind.is_in([kw, kr, kbp]))
        .then(pl.col("path_str"))
        .otherwise(null_utf8)
        .alias("path")
    )

    # ``writer`` populated on receive/gap_detected/gap_filled/eot_received.
    writer_expr = (
        pl.when(kind.is_in([kr, kgd, kgf, ker]))
        .then(pl.col("peer_str"))
        .otherwise(null_utf8)
        .alias("writer")
    )

    # ``qos`` populated on write/receive/backpressure_skipped.
    qos_expr = (
        pl.when(kind.is_in([kw, kr, kbp]))
        .then(pl.col("qos").cast(pl.Int8, strict=False))
        .otherwise(pl.lit(None, dtype=pl.Int8))
        .alias("qos")
    )

    # ``elapsed_ms`` populated on connected (extra_f32).
    elapsed_ms_expr = (
        pl.when(kind == kc)
        .then(pl.col("extra_f32").cast(pl.Float64))
        .otherwise(pl.lit(None, dtype=pl.Float64))
        .alias("elapsed_ms")
    )

    # ``phase`` populated on phase (extra_utf8).
    phase_expr = (
        pl.when(kind == kph)
        .then(pl.col("extra_utf8"))
        .otherwise(null_utf8)
        .alias("phase")
    )

    # ``missing_seq`` on gap_detected (extra_i64); ``recovered_seq`` on
    # gap_filled (extra_i64).
    missing_seq_expr = (
        pl.when(kind == kgd)
        .then(pl.col("extra_i64"))
        .otherwise(pl.lit(None, dtype=pl.Int64))
        .alias("missing_seq")
    )
    recovered_seq_expr = (
        pl.when(kind == kgf)
        .then(pl.col("extra_i64"))
        .otherwise(pl.lit(None, dtype=pl.Int64))
        .alias("recovered_seq")
    )

    # ``cpu_percent`` / ``memory_mb`` on resource (extra_f32, extra_f32_b).
    cpu_percent_expr = (
        pl.when(kind == kres)
        .then(pl.col("extra_f32").cast(pl.Float32))
        .otherwise(pl.lit(None, dtype=pl.Float32))
        .alias("cpu_percent")
    )
    memory_mb_expr = (
        pl.when(kind == kres)
        .then(pl.col("extra_f32_b").cast(pl.Float32))
        .otherwise(pl.lit(None, dtype=pl.Float32))
        .alias("memory_mb")
    )

    # ``peer`` -- connected (null when peer_idx == PEER_SELF) and
    # clock_sync (always populated).
    peer_expr = (
        pl.when(kind == kc)
        .then(
            pl.when(pl.col("peer_idx") == PEER_SELF)
            .then(null_utf8)
            .otherwise(pl.col("peer_str"))
        )
        .when(kind == kcs)
        .then(pl.col("peer_str"))
        .otherwise(null_utf8)
        .alias("peer")
    )

    # ``offset_ms`` on clock_sync (extra_i64 holds offset_ns -> /1e6).
    offset_ms_expr = (
        pl.when(kind == kcs)
        .then(pl.col("extra_i64").cast(pl.Float64) / 1_000_000.0)
        .otherwise(pl.lit(None, dtype=pl.Float64))
        .alias("offset_ms")
    )

    # ``rtt_ms`` on clock_sync (extra_f32).
    rtt_ms_expr = (
        pl.when(kind == kcs)
        .then(pl.col("extra_f32").cast(pl.Float64))
        .otherwise(pl.lit(None, dtype=pl.Float64))
        .alias("rtt_ms")
    )

    # ``eot_id`` on eot_sent/eot_received (extra_i64 cast to UInt64).
    eot_id_expr = (
        pl.when(kind.is_in([kes, ker]))
        .then(pl.col("extra_i64").cast(pl.UInt64, strict=False))
        .otherwise(pl.lit(None, dtype=pl.UInt64))
        .alias("eot_id")
    )

    # ``eot_missing`` on eot_timeout (extra_utf8).
    eot_missing_expr = (
        pl.when(kind == ket)
        .then(pl.col("extra_utf8"))
        .otherwise(null_utf8)
        .alias("eot_missing")
    )

    # ``wait_ms`` on eot_timeout (extra_i64 -> UInt64).
    wait_ms_expr = (
        pl.when(kind == ket)
        .then(pl.col("extra_i64").cast(pl.UInt64, strict=False))
        .otherwise(pl.lit(None, dtype=pl.UInt64))
        .alias("wait_ms")
    )

    # ``threading_mode`` on connected: per-row extra_utf8 with metadata
    # fallback.
    if threading_mode_meta is not None:
        tm_inner = pl.col("extra_utf8").fill_null(pl.lit(threading_mode_meta))
    else:
        tm_inner = pl.col("extra_utf8")
    threading_mode_expr = (
        pl.when(kind == kc).then(tm_inner).otherwise(null_utf8).alias("threading_mode")
    )

    # ``recv_buffer_kb`` -- spawn-level metadata stamped onto connected rows.
    recv_buf_expr = (
        pl.when(kind == kc)
        .then(pl.lit(recv_buffer_kb_meta, dtype=pl.UInt32))
        .otherwise(pl.lit(None, dtype=pl.UInt32))
        .alias("recv_buffer_kb")
    )

    # ``leaf_count`` / ``shape`` populated on write only. ``leaf_count``
    # defaults to 1 and ``shape`` to ``"scalar"`` when null (legacy
    # / pre-E19 rows).
    leaf_count_expr = (
        pl.when(kind == kw)
        .then(pl.col("leaf_count").cast(pl.UInt32, strict=False).fill_null(1))
        .otherwise(pl.lit(None, dtype=pl.UInt32))
        .alias("leaf_count")
    )
    shape_expr = (
        pl.when(kind == kw)
        .then(pl.col("shape_str").fill_null(pl.lit("scalar")))
        .otherwise(null_utf8)
        .alias("shape")
    )

    # ``bytes`` populated on write / receive.
    bytes_expr = (
        pl.when(kind.is_in([kw, kr]))
        .then(pl.col("bytes").cast(pl.Int64, strict=False))
        .otherwise(pl.lit(None, dtype=pl.Int64))
        .alias("bytes")
    )

    # Build the final projection in SHARD_SCHEMA column order. ``ts`` /
    # ``variant`` / ``runner`` / ``run`` are stamped per row using
    # ``pl.lit``; the remaining columns come from the per-kind
    # conditional chains above.
    projected = enriched.select(
        pl.col("ts_ns").cast(pl.Datetime("ns", "UTC")).alias("ts"),
        pl.lit(variant, dtype=pl.Categorical).alias("variant"),
        pl.lit(runner, dtype=pl.Categorical).alias("runner"),
        pl.lit(run, dtype=pl.Categorical).alias("run"),
        event_expr.cast(pl.Categorical),
        seq_expr,
        path_expr,
        writer_expr,
        qos_expr,
        elapsed_ms_expr,
        phase_expr,
        missing_seq_expr,
        recovered_seq_expr,
        cpu_percent_expr,
        memory_mb_expr,
        peer_expr,
        offset_ms_expr,
        rtt_ms_expr,
        eot_id_expr,
        eot_missing_expr,
        wait_ms_expr,
        threading_mode_expr,
        recv_buf_expr,
        leaf_count_expr,
        shape_expr,
        bytes_expr,
    )

    # Match the canonical schema dtypes (especially Categorical/Utf8
    # for ``event``) and sort by wall-clock ts -- the JSONL parser also
    # produces ts-sorted output because the variant writes lines in
    # wall-clock order.
    projected = projected.select(
        [pl.col(name).cast(dtype) for name, dtype in SHARD_SCHEMA.items()]
    )
    if sort_by_ts:
        projected = projected.sort("ts")
    return projected


def _scan_compact_source(path: Path) -> pl.LazyFrame:
    """Open a compact-Parquet file as a polars ``LazyFrame``.

    Wraps :func:`pl.scan_parquet` in the same ``CompactLoadError``
    surface as :func:`read_compact_parquet` so callers see one error
    type regardless of the read path.
    """
    try:
        return pl.scan_parquet(str(path))
    except Exception as exc:  # noqa: BLE001 -- surface as CompactLoadError
        raise CompactLoadError(
            f"failed to scan parquet rows from {path}: {exc}"
        ) from exc


def stream_compact_to_parquet(
    src: Path,
    dst: Path,
    *,
    compression: str = "snappy",
    row_group_size: int | None = None,
) -> None:
    """Stream-project a compact-Parquet source into a SHARD_SCHEMA shard.

    Memory-bounded counterpart to :func:`read_compact_parquet`:
    instead of materialising the full source frame plus 12 per-kind
    sub-frames (which scaled with file size and was the root cause of
    the multi-GB worker OOM observed on 270 MB compact files), this
    function opens the source lazily via :func:`pl.scan_parquet`,
    projects every ``SHARD_SCHEMA`` column with one ``when/then`` chain
    per column, and writes the result via :meth:`pl.LazyFrame.sink_parquet`
    so the streaming engine processes the rows in bounded-memory
    batches.

    The KV metadata is loaded eagerly (it is small -- just the spawn
    identity plus the intern dictionaries) so the lazy projection can
    reference it via ``pl.lit`` and small join tables.

    ``compression`` and ``row_group_size`` are passed through to
    ``sink_parquet``. The default compression matches the legacy cache
    writer (``snappy``).

    Raises :class:`CompactLoadError` if the source's KV metadata is
    missing the spawn identity fields (``variant``/``runner``/``run``).
    """
    meta = read_compact_metadata(src)
    _validate_meta(src, meta)

    source = _scan_compact_source(src)
    # ``sort_by_ts=False`` keeps the projection streamable: a final
    # ``sort`` is a streaming barrier (it forces full materialisation),
    # and the source compact-Parquet is already ts-ordered because the
    # variant writers emit events in wall-clock order. The
    # JSONL-compat ``read_compact_parquet`` path still sorts.
    projected = _build_projection_lazyframe(source, meta=meta, sort_by_ts=False)
    try:
        # ``engine="streaming"`` opts into the bounded-memory streaming
        # engine explicitly (the ``"auto"`` default may pick the
        # in-memory engine for query plans the optimiser thinks fit in
        # RAM -- which is exactly the assumption that blew up on the
        # 270 MB compact files). ``maintain_order=False`` lets the
        # engine emit row groups as soon as they are ready instead of
        # buffering for global ordering; the source is already
        # ts-sorted so this is a no-op semantically.
        projected.sink_parquet(
            str(dst),
            compression=compression,
            row_group_size=row_group_size,
            maintain_order=False,
            engine="streaming",
        )
    except Exception as exc:  # noqa: BLE001
        raise CompactLoadError(
            f"failed to sink-write compact projection for {src} -> {dst}: {exc}"
        ) from exc


def read_compact_parquet(path: Path) -> pl.DataFrame:
    """Read a ``.compact.parquet`` file and project to ``SHARD_SCHEMA``.

    Returns the projected DataFrame in memory. Prefer
    :func:`stream_compact_to_parquet` for the cache-build path so peak
    memory stays bounded by the streaming engine rather than by the
    source file size; this function is retained for unit-test callers
    that need a materialised frame.

    Raises ``CompactLoadError`` if the file's KV metadata is missing
    the spawn identity fields the analyzer needs (``variant``,
    ``runner``, ``run``).
    """
    meta = read_compact_metadata(path)
    _validate_meta(path, meta)

    source = _scan_compact_source(path)
    projected = _build_projection_lazyframe(source, meta=meta)
    try:
        return projected.collect()
    except Exception as exc:  # noqa: BLE001
        raise CompactLoadError(
            f"failed to project compact-parquet rows from {path}: {exc}"
        ) from exc
