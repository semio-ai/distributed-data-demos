"""Comparison bar chart generation for benchmark analysis.

Produces a side-by-side throughput and latency plot grouped by load label,
with one bar per transport variant.
"""

from __future__ import annotations

from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np

from performance import PerformanceResult


def _split_variant(name: str) -> tuple[str, str]:
    """Split 'custom-udp-10vpt' into ('custom-udp', '10vpt').

    Splits on the LAST hyphen. If there is no hyphen, returns (name, "").
    """
    idx = name.rfind("-")
    if idx == -1:
        return name, ""
    return name[:idx], name[idx + 1 :]


def generate_comparison_plot(
    results: list[PerformanceResult], output_dir: Path
) -> Path:
    """Generate a comparison bar chart PNG with throughput and latency subplots.

    Parameters
    ----------
    results:
        Performance results to visualize.
    output_dir:
        Directory where the PNG will be saved (created if needed).

    Returns
    -------
    Path to the generated PNG file.
    """
    # Parse variant names into (transport, load_label)
    parsed: list[tuple[str, str, PerformanceResult]] = []
    for r in results:
        transport, load_label = _split_variant(r.variant)
        parsed.append((transport, load_label, r))

    # Discover unique transports and load labels (preserving order of appearance)
    transport_order: list[str] = []
    load_order: list[str] = []
    for transport, load_label, _ in parsed:
        if transport not in transport_order:
            transport_order.append(transport)
        if load_label not in load_order:
            load_order.append(load_label)

    # Build lookup: (transport, load_label) -> PerformanceResult
    lookup: dict[tuple[str, str], PerformanceResult] = {}
    for transport, load_label, r in parsed:
        lookup[(transport, load_label)] = r

    # Assign colors per transport using tab10 colormap
    cmap = plt.get_cmap("tab10")
    transport_colors: dict[str, object] = {}
    for i, t in enumerate(transport_order):
        transport_colors[t] = cmap(i % 10)

    n_groups = len(load_order)
    n_bars = len(transport_order)

    if n_groups == 0 or n_bars == 0:
        # Nothing to plot -- create an empty figure with a message
        fig, ax = plt.subplots(figsize=(14, 6))
        ax.text(0.5, 0.5, "No data to plot", ha="center", va="center", fontsize=14)
        output_dir.mkdir(parents=True, exist_ok=True)
        out_path = output_dir / "comparison.png"
        fig.savefig(str(out_path), dpi=150)
        plt.close(fig)
        return out_path

    bar_width = 0.8 / n_bars
    x = np.arange(n_groups)

    fig, (ax_tp, ax_lat) = plt.subplots(1, 2, figsize=(14, 6))

    # --- Left subplot: Throughput ---
    for i, transport in enumerate(transport_order):
        throughputs = []
        for load_label in load_order:
            r = lookup.get((transport, load_label))
            throughputs.append(r.writes_per_sec if r else 0.0)

        offset = (i - (n_bars - 1) / 2) * bar_width
        bars = ax_tp.bar(
            x + offset,
            throughputs,
            bar_width,
            label=transport,
            color=transport_colors[transport],
        )

        # Value labels on top of bars
        for bar in bars:
            height = bar.get_height()
            if height > 0:
                ax_tp.annotate(
                    f"{height:.0f}",
                    xy=(bar.get_x() + bar.get_width() / 2, height),
                    xytext=(0, 3),
                    textcoords="offset points",
                    ha="center",
                    va="bottom",
                    fontsize=7,
                )

    ax_tp.set_xlabel("Load")
    ax_tp.set_ylabel("writes/s")
    ax_tp.set_title("Throughput (writes/s)")
    ax_tp.set_xticks(x)
    ax_tp.set_xticklabels(load_order)
    ax_tp.legend()
    ax_tp.yaxis.grid(True, linestyle="--", alpha=0.7)
    ax_tp.set_axisbelow(True)

    # --- Right subplot: Latency with whiskers ---
    for i, transport in enumerate(transport_order):
        p50_vals = []
        p95_vals = []
        p99_vals = []
        for load_label in load_order:
            r = lookup.get((transport, load_label))
            if r:
                p50_vals.append(r.latency_p50_ms)
                p95_vals.append(r.latency_p95_ms)
                p99_vals.append(r.latency_p99_ms)
            else:
                p50_vals.append(0.0)
                p95_vals.append(0.0)
                p99_vals.append(0.0)

        # Error bars: lower = p95 - p50, upper = p99 - p95
        yerr_lower = [max(p95 - p50, 0.0) for p95, p50 in zip(p95_vals, p50_vals)]
        yerr_upper = [max(p99 - p95, 0.0) for p99, p95 in zip(p99_vals, p95_vals)]
        yerr = [yerr_lower, yerr_upper]

        offset = (i - (n_bars - 1) / 2) * bar_width
        ax_lat.bar(
            x + offset,
            p95_vals,
            bar_width,
            label=transport,
            color=transport_colors[transport],
            yerr=yerr,
            capsize=3,
            ecolor="black",
        )

    ax_lat.set_xlabel("Load")
    ax_lat.set_ylabel("latency (ms)")
    ax_lat.set_title("Latency (ms)")
    ax_lat.set_xticks(x)
    ax_lat.set_xticklabels(load_order)
    ax_lat.legend()
    ax_lat.yaxis.grid(True, linestyle="--", alpha=0.7)
    ax_lat.set_axisbelow(True)

    fig.tight_layout()

    # Save
    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / "comparison.png"
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)

    return out_path
