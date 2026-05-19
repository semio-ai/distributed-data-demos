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
    # E19 / T19.6: leaf-level loss accounting. Per the locked spec
    # ``api-contracts/jsonl-log-schema.md`` § E19 additions, ``leaf_count``
    # is per-WriteOp and a lost op contributes ``leaf_count`` leaves to
    # the total "scalar values lost" tally. The analyzer surfaces this
    # alongside the existing op-level loss% so the operator can read
    # off both "how many publish calls dropped" (existing) and "how
    # many scalar leaves dropped" (new) on the same row. Backward
    # compatible: for pre-E19 data where ``leaf_count == 1`` everywhere,
    # ``leaves_lost == ops_lost`` by construction. ``ops_lost`` is
    # ``max(0, write_count - receive_count)`` -- the same arithmetic
    # the delivery% column reflects, surfaced explicitly so the
    # ``Leaves Lost`` column stays self-contained.
    ops_lost: int = 0
    leaves_lost: int = 0
    # T17.9: count of ``backpressure_skipped`` events that the writer
    # emitted at the row's QoS level when ``qos >= 3``. Per
    # ``DESIGN.md`` § 6.5 (Strict No-Skip Contract for QoS 3/4) the
    # variant MUST block the publish call at QoS 3/4 rather than skip,
    # so any non-zero count here is a contract violation. The count is
    # always ``0`` for QoS 1/2 rows (skips are the contractual
    # back-pressure mechanism at those levels). Surfaces as
    # ``skip_at_reliable_error = True`` on this dataclass and as a
    # ``[FAIL: skip-at-reliable]`` annotation on the integrity table
    # row.
    skip_at_reliable_count: int = 0
    skip_at_reliable_error: bool = False
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


def _sum_leaves_written_per_writer(group: pl.LazyFrame) -> pl.DataFrame:
    """Sum ``leaf_count`` over write events per (variant, run, writer).

    E19 / T19.6: returns one row per writer carrying the total number
    of *scalar leaves* the writer published. Pre-E19 caches default the
    ``leaf_count`` column to ``1`` per write so the legacy behaviour
    (``sum == count``) is preserved without a special case. Null
    leaf_counts (defensive against malformed cache rows) are treated
    as ``1`` so a half-populated column still produces meaningful
    totals.
    """
    if "leaf_count" not in group.collect_schema().names():
        # Pre-T19.5 cache shards predate the column; fall back to
        # treating every write as one leaf. The schema-version bump in
        # T19.5 forces a one-shot rebuild on first read so in practice
        # this branch only fires for in-flight tests against a
        # synthetic LazyFrame missing the column.
        return (
            group.filter(pl.col("event") == "write")
            .filter(pl.col("seq").is_not_null() & pl.col("path").is_not_null())
            .group_by(["variant", "run", "runner"])
            .agg(pl.len().cast(pl.Int64).alias("leaves_written"))
            .rename({"runner": "writer"})
            .with_columns(
                pl.col("variant").cast(pl.Utf8),
                pl.col("run").cast(pl.Utf8),
                pl.col("writer").cast(pl.Utf8),
            )
            .collect()
        )

    return (
        group.filter(pl.col("event") == "write")
        .filter(pl.col("seq").is_not_null() & pl.col("path").is_not_null())
        .group_by(["variant", "run", "runner"])
        .agg(
            pl.col("leaf_count")
            .fill_null(1)
            .cast(pl.Int64)
            .sum()
            .alias("leaves_written")
        )
        .rename({"runner": "writer"})
        .with_columns(
            pl.col("variant").cast(pl.Utf8),
            pl.col("run").cast(pl.Utf8),
            pl.col("writer").cast(pl.Utf8),
        )
        .collect()
    )


