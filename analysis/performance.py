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
and ``plots.py`` consumers can opt into the new metric. The result
also carries a downsampled per-message ``latency_samples_ms`` vector
(cap ``LATENCY_SAMPLE_CAP`` per result) so plots that need
distribution shape -- the latency CDF in particular -- do not have
to re-correlate the source shards.
"""

from __future__ import annotations

import statistics
from dataclasses import dataclass, field
from datetime import datetime

import polars as pl

# Maximum number of per-message latency samples retained on each
# ``PerformanceResult`` for distribution-shape plots (e.g. the CDF in
# ``plots.generate_latency_cdf_plot``). 50k samples gives a faithful
# empirical CDF -- the 99.99th percentile is bracketed within ~5 samples
# -- while keeping memory bounded: a Python ``list[float]`` of 50k
# entries is ~3 MB, so 64 (variant, run) groups stay under 200 MB.
# When a group has more than ``LATENCY_SAMPLE_CAP`` deliveries we
# downsample with a deterministic stride ``ceil(n / cap)`` over the
# delivery-record order. The order is the polars-emitted row order from
# ``correlate_lazy``: receives sorted by (receiver, writer, receive_ts)
# after the asof join, which is a stable temporal traversal -- a
# stride-sample of it is unbiased w.r.t. latency distribution. We do
# NOT reservoir-sample because determinism across runs is more useful
# for diff-debugging plots than the marginal accuracy improvement.
LATENCY_SAMPLE_CAP: int = 50_000


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
    # Threading-mode dimension (E14 / T11.5). Read from the
    # ``connected`` event's ``threading_mode`` field; defaults to
    # ``"single"`` for pre-T14.8 logs where the field is absent. When
    # a group has multiple distinct values across runners (mixed
    # spawn -- unusual), the first sorted value is used; in practice
    # all runners in a single run share the same mode because the
    # runner sets it from the expanded TOML dimension.
    threading_mode: str = "single"
    # Late receives (E12): receives whose ``ts`` falls strictly after a
    # writer's ``eot_sent_ts`` but at or before the group's
    # ``silent_start``. ``None`` when no ``eot_sent`` events are present
    # for any writer in this group (legacy logs without EOT) -- the
    # tables render this as ``-``.
    late_receives: int | None = None
    # T11.5 late-receive-TAIL metric: count and percentage of delivery
    # records whose latency exceeds 10x the group's p99 latency. This
    # is distinct from ``late_receives`` (which is the EOT-window
    # boundary metric from E12). The tail metric surfaces extreme
    # outliers within the delivery distribution itself. ``late_count``
    # is the absolute count; ``late_pct`` is 100 * late_count /
    # total_receives. Both are 0.0 when there are no deliveries (no
    # outliers to find).
    late_receives_tail_count: int = 0
    late_receives_tail_pct: float = 0.0
    # Downsampled per-message latency vector for distribution-shape
    # plots (latency CDF). Capped at ``LATENCY_SAMPLE_CAP``; see the
    # module docstring for the sampling strategy. Empty when the group
    # had no deliveries. Populated from the same delivery DataFrame
    # that produces ``latency_p50_ms`` etc., so values are already
    # clock-skew-corrected when ``has_uncorrected_latency`` is False.
    latency_samples_ms: list[float] = field(default_factory=list)
    # Pivot-tables additions (T-pivot.1). ``latency_mean_ms`` and
    # ``latency_std_ms`` are computed from ``latency_samples_ms`` using
    # ``statistics.mean`` / ``statistics.stdev`` from the stdlib (sample
    # std-dev, ddof=1). When the sample vector is empty (no deliveries),
    # both are ``nan`` so the pivot renderer can distinguish "no data"
    # from a genuine zero. When the vector has exactly one sample,
    # std-dev is 0.0 (a one-sample population has no spread).
    #
    # ``expected_writes_per_sec`` is the nominal per-writer publish rate
    # parsed from the spawn name (``tick_rate_hz * values_per_tick``) for
    # the ``scalar-flood`` workload. It is ``None`` for the unbounded
    # ``max-throughput`` workload where no nominal rate exists.
    #
    # ``receives_to_expected_ratio_pct`` is
    # ``100 * receives_per_sec / expected_writes_per_sec``. It is the
    # "expected 10k/s but got 5k/s = 50%" metric used by the pivot
    # tables. Can exceed 100% for multicast variants where the receiver
    # also gets its own loopback writes (e.g. custom-udp single-mode
    # subscribes to its own multicast group). ``None`` when the
    # expected rate is undefined (max-throughput).
    latency_mean_ms: float = float("nan")
    latency_std_ms: float = float("nan")
    expected_writes_per_sec: float | None = None
    receives_to_expected_ratio_pct: float | None = None
    # E19 / T19.5: workload-shape headline metrics. All three are
    # derived from correlated deliveries (receive side) so they reflect
    # what the wire actually delivered, not just what was requested.
    #
    # - ``ops_per_sec`` is the count of received WriteOps per operate
    #   second. Equal to ``receives_per_sec`` (kept as a separate field
    #   for the workload-shape vocabulary).
    # - ``leaves_per_sec`` is the sum of ``leaf_count`` across all
    #   correlated deliveries divided by ``operate_secs`` -- the
    #   canonical cross-workload comparable metric. For ``scalar-flood``
    #   data ``leaves_per_sec == ops_per_sec`` because every WriteOp
    #   carries one leaf.
    # - ``bytes_per_sec`` is the sum of ``bytes`` across all correlated
    #   deliveries divided by ``operate_secs``. Falls back to ``0.0`` on
    #   legacy data where the ``bytes`` column is null.
    # - ``shape`` is the dominant ``shape`` value across this group's
    #   delivered writes (the writer is supposed to emit a single
    #   profile per spawn so this is a near-degenerate aggregation in
    #   practice; we pick the lexicographically-first non-null value
    #   for determinism when a group happens to mix shapes).
    ops_per_sec: float = 0.0
    leaves_per_sec: float = 0.0
    bytes_per_sec: float = 0.0
    shape: str = "scalar"


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


def _threading_mode(group: pl.LazyFrame) -> str:
    """Threading-mode dimension for this group.

    Read from the ``threading_mode`` column on ``connected`` events.
    Defaults to ``"single"`` when:

    - the column is null on every connected row (pre-T14.8 logs); or
    - there are no ``connected`` events in the group.

    When multiple distinct values are present (rare -- a mixed spawn),
    the lexicographically-first non-null value is chosen for stability
    across runs. The contract in ``api-contracts/variant-cli.md`` is
    that the runner picks a single mode per spawn from the expanded
    TOML dimension, so a stable single-value choice matches reality.
    """
    # ``collect_schema().names()`` is the polars-recommended way to
    # ask for column names without triggering the eager-schema warning
    # that ``LazyFrame.columns`` emits.
    if "threading_mode" not in group.collect_schema().names():
        # Pre-T11.5 cached shards predate the column; absence means
        # "default" by definition.
        return "single"

    df = (
        group.filter(pl.col("event") == "connected")
        .filter(pl.col("threading_mode").is_not_null())
        .select(pl.col("threading_mode"))
        .unique()
        .sort("threading_mode")
        .collect()
    )
    if df.is_empty():
        return "single"
    value = df.item(0, "threading_mode")
    if value is None or value == "":
        return "single"
    return str(value)


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


def _latency_mean_std(samples: list[float]) -> tuple[float, float]:
    """Mean and sample std-dev of ``samples`` in ms.

    Returns ``(nan, nan)`` when ``samples`` is empty so the pivot
    renderer can distinguish "no deliveries" from a genuine zero
    latency. A single-sample vector returns ``(value, 0.0)`` -- one
    sample has no spread, but its mean is well-defined.

    Uses ``statistics.mean`` / ``statistics.stdev`` from the stdlib (no
    new dependencies). ``stdev`` uses ddof=1 (sample stddev) which is
    the same convention used by ``_jitter`` above.
    """
    n = len(samples)
    if n == 0:
        return float("nan"), float("nan")
    mean = statistics.mean(samples)
    if n == 1:
        return float(mean), 0.0
    return float(mean), float(statistics.stdev(samples))


def _latency_samples(deliveries: pl.DataFrame) -> list[float]:
    """Downsample the per-message latency column to at most ``LATENCY_SAMPLE_CAP``.

    Strategy: take every ``stride``-th row in the delivery record's
    natural order, where ``stride = ceil(n / LATENCY_SAMPLE_CAP)``.
    For ``n <= LATENCY_SAMPLE_CAP`` the full vector is returned. Null
    values are filtered (a null latency would also be excluded from the
    percentile pipeline, so the sample vector mirrors that contract).
    """
    if deliveries.is_empty() or "latency_ms" not in deliveries.columns:
        return []

    col = deliveries.get_column("latency_ms").drop_nulls()
    n = col.len()
    if n == 0:
        return []
    if n <= LATENCY_SAMPLE_CAP:
        return [float(v) for v in col.to_list()]

    # ceil-division stride keeps us at or below the cap.
    stride = (n + LATENCY_SAMPLE_CAP - 1) // LATENCY_SAMPLE_CAP
    sampled = col.gather_every(stride)
    return [float(v) for v in sampled.to_list()]


def _late_tail_stats(deliveries: pl.DataFrame, p99_ms: float) -> tuple[int, float]:
    """T11.5 late-receive-tail count + percentage.

    Definition: a delivery whose ``latency_ms`` exceeds ``10 * p99``
    (where ``p99`` is this group's 99th-percentile latency) is part of
    the late tail. Returns ``(count, percentage_of_total_receives)``.

    Clock-correction is already applied to ``latency_ms`` upstream
    (see ``correlate._attach_offsets``), so the threshold operates on
    the same corrected values as the percentiles. Groups whose p99 is
    zero (e.g. degenerate single-delivery groups, or groups where every
    latency is zero) define the threshold as zero too -- which means
    *any* non-zero latency would be flagged. The unit test for hand-
    computed parity uses a non-degenerate group where ``p99 = 10`` ms,
    so the practical contract is unambiguous; the zero-p99 edge case
    only occurs in synthetic or pathological inputs.

    Returns ``(0, 0.0)`` when there are no deliveries.
    """
    if deliveries.is_empty() or "latency_ms" not in deliveries.columns:
        return 0, 0.0

    threshold = p99_ms * 10.0
    counts = deliveries.select(
        pl.col("latency_ms").is_not_null().sum().alias("total"),
        (pl.col("latency_ms") > threshold).sum().alias("late"),
    ).row(0)
    total = int(counts[0] or 0)
    late = int(counts[1] or 0)
    if total == 0:
        return 0, 0.0
    return late, 100.0 * late / total


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
    group: pl.LazyFrame,
    deliveries: pl.DataFrame,
    windows: _OperateWindows,
) -> tuple[int, int]:
    """(write_count, receive_count) scoped to per-writer operate windows.

    For each writer the window is ``[operate_start, eot_sent_ts]``
    when ``eot_sent_ts`` is available, else ``[operate_start,
    silent_start]`` (legacy fallback).

    **Writer-clock-only accounting (T16.16)**: both halves use the
    *writer's* ``write_ts`` as the boundary input -- never the
    receiver's ``receive_ts``. The write count is the number of
    ``write`` events whose ``ts`` (writer clock) falls in the writer's
    window. The receive count is the number of correlated deliveries
    (rows in ``deliveries``) whose source ``write_ts`` (writer clock)
    falls in the writer's window.

    Before T16.16 the receive count was the number of ``receive``
    events whose receiver-clock ``ts`` fell in the writer's window.
    That cross-clock comparison broke on two-machine runs with
    unsynchronised OS clocks: legitimate receives whose corresponding
    writes were in-window got systematically excluded when the
    receiver's clock drift pushed ``receive_ts`` past the writer's
    ``eot_sent_ts``, which manifested as a spurious ~1% loss baseline
    across every transport at QoS 4 low rates. Counting by message
    identity (matched delivery) instead of by raw timestamp removes
    the dependency on cross-clock comparability for the delivery
    accounting. Latency reporting still needs E8 clock-sync for
    cross-machine correctness; delivery accounting no longer does.

    Late receives (post-EOT, pre-silent in the receiver's local clock)
    are reported separately via ``_late_receives_count`` as an
    observability metric and are not part of the loss formula.
    """
    if windows.operate_start is None:
        # No operate phase at all -- fall through to the legacy
        # all-rows count so empty/degenerate groups still return
        # ``write_count`` from raw event counts and ``receive_count``
        # from the delivery row count. This matches the pre-T16.16
        # fallback for empty/degenerate groups; the asymmetry vs the
        # main branch is benign because no operate window means no
        # meaningful loss% anyway.
        df = (
            group.filter(pl.col("event") == "write")
            .select(pl.len().alias("n"))
            .collect()
        )
        write_count = int(df.item(0, "n")) if not df.is_empty() else 0
        receive_count = int(deliveries.height)
        return write_count, receive_count

    operate_start = windows.operate_start

    # Build the per-writer end-boundary table once.
    fallback_end = windows.silent_start
    end_rows: list[dict] = []
    # Discover the candidate writer set from the EOT events, the
    # writes themselves, AND the deliveries DataFrame so we cover
    # every writer that contributes to either half of the count.
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
    if not deliveries.is_empty() and "writer" in deliveries.columns:
        for w in deliveries.get_column("writer").unique().to_list():
            if w is not None:
                candidate_writers.add(str(w))

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

    # Writes: per-writer scoping on (runner, ts). The boundary is the
    # writer's own clock on both sides -- no cross-clock issue.
    writes_in_window = (
        group.filter(pl.col("event") == "write")
        .filter(pl.col("ts") >= operate_start)
        .select(pl.col("runner").cast(pl.Utf8).alias("writer"), pl.col("ts"))
        .collect()
        .join(end_df, on="writer", how="inner")
        .filter(pl.col("ts") <= pl.col("end_ts"))
        .height
    )

    # Receives: count deliveries whose source write happened in the
    # writer's window. ``write_ts`` is in the writer's clock so the
    # comparison is intra-clock by construction. This is the T16.16
    # fix -- pre-fix code filtered ``receive_ts`` (receiver clock)
    # against the writer's ``end_ts`` (writer clock), which mis-counted
    # on unsynced two-machine runs.
    if deliveries.is_empty() or "write_ts" not in deliveries.columns:
        receives_in_window = 0
    else:
        receives_in_window = (
            deliveries.select(
                pl.col("writer").cast(pl.Utf8),
                pl.col("write_ts"),
            )
            .join(end_df, on="writer", how="inner")
            .filter(pl.col("write_ts") >= operate_start)
            .filter(pl.col("write_ts") <= pl.col("end_ts"))
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


def _shape_aggregates(
    deliveries: pl.DataFrame, windows: _OperateWindows
) -> tuple[int, int, str]:
    """E19 / T19.5: per-group leaf/byte totals + dominant shape.

    Returns ``(leaves_total, bytes_total, shape)`` for the deliveries
    that fall in each writer's operate window. The window scoping
    matches ``_write_receive_counts``: a delivery counts when its
    ``write_ts`` (writer clock) is in ``[operate_start, end_ts]`` per
    writer, where ``end_ts`` is the writer's ``eot_sent_ts`` if
    available else the group's ``silent_start``. This keeps the leaf /
    bytes accounting consistent with the existing throughput numbers.

    ``leaf_count`` / ``bytes`` are inherited from the matching write
    row via ``correlate_lazy``; legacy data defaults to
    ``leaf_count = 1`` and a null ``bytes``. Null ``bytes`` contribute
    ``0`` to the byte total.

    ``shape`` picks the lexicographically-first non-null shape across
    the in-window deliveries. The E19 contract specifies that a single
    spawn emits exactly one workload profile, so this is normally
    degenerate (every row carries the same shape); the deterministic
    tie-break is for stability when a synthetic / mixed fixture
    deliberately violates the per-spawn-single-shape assumption.
    Falls back to ``"scalar"`` when no deliveries are present.
    """
    if deliveries.is_empty():
        return 0, 0, "scalar"

    # Build the per-writer end-boundary table -- same shape as the one
    # ``_write_receive_counts`` constructs, but we don't share it
    # because the caller controls the lifecycle of those temporaries.
    fallback_end = windows.silent_start
    if windows.operate_start is None:
        # No operate phase -- the existing throughput numbers also fall
        # back to "count every delivery"; mirror that here so the leaf
        # / byte totals stay in lockstep with ``receive_count``.
        df = deliveries.select(
            pl.col("leaf_count").cast(pl.Int64, strict=False).sum().alias("leaves"),
            pl.col("bytes").cast(pl.Int64, strict=False).sum().alias("bytes"),
        ).row(0)
        leaves = int(df[0] or 0)
        bytes_total = int(df[1] or 0)
        shape = _dominant_shape(deliveries)
        return leaves, bytes_total, shape

    operate_start = windows.operate_start

    candidate_writers: set[str] = set(windows.per_writer_eot_ts.keys())
    if "writer" in deliveries.columns:
        for w in deliveries.get_column("writer").unique().to_list():
            if w is not None:
                candidate_writers.add(str(w))

    end_rows: list[dict] = []
    for writer in candidate_writers:
        end = windows.per_writer_eot_ts.get(writer, fallback_end)
        if end is None:
            continue
        end_rows.append({"writer": writer, "end_ts": end})

    if not end_rows:
        return 0, 0, "scalar"

    end_df = pl.DataFrame(
        end_rows,
        schema={"writer": pl.Utf8, "end_ts": pl.Datetime("ns", "UTC")},
        orient="row",
    )

    scoped = (
        deliveries.select(
            pl.col("writer").cast(pl.Utf8),
            pl.col("write_ts"),
            pl.col("leaf_count").cast(pl.Int64, strict=False),
            pl.col("bytes").cast(pl.Int64, strict=False),
            pl.col("shape"),
        )
        .join(end_df, on="writer", how="inner")
        .filter(pl.col("write_ts") >= operate_start)
        .filter(pl.col("write_ts") <= pl.col("end_ts"))
    )

    if scoped.is_empty():
        return 0, 0, "scalar"

    totals = scoped.select(
        pl.col("leaf_count").sum().alias("leaves"),
        pl.col("bytes").sum().alias("bytes"),
    ).row(0)
    leaves = int(totals[0] or 0)
    bytes_total = int(totals[1] or 0)
    shape = _dominant_shape(scoped)
    return leaves, bytes_total, shape


def _dominant_shape(deliveries: pl.DataFrame) -> str:
    """Pick a stable single shape value across a delivery DataFrame.

    Returns the lexicographically-first non-null shape; falls back to
    ``"scalar"`` when no value is present. See ``_shape_aggregates``
    for rationale.
    """
    if deliveries.is_empty() or "shape" not in deliveries.columns:
        return "scalar"
    distinct = (
        deliveries.select(pl.col("shape"))
        .filter(pl.col("shape").is_not_null())
        .unique()
        .sort("shape")
    )
    if distinct.is_empty():
        return "scalar"
    value = distinct.item(0, "shape")
    if value is None or value == "":
        return "scalar"
    return str(value)


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
    otherwise ``[operate_start, silent_start]``. **Both halves of the
    loss formula are evaluated in the writer's clock** (T16.16):
    ``write_count`` counts ``write`` events whose ``ts`` is in the
    writer's window, ``receive_count`` counts correlated deliveries
    whose source ``write_ts`` is in the writer's window. The receive
    side no longer depends on the receiver's local clock, which fixes
    a spurious ~1% loss baseline on cross-machine runs with unsynced
    OS clocks. ``late_receives`` continues to count receives in
    ``(eot_sent_ts, silent_start]`` (receiver's clock) for each writer
    as a separate observability metric; it is ``None`` for legacy
    logs without any ``eot_sent`` event.
    """
    connect_mean, connect_max = _connection_metrics(group, variant, run)
    p50, p95, p99, mx = _latency_stats(deliveries)
    latency_samples = _latency_samples(deliveries)
    windows = _operate_windows(group)
    write_count, receive_count = _write_receive_counts(group, deliveries, windows)
    duration = _operate_duration_seconds(windows)
    writes_per_sec = write_count / duration if duration > 0 else 0.0
    receives_per_sec = receive_count / duration if duration > 0 else 0.0
    # E19 / T19.5: leaves + bytes throughput. Derived from correlated
    # deliveries scoped to each writer's operate window so the
    # accounting matches the writer-clock semantics of ``receives_per_sec``
    # (T16.16). When ``deliveries`` is empty (no correlated rows) the
    # totals are zero and the three throughput fields collapse to
    # ``0.0``. ``shape`` picks the lexicographically-first non-null
    # value; per the E19 contract a single spawn emits exactly one
    # workload profile so this is normally degenerate.
    leaves_sum, bytes_sum, group_shape = _shape_aggregates(deliveries, windows)
    ops_per_sec = receives_per_sec
    leaves_per_sec = leaves_sum / duration if duration > 0 else 0.0
    bytes_per_sec = bytes_sum / duration if duration > 0 else 0.0
    jitter, jitter_p95 = _jitter(deliveries)
    if write_count > 0:
        loss_pct = max(0.0, (1.0 - receive_count / write_count) * 100.0)
    else:
        loss_pct = 0.0
    resources = _resource_metrics(group, variant, run)
    has_uncorrected_latency = _any_uncorrected(deliveries)
    late_receives = _late_receives_count(group, windows)
    late_tail_count, late_tail_pct = _late_tail_stats(deliveries, p99)
    threading_mode = _threading_mode(group)
    latency_mean, latency_std = _latency_mean_std(latency_samples)

    # T-pivot.1: expected per-writer publish rate parsed from the spawn
    # name. Imported lazily to avoid a module-load cycle (pivot_tables
    # imports PerformanceResult from this module). When the spawn name
    # does not match the canonical ``<family>-<vpt>x<hz>hz-qos<N>-<mode>``
    # pattern (e.g. legacy logs predating T14.8) the parser returns
    # ``None``; the pivot tables then render the ratio cell as ``n/a``.
    from pivot_tables import parse_spawn_name

    identity = parse_spawn_name(variant)
    if identity is not None and identity.workload_kind == "scalar-flood":
        expected_wps: float | None = float(
            identity.tick_rate_hz * identity.values_per_tick
        )
        if expected_wps > 0:
            ratio_pct: float | None = 100.0 * receives_per_sec / expected_wps
        else:
            ratio_pct = None
    else:
        expected_wps = None
        ratio_pct = None

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
        late_receives_tail_count=late_tail_count,
        late_receives_tail_pct=late_tail_pct,
        latency_samples_ms=latency_samples,
        threading_mode=threading_mode,
        latency_mean_ms=latency_mean,
        latency_std_ms=latency_std,
        expected_writes_per_sec=expected_wps,
        receives_to_expected_ratio_pct=ratio_pct,
        ops_per_sec=ops_per_sec,
        leaves_per_sec=leaves_per_sec,
        bytes_per_sec=bytes_per_sec,
        shape=group_shape,
    )
