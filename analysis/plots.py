"""Comparison bar chart generation for benchmark analysis.

The figure layout chosen for the post-E9 ``<transport>-<workload>-qos<N>``
variant naming is **Option A**: a single 1x2 row with throughput on the
left and latency on the right. The x-axis is QoS level (qos1..qos4 in
ascending order), and within each QoS group the bars are arranged by
transport family then by workload load-intensity. Each transport family
gets its own sequential matplotlib colormap (Oranges/Purples/Blues/
Greens) and within a family the workload tone tracks the load-intensity
ranking. The latency y-axis is log-scaled so reliable sub-millisecond
transports (qos3/qos4) and high-rate lossy transports (qos1/qos2) are
both legible on the same panel. Missing (transport, workload, qos)
combinations are rendered as gaps -- not zero-height bars -- so a
qos3-only entry does not collapse the y-axis at qos1/qos2.

Rationale for Option A: the user wants relative throughput / latency at
a glance across all transports. Stacking 4 small-multiple rows (Option
B) would give per-transport y-scaling but obscures cross-family
comparisons, which is the primary read of this chart. The grouped-bar
layout keeps every (transport, workload, qos) datapoint in one panel
while the colormap coding gives the family/load-intensity context.
"""

from __future__ import annotations

import re
from pathlib import Path

import matplotlib

# Use the non-interactive Agg backend so plot generation does not depend
# on a display server or Tk install (CI / headless / fresh Windows).
matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

from performance import PerformanceResult  # noqa: E402

# Known transport prefixes. Order is preserved for legend ordering and
# colormap assignment. Adding a fifth family is a one-line change here
# plus a colormap entry in ``_FAMILY_COLORMAPS``.
TRANSPORT_FAMILIES: tuple[str, ...] = ("custom-udp", "hybrid", "quic", "zenoh")

# One sequential colormap per transport family. Anything not in
# ``TRANSPORT_FAMILIES`` falls back to "Greys" under the synthetic
# transport label "other".
_FAMILY_COLORMAPS: dict[str, str] = {
    "custom-udp": "Oranges",
    "hybrid": "Purples",
    "quic": "Blues",
    "zenoh": "Greens",
    "other": "Greys",
}

# Range over which workload tones are sampled within a family colormap.
# 0.4 keeps the lightest tone visible against white; 0.95 stops short of
# pure black on dark colormaps.
_TONE_RANGE: tuple[float, float] = (0.4, 0.95)

# QoS suffix matcher applied to variant names.
_QOS_SUFFIX_RE = re.compile(r"-qos(\d+)$")

# Workload <vps>x<hz> matcher. The product (vps * hz) is the
# load-intensity rank used to order workloads inside a family.
_WORKLOAD_VPS_HZ_RE = re.compile(r"^(\d+)x(\d+)hz$")

# Sentinel value used to push the literal "max" workload to the end of
# the load-intensity ranking. Larger than any plausible vps*hz product.
_MAX_WORKLOAD_RANK: int = 10**12

# Small positive epsilon used to clamp lower whisker bounds so the
# log-scale latency axis does not emit "non-positive value" warnings.
_LATENCY_EPSILON_MS: float = 1e-3


def _split_variant_name(name: str) -> tuple[str, str, int | None]:
    """Split a variant name into ``(transport, workload, qos)``.

    The post-E9 canonical shape is ``<transport>-<workload>-qos<N>`` where
    ``transport`` is one of ``TRANSPORT_FAMILIES`` (some of which contain
    hyphens, e.g. ``custom-udp``). ``qos`` may be absent on legacy single-
    QoS runs, in which case it is returned as ``None``. Names that do not
    start with any known transport prefix are surfaced as
    ``transport="other"`` with the full pre-qos string as the workload, so
    plotting never crashes on a renamed variant.

    Examples
    --------
    >>> _split_variant_name("custom-udp-1000x100hz-qos1")
    ('custom-udp', '1000x100hz', 1)
    >>> _split_variant_name("zenoh-max")
    ('zenoh', 'max', None)
    >>> _split_variant_name("hybrid-100x10hz-qos4")
    ('hybrid', '100x10hz', 4)
    >>> _split_variant_name("weird-name")
    ('other', 'weird-name', None)
    """
    qos: int | None = None
    base = name
    m = _QOS_SUFFIX_RE.search(name)
    if m is not None:
        qos = int(m.group(1))
        base = name[: m.start()]

    # Match the longest known transport prefix first so that, e.g.,
    # ``custom-udp-...`` is matched as ``custom-udp`` rather than
    # ``custom``. Order ``TRANSPORT_FAMILIES`` by length-descending for
    # the lookup.
    for transport in sorted(TRANSPORT_FAMILIES, key=len, reverse=True):
        prefix = transport + "-"
        if base.startswith(prefix):
            workload = base[len(prefix) :]
            return transport, workload, qos
        if base == transport:
            return transport, "", qos

    return "other", base, qos


