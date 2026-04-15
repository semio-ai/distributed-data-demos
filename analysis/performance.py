"""Performance metrics computation for benchmark analysis."""

from __future__ import annotations

import statistics
from collections import defaultdict
from dataclasses import dataclass, field

from parse import DeliveryRecord, Event


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
    """Aggregated performance metrics for one (variant, run)."""

    variant: str
    run: str
    # Connection time (mean and max across runners)
    connect_mean_ms: float
    connect_max_ms: float
    # Latency percentiles (from delivery records)
    latency_p50_ms: float
    latency_p95_ms: float
    latency_p99_ms: float
    latency_max_ms: float
    # Throughput
    writes_per_sec: float
    receives_per_sec: float
    # Jitter (std-dev of latency)
    jitter_ms: float
    # Loss
    loss_pct: float
    # Resource usage
    resources: list[ResourceMetric] = field(default_factory=list)


def _percentile(data: list[float], p: float) -> float:
    """Compute the p-th percentile of a sorted list.

    Uses linear interpolation between closest ranks.
    """
    if not data:
        return 0.0
    sorted_data = sorted(data)
    n = len(sorted_data)
    if n == 1:
        return sorted_data[0]

    # Use the "exclusive" method: rank = p/100 * (n+1)
    rank = p / 100.0 * (n - 1)
    lo = int(rank)
    hi = lo + 1
    frac = rank - lo

    if hi >= n:
        return sorted_data[-1]
    return sorted_data[lo] + frac * (sorted_data[hi] - sorted_data[lo])


def _compute_jitter(records: list[DeliveryRecord]) -> float:
    """Compute jitter as std-dev of latency within 1-second windows.

    Returns the mean of per-window std-devs. If there is only one window
    or not enough data, returns the overall latency std-dev.
    """
    if len(records) < 2:
        return 0.0

    # Sort by receive timestamp
    sorted_recs = sorted(records, key=lambda r: r.receive_ts)

    # Group into 1-second windows based on receive_ts
    windows: list[list[float]] = []
    window_start = sorted_recs[0].receive_ts
    current_window: list[float] = []

    for rec in sorted_recs:
        delta_s = (rec.receive_ts - window_start).total_seconds()
        if delta_s >= 1.0:
            if len(current_window) >= 2:
                windows.append(current_window)
            current_window = []
            window_start = rec.receive_ts
        current_window.append(rec.latency_ms)

    if len(current_window) >= 2:
        windows.append(current_window)

    if windows:
        stddevs = [statistics.stdev(w) for w in windows]
        return statistics.mean(stddevs)

    # Fallback: overall std-dev
    latencies = [r.latency_ms for r in records]
    return statistics.stdev(latencies)


def _get_operate_duration(events: list[Event], variant: str, run: str) -> float:
    """Compute the operate phase duration in seconds.

    Uses the time between the operate phase event and the silent phase
    event (or the last event if silent is missing).
    """
    operate_start = None
    operate_end = None

    for ev in events:
        if ev.variant != variant or ev.run != run:
            continue
        if ev.event == "phase":
            phase = ev.data.get("phase")
            if phase == "operate" and operate_start is None:
                operate_start = ev.ts
            elif phase == "silent":
                if operate_end is None or ev.ts > operate_end:
                    operate_end = ev.ts

    if operate_start is None:
        return 0.0

    if operate_end is None:
        # Fallback: use the last event for this variant/run
        last_ts = operate_start
        for ev in events:
            if ev.variant == variant and ev.run == run and ev.ts > last_ts:
                last_ts = ev.ts
        operate_end = last_ts

    duration = (operate_end - operate_start).total_seconds()
    return max(duration, 0.001)  # avoid division by zero