def _sum_leaves_received_per_pair(deliveries: pl.DataFrame) -> pl.DataFrame:
    """Sum ``leaf_count`` over delivery rows per (variant, run, writer, receiver).

    E19 / T19.6: receives don't carry ``leaf_count`` on the wire; the
    correlator (``correlate_lazy``) propagates it from the matching
    write row via the (writer, seq, path) join key. Summing here gives
    the per-pair "scalar leaves delivered" number that pairs naturally
    with ``leaves_written`` from :func:`_sum_leaves_written_per_writer`
    to compute ``leaves_lost = leaves_written - leaves_received`` per
    (writer, receiver, qos) pair.

    Pre-E19 caches default ``leaf_count`` to ``1`` per row so the
    legacy behaviour (``leaves_received == receive_count``) is
    preserved with no special-casing.
    """
    if deliveries.is_empty() or "leaf_count" not in deliveries.columns:
        return pl.DataFrame(
            schema={
                "variant": pl.Utf8,
                "run": pl.Utf8,
                "writer": pl.Utf8,
                "receiver": pl.Utf8,
                "leaves_received": pl.Int64,
            }
        )
    return (
        deliveries.lazy()
        .group_by(["variant", "run", "writer", "receiver"])
        .agg(
            pl.col("leaf_count")
            .fill_null(1)
            .cast(pl.Int64)
            .sum()
            .alias("leaves_received")
        )
        .with_columns(
            pl.col("variant").cast(pl.Utf8),
            pl.col("run").cast(pl.Utf8),
            pl.col("writer").cast(pl.Utf8),
            pl.col("receiver").cast(pl.Utf8),
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


def _count_skip_at_reliable_per_writer_qos(group: pl.LazyFrame) -> pl.DataFrame:
    """Count ``backpressure_skipped`` events at QoS 3/4 per (writer, qos).

    Per ``DESIGN.md`` § 6.5 (Strict No-Skip Contract for QoS 3/4) and
    ``api-contracts/jsonl-log-schema.md``, ``backpressure_skipped`` is
    valid only at QoS 1/2. Any event emitted at ``qos in (3, 4)`` is a
    contract violation -- the variant should have blocked the publish
    call rather than reported the skip. Returns one row per
    ``(variant, run, writer, qos)`` where ``qos >= 3`` and the writer
    emitted at least one skip event; absence of a row means "no
    violation at that (writer, qos)". Used by ``integrity_for_group``
    to attach ``skip_at_reliable_count`` to every integrity row whose
    QoS matches, and by ``incomplete_warnings`` to emit per-violation
    WARN lines.
    """
    return (
        group.filter((pl.col("event") == "backpressure_skipped") & (pl.col("qos") >= 3))
        .group_by(["variant", "run", "runner", "qos"])
        .agg(pl.len().cast(pl.UInt32).alias("skip_at_reliable_count"))
        .rename({"runner": "writer"})
        .with_columns(
            pl.col("variant").cast(pl.Utf8),
            pl.col("run").cast(pl.Utf8),
            pl.col("writer").cast(pl.Utf8),
            pl.col("qos").cast(pl.Int64),
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
    leaves_written = _sum_leaves_written_per_writer(group)
    leaves_received = _sum_leaves_received_per_pair(deliveries)
    skip_counts = _count_backpressure_skipped_per_writer(group)
    skip_at_reliable_counts = _count_skip_at_reliable_per_writer_qos(group)
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
    if not leaves_written.is_empty():
        joined = joined.join(
            leaves_written,
            on=["variant", "run", "writer"],
            how="left",
        )
    else:
        joined = joined.with_columns(pl.lit(0).cast(pl.Int64).alias("leaves_written"))
    if not leaves_received.is_empty():
        joined = joined.join(
            leaves_received,
            on=["variant", "run", "writer", "receiver"],
            how="left",
        )
    else:
        joined = joined.with_columns(pl.lit(0).cast(pl.Int64).alias("leaves_received"))
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
    # T17.9: attach the per-(writer, qos) skip-at-reliable count. The
    # join keys include ``qos`` so the count only lands on rows whose
    # QoS level actually produced the violation. Rows at QoS 1/2 stay
    # null and are filled with 0 below.
    if not skip_at_reliable_counts.is_empty():
        joined = joined.with_columns(pl.col("qos").cast(pl.Int64)).join(
            skip_at_reliable_counts,
            on=["variant", "run", "writer", "qos"],
            how="left",
        )
    else:
        joined = joined.with_columns(
            pl.lit(0).cast(pl.UInt32).alias("skip_at_reliable_count")
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
        pl.col("skip_at_reliable_count").fill_null(0),
        pl.col("leaves_written").fill_null(0),
        pl.col("leaves_received").fill_null(0),
    ).sort(["variant", "run", "writer", "receiver"])

    results: list[IntegrityResult] = []
    for row in joined.iter_rows(named=True):
        qos = int(row["qos"]) if row["qos"] is not None else 1
        write_count = int(row["write_count"])
        receive_count = int(row["receive_count"])
        out_of_order = int(row["out_of_order"])
        duplicates = int(row["duplicates"])
        backpressure_skipped_count = int(row["backpressure_skipped_count"])
        skip_at_reliable_count = int(row["skip_at_reliable_count"])
        # E19 / T19.6: leaf-level loss accounting. ``ops_lost`` is the
        # writer-side count of writes that never made it to this
        # receiver (clamped at zero -- a writer may publish *after* a
        # receiver's window closes, which is benign and we don't want
        # to surface as "negative loss"). ``leaves_lost`` is the
        # corresponding scalar-value count, derived from the
        # ``leaf_count`` propagated through the (writer, seq, path)
        # join key by ``correlate_lazy``. Both collapse to the
        # op-equivalent on pre-E19 data where ``leaf_count = 1`` for
        # every row.
        leaves_written_val = int(row.get("leaves_written") or 0)
        leaves_received_val = int(row.get("leaves_received") or 0)
        ops_lost = max(0, write_count - receive_count)
        leaves_lost = max(0, leaves_written_val - leaves_received_val)

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
        # T17.9: skip-at-reliable is a contract violation independent
        # of completeness/ordering/duplicate checks. It fires whenever
        # the row's QoS is 3 or 4 and the writer emitted at least one
        # ``backpressure_skipped`` event at that QoS. Per
        # ``DESIGN.md`` § 6.5 the variant should have blocked instead.
        skip_at_reliable_error = qos >= 3 and skip_at_reliable_count > 0

        # T14.17 follow-up (2026-05-14): the ordering check is QoS-aware.
        # qos1 (best-effort) and qos2 (latest-value) are datagram-style
        # QoS levels with no ordering guarantee by design -- the
        # WebRTC qos1/qos2 implementations rely on the underlying
        # transport's unreliable/unordered datagram channel and
        # therefore observe out-of-order receives as a normal feature
        # of the protocol, not a failure. Only qos3 (reliable-ordered)
        # and qos4 (reliable-tcp) carry an ordering contract; the
        # ``[FAIL: ordering]`` annotation is reserved for those.
        if qos >= 3:
            completeness_error = receive_count < write_count
            ordering_error = out_of_order > 0
            duplicate_error = duplicates > 0

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
                skip_at_reliable_count=skip_at_reliable_count,
                skip_at_reliable_error=skip_at_reliable_error,
                timeout_classification=t_class,
                timeout_sub_tags=t_sub,
                ops_lost=ops_lost,
                leaves_lost=leaves_lost,
            )
        )

    return results