def _workload_load_rank(workload: str) -> tuple[int, int, str]:
    """Return a sort key encoding the load-intensity of a workload.

    The primary key is the integer ``vps * hz`` load-intensity score.
    For tied products the secondary key is ``vps`` (lower-vps-first, so
    ``100x1000hz`` ranks before ``1000x100hz`` even though both equal
    100k msgs/s). The tertiary key is the workload name itself for
    stable ordering of unparseable workloads. The literal string
    ``max`` is ranked last via ``_MAX_WORKLOAD_RANK``. Anything else
    falls back to ``-1`` so unknown workloads sort first (then
    alphabetically by tie-break).
    """
    if workload == "max":
        return _MAX_WORKLOAD_RANK, _MAX_WORKLOAD_RANK, workload
    m = _WORKLOAD_VPS_HZ_RE.match(workload)
    if m is None:
        return -1, -1, workload
    vps = int(m.group(1))
    hz = int(m.group(2))
    return vps * hz, vps, workload


def _family_palette(
    transport: str, workloads: list[str]
) -> dict[str, tuple[float, float, float, float]]:
    """Map each workload of a transport to a distinct RGBA tone.

    Tones are sampled at evenly spaced positions in
    ``_TONE_RANGE`` from the family's sequential colormap. With four
    or more workloads this yields visibly distinct shades; with one
    workload the single tone is the midpoint of the range.
    """
    cmap_name = _FAMILY_COLORMAPS.get(transport, _FAMILY_COLORMAPS["other"])
    cmap = plt.get_cmap(cmap_name)
    n = len(workloads)
    if n == 0:
        return {}
    if n == 1:
        positions = [0.5 * (_TONE_RANGE[0] + _TONE_RANGE[1])]
    else:
        positions = list(np.linspace(_TONE_RANGE[0], _TONE_RANGE[1], n))
    return {w: tuple(cmap(p)) for w, p in zip(workloads, positions)}


def _empty_plot(output_dir: Path) -> Path:
    """Render the placeholder used when there is no data."""
    fig, ax = plt.subplots(figsize=(14, 6))
    ax.text(0.5, 0.5, "No data to plot", ha="center", va="center", fontsize=14)
    ax.set_axis_off()
    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / "comparison.png"
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)
    return out_path


