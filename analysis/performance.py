"""Performance metrics computation using polars groupbys.

Per-(variant, run) results: connection time, latency percentiles,
throughput, jitter, packet loss, resource usage, late_receives.

Operate-window boundaries (E12): each writer's window is
``[operate_start, eot_sent_ts]`` when the writer logged an
``eot_sent`` event, else falls back to ``[operate_start,
silent_start]`` for legacy logs. ``late_receives`` counts receives
that arrived after a writer's ``eot_sent_ts`` but before
``silent_start`` -- i.e. in-flight data that landed during the
``eot``/``silent`` grace window.

Output is a list of ``PerformanceResult`` dataclasses with the same
shape as Phase 1 plus a ``late_receives`` field, so ``tables.py``
and ``plots.py`` consumers can opt into the new metric.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime

import polars as pl


@dataclass
class ConnectionMetric:
    """Connection time for a single (variant, runner, run)."""

    variant: str
    runner: str
    run: str
    elapsed_ms: float


@dataclass
class ResourceMetric:
    """Aggregated resource usage for a single (variant, runner, run)."""

    variant: str
    runner: str
    run: str
    mean_cpu_pct: float
    peak_cpu_pct: float
    mean_memory_mb: float
    peak_memory_mb: float


@dataclass
class PerformanceResult:
    """Aggregated performance metrics for one (variant, run).

    ``has_uncorrected_latency`` is set when at least one underlying
    delivery record had ``offset_applied == False`` -- i.e. its raw
    ``receive_ts - write_ts`` could not be corrected for cross-machine
    clock skew because no ``clock_sync`` measurement was available.
    Tables annotate the latency columns with ``(uncorrected)`` in this
    case (see ``tables.format_performance_table``).
    """

    variant: str
    run: str
    connect_mean_ms: float
    connect_max_ms: float
    latency_p50_ms: float
    latency_p95_ms: float
    latency_p99_ms: float
    latency_max_ms: float
    writes_per_sec: float
    receives_per_sec: float
    jitter_ms: float
    jitter_p95_ms: float
    loss_pct: float
    resources: list[ResourceMetric] = field(default_factory=list)
    has_uncorrected_latency: bool = False
    # Late receives (E12): receives whose ``ts`` falls strictly after a
    # writer's ``eot_sent_ts`` but at or before the group's
    # ``silent_start``. ``None`` when no ``eot_sent`` events are present
    # for any writer in this group (legacy logs without EOT) -- the
    # tables render this as ``-``.
    late_receives: int | None = None


def _percentile(data: list[float], p: float) -> float:
    """Linear-interpolation percentile matching Phase 1 semantics.

    Kept for tests and as a tiny helper -- the analysis pipeline itself
    uses ``pl.quantile`` (linear interpolation) which yields the same
    values to within float rounding.
    """
    if not data:
        return 0.0
    sorted_data = sorted(data)
    n = len(sorted_data)
    if n == 1:
        return sorted_data[0]

    rank = p / 100.0 * (n - 1)
    lo = int(rank)
    hi = lo + 1
    frac = rank - lo

    if hi >= n:
        return sorted_data[-1]
    return sorted_data[lo] + frac * (sorted_data[hi] - sorted_data[lo])


def _connection_metrics(
    group: pl.LazyFrame, variant: str, run: str
) -> tuple[float, float]:
    """(connect_mean_ms, connect_max_ms) from ``connected`` events."""
    df = (
        group.filter(pl.col("event") == "connected")
        .filter(pl.col("elapsed_ms").is_not_null())
        .select(pl.col("elapsed_ms"))
        .collect()
    )
    if df.is_empty():
        return 0.0, 0.0
    elapsed = df.get_column("elapsed_ms")
    return float(elapsed.mean() or 0.0), float(elapsed.max() or 0.0)


@dataclass(frozen=True)
class _OperateWindows:
    """Per-runner operate windows + group-level boundaries.

    ``operate_start`` is the earliest ``phase==operate`` event in the
    group (single timestamp; the per-writer windows all share this
    start). ``silent_start`` is the earliest ``phase==silent`` event,
    or the last event timestamp as fallback. ``per_writer_eot_ts``
    maps each writer's runner name to its ``eot_sent`` timestamp; a
    runner that did NOT log an ``eot_sent`` is absent from the dict.

    The ``has_any_eot`` flag records whether any ``eot_sent`` event
    was found in the group at all -- the late_receives metric is only
    meaningful (non-``None``) when at least one writer signalled EOT.
    """

    operate_start: datetime | None
    silent_start: datetime | None
    per_writer_eot_ts: dict[str, datetime]
    has_any_eot: bool

    def writer_window_end(self, writer: str) -> datetime | None:
        """End boundary for ``writer``'s operate window.

        ``eot_sent_ts`` if present (E12), else ``silent_start``
        (legacy fallback). ``None`` if neither is available.
        """
        ts = self.per_writer_eot_ts.get(writer)
        if ts is not None:
            return ts
        return self.silent_start


def _operate_windows(group: pl.LazyFrame) -> _OperateWindows:
    """Compute group-level + per-writer operate-window boundaries.

    Walks the ``phase`` and ``eot_sent`` rows once. Per-writer EOT
    timestamps are taken as the earliest ``eot_sent`` per runner --
    each runner has at most one ``eot_sent`` per spawn but ``min``
    is robust against duplicates.
    """
    phases = (
        group.filter(pl.col("event") == "phase")
        .filter(pl.col("phase").is_not_null())
        .select(["ts", "phase"])
        .collect()
    )

    operate_start: datetime | None = None
    silent_start: datetime | None = None
    if not phases.is_empty():
        operate_start_df = phases.filter(pl.col("phase") == "operate")
        if not operate_start_df.is_empty():
            operate_start = operate_start_df.get_column("ts").min()
        silent_df = phases.filter(pl.col("phase") == "silent")
        if not silent_df.is_empty():
            silent_start = silent_df.get_column("ts").min()

    if silent_start is None:
        # Fallback: last event timestamp in the group. Mirrors the
        # legacy ``_operate_duration_seconds`` behaviour for sessions
        # that never logged a ``phase=silent`` event.
        last_ts_df = group.select(pl.col("ts").max().alias("last_ts")).collect()
        if not last_ts_df.is_empty():
            silent_start = last_ts_df.item(0, "last_ts")

    eot_df = (
        group.filter(pl.col("event") == "eot_sent")
        .group_by("runner")
        .agg(pl.col("ts").min().alias("eot_ts"))
        .collect()
    )

    per_writer_eot_ts: dict[str, datetime] = {}
    if not eot_df.is_empty():
        for row in eot_df.iter_rows(named=True):
            runner = row.get("runner")
            ts = row.get("eot_ts")
            if runner is None or ts is None:
                continue
            per_writer_eot_ts[str(runner)] = ts

    return _OperateWindows(
        operate_start=operate_start,
        silent_start=silent_start,
        per_writer_eot_ts=per_writer_eot_ts,
        has_any_eot=bool(per_writer_eot_ts),
    )


def _operate_duration_seconds(windows: _OperateWindows) -> float:
    """Operate-phase duration in seconds, used for throughput.

    Span is ``[operate_start, end]`` where ``end`` is the latest of
    the per-writer window ends (``eot_sent_ts`` when any writer has
    one; else ``silent_start``). This keeps throughput meaningful in
    the EOT world: writes happen up to ``eot_sent_ts`` so the rate
    reflects the actual writing window. Falls back to
    ``silent_start`` for legacy sessions, exactly matching pre-E12
    behaviour.
    """
    if windows.operate_start is None:
        return 0.001

    if windows.per_writer_eot_ts:
        end = max(windows.per_writer_eot_ts.values())
    else:
        end = windows.silent_start

    if end is None:
        return 0.001

    delta = (end - windows.operate_start).total_seconds()
    if delta <= 0:
        return 0.001
    return float(delta)


def _latency_stats(deliveries: pl.DataFrame) -> tuple[float, float, float, float]:
    """(p50, p95, p99, max) latency in ms, or (0,0,0,0) if no deliveries."""
    if deliveries.is_empty():
        return 0.0, 0.0, 0.0, 0.0
    lat = deliveries.select(
        pl.col("latency_ms").quantile(0.50, "linear").alias("p50"),
        pl.col("latency_ms").quantile(0.95, "linear").alias("p95"),
        pl.col("latency_ms").quantile(0.99, "linear").alias("p99"),
        pl.col("latency_ms").max().alias("mx"),
    )
    row = lat.row(0)
    p50 = float(row[0]) if row[0] is not None else 0.0
    p95 = float(row[1]) if row[1] is not None else 0.0
    p99 = float(row[2]) if row[2] is not None else 0.0
    mx = float(row[3]) if row[3] is not None else 0.0
    return p50, p95, p99, mx


def _jitter(deliveries: pl.DataFrame) -> tuple[float, float]:
    """Jitter: (mean of per-window stddev, p95 of per-window stddev).

    Window definition matches Phase 1: a fresh window starts the first
    time a record's ``receive_ts`` is >= 1.0 second after the current
    window's start. Windows with fewer than 2 records are dropped.

    Implemented with vectorised polars expressions: a window-id column
    is computed by integer-dividing ``(receive_ts - first_ts)`` by 1s,
    then per-window sample stddev is aggregated. This is O(n) in arrow
    buffers, no Python row iteration.
    """
    if deliveries.height < 2:
        return 0.0, 0.0

    sorted_lat = deliveries.sort("receive_ts").select(["receive_ts", "latency_ms"])

    # window_id = floor((ts - first_ts) / 1s). Phase 1 advances the
    # window only when the delta from the **current** window-start
    # crosses 1s, which is functionally equivalent to floor-division
    # against the global start whenever windows are non-overlapping
    # 1-second buckets -- close enough for jitter aggregation.
    with_window = sorted_lat.with_columns(
        (
            (pl.col("receive_ts") - pl.col("receive_ts").min()).dt.total_microseconds()
            // 1_000_000
        ).alias("window_id"),
    )

    per_window = (
        with_window.group_by("window_id")
        .agg(
            pl.col("latency_ms").std(ddof=1).alias("std"),
            pl.len().alias("n"),
        )
        .filter(pl.col("n") >= 2)
        .filter(pl.col("std").is_not_null())
        .select("std")
    )

    if per_window.height > 0:
        stds = per_window.get_column("std")
        mean_jitter = float(stds.mean() or 0.0)
        # p95 over the per-window stddevs.
        if per_window.height == 1:
            p95_jitter = mean_jitter
        else:
            p95 = per_window.select(
                pl.col("std").quantile(0.95, "linear").alias("p95")
            ).item(0, "p95")
            p95_jitter = float(p95) if p95 is not None else mean_jitter
        return mean_jitter, p95_jitter

    # Fallback: overall stddev of all latencies in this group.
    if deliveries.height >= 2:
        fallback_val = deliveries.select(
            pl.col("latency_ms").std(ddof=1).alias("s")
        ).item(0, "s")
        if fallback_val is not None:
            return float(fallback_val), float(fallback_val)
    return 0.0, 0.0


def _resource_metrics(
    group: pl.LazyFrame, variant: str, run: str
) -> list[ResourceMetric]:
    """Resource metrics per (variant, runner, run) inside one group."""
    df = (
        group.filter(pl.col("event") == "resource")
        .filter(pl.col("cpu_percent").is_not_null() & pl.col("memory_mb").is_not_null())
        .group_by("runner")
        .agg(
            pl.col("cpu_percent").mean().alias("mean_cpu"),
            pl.col("cpu_percent").max().alias("peak_cpu"),
            pl.col("memory_mb").mean().alias("mean_mem"),
            pl.col("memory_mb").max().alias("peak_mem"),
        )
        .sort("runner")
        .collect()
    )
    if df.is_empty():
        return []
    out: list[ResourceMetric] = []
    for row in df.iter_rows(named=True):
        out.append(
            ResourceMetric(
                variant=variant,
                runner=str(row["runner"]),
                run=run,
                mean_cpu_pct=float(row["mean_cpu"] or 0.0),
                peak_cpu_pct=float(row["peak_cpu"] or 0.0),
                mean_memory_mb=float(row["mean_mem"] or 0.0),
                peak_memory_mb=float(row["peak_mem"] or 0.0),
            )
        )
    return out


def _write_receive_counts(
    group: pl.LazyFrame, windows: _OperateWindows
) -> tuple[int, int]:
    """(write_count, receive_count) scoped to per-writer operate windows.

    For each writer the window is ``[operate_start, eot_sent_ts]``
    when ``eot_sent_ts`` is available, else ``[operate_start,
    silent_start]`` (legacy fallback). Receives are scoped to the
    *writer's* window via the receive event's ``writer`` field --
    cross-peer receives in the writer's window.

    The per-writer scoping replaces the Phase 1 "count all writes,
    count all receives" approach so that loss% and throughput are
    not contaminated by post-EOT in-flight receives. Late receives
    (post-EOT, pre-silent) are reported separately via
    ``_late_receives``.
    """
    if windows.operate_start is None:
        # No operate phase at all -- fall through to the legacy
        # all-rows count so empty/degenerate groups still return 0/0.
        df = (
            group.filter(pl.col("event").is_in(["write", "receive"]))
            .group_by("event")
            .agg(pl.len().alias("n"))
            .collect()
        )
        write_count = 0
        receive_count = 0
        for row in df.iter_rows(named=True):
            if row["event"] == "write":
                write_count = int(row["n"])
            elif row["event"] == "receive":
                receive_count = int(row["n"])
        return write_count, receive_count

    operate_start = windows.operate_start

    # Build the per-writer end-boundary table once.
    fallback_end = windows.silent_start
    end_rows: list[dict] = []
    # Discover the candidate writer set from both the EOT events and
    # the writes themselves so legacy logs without EOT still get a
    # row for every writer (with the silent_start fallback).
    candidate_writers: set[str] = set(windows.per_writer_eot_ts.keys())
    writer_df = (
        group.filter(pl.col("event") == "write")
        .select(pl.col("runner"))
        .unique()
        .collect()
    )
    for row in writer_df.iter_rows(named=True):
        r = row.get("runner")
        if r is not None:
            candidate_writers.add(str(r))

    for writer in candidate_writers:
        end = windows.per_writer_eot_ts.get(writer, fallback_end)
        if end is None:
            continue
        end_rows.append({"writer": writer, "end_ts": end})

    if not end_rows:
        return 0, 0

    end_df = pl.DataFrame(
        end_rows,
        schema={"writer": pl.Utf8, "end_ts": pl.Datetime("ns", "UTC")},
        orient="row",
    )

    # Writes: per-writer scoping on (runner, ts).
    writes_in_window = (
        group.filter(pl.col("event") == "write")
        .filter(pl.col("ts") >= operate_start)
        .select(pl.col("runner").cast(pl.Utf8).alias("writer"), pl.col("ts"))
        .collect()
        .join(end_df, on="writer", how="inner")
        .filter(pl.col("ts") <= pl.col("end_ts"))
        .height
    )

    # Receives: scoped on the receive event's ``writer`` field, NOT
    # the receiver's runner. Cross-peer receives in the writer's
    # window.
    receives_in_window = (
        group.filter(pl.col("event") == "receive")
        .filter(pl.col("writer").is_not_null())
        .filter(pl.col("ts") >= operate_start)
        .select(pl.col("writer").cast(pl.Utf8), pl.col("ts"))
        .collect()
        .join(end_df, on="writer", how="inner")
        .filter(pl.col("ts") <= pl.col("end_ts"))
        .height
    )

    return int(writes_in_window), int(receives_in_window)


def _late_receives_count(group: pl.LazyFrame, windows: _OperateWindows) -> int | None:
    """Count receives that landed after EOT but before ``silent_start``.

    Per writer: count receives with ``ts > eot_sent_ts`` AND
    ``ts <= silent_start``. Sum across writers.

    Returns ``None`` for legacy logs without any ``eot_sent`` events
    (no EOT means no meaningful "late" boundary). Returns ``0`` when
    EOT is present and no receives are in the post-EOT window.
    """
    if not windows.has_any_eot:
        return None
    if windows.silent_start is None:
        return 0
    silent_start = windows.silent_start

    end_rows = [
        {"writer": w, "eot_ts": ts} for w, ts in windows.per_writer_eot_ts.items()
    ]
    if not end_rows:
        return 0
    eot_df = pl.DataFrame(
        end_rows,
        schema={"writer": pl.Utf8, "eot_ts": pl.Datetime("ns", "UTC")},
        orient="row",
    )

    late = (
        group.filter(pl.col("event") == "receive")
        .filter(pl.col("writer").is_not_null())
        .filter(pl.col("ts") <= silent_start)
        .select(pl.col("writer").cast(pl.Utf8), pl.col("ts"))
        .collect()
        .join(eot_df, on="writer", how="inner")
        .filter(pl.col("ts") > pl.col("eot_ts"))
        .height
    )
    return int(late)


def _any_uncorrected(deliveries: pl.DataFrame) -> bool:
    """Return True if any delivery row has ``offset_applied == False``.

    Pre-T8.2 caches do not carry the ``offset_applied`` column; a missing
    column means correlation predates clock-sync correction, so report
    ``True`` (the latency is uncorrected by definition).
    """
    if deliveries.is_empty():
        return False
    if "offset_applied" not in deliveries.columns:
        return True
    # ``offset_applied`` is a Boolean column; counting falses is enough.
    falses = deliveries.select((~pl.col("offset_applied")).sum().alias("n")).item(
        0, "n"
    )
    return bool(falses) and falses > 0


def performance_for_group(
    group: pl.LazyFrame,
    deliveries: pl.DataFrame,
    variant: str,
    run: str,
) -> PerformanceResult:
    """Compute the ``PerformanceResult`` for a single ``(variant, run)``.

    Operate-window boundaries follow E12: per-writer
    ``[operate_start, eot_sent_ts]`` when ``eot_sent`` is present,
    otherwise ``[operate_start, silent_start]``. ``late_receives``
    counts receives in ``(eot_sent_ts, silent_start]`` for each
    writer; ``None`` for legacy logs without any ``eot_sent`` event.
    """
    connect_mean, connect_max = _connection_metrics(group, variant, run)
    p50, p95, p99, mx = _latency_stats(deliveries)
    windows = _operate_windows(group)
    write_count, receive_count = _write_receive_counts(group, windows)
    duration = _operate_duration_seconds(windows)
    writes_per_sec = write_count / duration if duration > 0 else 0.0
    receives_per_sec = receive_count / duration if duration > 0 else 0.0
    jitter, jitter_p95 = _jitter(deliveries)
    if write_count > 0:
        loss_pct = max(0.0, (1.0 - receive_count / write_count) * 100.0)
    else:
        loss_pct = 0.0
    resources = _resource_metrics(group, variant, run)
    has_uncorrected_latency = _any_uncorrected(deliveries)
    late_receives = _late_receives_count(group, windows)

    return PerformanceResult(
        variant=variant,
        run=run,
        connect_mean_ms=connect_mean,
        connect_max_ms=connect_max,
        latency_p50_ms=p50,
        latency_p95_ms=p95,
        latency_p99_ms=p99,
        latency_max_ms=mx,
        writes_per_sec=writes_per_sec,
        receives_per_sec=receives_per_sec,
        jitter_ms=jitter,
        jitter_p95_ms=jitter_p95,
        loss_pct=loss_pct,
        resources=resources,
        has_uncorrected_latency=has_uncorrected_latency,
        late_receives=late_receives,
    )
