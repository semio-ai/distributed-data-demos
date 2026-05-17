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

# QoS suffix matcher applied to variant names (after the optional
# threading suffix has been stripped). Post-E14 variant names end in
# ``-single`` or ``-multi`` (e.g. ``custom-udp-100x100hz-qos1-multi``);
# the qos regex therefore must run against the *base* name with the
# threading suffix removed, otherwise it never matches a post-E14 name
# and every spawn falls into the qos=None bucket -- see T16.13.
_QOS_SUFFIX_RE = re.compile(r"-qos(\d+)$")

# Threading-mode suffix matcher. The post-E14 canonical shape is
# ``<transport>-<workload>-qos<N>-(single|multi)``. Legacy datasets
# (pre-E14) have no threading suffix; ``_split_variant_name`` returns
# ``threading_mode=None`` for those so callers can detect and render
# them with a documented fallback (currently: treat as ``multi``).
_THREADING_SUFFIX_RE = re.compile(r"-(single|multi)$")

# Workload <vps>x<hz> matcher. The product (vps * hz) is the
# load-intensity rank used to order workloads inside a family.
_WORKLOAD_VPS_HZ_RE = re.compile(r"^(\d+)x(\d+)hz$")

# Sentinel value used to push the literal "max" workload to the end of
# the load-intensity ranking. Larger than any plausible vps*hz product.
_MAX_WORKLOAD_RANK: int = 10**12

# Small positive epsilon used to clamp lower whisker bounds so the
# log-scale latency axis does not emit "non-positive value" warnings.
# Set well below any plausible measurement (10 ns) so it only protects
# against log-scale crashes from clock-noise quantiles, never silently
# pancakes genuinely sub-microsecond latencies onto a visible floor.
# Where a percentile itself is <= 0 the bar is dropped (NaN) rather
# than clamped, so the chart visibly communicates "no positive data"
# instead of implying ~10 ns.
_LATENCY_EPSILON_MS: float = 1e-5


def _tier_marker_for_target(target: int | None) -> str | None:
    """Return the star-tier marker for an aggregate write-rate target.

    Each 10x of target adds one star, with 1 K/s as the 1-star anchor:
    1 K/s -> ``*``, 10 K/s -> ``**``, 100 K/s -> ``***``, 1 M/s -> ``****``.
    Targets below 1000 (or unknown, i.e. ``max`` / parser returned None)
    yield ``None`` so the caller skips the annotation entirely rather
    than rendering an empty string.

    The mapping is computed as ``int(round(log10(target))) - 2`` so any
    future order-of-magnitude target (e.g. 1 M, 10 M) extends the scheme
    without a table edit. Off-decade targets (e.g. 30 K/s) round to the
    nearest decade and still get a marker -- useful for forward
    compatibility, since the canonical workload grid only produces clean
    powers of 10 today.

    Examples
    --------
    >>> _tier_marker_for_target(1_000)
    '*'
    >>> _tier_marker_for_target(10_000)
    '**'
    >>> _tier_marker_for_target(100_000)
    '***'
    >>> _tier_marker_for_target(1_000_000)
    '****'
    >>> _tier_marker_for_target(500) is None
    True
    >>> _tier_marker_for_target(None) is None
    True
    """
    if target is None or target < 1000:
        return None
    # log10 of a positive integer >= 1000 is always >= 3, so the
    # subtraction never goes below 1.
    n_stars = int(round(np.log10(float(target)))) - 2
    if n_stars < 1:
        return None
    return "*" * n_stars