def generate_comparison_plot(
    results: list[PerformanceResult], output_dir: Path
) -> Path:
    """Generate the comparison bar chart PNG.

    Parameters
    ----------
    results:
        Performance results to visualise.
    output_dir:
        Directory where the PNG will be saved (created if needed).

    Returns
    -------
    Path to the generated ``comparison.png``.
    """
    if not results:
        return _empty_plot(output_dir)

    # Parse variant names and group results by (transport, workload, qos).
    # Keep only the first entry per key (typical input has one
    # PerformanceResult per (variant, run); the comparison plot collapses
    # runs by taking the first one encountered).
    parsed: dict[tuple[str, str, int | None], PerformanceResult] = {}
    for r in results:
        transport, workload, qos = _split_variant_name(r.variant)
        key = (transport, workload, qos)
        parsed.setdefault(key, r)

    if not parsed:
        return _empty_plot(output_dir)

    # Collect distinct transports and workloads in deterministic order.
    transports_seen = {t for t, _, _ in parsed.keys()}
    transport_order: list[str] = [t for t in TRANSPORT_FAMILIES if t in transports_seen]
    if "other" in transports_seen:
        transport_order.append("other")

    workload_set: set[str] = {w for _, w, _ in parsed.keys()}
    workload_order: list[str] = sorted(workload_set, key=_workload_load_rank)

    # QoS x-axis: every distinct QoS observed, ascending. ``None`` (no
    # qos suffix) is plotted as a single "n/a" group so legacy single-qos
    # runs still draw.
    qos_values_seen: set[int | None] = {q for _, _, q in parsed.keys()}
    qos_order: list[int | None] = sorted(
        qos_values_seen, key=lambda q: (q is None, q if q is not None else -1)
    )
    qos_labels: list[str] = [f"qos{q}" if q is not None else "n/a" for q in qos_order]

    # Build per-family palettes keyed by (transport, workload).
    palettes: dict[str, dict[str, tuple[float, float, float, float]]] = {}
    for t in transport_order:
        palettes[t] = _family_palette(t, workload_order)

    # Order the (transport, workload) pairs that will become bars within
    # each QoS group: family blocks (preserving ``transport_order``) of
    # workloads (preserving ``workload_order``).
    bar_keys: list[tuple[str, str]] = [
        (t, w) for t in transport_order for w in workload_order
    ]
    n_bars = len(bar_keys)
    n_qos_groups = len(qos_order)

    if n_bars == 0 or n_qos_groups == 0:
        return _empty_plot(output_dir)

    # Bar width / x-positions. ``0.85`` of the slot reserved per QoS
    # group leaves a small gap between groups so the bar block is
    # visually distinct from its neighbours.
    bar_width = 0.85 / n_bars
    x = np.arange(n_qos_groups)

    fig_width = max(20.0, 0.45 * n_bars + 4.0)
    fig, (ax_tp, ax_lat) = plt.subplots(1, 2, figsize=(fig_width, 8.0))

    # Track legend handles in (transport, workload) order so the shared
    # legend reads in the same family/load-intensity order as the bars.
    legend_handles: list[matplotlib.patches.Patch] = []

    for i, (transport, workload) in enumerate(bar_keys):
        color = palettes[transport][workload]
        offset = (i - (n_bars - 1) / 2) * bar_width

        throughputs: list[float] = []
        p50_vals: list[float] = []
        p95_vals: list[float] = []
        p99_vals: list[float] = []
        # Track which qos slots are actually populated so we can render
        # missing entries as gaps (NaN bars).
        for q in qos_order:
            r = parsed.get((transport, workload, q))
            if r is None:
                throughputs.append(float("nan"))
                p50_vals.append(float("nan"))
                p95_vals.append(float("nan"))
                p99_vals.append(float("nan"))
            else:
                throughputs.append(float(r.writes_per_sec))
                p50_vals.append(float(r.latency_p50_ms))
                p95_vals.append(float(r.latency_p95_ms))
                p99_vals.append(float(r.latency_p99_ms))

        ax_tp.bar(
            x + offset,
            throughputs,
            bar_width,
            color=color,
            edgecolor="black",
            linewidth=0.3,
        )

        # Latency bars use p95 with whiskers from p50 (lower) to p99
        # (upper). Under log scale the lower whisker must be strictly
        # positive, so clamp to ``_LATENCY_EPSILON_MS`` and skip whisker
        # rows that are NaN entirely.
        bar_p95: list[float] = []
        yerr_lower: list[float] = []
        yerr_upper: list[float] = []
        for p50, p95, p99 in zip(p50_vals, p95_vals, p99_vals):
            if np.isnan(p95):
                bar_p95.append(float("nan"))
                yerr_lower.append(0.0)
                yerr_upper.append(0.0)
                continue
            safe_p95 = max(p95, _LATENCY_EPSILON_MS)
            bar_p95.append(safe_p95)
            lower = max(safe_p95 - max(p50, _LATENCY_EPSILON_MS), 0.0)
            upper = max(p99 - safe_p95, 0.0)
            yerr_lower.append(lower)
            yerr_upper.append(upper)

        ax_lat.bar(
            x + offset,
            bar_p95,
            bar_width,
            color=color,
            edgecolor="black",
            linewidth=0.3,
            yerr=[yerr_lower, yerr_upper],
            capsize=2,
            ecolor="black",
            error_kw={"linewidth": 0.6},
        )

        legend_handles.append(
            matplotlib.patches.Patch(
                facecolor=color,
                edgecolor="black",
                linewidth=0.3,
                label=f"{transport} / {workload}" if workload else transport,
            )
        )

    # Throughput axis cosmetics.
    ax_tp.set_xlabel("QoS")
    ax_tp.set_ylabel("writes/s")
    ax_tp.set_title("Throughput (writes/s)")
    ax_tp.set_xticks(x)
    ax_tp.set_xticklabels(qos_labels)
    ax_tp.yaxis.grid(True, linestyle="--", alpha=0.5)
    ax_tp.set_axisbelow(True)

    # Latency axis cosmetics. Log scale exposes both reliable sub-ms and
    # lossy tens-of-ms regimes simultaneously.
    ax_lat.set_xlabel("QoS")
    ax_lat.set_ylabel("latency (ms, log scale)")
    ax_lat.set_title("Latency p95 with p50/p99 whiskers")
    ax_lat.set_xticks(x)
    ax_lat.set_xticklabels(qos_labels)
    ax_lat.set_yscale("log")
    ax_lat.yaxis.grid(True, which="both", linestyle="--", alpha=0.5)
    ax_lat.set_axisbelow(True)

    # Single shared legend outside the plot area. ``ncol`` is chosen so
    # the legend has a roughly square footprint: with up to ~32 entries
    # we stretch across 8 columns.
    legend_ncol = max(4, min(8, (n_bars + 3) // 4))
    legend_rows = (len(legend_handles) + legend_ncol - 1) // legend_ncol
    # Reserve a band along the bottom of the figure for the legend. The
    # band height grows with the number of legend rows so dense (32-bar)
    # plots still leave the legend fully visible.
    row_height = 0.025
    bottom_reserve = min(0.45, 0.08 + row_height * legend_rows)
    # Anchor the legend inside the reserved band: ``loc="lower center"``
    # plus ``bbox_to_anchor=(0.5, 0.02)`` keeps the legend inside the
    # figure boundary instead of clipping it off at y=0.
    fig.legend(
        handles=legend_handles,
        loc="lower center",
        bbox_to_anchor=(0.5, 0.01),
        ncol=legend_ncol,
        frameon=True,
        fontsize=8,
        title="Transport / workload",
        title_fontsize=9,
    )

    fig.subplots_adjust(bottom=bottom_reserve, top=0.92, left=0.05, right=0.98)

    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / "comparison.png"
    # ``bbox_inches="tight"`` would clip the carefully reserved bottom
    # band, so save at the figure size we computed.
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)

    return out_path
