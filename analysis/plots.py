"""Comparison bar chart generation for benchmark analysis.

The figure layout for the post-E9 ``<transport>-<workload>-qos<N>``
variant naming is **N_qos rows x 2 cols**: one row per observed QoS
level, throughput on the left column and latency on the right column.
Within each row the bars are the (transport, workload) combinations for
that QoS, arranged by transport family then by workload load-intensity.
Each transport family gets its own sequential matplotlib colormap
(Oranges/Purples/Blues/Greens/Reds/YlOrBr) and within a family the
workload tone tracks the load-intensity ranking. The latency y-axis is
log-scaled so reliable sub-millisecond transports (qos3/qos4) and high-
rate lossy transports (qos1/qos2) remain legible. Missing (transport,
workload, qos) combinations are rendered as gaps -- not zero-height
bars -- so a qos3-only entry does not collapse the y-axis at qos1/qos2.

Rationale for the per-QoS row layout: the previous single-row layout
collapsed all QoS levels into x-axis groups, which became unreadable
once 6+ transport families x 8 workloads x 4 QoS levels were drawn into
the same two cells. Splitting QoS levels into separate rows keeps each
cell to a single bar group, restores legibility, and still allows
cross-family comparisons row-by-row. A single shared legend at the
bottom of the figure carries the (transport, workload) colour key for
every row.
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
# colormap assignment. The four original families (``custom-udp``,
# ``hybrid``, ``quic``, ``zenoh``) come first, then ``websocket`` and
# ``webrtc`` so the legend reads with the established families first
# and the newer browser-style transports grouped together at the end.
# Adding another family is a one-line change here plus a colormap entry
# in ``_FAMILY_COLORMAPS``.
TRANSPORT_FAMILIES: tuple[str, ...] = (
    "custom-udp",
    "hybrid",
    "quic",
    "zenoh",
    "websocket",
    "webrtc",
)

# One sequential colormap per transport family. Anything not in
# ``TRANSPORT_FAMILIES`` falls back to "Greys" under the synthetic
# transport label "other". The websocket/webrtc colormaps are picked to
# stay distinguishable from the four originals (Oranges/Purples/Blues/
# Greens) and from each other: Reds is a clean primary contrast for
# websocket; YlOrBr (yellow->brown) gives webrtc a warm earth tone that
# does not clash with Reds or Oranges at typical workload tones.
_FAMILY_COLORMAPS: dict[str, str] = {
    "custom-udp": "Oranges",
    "hybrid": "Purples",
    "quic": "Blues",
    "zenoh": "Greens",
    "websocket": "Reds",
    "webrtc": "YlOrBr",
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

    # Bar width / x-positions. Each row plots the same set of
    # (transport, workload) bars for a single QoS, so the x-axis carries
    # the bar keys themselves rather than QoS groups. Bars are evenly
    # spaced at integer x positions.
    x = np.arange(n_bars)
    bar_width = 0.85

    # Figure sizing: width tracks the per-row bar count; height grows
    # with the number of QoS rows so dense charts do not squish.
    fig_width = max(14.0, 0.45 * n_bars + 4.0)
    per_row_height = 3.5
    legend_band_height = 1.5
    fig_height = per_row_height * n_qos_groups + legend_band_height
    fig, axes = plt.subplots(
        n_qos_groups,
        2,
        figsize=(fig_width, fig_height),
        squeeze=False,
    )

    # Track legend handles in (transport, workload) order so the shared
    # legend reads in the same family/load-intensity order as the bars.
    # Build the handles once -- they do not depend on the QoS row.
    legend_handles: list[matplotlib.patches.Patch] = []
    bar_colors: list[tuple[float, float, float, float]] = []
    for transport, workload in bar_keys:
        color = palettes[transport][workload]
        bar_colors.append(color)
        legend_handles.append(
            matplotlib.patches.Patch(
                facecolor=color,
                edgecolor="black",
                linewidth=0.3,
                label=f"{transport} / {workload}" if workload else transport,
            )
        )

    # Short tick labels per bar -- workload (or transport name when the
    # workload string is empty). The colour-coded legend carries the
    # transport family, so the per-bar tick label can stay compact.
    bar_tick_labels: list[str] = [w if w else t for t, w in bar_keys]

    for row_idx, q in enumerate(qos_order):
        ax_tp = axes[row_idx][0]
        ax_lat = axes[row_idx][1]
        qos_label = f"qos{q}" if q is not None else "n/a"

        throughputs: list[float] = []
        p50_vals: list[float] = []
        p95_vals: list[float] = []
        p99_vals: list[float] = []
        for transport, workload in bar_keys:
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
            x,
            throughputs,
            bar_width,
            color=bar_colors,
            edgecolor="black",
            linewidth=0.3,
        )

        # Latency bars use p95 with whiskers from p50 (lower) to p99
        # (upper). Under log scale the lower whisker must be strictly
        # positive, so clamp to ``_LATENCY_EPSILON_MS`` and zero the
        # whisker rows that are NaN entirely.
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
            x,
            bar_p95,
            bar_width,
            color=bar_colors,
            edgecolor="black",
            linewidth=0.3,
            yerr=[yerr_lower, yerr_upper],
            capsize=2,
            ecolor="black",
            error_kw={"linewidth": 0.6},
        )

        # Throughput axis cosmetics.
        ax_tp.set_ylabel(f"{qos_label} - writes/s")
        ax_tp.set_title(f"{qos_label} - Throughput (writes/s)")
        ax_tp.set_xticks(x)
        ax_tp.set_xticklabels(bar_tick_labels, rotation=45, ha="right", fontsize=7)
        ax_tp.yaxis.grid(True, linestyle="--", alpha=0.5)
        ax_tp.set_axisbelow(True)

        # Latency axis cosmetics. Log scale exposes both reliable sub-ms
        # and lossy tens-of-ms regimes simultaneously.
        ax_lat.set_ylabel(f"{qos_label} - latency (ms, log scale)")
        ax_lat.set_title(f"{qos_label} - Latency p95 with p50/p99 whiskers")
        ax_lat.set_xticks(x)
        ax_lat.set_xticklabels(bar_tick_labels, rotation=45, ha="right", fontsize=7)
        ax_lat.set_yscale("log")
        ax_lat.yaxis.grid(True, which="both", linestyle="--", alpha=0.5)
        ax_lat.set_axisbelow(True)

    # Single shared legend below all rows. ``ncol`` is chosen so the
    # legend has a roughly square footprint relative to the bar count.
    legend_ncol = max(4, min(8, (n_bars + 3) // 4))
    legend_rows = (len(legend_handles) + legend_ncol - 1) // legend_ncol
    # Reserve a band along the bottom of the figure for the legend. The
    # band's relative height shrinks as the figure grows taller (more
    # QoS rows) but is bounded so it never collapses to nothing.
    row_height_in = 0.22
    legend_band_in = max(0.6, 0.4 + row_height_in * legend_rows)
    bottom_reserve = min(0.4, legend_band_in / fig_height)
    fig.legend(
        handles=legend_handles,
        loc="lower center",
        bbox_to_anchor=(0.5, 0.005),
        ncol=legend_ncol,
        frameon=True,
        fontsize=8,
        title="Transport / workload",
        title_fontsize=9,
    )

    # Top reserve is a constant slice of the figure -- enough for the
    # first row's title -- and ``hspace`` keeps the inter-row titles
    # from overlapping the row above.
    top_reserve = max(0.6, fig_height - 0.4) / fig_height
    fig.subplots_adjust(
        bottom=bottom_reserve,
        top=top_reserve,
        left=0.07,
        right=0.98,
        hspace=0.6,
        wspace=0.18,
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / "comparison.png"
    # ``bbox_inches="tight"`` would clip the carefully reserved bottom
    # band, so save at the figure size we computed.
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)

    return out_path