def _format_target_rate_label(target: float) -> str:
    """Format an aggregate write-rate target as a short SI string.

    Used to label horizontal target lines on the throughput subplots
    of the comparison chart. The intended write rate of a workload is
    ``vpt * tick_rate_hz``; for the standard workload grid these values
    are clean powers of 10 (1 K/s, 10 K/s, 100 K/s, 1 M/s) and the
    formatter produces correspondingly clean labels. For odd values
    that fall between SI decades the function rounds the mantissa to
    one decimal place and falls back to the next-smaller SI suffix.

    Examples
    --------
    >>> _format_target_rate_label(1_000)
    '1 K/s'
    >>> _format_target_rate_label(10_000)
    '10 K/s'
    >>> _format_target_rate_label(100_000)
    '100 K/s'
    >>> _format_target_rate_label(1_000_000)
    '1 M/s'
    >>> _format_target_rate_label(50)
    '50/s'
    """
    abs_target = abs(target)
    if abs_target >= 1_000_000_000:
        scaled = target / 1_000_000_000
        suffix = "G/s"
    elif abs_target >= 1_000_000:
        scaled = target / 1_000_000
        suffix = "M/s"
    elif abs_target >= 1_000:
        scaled = target / 1_000
        suffix = "K/s"
    else:
        # Sub-thousand targets keep their absolute value with no SI
        # prefix. Integers render without a trailing ".0".
        if float(target).is_integer():
            return f"{int(target)}/s"
        return f"{target:g}/s"
    # Clean integer mantissa (e.g. 1, 10, 100) -> drop the decimal.
    if float(scaled).is_integer():
        return f"{int(scaled)} {suffix}"
    return f"{scaled:g} {suffix}"


def _split_variant_name(
    name: str,
) -> tuple[str, str, int | None, str | None]:
    """Split a variant name into ``(transport, workload, qos, threading_mode)``.

    The post-E14 canonical shape is
    ``<transport>-<workload>-qos<N>-(single|multi)`` where ``transport``
    is one of ``TRANSPORT_FAMILIES`` (some of which contain hyphens,
    e.g. ``custom-udp``). ``qos`` may be absent on legacy single-QoS
    runs, in which case it is returned as ``None``. ``threading_mode``
    is one of ``"single"``/``"multi"`` for post-E14 names; legacy
    (pre-E14) names yield ``None`` so the caller can apply a documented
    fallback (currently rendered as if ``multi``).

    The parser strips the threading suffix *first*, then the qos
    suffix. Doing it in the other order means a post-E14 name like
    ``custom-udp-100x100hz-qos1-multi`` never matches the qos regex
    (the regex anchors at end of string) and the spawn collapses into
    the ``qos=None`` bucket -- the symptom that produced the
    unreadable monolithic chart in T16.13.

    Names that do not start with any known transport prefix are
    surfaced as ``transport="other"`` with the full pre-qos string as
    the workload, so plotting never crashes on a renamed variant.

    Examples
    --------
    >>> _split_variant_name("custom-udp-1000x100hz-qos1-multi")
    ('custom-udp', '1000x100hz', 1, 'multi')
    >>> _split_variant_name("hybrid-100x10hz-qos4-single")
    ('hybrid', '100x10hz', 4, 'single')
    >>> _split_variant_name("custom-udp-1000x100hz-qos1")
    ('custom-udp', '1000x100hz', 1, None)
    >>> _split_variant_name("zenoh-max")
    ('zenoh', 'max', None, None)
    >>> _split_variant_name("weird-name")
    ('other', 'weird-name', None, None)
    """
    threading_mode: str | None = None
    base = name
    m_thr = _THREADING_SUFFIX_RE.search(base)
    if m_thr is not None:
        threading_mode = m_thr.group(1)
        base = base[: m_thr.start()]

    qos: int | None = None
    m_qos = _QOS_SUFFIX_RE.search(base)
    if m_qos is not None:
        qos = int(m_qos.group(1))
        base = base[: m_qos.start()]

    # Match the longest known transport prefix first so that, e.g.,
    # ``custom-udp-...`` is matched as ``custom-udp`` rather than
    # ``custom``. Order ``TRANSPORT_FAMILIES`` by length-descending for
    # the lookup.
    for transport in sorted(TRANSPORT_FAMILIES, key=len, reverse=True):
        prefix = transport + "-"
        if base.startswith(prefix):
            workload = base[len(prefix) :]
            return transport, workload, qos, threading_mode
        if base == transport:
            return transport, "", qos, threading_mode

    return "other", base, qos, threading_mode


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