def compute_performance(
    events: list[Event],
    records: list[DeliveryRecord],
) -> list[PerformanceResult]:
    """Compute performance metrics for all (variant, run) pairs."""
    # Group delivery records by (variant, run)
    delivery_groups: dict[tuple[str, str], list[DeliveryRecord]] = defaultdict(list)
    for rec in records:
        delivery_groups[(rec.variant, rec.run)].append(rec)

    # Gather connection times per (variant, run)
    connect_metrics: dict[tuple[str, str], list[ConnectionMetric]] = defaultdict(list)
    for ev in events:
        if ev.event == "connected":
            elapsed = ev.data.get("elapsed_ms")
            if elapsed is not None:
                cm = ConnectionMetric(
                    variant=ev.variant,
                    runner=ev.runner,
                    run=ev.run,
                    elapsed_ms=float(elapsed),
                )
                connect_metrics[(ev.variant, ev.run)].append(cm)

    # Gather resource metrics per (variant, runner, run)
    resource_data: dict[tuple[str, str, str], list[tuple[float, float]]] = defaultdict(
        list
    )
    for ev in events:
        if ev.event == "resource":
            cpu = ev.data.get("cpu_percent")
            mem = ev.data.get("memory_mb")
            if cpu is not None and mem is not None:
                key = (ev.variant, ev.runner, ev.run)
                resource_data[key].append((float(cpu), float(mem)))

    # Count writes and receives per (variant, run)
    write_counts: dict[tuple[str, str], int] = defaultdict(int)
    receive_counts: dict[tuple[str, str], int] = defaultdict(int)
    for ev in events:
        if ev.event == "write":
            write_counts[(ev.variant, ev.run)] += 1
        elif ev.event == "receive":
            receive_counts[(ev.variant, ev.run)] += 1

    # Discover all (variant, run) pairs from events
    all_pairs: set[tuple[str, str]] = set()
    for ev in events:
        all_pairs.add((ev.variant, ev.run))

    results: list[PerformanceResult] = []

    for variant, run in sorted(all_pairs):
        pair_records = delivery_groups.get((variant, run), [])
        latencies = [r.latency_ms for r in pair_records]

        # Connection time
        cms = connect_metrics.get((variant, run), [])
        if cms:
            connect_mean = statistics.mean([c.elapsed_ms for c in cms])
            connect_max = max(c.elapsed_ms for c in cms)
        else:
            connect_mean = 0.0
            connect_max = 0.0

        # Latency percentiles
        if latencies:
            lat_p50 = _percentile(latencies, 50)
            lat_p95 = _percentile(latencies, 95)
            lat_p99 = _percentile(latencies, 99)
            lat_max = max(latencies)
        else:
            lat_p50 = lat_p95 = lat_p99 = lat_max = 0.0

        # Throughput
        duration = _get_operate_duration(events, variant, run)
        w_count = write_counts.get((variant, run), 0)
        r_count = receive_counts.get((variant, run), 0)
        writes_per_sec = w_count / duration if duration > 0 else 0.0
        receives_per_sec = r_count / duration if duration > 0 else 0.0

        # Jitter
        jitter = _compute_jitter(pair_records) if pair_records else 0.0

        # Loss
        total_writes = w_count
        total_receives = r_count
        if total_writes > 0:
            loss_pct = (1.0 - total_receives / total_writes) * 100.0
            loss_pct = max(loss_pct, 0.0)
        else:
            loss_pct = 0.0

        # Resource usage
        resources: list[ResourceMetric] = []
        for (v, runner, r), samples in sorted(resource_data.items()):
            if v != variant or r != run:
                continue
            cpus = [s[0] for s in samples]
            mems = [s[1] for s in samples]
            resources.append(
                ResourceMetric(
                    variant=v,
                    runner=runner,
                    run=r,
                    mean_cpu_pct=statistics.mean(cpus),
                    peak_cpu_pct=max(cpus),
                    mean_memory_mb=statistics.mean(mems),
                    peak_memory_mb=max(mems),
                )
            )

        results.append(
            PerformanceResult(
                variant=variant,
                run=run,
                connect_mean_ms=connect_mean,
                connect_max_ms=connect_max,
                latency_p50_ms=lat_p50,
                latency_p95_ms=lat_p95,
                latency_p99_ms=lat_p99,
                latency_max_ms=lat_max,
                writes_per_sec=writes_per_sec,
                receives_per_sec=receives_per_sec,
                jitter_ms=jitter,
                loss_pct=loss_pct,
                resources=resources,
            )
        )

    return results
