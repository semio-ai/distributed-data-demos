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
    """

    schema_version: int | None
    variant: str | None
    runner: str | None
    run: str | None
    threading_mode: str | None
    recv_buffer_kb: int | None
    paths: list[str]
    peers: list[str]


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

    return CompactMeta(
        schema_version=_decode_int("schema_version"),
        variant=_decode_str("variant"),
        runner=_decode_str("runner"),
        run=_decode_str("run"),
        threading_mode=_decode_str("threading_mode"),
        recv_buffer_kb=_decode_int("recv_buffer_kb"),
        paths=_decode_list("paths"),
        peers=_decode_list("peers"),
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
    """``Write (0)``: ts / seq / path / qos / bytes. ``writer`` left null.

    The ``bytes`` column is not in ``SHARD_SCHEMA`` -- the legacy JSONL
    parser drops it too -- so we don't propagate it.
    """
    sub = frame.filter(pl.col("kind") == KIND_WRITE)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
    return sub.select(
        pl.col("ts"),
        pl.col("variant"),
        pl.col("runner"),
        pl.col("run"),
        pl.lit("write", dtype=pl.Categorical).alias("event"),
        pl.col("seq").cast(pl.Int64),
        pl.col("path_str").alias("path"),
        pl.col("qos").cast(pl.Int8),
    )


def _project_receive(
    frame: pl.DataFrame, *, variant: str, runner: str, run: str
) -> pl.DataFrame:
    """``Receive (1)``: ts / seq / path / writer / qos."""
    sub = frame.filter(pl.col("kind") == KIND_RECEIVE)
    if sub.is_empty():
        return _empty_shard_frame()
    sub = _common_cols(sub, variant=variant, runner=runner, run=run)
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


def read_compact_parquet(path: Path) -> pl.DataFrame:
    """Read a ``.compact.parquet`` file and project to ``SHARD_SCHEMA``.

    Steps:
      1. Read the Parquet KV metadata (schema version, spawn identity,
         path/peer intern dictionaries).
      2. Read the ``compact_events`` columnar table.
      3. Resolve ``path_idx`` / ``peer_idx`` to the interned strings.
      4. Dispatch by ``kind`` and project each row into the
         ``SHARD_SCHEMA`` slot defined in the compact-log contract.
      5. Concatenate the per-kind frames into a single output DataFrame
         with every ``SHARD_SCHEMA`` column present.

    Raises ``CompactLoadError`` if the file's KV metadata is missing
    the spawn identity fields the analyzer needs (``variant``,
    ``runner``, ``run``).
    """
    meta = read_compact_metadata(path)

    if meta.variant is None or meta.runner is None or meta.run is None:
        raise CompactLoadError(
            f"{path}: compact-parquet KV metadata is missing one of "
            f"variant/runner/run (got variant={meta.variant!r}, "
            f"runner={meta.runner!r}, run={meta.run!r})"
        )

    try:
        raw = pl.read_parquet(str(path))
    except Exception as exc:  # noqa: BLE001 -- surface as CompactLoadError
        raise CompactLoadError(
            f"failed to read parquet rows from {path}: {exc}"
        ) from exc

    if raw.is_empty():
        return _empty_shard_frame()

    # The variant's Parquet writer encodes physical types: ts_ns:i64,
    # kind/path_idx/peer_idx/qos/bytes:i32, seq:i64,
    # extra_f32/extra_f32_b:f32, extra_i64:i64, extra_utf8:utf8.
    # Cast to canonical Python-side dtypes so downstream filters /
    # arithmetic don't see Int32-vs-Int64 surprises.
    frame = raw.with_columns(
        pl.col("ts_ns").cast(pl.Int64),
        pl.col("kind").cast(pl.Int32),
        pl.col("seq").cast(pl.Int64),
        pl.col("path_idx").cast(pl.Int32),
        pl.col("peer_idx").cast(pl.Int32),
        pl.col("qos").cast(pl.Int32),
    )

    # Resolve intern indices to strings via in-memory lookups. Polars
    # has no native gather-by-index over a Python list, so we build a
    # small DataFrame for each dictionary and ``join`` on the index.
    if meta.paths:
        paths_df = pl.DataFrame(
            {
                "path_idx": list(range(len(meta.paths))),
                "path_str": meta.paths,
            },
            schema={"path_idx": pl.Int32, "path_str": pl.Utf8},
        )
        frame = frame.join(paths_df, on="path_idx", how="left")
    else:
        frame = frame.with_columns(pl.lit(None, dtype=pl.Utf8).alias("path_str"))

    if meta.peers:
        peers_df = pl.DataFrame(
            {
                "peer_idx": list(range(len(meta.peers))),
                "peer_str": meta.peers,
            },
            schema={"peer_idx": pl.Int32, "peer_str": pl.Utf8},
        )
        frame = frame.join(peers_df, on="peer_idx", how="left")
    else:
        frame = frame.with_columns(pl.lit(None, dtype=pl.Utf8).alias("peer_str"))

    variant = meta.variant
    runner = meta.runner
    run = meta.run

    per_kind: list[pl.DataFrame] = [
        _project_write(frame, variant=variant, runner=runner, run=run),
        _project_receive(frame, variant=variant, runner=runner, run=run),
        _project_backpressure_skipped(frame, variant=variant, runner=runner, run=run),
        _project_gap_detected(frame, variant=variant, runner=runner, run=run),
        _project_gap_filled(frame, variant=variant, runner=runner, run=run),
        _project_phase(frame, variant=variant, runner=runner, run=run),
        _project_connected(
            frame,
            variant=variant,
            runner=runner,
            run=run,
            threading_mode_meta=meta.threading_mode,
            recv_buffer_kb_meta=meta.recv_buffer_kb,
        ),
        _project_eot_sent(frame, variant=variant, runner=runner, run=run),
        _project_eot_received(frame, variant=variant, runner=runner, run=run),
        _project_eot_timeout(frame, variant=variant, runner=runner, run=run),
        _project_resource(frame, variant=variant, runner=runner, run=run),
        _project_clock_sync(frame, variant=variant, runner=runner, run=run),
    ]

    # Stack the per-kind frames. ``how="diagonal"`` fills missing columns
    # with nulls -- exactly what we want, since each kind only populates
    # a subset of ``SHARD_SCHEMA``. Re-order columns to match the
    # canonical schema and re-cast to keep dtypes deterministic.
    non_empty = [df for df in per_kind if df.height > 0]
    if not non_empty:
        return _empty_shard_frame()
    combined = pl.concat(non_empty, how="diagonal")

    # Add any SHARD_SCHEMA columns the per-kind branches didn't touch
    # (e.g. ``peer`` only appears on clock_sync; if no clock_sync rows
    # exist the column won't be in ``combined``). Then select in
    # canonical order with the canonical dtype.
    add_missing = [
        pl.lit(None, dtype=dtype).alias(name)
        for name, dtype in SHARD_SCHEMA.items()
        if name not in combined.columns
    ]
    if add_missing:
        combined = combined.with_columns(add_missing)
    combined = combined.select(
        [pl.col(name).cast(dtype) for name, dtype in SHARD_SCHEMA.items()]
    )

    # Stable order matches the row order of the source compact file
    # (which is the same insertion order the variant pushed events in)
    # after the per-kind concat. Sort by ``ts`` to put the rows in
    # wall-clock order, which is also what the JSONL parser produces
    # because the variant writes lines in wall-clock order.
    combined = combined.sort("ts")

    return combined