def _empty_plot(output_dir: Path, filename: str = "comparison.png") -> Path:
    """Render the placeholder used when there is no data."""
    fig, ax = plt.subplots(figsize=(14, 6))
    ax.text(0.5, 0.5, "No data to plot", ha="center", va="center", fontsize=14)
    ax.set_axis_off()
    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / filename
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)
    return out_path


def empirical_cdf(samples: list[float] | np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Compute the empirical CDF over ``samples``.

    Returns ``(x, y)`` where ``x`` is the sorted sample values
    (positive-only, since the consumer plots them on a log axis) and
    ``y`` is the cumulative fraction in ``[0, 1]``. ``y[i] = (i + 1) / n``
    so the curve starts at ``1/n`` (not 0) and ends at ``1.0`` -- the
    standard ECDF convention. Empty input returns two empty arrays.

    Non-finite values (``NaN``, ``+/- inf``) and non-positive values
    are dropped: the CDF is consumed by a log-scale plot, where
    non-positive x is undefined; clock-noise artifacts producing
    negative latency would distort the curve and are not part of any
    meaningful "delivery latency" distribution.
    """
    if isinstance(samples, list):
        arr = np.asarray(samples, dtype=float)
    else:
        arr = np.asarray(samples, dtype=float)
    if arr.size == 0:
        return np.empty(0, dtype=float), np.empty(0, dtype=float)
    finite = arr[np.isfinite(arr) & (arr > 0.0)]
    if finite.size == 0:
        return np.empty(0, dtype=float), np.empty(0, dtype=float)
    x = np.sort(finite)
    n = x.size
    y = np.arange(1, n + 1, dtype=float) / float(n)
    return x, y


def generate_comparison_plot(
    results: list[PerformanceResult],
    output_dir: Path,
    *,
    log_throughput: bool = False,
) -> Path:
    """Generate the comparison bar chart PNG.

    Parameters
    ----------
    results:
        Performance results to visualise.
    output_dir:
        Directory where the PNG will be saved (created if needed).
    log_throughput:
        When True, render the throughput panels on a log y-axis. Bars
        with non-positive ``writes_per_sec`` (a spawn that produced
        zero writes, e.g. ``websocket-max-qos4`` rows in dense runs)
        cannot be plotted on a log axis; those entries are dropped to
        NaN -- matching the latency-panel convention -- so the bar
        disappears rather than being clamped to a misleading visible
        floor. Default ``False`` keeps the existing linear scale.

    Returns
    -------
    Path to the generated ``comparison.png`` (or ``comparison-log.png``
    when ``log_throughput=True``, so the log-scale variant does not
    overwrite the linear-scale output in the same ``--output`` dir).
    """
    # Filename suffix differs by throughput scale so both flavours can
    # coexist in the same output directory.
    out_filename = "comparison-log.png" if log_throughput else "comparison.png"

    if not results:
        return _empty_plot(output_dir, filename=out_filename)

    # Parse variant names and group results by (transport, workload, qos).
    # Keep only the first entry per key (typical input has one
    # PerformanceResult per (variant, run); the comparison plot collapses
    # runs by taking the first one encountered).
    parsed: dict[tuple[str, str, int | None], PerformanceResult] = {}
    for r in results:
        transport, workload, qos, _threading = _split_variant_name(r.variant)
        key = (transport, workload, qos)
        parsed.setdefault(key, r)

    if not parsed:
        return _empty_plot(output_dir, filename=out_filename)

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

    # Compute the set of unique aggregate write-rate targets present in
    # the input. A workload named ``<vpt>x<hz>hz`` has an intended write
    # rate of ``vpt * hz``; the ``max`` workload has no fixed target and
    # is excluded. Drawing one horizontal reference line per unique
    # target lets the reader see at a glance whether each bar reached
    # its intended rate. Targets are independent of QoS, so the same
    # set of lines is drawn on every throughput subplot.
    target_rates: set[int] = set()
    for _, workload, _ in parsed.keys():
        if workload == "max":
            continue
        m = _WORKLOAD_VPS_HZ_RE.match(workload)
        if m is None:
            continue
        target_rates.add(int(m.group(1)) * int(m.group(2)))
    target_rate_order: list[int] = sorted(target_rates)

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
        return _empty_plot(output_dir, filename=out_filename)

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

    # Per-bar tier marker (``*`` / ``**`` / ``***`` / ...). The marker
    # depends only on the workload's intended write rate (``vpt * hz``),
    # which is identical across QoS rows, so compute it once here and
    # reuse for every row. ``max`` workloads and unparseable workloads
    # yield ``None`` -> no annotation is drawn for that bar.
    bar_tier_markers: list[str | None] = []
    for _, workload in bar_keys:
        if workload == "max":
            bar_tier_markers.append(None)
            continue
        m = _WORKLOAD_VPS_HZ_RE.match(workload)
        if m is None:
            bar_tier_markers.append(None)
            continue
        target = int(m.group(1)) * int(m.group(2))
        bar_tier_markers.append(_tier_marker_for_target(target))

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

        # Under log scale a non-positive throughput cannot render; drop
        # those bars to NaN so the bar disappears rather than being
        # clamped to a visible "1" floor. This mirrors the latency-panel
        # treatment of non-positive percentiles below.
        if log_throughput:
            throughputs = [
                t if (not np.isnan(t) and t > 0.0) else float("nan")
                for t in throughputs
            ]

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
        # positive, so the epsilon floor only protects against
        # log-axis warnings -- a percentile <= 0 (clock-noise artifact)
        # is dropped to NaN so the bar disappears rather than visually
        # implying a measurement near the floor.
        bar_p95: list[float] = []
        yerr_lower: list[float] = []
        yerr_upper: list[float] = []
        for p50, p95, p99 in zip(p50_vals, p95_vals, p99_vals):
            if np.isnan(p95) or p95 <= 0.0:
                bar_p95.append(float("nan"))
                yerr_lower.append(0.0)
                yerr_upper.append(0.0)
                continue
            # p95 is strictly positive -- clamp only protects log-scale
            # rendering against any tiny float underflow on the lower
            # whisker arithmetic.
            safe_p95 = max(p95, _LATENCY_EPSILON_MS)
            bar_p95.append(safe_p95)
            # ``p50`` may legitimately be <= 0 even when p95 > 0 (e.g.
            # half the rows have clock-skew artifacts). Floor the
            # subtraction at the epsilon so the lower whisker stays in
            # log-positive space without hiding the bar.
            safe_p50 = max(p50, _LATENCY_EPSILON_MS)
            lower = max(safe_p95 - safe_p50, 0.0)
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

        # Throughput axis cosmetics. Optional log scale (opt-in via
        # ``log_throughput``) brings high-rate transports (~400k
        # writes/s) onto the same panel as low-rate transports (~10k
        # writes/s) without the slow bars collapsing to near-zero.
        ax_tp.set_ylabel(f"{qos_label} - writes/s")
        ax_tp.set_title(f"{qos_label} - Throughput (writes/s)")
        ax_tp.set_xticks(x)
        ax_tp.set_xticklabels(bar_tick_labels, rotation=45, ha="right", fontsize=7)
        if log_throughput:
            ax_tp.set_yscale("log")
            ax_tp.yaxis.grid(True, which="both", linestyle="--", alpha=0.5)
        else:
            ax_tp.yaxis.grid(True, linestyle="--", alpha=0.5)
        ax_tp.set_axisbelow(True)

        # Draw horizontal target-rate reference lines. One line per
        # unique ``vpt * hz`` target observed across all workloads in
        # the input (``max`` excluded). The bars use the default
        # ``zorder=2``; lines at ``zorder=1`` therefore sit behind
        # them. A short SI label at the right edge of the plot, just
        # above the line, identifies each target. On the log-scale
        # chart the standard 1 K / 10 K / 100 K / 1 M targets land on
        # clean log decades so the labels sit at natural gridlines.
        right_x = ax_tp.get_xlim()[1]
        for target in target_rate_order:
            ax_tp.axhline(
                y=target,
                color="grey",
                linestyle="--",
                linewidth=0.8,
                alpha=0.6,
                zorder=1,
            )
            ax_tp.text(
                x=right_x,
                y=target,
                s=_format_target_rate_label(target),
                ha="right",
                va="bottom",
                fontsize=8,
                color="grey",
                alpha=0.8,
            )

        # Per-bar tier markers. Each bar gets a ``*`` / ``**`` / ``***``
        # annotation just above its top, identifying which target line
        # the bar's workload was aiming at. ``max`` workloads (and any
        # workload with no parseable target) get no marker. Placement
        # depends on the scale: a multiplicative offset for log-scale
        # axes keeps the visible gap above the bar roughly constant,
        # while a small fraction of the linear panel height does the
        # same on linear-scale axes. NaN bars on log scale have no
        # valid y position and are skipped entirely; NaN bars on
        # linear scale still get a marker just above the x-axis so the
        # reader can see which target the column was meant to hit.
        is_log_tp = ax_tp.get_yscale() == "log"
        if not is_log_tp:
            linear_offset = 0.02 * ax_tp.get_ylim()[1]
        else:
            linear_offset = 0.0  # unused on log scale
        for bar_x, bar_height, marker in zip(x, throughputs, bar_tier_markers):
            if marker is None:
                continue
            if is_log_tp:
                if np.isnan(bar_height) or bar_height <= 0.0:
                    # No valid y position on a log axis -- skip rather
                    # than render at an arbitrary floor.
                    continue
                y_pos = bar_height * 1.15
            else:
                if np.isnan(bar_height):
                    # NaN bar on linear scale: anchor the marker just
                    # above the x-axis so the target tier is still
                    # legible for columns like a zero-write spawn.
                    y_pos = linear_offset
                else:
                    y_pos = bar_height + linear_offset
            ax_tp.text(
                bar_x,
                y_pos,
                marker,
                ha="center",
                va="bottom",
                fontsize=7,
                color="dimgrey",
                alpha=0.85,
            )

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
    out_path = output_dir / out_filename
    # ``bbox_inches="tight"`` would clip the carefully reserved bottom
    # band, so save at the figure size we computed.
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)

    return out_path


def generate_latency_cdf_plot(
    results: list[PerformanceResult], output_dir: Path
) -> Path:
    """Generate a per-QoS latency CDF chart PNG.

    One subplot per observed QoS row, one CDF line per
    ``(transport, workload)`` combo. ``x`` axis is latency in ms on a
    log scale; ``y`` axis is the empirical CDF in ``[0, 1]``. The
    family colormap / tone scheme matches ``generate_comparison_plot``
    so a viewer can correlate distribution shape with the percentile
    bars in the comparison chart.

    Source: ``PerformanceResult.latency_samples_ms`` -- a downsampled
    per-message latency vector (cap ``LATENCY_SAMPLE_CAP`` per result;
    see ``performance.py``). Results with no samples for a given QoS
    contribute no line; empty rows are skipped entirely.

    Parameters
    ----------
    results:
        Performance results to visualise.
    output_dir:
        Directory where the PNG will be saved (created if needed).

    Returns
    -------
    Path to the generated ``latency_cdf.png``.
    """
    if not results:
        return _empty_plot(output_dir, filename="latency_cdf.png")

    # Index results by (transport, workload, qos), keeping the first
    # entry per key (matches the comparison plot's collapse rule).
    parsed: dict[tuple[str, str, int | None], PerformanceResult] = {}
    for r in results:
        transport, workload, qos, _threading = _split_variant_name(r.variant)
        key = (transport, workload, qos)
        parsed.setdefault(key, r)

    if not parsed:
        return _empty_plot(output_dir, filename="latency_cdf.png")

    # Distinct transports, workloads, qos values (deterministic order
    # mirroring the bar chart so the colour key is consistent).
    transports_seen = {t for t, _, _ in parsed.keys()}
    transport_order: list[str] = [t for t in TRANSPORT_FAMILIES if t in transports_seen]
    if "other" in transports_seen:
        transport_order.append("other")

    workload_set: set[str] = {w for _, w, _ in parsed.keys()}
    workload_order: list[str] = sorted(workload_set, key=_workload_load_rank)

    qos_values_seen: set[int | None] = {q for _, _, q in parsed.keys()}
    qos_order: list[int | None] = sorted(
        qos_values_seen, key=lambda q: (q is None, q if q is not None else -1)
    )

    palettes: dict[str, dict[str, tuple[float, float, float, float]]] = {}
    for t in transport_order:
        palettes[t] = _family_palette(t, workload_order)

    line_keys: list[tuple[str, str]] = [
        (t, w) for t in transport_order for w in workload_order
    ]
    n_qos_groups = len(qos_order)
    if not line_keys or n_qos_groups == 0:
        return _empty_plot(output_dir, filename="latency_cdf.png")

    # Figure layout: one row per QoS, single column. Width comfortably
    # fits the legend on the right; height grows with QoS rows.
    fig_width = 14.0
    per_row_height = 3.5
    legend_band_height = 1.5
    fig_height = per_row_height * n_qos_groups + legend_band_height
    fig, axes = plt.subplots(
        n_qos_groups,
        1,
        figsize=(fig_width, fig_height),
        squeeze=False,
    )

    # Build legend handles in the same order as the bars in the
    # comparison chart so the two figures share a colour key.
    legend_handles: list[matplotlib.lines.Line2D] = []
    for transport, workload in line_keys:
        color = palettes[transport][workload]
        legend_handles.append(
            matplotlib.lines.Line2D(
                [],
                [],
                color=color,
                linewidth=1.4,
                label=f"{transport} / {workload}" if workload else transport,
            )
        )

    for row_idx, q in enumerate(qos_order):
        ax = axes[row_idx][0]
        qos_label = f"qos{q}" if q is not None else "n/a"

        plotted_any = False
        for transport, workload in line_keys:
            r = parsed.get((transport, workload, q))
            if r is None:
                continue
            samples = r.latency_samples_ms
            if not samples:
                continue
            x, y = empirical_cdf(samples)
            if x.size == 0:
                continue
            color = palettes[transport][workload]
            ax.plot(x, y, color=color, linewidth=1.4)
            plotted_any = True

        ax.set_xscale("log")
        ax.set_xlabel("latency (ms, log scale)")
        ax.set_ylabel(f"{qos_label} - empirical CDF")
        ax.set_title(f"{qos_label} - Latency CDF")
        ax.set_ylim(0.0, 1.0)
        ax.grid(True, which="both", linestyle="--", alpha=0.5)
        ax.set_axisbelow(True)
        if not plotted_any:
            # Annotate empty rows so the viewer knows the QoS is
            # absent rather than the data being clipped off-axis.
            ax.text(
                0.5,
                0.5,
                f"no positive latency samples for {qos_label}",
                ha="center",
                va="center",
                transform=ax.transAxes,
                fontsize=10,
                alpha=0.6,
            )

    legend_ncol = max(4, min(8, (len(legend_handles) + 3) // 4))
    legend_rows = (len(legend_handles) + legend_ncol - 1) // legend_ncol
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

    top_reserve = max(0.6, fig_height - 0.4) / fig_height
    fig.subplots_adjust(
        bottom=bottom_reserve,
        top=top_reserve,
        left=0.07,
        right=0.98,
        hspace=0.6,
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / "latency_cdf.png"
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)

    return out_path
