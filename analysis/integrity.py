"""Integrity verification using polars groupbys.

Output is a list of ``IntegrityResult`` dataclasses, one per
``(variant, run, writer -> receiver)`` pair. The dataclass shape and
field set match Phase 1 so ``tables.py`` consumers do not need to
change.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import polars as pl

from timeout_classification import (
    SpawnClassification,
    classify_group,
)


@dataclass
class IntegrityResult:
    """Integrity check result for one (variant, run, writer -> receiver) pair.

    ``backpressure_skipped_count`` is the number of times the writer's
    driver tick skipped a value because ``Variant::try_publish`` reported
    backpressure (per ``metak-shared/api-contracts/jsonl-log-schema.md``
    -- T-impl.6). Aggregated per ``(variant, run, writer)`` -- the same
    writer's count is replicated onto every (writer -> receiver) row in
    the report. Defaults to 0 when no ``backpressure_skipped`` events
    are present (e.g. legacy logs from before T-impl.6 / T-impl.7).

    ``timeout_classification`` (T14.17, extended in T15.6, T15.11, and
    a 2026-05-14 follow-up that added ``variant_crashed``) is the
    per-spawn failure-cause bucket for the WRITER side of this row --
    one of ``completed``, ``runner_idle_terminated``, ``deadlock``,
    ``eot_lost``, ``variant_rejected``, ``variant_self_killed_idle``,
    ``variant_crashed``, ``eot_timeout_internal``, ``unknown``.
    Defaults to ``"unknown"`` when classification was skipped (e.g.
    ``logs_dir`` was not passed to ``integrity_for_group``).
    ``timeout_sub_tags`` carries refinements such as
    ``("eot_lost_likely_saturation",)``.
    """

    variant: str
    run: str
    writer: str
    receiver: str
    qos: int
    write_count: int
    receive_count: int
    delivery_pct: float
    out_of_order: int
    duplicates: int
    unresolved_gaps: int | None  # None when gap checking does not apply
    backpressure_skipped_count: int
    completeness_error: bool
    ordering_error: bool
    duplicate_error: bool
    gap_error: bool
    timeout_classification: str = "unknown"
    timeout_sub_tags: tuple[str, ...] = field(default_factory=tuple)


def _count_writes_per_writer(group: pl.LazyFrame) -> pl.DataFrame:
    """Count write events per (variant, run, writer) inside one group."""
    return (
        group.filter(pl.col("event") == "write")
        .filter(pl.col("seq").is_not_null() & pl.col("path").is_not_null())
        .group_by(["variant", "run", "runner"])
        .agg(pl.len().alias("write_count"))
        .rename({"runner": "writer"})
        .with_columns(
            pl.col("variant").cast(pl.Utf8),
            pl.col("run").cast(pl.Utf8),
            pl.col("writer").cast(pl.Utf8),
        )
        .collect()
    )


def _count_backpressure_skipped_per_writer(group: pl.LazyFrame) -> pl.DataFrame:
    """Count ``backpressure_skipped`` events per (variant, run, writer).

    The writer of a ``backpressure_skipped`` event is the ``runner``
    field on the line (the driver emits the event from the writer's
    side). Returns one row per writer that produced at least one skip
    event. Joiners must fill missing rows with ``0``. See
    ``metak-shared/api-contracts/jsonl-log-schema.md`` (T-impl.6).
    """
    return (
        group.filter(pl.col("event") == "backpressure_skipped")
        .group_by(["variant", "run", "runner"])
        .agg(pl.len().cast(pl.UInt32).alias("backpressure_skipped_count"))
        .rename({"runner": "writer"})
        .with_columns(
            pl.col("variant").cast(pl.Utf8),
            pl.col("run").cast(pl.Utf8),
            pl.col("writer").cast(pl.Utf8),
        )
        .collect()
    )


def _check_per_pair(deliveries: pl.DataFrame) -> pl.DataFrame:
    """Per ``(variant, run, writer, receiver)`` integrity stats.

    Computes:
      - ``receive_count``
      - ``out_of_order`` (count of receives whose seq is < the previous
        seq in receive-time order, matching Phase 1's prev-seq scan)
      - ``duplicates`` (count of duplicate ``(writer, seq, path)`` on a
        receiver, summed across all duplicate groups)
      - ``qos`` (the qos of the first delivery in receive-time order)

    Implementation note: every aggregation here is built on the same
    columns of ``deliveries``. We do the heavy sort once, project to
    the minimum needed columns up front, and run the aggregations as a
    single lazy plan so polars can fuse the scan-pass and minimise
    intermediate copies. On a 3M-row group this drops working-set RSS
    from a few hundred MB down to under 100 MB.
    """
    if deliveries.is_empty():
        return pl.DataFrame(
            schema={
                "variant": pl.Utf8,
                "run": pl.Utf8,
                "writer": pl.Utf8,
                "receiver": pl.Utf8,
                "receive_count": pl.UInt32,
                "out_of_order": pl.UInt32,
                "duplicates": pl.UInt32,
                "qos": pl.Int64,
            }
        )

    # Project the bare minimum columns the integrity checks need.
    minimal = deliveries.lazy().select(
        "variant",
        "run",
        "writer",
        "receiver",
        "seq",
        "path",
        "qos",
        "receive_ts",
    )

    # Sort once and reuse the sorted lazy frame for all reductions.
    sorted_lazy = minimal.sort(["variant", "run", "writer", "receiver", "receive_ts"])

    out_of_order = (
        sorted_lazy.with_columns(
            pl.col("seq")
            .shift(1)
            .over(["variant", "run", "writer", "receiver"])
            .alias("prev_seq")
        )
        .with_columns(
            (
                pl.col("prev_seq").is_not_null() & (pl.col("seq") < pl.col("prev_seq"))
            ).alias("ooo_flag")
        )
        .group_by(["variant", "run", "writer", "receiver"])
        .agg(pl.col("ooo_flag").sum().cast(pl.UInt32).alias("out_of_order"))
    )

    # Duplicates: every same-key group of size N contributes N-1 dupes.
    duplicates = (
        minimal.group_by(["variant", "run", "writer", "receiver", "seq", "path"])
        .agg(pl.len().alias("n"))
        .with_columns((pl.col("n") - 1).alias("dupes"))
        .group_by(["variant", "run", "writer", "receiver"])
        .agg(pl.col("dupes").sum().cast(pl.UInt32).alias("duplicates"))
    )

    base = sorted_lazy.group_by(["variant", "run", "writer", "receiver"]).agg(
        pl.len().cast(pl.UInt32).alias("receive_count"),
        pl.col("qos").first().cast(pl.Int64).alias("qos"),
    )

    return (
        base.join(
            out_of_order,
            on=["variant", "run", "writer", "receiver"],
            how="left",
        )
        .join(
            duplicates,
            on=["variant", "run", "writer", "receiver"],
            how="left",
        )
        .collect()
    )


def _gap_counts(group: pl.LazyFrame) -> pl.DataFrame:
    """Per (variant, run, writer, receiver) unresolved-gap counts.

    ``unresolved = |detected - filled|`` (set difference, not arithmetic
    difference, matching Phase 1).

    Returns rows only for pairs that have at least one gap_detected or
    gap_filled event; absence of a row means "no gap data".
    """
    gaps = group.filter(
        pl.col("event").is_in(["gap_detected", "gap_filled"])
        & pl.col("writer").is_not_null()
    )

    # Use lazy collect once -- gap events are small.
    gaps_df = gaps.collect()
    if gaps_df.is_empty():
        return pl.DataFrame(
            schema={
                "variant": pl.Utf8,
                "run": pl.Utf8,
                "writer": pl.Utf8,
                "receiver": pl.Utf8,
                "unresolved_gaps": pl.UInt32,
            }
        )

    # Coerce categoricals to Utf8 for hashing on join.
    gaps_df = gaps_df.with_columns(
        pl.col("variant").cast(pl.Utf8),
        pl.col("run").cast(pl.Utf8),
        pl.col("runner").cast(pl.Utf8).alias("receiver"),
        pl.col("writer").cast(pl.Utf8),
    )

    detected = (
        gaps_df.filter(pl.col("event") == "gap_detected")
        .filter(pl.col("missing_seq").is_not_null())
        .select(
            "variant",
            "run",
            "writer",
            "receiver",
            pl.col("missing_seq").alias("seq"),
        )
        .unique()
    )
    filled = (
        gaps_df.filter(pl.col("event") == "gap_filled")
        .filter(pl.col("recovered_seq").is_not_null())
        .select(
            "variant",
            "run",
            "writer",
            "receiver",
            pl.col("recovered_seq").alias("seq"),
        )
        .unique()
    )

    detected_with_status = detected.join(
        filled.with_columns(pl.lit(True).alias("filled")),
        on=["variant", "run", "writer", "receiver", "seq"],
        how="left",
    ).with_columns(pl.col("filled").is_null().alias("unresolved"))

    unresolved = detected_with_status.group_by(
        ["variant", "run", "writer", "receiver"]
    ).agg(pl.col("unresolved").sum().cast(pl.UInt32).alias("unresolved_gaps"))

    # Also include pairs that have only filled events with no detected:
    # they have 0 unresolved gaps but still count as "gap data exists"
    # so the report shows "0" rather than "-".
    only_filled = (
        filled.join(
            detected.select("variant", "run", "writer", "receiver").unique(),
            on=["variant", "run", "writer", "receiver"],
            how="anti",
        )
        .select("variant", "run", "writer", "receiver")
        .unique()
        .with_columns(pl.lit(0).cast(pl.UInt32).alias("unresolved_gaps"))
    )

    return pl.concat([unresolved, only_filled], how="vertical_relaxed")


def integrity_for_group(
    group: pl.LazyFrame,
    deliveries: pl.DataFrame,
    *,
    logs_dir: Path | None = None,
    variant: str | None = None,
    run: str | None = None,
) -> list[IntegrityResult]:
    """Compute integrity results for a single ``(variant, run)`` group.

    ``group`` is the per-group lazy frame over the cache. ``deliveries``
    is the materialized delivery-records DataFrame for that group.

    ``logs_dir``, ``variant`` and ``run`` are required to populate the
    T14.17 ``timeout_classification`` column. When ``logs_dir`` is
    ``None`` (legacy callers, tests that don't care about the column)
    classification falls back to ``"unknown"`` on every row.
    """
    write_counts = _count_writes_per_writer(group)
    skip_counts = _count_backpressure_skipped_per_writer(group)
    pair_stats = _check_per_pair(deliveries)
    gaps = _gap_counts(group)

    # T14.17: classify every spawn (per writer side) in this group up
    # front so the per-row attachment below is a dict lookup.
    classifications: dict[str, SpawnClassification] = {}
    if variant is not None and run is not None:
        classifications = classify_group(
            group,
            variant=variant,
            run=run,
            logs_dir=logs_dir,
        )

    # Pull writers' receivers from deliveries; also add pairs from
    # write_counts that have no deliveries (writer wrote but nothing
    # was received) but only when we know about a candidate receiver.
    # The Phase 1 implementation only listed pairs that appeared in
    # deliveries OR pairs reachable via the receivers_map from
    # deliveries -- which is identical to the deliveries set itself.
    # So we use the deliveries pair set directly, joining writes for
    # the count.

    if pair_stats.is_empty():
        return []

    pair_stats = pair_stats.with_columns(
        pl.col("variant").cast(pl.Utf8),
        pl.col("run").cast(pl.Utf8),
        pl.col("writer").cast(pl.Utf8),
        pl.col("receiver").cast(pl.Utf8),
    )

    joined = pair_stats.join(
        write_counts,
        on=["variant", "run", "writer"],
        how="left",
    )
    if not skip_counts.is_empty():
        joined = joined.join(
            skip_counts,
            on=["variant", "run", "writer"],
            how="left",
        )
    else:
        joined = joined.with_columns(
            pl.lit(0).cast(pl.UInt32).alias("backpressure_skipped_count")
        )
    if not gaps.is_empty():
        joined = joined.join(
            gaps,
            on=["variant", "run", "writer", "receiver"],
            how="left",
        )
    else:
        joined = joined.with_columns(
            pl.lit(None).cast(pl.UInt32).alias("unresolved_gaps")
        )

    joined = joined.with_columns(
        pl.col("write_count").fill_null(0),
        pl.col("out_of_order").fill_null(0),
        pl.col("duplicates").fill_null(0),
        pl.col("backpressure_skipped_count").fill_null(0),
    ).sort(["variant", "run", "writer", "receiver"])

    results: list[IntegrityResult] = []
    for row in joined.iter_rows(named=True):
        qos = int(row["qos"]) if row["qos"] is not None else 1
        write_count = int(row["write_count"])
        receive_count = int(row["receive_count"])
        out_of_order = int(row["out_of_order"])
        duplicates = int(row["duplicates"])
        backpressure_skipped_count = int(row["backpressure_skipped_count"])

        delivery_pct = (receive_count / write_count * 100.0) if write_count > 0 else 0.0

        unresolved_gaps_raw = row.get("unresolved_gaps")
        if qos == 3:
            unresolved_gaps: int | None = (
                int(unresolved_gaps_raw) if unresolved_gaps_raw is not None else 0
            )
        else:
            unresolved_gaps = None

        completeness_error = False
        ordering_error = False
        duplicate_error = False
        gap_error = False

        if qos >= 3:
            completeness_error = receive_count < write_count
            ordering_error = out_of_order > 0
            duplicate_error = duplicates > 0
        elif qos == 2:
            ordering_error = out_of_order > 0

        if qos == 3 and unresolved_gaps is not None:
            gap_error = unresolved_gaps > 0

        # T14.17: attach the per-spawn timeout-classification for the
        # writer side of this row. Same classification value appears on
        # every (writer -> receiver) row that shares the writer, since
        # classification is a property of the writer's spawn, not the
        # writer/receiver pair.
        cls = classifications.get(row["writer"])
        if cls is not None:
            t_class = cls.classification
            t_sub = cls.sub_tags
        else:
            t_class = "unknown"
            t_sub = ()

        results.append(
            IntegrityResult(
                variant=row["variant"],
                run=row["run"],
                writer=row["writer"],
                receiver=row["receiver"],
                qos=qos,
                write_count=write_count,
                receive_count=receive_count,
                delivery_pct=delivery_pct,
                out_of_order=out_of_order,
                duplicates=duplicates,
                unresolved_gaps=unresolved_gaps,
                backpressure_skipped_count=backpressure_skipped_count,
                completeness_error=completeness_error,
                ordering_error=ordering_error,
                duplicate_error=duplicate_error,
                gap_error=gap_error,
                timeout_classification=t_class,
                timeout_sub_tags=t_sub,
            )
        )

    return results
