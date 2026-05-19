"""Polars-based write-receive correlation.

Joins ``write`` events with ``receive`` events on
``(variant, run, writer, seq, path)`` where the receive event's
``writer`` field equals the write event's ``runner``. Produces a polars
``DataFrame`` of delivery records with the schema documented in
``metak-shared/ANALYSIS.md`` section 4.2.

Cross-machine clock-skew correction (E8): when ``clock_sync`` rows are
present in the group, ``correlate_lazy`` attaches a per-row offset to
every delivery record via ``polars.DataFrame.join_asof``. Same-runner
deliveries are forced to ``offset_ms = 0`` and ``offset_applied = True``;
cross-runner deliveries with no matching offset row keep their raw
latency and are flagged ``offset_applied = False``. See
``clock_offsets.build_offset_table`` and
``metak-shared/api-contracts/clock-sync.md`` for the protocol.

The hot path is polars throughout. A ``DeliveryRecord`` dataclass shape
is preserved for tests / output-side compatibility but the analysis
itself never materializes per-row dataclasses.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime

import polars as pl

from clock_offsets import build_offset_table


@dataclass
class DeliveryRecord:
    """A correlated write-receive pair representing one delivery.

    Kept as a dataclass for back-compat with any external consumer or
    test that wants per-record access. The polars pipeline emits a
    ``DataFrame`` with columns matching these fields; convert via
    ``deliveries_to_records`` only at API boundaries.

    ``offset_ms`` is the clock-skew correction applied to this row's
    ``latency_ms`` (peer.clock - self.clock, in ms). It is ``0.0`` for
    same-runner deliveries, the matched offset for cross-runner
    deliveries with a clock-sync row available, and ``None`` for
    cross-runner deliveries with no matching offset.

    ``offset_applied`` is ``True`` when the latency was corrected (or
    no correction was needed because the writer is the receiver) and
    ``False`` when no correction could be applied.
    """

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
    offset_ms: float | None
    offset_applied: bool
    # E19 / T19.5: workload-shape dimension. Inherited from the
    # matching ``write`` row via the (writer, seq, path) join key.
    # Defaults to ``1`` / ``"scalar"`` for legacy data per the
    # api-contracts backward-compat rule. ``bytes`` is the serialized
    # payload size recorded by the writer (null on legacy data that
    # predates the column).
    leaf_count: int = 1
    shape: str = "scalar"
    bytes: int | None = None


# Output column order on the delivery DataFrame.
#
# ``leaf_count`` / ``shape`` / ``bytes`` (E19 / T19.5) are inherited
# from the matching ``write`` row -- the wire is opaque so receive
# events never carry them directly. ``leaf_count`` defaults to ``1``
# and ``shape`` to ``"scalar"`` on legacy data per the api-contracts
# backward-compat rule.
DELIVERY_COLUMNS: tuple[str, ...] = (
    "variant",
    "run",
    "writer",
    "receiver",
    "seq",
    "path",
    "qos",
    "write_ts",
    "receive_ts",
    "latency_ms",
    "offset_ms",
    "offset_applied",
    "leaf_count",
    "shape",
    "bytes",
)


def _attach_offsets(deliveries: pl.DataFrame, offsets: pl.DataFrame) -> pl.DataFrame:
    """Attach a per-row clock offset to ``deliveries`` via asof joins.

    Strategy: do a per-variant asof join first, then a fallback asof
    join against the ``variant == ""`` initial sync, and coalesce. This
    matches the contract in ``clock-sync.md`` -- per-variant resync is
    preferred, the pre-variant initial sync is the fallback.

    Same-runner rows (writer == receiver) are forced to ``offset_ms = 0``
    and ``offset_applied = True``: there is no skew to correct.

    Cross-runner rows with no matching offset row in either pass keep
    their raw ``latency_ms`` and get ``offset_ms = None``,
    ``offset_applied = False``.
    """
    # Bail out early if there are no deliveries to enrich.
    if deliveries.is_empty():
        return deliveries.with_columns(
            pl.lit(None, dtype=pl.Float64).alias("offset_ms"),
            pl.lit(False).alias("offset_applied"),
        )

    base = deliveries.with_columns(
        pl.col("receiver").cast(pl.Utf8),
        pl.col("writer").cast(pl.Utf8),
        pl.col("run").cast(pl.Utf8),
    ).with_row_index("__row_idx")

    # ``join_asof`` requires both sides sorted by the asof key within
    # each ``by`` group. Sort by (receiver, writer, receive_ts) so the
    # ``by`` columns are clustered, then receive_ts is non-decreasing
    # within each cluster -- matches the right-hand side prepared by
    # ``build_offset_table`` (sorted by runner, peer, variant, ts).
    base_sorted = base.sort(["receiver", "writer", "receive_ts"])

    def _asof(variant_filter: pl.Expr) -> pl.DataFrame:
        right = (
            offsets.lazy()
            .filter(variant_filter)
            .select(
                pl.col("runner").alias("receiver"),
                pl.col("peer").alias("writer"),
                pl.col("ts").alias("offset_ts"),
                pl.col("offset_ms"),
            )
            .sort(["receiver", "writer", "offset_ts"])
            .collect()
        )
        if right.is_empty():
            return base_sorted.select(
                pl.col("__row_idx"),
                pl.lit(None, dtype=pl.Float64).alias("offset_ms"),
            )
        joined = base_sorted.join_asof(
            right,
            left_on="receive_ts",
            right_on="offset_ts",
            by=["receiver", "writer"],
            strategy="backward",
            check_sortedness=False,
        )
        return joined.select(
            pl.col("__row_idx"),
            pl.col("offset_ms"),
        )

    # Determine the current variant for this group from the deliveries
    # themselves -- correlate_lazy is invoked per (variant, run) so the
    # variant column is constant across rows.
    variants = base.get_column("variant").unique().to_list()
    current_variant: str | None = None
    for v in variants:
        if v is not None:
            current_variant = str(v)
            break

    if current_variant is not None:
        per_variant = _asof(pl.col("variant") == current_variant).rename(
            {"offset_ms": "offset_ms_variant"}
        )
    else:
        per_variant = base_sorted.select(
            pl.col("__row_idx"),
            pl.lit(None, dtype=pl.Float64).alias("offset_ms_variant"),
        )

    initial = _asof(pl.col("variant") == "").rename({"offset_ms": "offset_ms_initial"})

    enriched = (
        base.join(per_variant, on="__row_idx", how="left")
        .join(initial, on="__row_idx", how="left")
        .with_columns(
            pl.coalesce(
                [pl.col("offset_ms_variant"), pl.col("offset_ms_initial")]
            ).alias("matched_offset_ms")
        )
    )

    same_runner = pl.col("writer") == pl.col("receiver")
    has_offset = pl.col("matched_offset_ms").is_not_null()

    enriched = enriched.with_columns(
        # offset_ms: 0 for same-runner, matched value for cross-runner
        # with offset, null otherwise.
        pl.when(same_runner)
        .then(pl.lit(0.0, dtype=pl.Float64))
        .when(has_offset)
        .then(pl.col("matched_offset_ms"))
        .otherwise(pl.lit(None, dtype=pl.Float64))
        .alias("offset_ms"),
        # offset_applied: True for same-runner and cross-runner-with-offset.
        (same_runner | has_offset).alias("offset_applied"),
    ).with_columns(
        # Apply the correction to latency_ms when an offset is available
        # and the runners differ. Same-runner deliveries already have
        # offset_ms == 0, so the addition is a no-op for them.
        pl.when(pl.col("offset_ms").is_not_null())
        .then(pl.col("latency_ms") + pl.col("offset_ms"))
        .otherwise(pl.col("latency_ms"))
        .alias("latency_ms")
    )

    return enriched.drop(
        ["__row_idx", "offset_ms_variant", "offset_ms_initial", "matched_offset_ms"]
    )


def correlate_lazy(group: pl.LazyFrame) -> pl.LazyFrame:
    """Build a per-group delivery-record lazy frame.

    ``group`` should already be filtered to a single ``(variant, run)``
    pair (modulo broadcast clock-sync rows -- see
    ``cache.discover_groups``). Joins write and receive rows on
    ``(variant, run, writer, seq, path)`` and computes
    ``latency_ms`` as ``(receive_ts - write_ts).total_milliseconds()``.

    A subsequent ``join_asof`` (per ``clock_offsets.build_offset_table``)
    attaches a clock-skew correction to each row. The lazy frame is
    materialized at the join boundary because ``join_asof`` requires a
    sorted right-hand side. The result is then re-wrapped as lazy so
    callers (``analyze.run_analysis``) can continue to compose with it.
    """
    # E19 / T19.5: ``leaf_count`` / ``shape`` / ``bytes`` are sourced from
    # the write side and inherited by the matching receive row via the
    # ``(variant, run, writer, seq, path)`` join. Schema columns may be
    # absent on caches built before SCHEMA_VERSION 5 -- guard with the
    # lazy frame's collected schema and synthesize default-value
    # expressions so the projection always emits the same column set.
    available = set(group.collect_schema().names())
    if "leaf_count" in available:
        leaf_count_expr = (
            pl.col("leaf_count").cast(pl.UInt32, strict=False).fill_null(1)
        )
    else:
        leaf_count_expr = pl.lit(1, dtype=pl.UInt32)
    if "shape" in available:
        shape_expr = pl.col("shape").fill_null(pl.lit("scalar"))
    else:
        shape_expr = pl.lit("scalar", dtype=pl.Utf8)
    if "bytes" in available:
        bytes_expr = pl.col("bytes").cast(pl.Int64, strict=False)
    else:
        bytes_expr = pl.lit(None, dtype=pl.Int64)

    writes = (
        group.filter(pl.col("event") == "write")
        .filter(pl.col("seq").is_not_null() & pl.col("path").is_not_null())
        .select(
            pl.col("variant"),
            pl.col("run"),
            pl.col("runner").cast(pl.Utf8).alias("writer"),
            pl.col("seq"),
            pl.col("path"),
            pl.col("ts").alias("write_ts"),
            pl.col("qos").alias("write_qos"),
            leaf_count_expr.alias("leaf_count"),
            shape_expr.alias("shape"),
            bytes_expr.alias("bytes"),
        )
    )
    receives = (
        group.filter(pl.col("event") == "receive")
        .filter(
            pl.col("writer").is_not_null()
            & pl.col("seq").is_not_null()
            & pl.col("path").is_not_null()
        )
        .select(
            pl.col("variant"),
            pl.col("run"),
            pl.col("runner").cast(pl.Utf8).alias("receiver"),
            pl.col("writer"),
            pl.col("seq"),
            pl.col("path"),
            pl.col("ts").alias("receive_ts"),
            pl.col("qos").alias("receive_qos"),
        )
    )

    joined = receives.join(
        writes,
        on=["variant", "run", "writer", "seq", "path"],
        how="inner",
    ).with_columns(
        # Prefer the receive event's qos (matches Phase 1 behaviour
        # where the qos was read off the receive event).
        pl.coalesce([pl.col("receive_qos"), pl.col("write_qos")])
        .cast(pl.Int64)
        .alias("qos"),
        (
            (pl.col("receive_ts") - pl.col("write_ts")).dt.total_microseconds() / 1000.0
        ).alias("latency_ms"),
    )

    base_lazy = joined.select(
        pl.col("variant").cast(pl.Utf8),
        pl.col("run").cast(pl.Utf8),
        pl.col("writer"),
        pl.col("receiver"),
        pl.col("seq"),
        pl.col("path"),
        pl.col("qos"),
        pl.col("write_ts"),
        pl.col("receive_ts"),
        pl.col("latency_ms"),
        pl.col("leaf_count"),
        pl.col("shape"),
        pl.col("bytes"),
    )

    # Materialize the deliveries here so we can run the asof-join against
    # the (already-collected) offsets table. This is the only collect on
    # the hot path; the result is rewrapped as a lazy frame so callers
    # see the same return type as before.
    offsets = build_offset_table(group)
    deliveries = base_lazy.collect()
    enriched = _attach_offsets(deliveries, offsets)
    return enriched.select(list(DELIVERY_COLUMNS)).lazy()


def deliveries_to_records(deliveries: pl.DataFrame) -> list[DeliveryRecord]:
    """Convert a delivery ``DataFrame`` into a list of ``DeliveryRecord``.

    Use only at API boundaries (tests, plot serialization). The polars
    pipeline does not need this.
    """
    records: list[DeliveryRecord] = []
    if deliveries.is_empty():
        return records
    for row in deliveries.iter_rows(named=True):
        offset_raw = row.get("offset_ms")
        offset_ms: float | None = float(offset_raw) if offset_raw is not None else None
        # E19 / T19.5: populate ``leaf_count`` / ``shape`` / ``bytes``.
        # Older delivery DataFrames (pre-T19.5 callers) won't have the
        # columns; we fall back to the dataclass defaults so this helper
        # stays back-compatible for any consumer that did its own
        # correlation outside ``correlate_lazy``.
        leaf_count_raw = row.get("leaf_count")
        leaf_count = int(leaf_count_raw) if leaf_count_raw is not None else 1
        shape_raw = row.get("shape")
        shape = str(shape_raw) if shape_raw is not None else "scalar"
        bytes_raw = row.get("bytes")
        bytes_val = int(bytes_raw) if bytes_raw is not None else None
        records.append(
            DeliveryRecord(
                variant=row["variant"],
                run=row["run"],
                path=row["path"],
                seq=int(row["seq"]),
                qos=int(row["qos"]) if row["qos"] is not None else 0,
                writer=row["writer"],
                receiver=row["receiver"],
                write_ts=row["write_ts"],
                receive_ts=row["receive_ts"],
                latency_ms=float(row["latency_ms"]),
                offset_ms=offset_ms,
                offset_applied=bool(row.get("offset_applied", False)),
                leaf_count=leaf_count,
                shape=shape,
                bytes=bytes_val,
            )
        )
    return records
