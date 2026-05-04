"""Per-group clock-offset extraction for cross-machine latency correction.

Reads ``clock_sync`` events out of a per-(variant, run) lazy frame and
returns a small DataFrame keyed by ``(runner, peer, variant, ts)`` that
``correlate.py`` then attaches to delivery records via a polars
``join_asof``.

Semantics (from ``metak-shared/api-contracts/clock-sync.md``): a
``clock_sync`` row with ``runner=R``, ``peer=P``, ``offset_ms=X`` means
that, as observed by R, P's clock was X ms ahead of R's clock at the
time of that measurement.

Therefore, to convert a ``receive_ts`` logged by R into the writer P's
frame, the analysis ADDS X to the raw delta:

    corrected_latency_ms = (receive_ts - write_ts) + offset_ms

Each ``clock_sync`` line carries the ``variant`` of the variant that is
about to start (or the empty string ``""`` for the initial sync that
runs before any variant). Analysis prefers the per-variant resync and
falls back to the initial sync when no per-variant entry is available
(see ``correlate.correlate_lazy``).

Note: ``clock_sync`` rows in a given group lazy frame may belong to
multiple variants because clock-sync logs are broadcast across every
``(variant, run)`` group during cache discovery (see
``cache.discover_groups``). The variant filter is applied at the
``correlate``-level join, not here.
"""

from __future__ import annotations

import polars as pl


# Output column order on the offset DataFrame.
OFFSET_COLUMNS: tuple[str, ...] = (
    "runner",
    "peer",
    "variant",
    "ts",
    "offset_ms",
)


def build_offset_table(group_lazy: pl.LazyFrame) -> pl.DataFrame:
    """Extract a sorted offset table from a per-group lazy frame.

    Filters ``group_lazy`` for ``event == "clock_sync"`` and projects out
    only the columns the asof-join needs. Returns a (small) materialized
    polars ``DataFrame`` because ``join_asof`` requires its right-hand
    side to be sorted; we sort once here and pass the result around.

    Returned schema:

    - ``runner`` (Utf8): the side that recorded the measurement (``self``)
    - ``peer`` (Utf8): the peer runner the offset is for
    - ``variant`` (Utf8): variant name in effect at measurement time, or
      ``""`` for the initial pre-variant sync
    - ``ts`` (Datetime): when the measurement was recorded by ``self``
    - ``offset_ms`` (Float64): ``peer.clock - self.clock`` in ms

    Sort order: ``(runner, peer, variant, ts)`` so the asof join can
    rely on ``ts`` being non-decreasing within each ``(runner, peer,
    variant)`` group.

    Returns an empty DataFrame with the correct schema when no
    ``clock_sync`` rows exist (typical for single-runner runs and any
    pre-T8.1 dataset).
    """
    df = (
        group_lazy.filter(pl.col("event") == "clock_sync")
        .filter(
            pl.col("peer").is_not_null()
            & pl.col("offset_ms").is_not_null()
            & pl.col("ts").is_not_null()
        )
        .select(
            pl.col("runner").cast(pl.Utf8).alias("runner"),
            pl.col("peer").cast(pl.Utf8).alias("peer"),
            pl.col("variant").cast(pl.Utf8).fill_null("").alias("variant"),
            pl.col("ts"),
            pl.col("offset_ms").cast(pl.Float64),
        )
        .sort(["runner", "peer", "variant", "ts"])
        .collect()
    )

    if df.is_empty():
        return pl.DataFrame(
            schema={
                "runner": pl.Utf8,
                "peer": pl.Utf8,
                "variant": pl.Utf8,
                "ts": pl.Datetime("ns", "UTC"),
                "offset_ms": pl.Float64,
            }
        )

    return df.select(list(OFFSET_COLUMNS))
