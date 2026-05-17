"""Comparison bar chart generation for benchmark analysis.

The post-E14 ``<transport>-<workload>-qos<N>-(single|multi)`` naming
introduced a threading-mode dimension on top of the existing
transport x workload x QoS matrix. Crammed into the previous "one
figure with one row per QoS" layout, this expanded to ~256 bars in a
single image and the chart became unreadable -- T16.13.

The current layout splits the chart into **N_qos PNGs**, one per
observed QoS level. Each PNG is a 1 row x 2 cols figure: **receive
throughput** on the left, latency on the right. Per T16.14 the
throughput column reads ``PerformanceResult.receives_per_sec`` (not
``writes_per_sec``) -- the project headline metric per
``metak-shared/overview.md``. The target-rate horizontal lines and
tier markers continue to encode the *intended* write rate, so the gap
between a bar and its target line is the visible delivery shortfall.
A parallel ``generate_drop_rate_plot`` emits a third chart family
(``drop-rate-qos<N>.png``) that plots ``loss_pct`` per slot.

Within each plot the bars are the
(transport, workload) *slots* for that QoS, arranged by transport
family then by workload load-intensity. Each slot holds either:

* one full-width bar (``multi`` only), for natively-multi-only
  transport families like QUIC, WebRTC, Zenoh -- they have no
  single-threaded mode per E14, so the slot is not a paired half-and-
  half layout (a future reader should not infer a missing ``single``
  half from this); or
* two paired half-width bars, ``single`` on the left and ``multi`` on
  the right, for transports that exist in both modes.

Each transport family gets its own sequential matplotlib colormap
(Oranges/Purples/Blues/Greens/Reds/YlOrBr). Within a family the
*threading mode* picks the colormap tone: ``single`` at 0.55 (lighter)
and ``multi`` at 0.85 (darker). Workload distinction comes from the
slot's x-position and tick label; it is not encoded in colour. The
latency y-axis is log-scaled so reliable sub-millisecond transports
(qos3/qos4) and high-rate lossy transports (qos1/qos2) remain legible.
Missing (transport, workload, threading, qos) combinations are
rendered as gaps -- not zero-height bars -- so a partial slot does not
collapse the y-axis on the QoS panel.

A legacy spawn (pre-E14, no threading suffix) renders as if it were
``multi`` -- the same tone position. This is a documented best-effort
fallback so old datasets do not crash; they just won't show a paired
single/multi distinction.

Filename convention: ``comparison-qos<N>.png`` (or
``comparison-qos<N>-log.png`` when ``log_throughput`` is set). The
parallel CDF generator emits ``latency-cdf-qos<N>.png``. The old
monolithic ``comparison.png`` / ``comparison-log.png`` /
``latency_cdf.png`` outputs are no longer produced.
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
# pure black on dark colormaps. Kept for the legacy ``_family_palette``
# helper, which is still exported and used by tests; the per-QoS plot
# code uses ``_THREADING_TONES`` for its single/multi distinction.
_TONE_RANGE: tuple[float, float] = (0.4, 0.95)

# Per-threading-mode tone positions within a family colormap. The
# ``single`` tone sits at 0.55 (a mid-light shade) and the ``multi``
# tone at 0.85 (a near-saturated shade). Legacy spawns with no
# threading suffix render with the ``multi`` tone -- documented
# fallback so unknown-mode bars stay visible against the
# transport-family colour while still being distinguishable from a
# definite ``single`` in the same slot.
_THREADING_TONES: dict[str | None, float] = {
    "single": 0.55,
    "multi": 0.85,
    None: 0.85,
}

# Natively-multi-only transport families. Per E14 these transports do
# not have a single-threaded mode -- their slots therefore render a
# single full-width ``multi`` bar instead of a paired single/multi
# layout. A legacy ``-single`` spawn for one of these (shouldn't exist
# in any post-E14 dataset, but we don't crash on it) still renders as
# the ``single`` half of a paired slot for visibility.
_MULTI_ONLY_TRANSPORTS: frozenset[str] = frozenset({"quic", "webrtc", "zenoh"})

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


def _threading_color(
    transport: str, threading_mode: str | None
) -> tuple[float, float, float, float]:
    """Return the RGBA fill colour for a (transport, threading_mode) pair.

    The colour is sampled from the family's sequential colormap at the
    position fixed by ``_THREADING_TONES`` -- ``single`` at 0.55,
    ``multi`` at 0.85. A legacy spawn with ``threading_mode=None``
    renders at the ``multi`` tone (documented fallback).
    """
    cmap_name = _FAMILY_COLORMAPS.get(transport, _FAMILY_COLORMAPS["other"])
    cmap = plt.get_cmap(cmap_name)
    pos = _THREADING_TONES.get(threading_mode, _THREADING_TONES[None])
    return tuple(cmap(pos))


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


def _qos_filename(prefix: str, qos: int | None, *, log_throughput: bool) -> str:
    """Build the per-QoS PNG filename.

    Examples::

        _qos_filename("comparison", 1, log_throughput=False) -> "comparison-qos1.png"
        _qos_filename("comparison", 1, log_throughput=True)  -> "comparison-qos1-log.png"
        _qos_filename("latency-cdf", 4, log_throughput=False) -> "latency-cdf-qos4.png"
        _qos_filename("comparison", None, log_throughput=False) -> "comparison-qosNA.png"
    """
    qos_part = f"qos{qos}" if qos is not None else "qosNA"
    suffix = "-log" if log_throughput else ""
    return f"{prefix}-{qos_part}{suffix}.png"


def _index_parsed_results(
    results: list[PerformanceResult],
) -> dict[tuple[str, str, int | None, str | None], PerformanceResult]:
    """Index results by ``(transport, workload, qos, threading_mode)``.

    The first entry per key wins -- matches the pre-T16.13 collapse
    rule. The four-component key keeps a single-threaded and a multi-
    threaded spawn of the same (transport, workload, qos) as distinct
    rows so the paired-bar layout can find both.
    """
    parsed: dict[tuple[str, str, int | None, str | None], PerformanceResult] = {}
    for r in results:
        transport, workload, qos, threading_mode = _split_variant_name(r.variant)
        key = (transport, workload, qos, threading_mode)
        parsed.setdefault(key, r)
    return parsed


def _qos_sort_key(q: int | None) -> tuple[int, int]:
    """Sort key that puts numeric QoS first ascending, ``None`` last."""
    if q is None:
        return (1, 0)
    return (0, q)


def _collect_layout_orders(
    parsed_keys: list[tuple[str, str, int | None, str | None]],
) -> tuple[list[str], list[str], list[int | None]]:
    """Return ordered transports, workloads, and QoS levels.

    Mirrors the pre-T16.13 ordering: ``TRANSPORT_FAMILIES`` order for
    transports (with ``other`` appended if present), load-intensity
    order for workloads, and ascending order for QoS (``None`` last).
    """
    transports_seen = {t for t, _, _, _ in parsed_keys}
    transport_order: list[str] = [t for t in TRANSPORT_FAMILIES if t in transports_seen]
    if "other" in transports_seen:
        transport_order.append("other")

    workload_set: set[str] = {w for _, w, _, _ in parsed_keys}
    workload_order: list[str] = sorted(workload_set, key=_workload_load_rank)

    qos_values_seen: set[int | None] = {q for _, _, q, _ in parsed_keys}
    qos_order: list[int | None] = sorted(qos_values_seen, key=_qos_sort_key)

    return transport_order, workload_order, qos_order


def _slot_threading_modes(
    transport: str,
    workload: str,
    qos: int | None,
    parsed: dict[tuple[str, str, int | None, str | None], PerformanceResult],
) -> list[str | None]:
    """Decide which threading-mode bars fill a (transport, workload, qos) slot.

    Returns one mode per bar to render in the slot, ordered left to
    right. The convention:

    * Natively-multi-only transports (``quic``, ``webrtc``, ``zenoh``)
      render a single full-width ``multi`` bar -- they have no
      single-threaded mode per E14 and the slot is intentionally not
      a paired half-and-half layout. If a legacy ``-single`` row
      somehow exists for such a transport, it still gets a paired
      slot so the data is not silently hidden.
    * Other transports render the modes actually observed for this
      slot. If only one of ``single``/``multi`` exists the slot stays
      half-width with one empty half (so the bar's x-position still
      lines up with the matching mode in sibling slots) -- this is
      the "missing data is a gap, not a zero" convention applied to
      the threading axis.
    * Legacy spawns (``threading_mode=None``) get a one-bar slot
      rendered at the ``multi`` tone (documented fallback).
    """
    observed = {
        mode
        for (t, w, q, mode) in parsed.keys()
        if t == transport and w == workload and q == qos
    }
    if not observed:
        return []

    if transport in _MULTI_ONLY_TRANSPORTS:
        # Multi-only families: render only the multi bar if it is
        # present; otherwise fall back to whatever modes were
        # observed (covers the pathological legacy-single case).
        if "multi" in observed:
            return ["multi"]
        return sorted(observed, key=lambda m: (m != "single", m != "multi", str(m)))

    # General case: order single, then multi, then None (legacy).
    ordered: list[str | None] = []
    for mode in ("single", "multi"):
        if mode in observed:
            ordered.append(mode)
    if None in observed:
        ordered.append(None)
    return ordered


def _slot_layout(
    transport: str,
    workload: str,
    qos: int | None,
    parsed: dict[tuple[str, str, int | None, str | None], PerformanceResult],
) -> tuple[list[str | None], list[float]]:
    """Return per-bar threading modes and their x-offsets within a slot.

    The slot occupies ``[-0.5, +0.5]`` around its centre. For one-bar
    slots the offset is ``0.0`` and the bar takes full slot width.
    For two-bar slots single sits at ``-0.22`` and multi at ``+0.22``
    so the two bars are visually grouped. A pathological 3-bar slot
    (single + multi + legacy) spreads evenly across the slot.
    """
    modes = _slot_threading_modes(transport, workload, qos, parsed)
    if not modes:
        return [], []
    if len(modes) == 1:
        return modes, [0.0]
    if len(modes) == 2:
        return modes, [-0.22, 0.22]
    # Fall back to evenly-spaced positions for 3+ bars.
    n = len(modes)
    step = 0.7 / n
    start = -0.35 + 0.5 * step
    return modes, [start + i * step for i in range(n)]


def _bar_width_for_slot(n_bars_in_slot: int) -> float:
    """Bar width matching the offsets produced by ``_slot_layout``."""
    if n_bars_in_slot <= 1:
        return 0.78
    if n_bars_in_slot == 2:
        return 0.40
    return 0.55 / n_bars_in_slot


def _collect_target_rates(
    parsed_keys: list[tuple[str, str, int | None, str | None]],
) -> list[int]:
    """Compute the sorted unique ``vpt * hz`` target rates across workloads.

    ``max`` and unparseable workloads contribute nothing.
    """
    target_rates: set[int] = set()
    for _, workload, _, _ in parsed_keys:
        if workload == "max":
            continue
        m = _WORKLOAD_VPS_HZ_RE.match(workload)
        if m is None:
            continue
        target_rates.add(int(m.group(1)) * int(m.group(2)))
    return sorted(target_rates)


def _bar_tier_marker(workload: str) -> str | None:
    """Return the tier marker for a workload's intended write rate."""
    if workload == "max":
        return None
    m = _WORKLOAD_VPS_HZ_RE.match(workload)
    if m is None:
        return None
    target = int(m.group(1)) * int(m.group(2))
    return _tier_marker_for_target(target)


def _generate_comparison_plot_for_qos(
    *,
    qos: int | None,
    parsed: dict[tuple[str, str, int | None, str | None], PerformanceResult],
    transport_order: list[str],
    workload_order: list[str],
    target_rate_order: list[int],
    output_dir: Path,
    log_throughput: bool,
) -> Path:
    """Render the comparison chart for a single QoS level.

    Slots are (transport, workload) pairs in family-block x
    load-intensity order. Each slot holds 1-2 bars: single (lighter
    tone) and/or multi (darker tone). The throughput column reads
    ``PerformanceResult.receives_per_sec`` -- per T16.14 the project
    headline metric is *receive* throughput, not write throughput.
    The target-rate horizontal lines and tier markers still encode
    the intended *write* rate; the gap between a bar (receives) and
    its target line (writes) is the visible delivery shortfall.
    Returns the path of the PNG written.

    Figure-width formula: ``max(14.0, 0.55 * n_slots + 4.0)``. The
    coefficient is tuned for the post-E14 6-family x 8-workload (~48
    slots) matrix so each slot gets roughly 30 px of x-axis space at
    150 dpi -- wide enough to read the tick label without zooming.
    Slots with two bars therefore get ~15 px per bar, still visibly
    distinct.
    """
    out_filename = _qos_filename("comparison", qos, log_throughput=log_throughput)
    qos_label = f"qos{qos}" if qos is not None else "n/a"

    # Slots in family-block x load-intensity order. Drop slots that
    # have no observation for this QoS at all (saves x-axis width on
    # sparse QoS levels).
    slots: list[tuple[str, str]] = []
    slot_layouts: list[tuple[list[str | None], list[float]]] = []
    for t in transport_order:
        for w in workload_order:
            modes, offsets = _slot_layout(t, w, qos, parsed)
            if modes:
                slots.append((t, w))
                slot_layouts.append((modes, offsets))

    if not slots:
        return _empty_plot(output_dir, filename=out_filename)

    n_slots = len(slots)
    x_centres = np.arange(n_slots, dtype=float)

    # Figure size: width grows with slot count (see docstring); height
    # is a single row plus a legend band.
    fig_width = max(14.0, 0.55 * n_slots + 4.0)
    row_height = 4.0
    legend_band_height = 1.4
    fig_height = row_height + legend_band_height
    fig, axes = plt.subplots(1, 2, figsize=(fig_width, fig_height), squeeze=False)
    ax_tp = axes[0][0]
    ax_lat = axes[0][1]

    # Per-bar data. Build flat lists of (x_pos, color, value, marker)
    # so we can pass everything to ax.bar in one shot per axis.
    tp_x: list[float] = []
    tp_y: list[float] = []
    tp_colors: list[tuple[float, float, float, float]] = []
    tp_markers: list[tuple[float, float, str]] = []
    lat_x: list[float] = []
    lat_y: list[float] = []
    lat_colors: list[tuple[float, float, float, float]] = []
    lat_yerr_lower: list[float] = []
    lat_yerr_upper: list[float] = []
    bar_widths_tp: list[float] = []
    bar_widths_lat: list[float] = []

    for (transport, workload), (modes, offsets) in zip(slots, slot_layouts):
        bar_w = _bar_width_for_slot(len(modes))
        slot_idx = slots.index((transport, workload))
        slot_centre = x_centres[slot_idx]
        marker = _bar_tier_marker(workload)
        for mode, off in zip(modes, offsets):
            bar_x = slot_centre + off
            color = _threading_color(transport, mode)
            r = parsed.get((transport, workload, qos, mode))
            # Throughput -- T16.14 reads receives_per_sec, not
            # writes_per_sec. Receive throughput is the headline metric
            # per metak-shared/overview.md: writers ship at the
            # requested rate almost always (kernel send buffer absorbs
            # back-pressure), but the receiver-side drain rate is what
            # decides whether peers are in sync. With the bars now
            # showing receive rate, the gap between a bar and the
            # target-rate line below becomes the visible
            # delivery-shortfall indicator.
            if r is None:
                tp_val = float("nan")
            else:
                tp_val = float(r.receives_per_sec)
            if log_throughput and (np.isnan(tp_val) or tp_val <= 0.0):
                tp_val = float("nan")
            tp_x.append(bar_x)
            tp_y.append(tp_val)
            tp_colors.append(color)
            bar_widths_tp.append(bar_w)
            if marker is not None:
                tp_markers.append((bar_x, tp_val, marker))
            # Latency
            if r is None:
                p50 = p95 = p99 = float("nan")
            else:
                p50 = float(r.latency_p50_ms)
                p95 = float(r.latency_p95_ms)
                p99 = float(r.latency_p99_ms)
            if np.isnan(p95) or p95 <= 0.0:
                lat_val = float("nan")
                lower = upper = 0.0
            else:
                safe_p95 = max(p95, _LATENCY_EPSILON_MS)
                safe_p50 = max(p50, _LATENCY_EPSILON_MS)
                lat_val = safe_p95
                lower = max(safe_p95 - safe_p50, 0.0)
                upper = max(p99 - safe_p95, 0.0)
            lat_x.append(bar_x)
            lat_y.append(lat_val)
            lat_colors.append(color)
            lat_yerr_lower.append(lower)
            lat_yerr_upper.append(upper)
            bar_widths_lat.append(bar_w)

    ax_tp.bar(
        tp_x,
        tp_y,
        bar_widths_tp,
        color=tp_colors,
        edgecolor="black",
        linewidth=0.3,
    )
    ax_lat.bar(
        lat_x,
        lat_y,
        bar_widths_lat,
        color=lat_colors,
        edgecolor="black",
        linewidth=0.3,
        yerr=[lat_yerr_lower, lat_yerr_upper],
        capsize=2,
        ecolor="black",
        error_kw={"linewidth": 0.6},
    )

    # x-axis: slot centres carry the tick labels.
    slot_tick_labels = [w if w else t for t, w in slots]
    for ax in (ax_tp, ax_lat):
        ax.set_xticks(x_centres)
        ax.set_xticklabels(slot_tick_labels, rotation=45, ha="right", fontsize=7)

    # Throughput axis cosmetics. T16.14: bars carry receives_per_sec;
    # the label / title call that out so the gap between a bar and its
    # write-rate target line is unambiguous.
    ax_tp.set_ylabel(f"{qos_label} - receives/s")
    ax_tp.set_title(f"{qos_label} - Receive throughput (receives/s)")
    if log_throughput:
        ax_tp.set_yscale("log")
        ax_tp.yaxis.grid(True, which="both", linestyle="--", alpha=0.5)
    else:
        ax_tp.yaxis.grid(True, linestyle="--", alpha=0.5)
    ax_tp.set_axisbelow(True)

    # Horizontal target-rate reference lines on the throughput panel.
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

    # Per-bar tier markers on the throughput axis. Placement convention
    # matches the legacy multi-row layout: multiplicative offset on log
    # axes, fractional linear offset otherwise. NaN log-axis bars are
    # skipped (no valid y position); NaN linear bars anchor the marker
    # just above the x-axis so the target tier stays legible.
    is_log_tp = ax_tp.get_yscale() == "log"
    if not is_log_tp:
        linear_offset = 0.02 * ax_tp.get_ylim()[1]
    else:
        linear_offset = 0.0
    for bar_x, bar_height, marker in tp_markers:
        if is_log_tp:
            if np.isnan(bar_height) or bar_height <= 0.0:
                continue
            y_pos = bar_height * 1.15
        else:
            if np.isnan(bar_height):
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
    ax_lat.set_yscale("log")
    ax_lat.yaxis.grid(True, which="both", linestyle="--", alpha=0.5)
    ax_lat.set_axisbelow(True)

    # Legend: one entry per (transport, threading_mode) actually used
    # on this QoS. Reads in family order, single before multi.
    legend_handles: list[matplotlib.patches.Patch] = []
    seen_legend_keys: set[tuple[str, str | None]] = set()
    for transport in transport_order:
        for mode in ("single", "multi", None):
            key = (transport, mode)
            if key in seen_legend_keys:
                continue
            # Only add if any slot of this transport actually rendered
            # this mode for this QoS.
            present = any(
                t == transport and mode in modes
                for (t, _w), (modes, _off) in zip(slots, slot_layouts)
            )
            if not present:
                continue
            color = _threading_color(transport, mode)
            label_mode = mode if mode is not None else "legacy"
            legend_handles.append(
                matplotlib.patches.Patch(
                    facecolor=color,
                    edgecolor="black",
                    linewidth=0.3,
                    label=f"{transport} / {label_mode}",
                )
            )
            seen_legend_keys.add(key)

    legend_ncol = max(3, min(6, (len(legend_handles) + 2) // 3))
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
        title="Transport / threading",
        title_fontsize=9,
    )

    top_reserve = max(0.6, fig_height - 0.4) / fig_height
    fig.subplots_adjust(
        bottom=bottom_reserve,
        top=top_reserve,
        left=0.07,
        right=0.98,
        wspace=0.18,
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / out_filename
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)
    return out_path


def generate_comparison_plot(
    results: list[PerformanceResult],
    output_dir: Path,
    *,
    log_throughput: bool = False,
) -> list[Path]:
    """Generate one comparison bar chart PNG per observed QoS level.

    Replaces the pre-T16.13 monolithic single-PNG output. Each PNG
    covers exactly one QoS level and pairs ``single``/``multi`` bars
    per (transport, workload) slot. See the module docstring for the
    full layout description.

    Parameters
    ----------
    results:
        Performance results to visualise.
    output_dir:
        Directory where the PNGs will be saved (created if needed).
    log_throughput:
        When True, render the throughput panels on a log y-axis. Bars
        with non-positive ``receives_per_sec`` are dropped to NaN
        (matching the latency-panel convention) so the bar disappears
        rather than being clamped to a misleading visible floor. The
        per-QoS filenames carry an extra ``-log`` suffix so the
        log-scale outputs do not overwrite the linear-scale outputs
        in the same ``--output`` dir.

    Returns
    -------
    List of paths to the generated PNGs, one per observed QoS, in
    ascending QoS order (``None`` last for legacy spawns). When
    ``results`` is empty a single placeholder PNG is generated and
    returned in a one-element list so callers (and the CLI status
    print) keep working.
    """
    if not results:
        out_filename = _qos_filename("comparison", None, log_throughput=log_throughput)
        return [_empty_plot(output_dir, filename=out_filename)]

    parsed = _index_parsed_results(results)
    if not parsed:
        out_filename = _qos_filename("comparison", None, log_throughput=log_throughput)
        return [_empty_plot(output_dir, filename=out_filename)]

    parsed_keys = list(parsed.keys())
    transport_order, workload_order, qos_order = _collect_layout_orders(parsed_keys)
    target_rate_order = _collect_target_rates(parsed_keys)

    if not transport_order or not workload_order or not qos_order:
        out_filename = _qos_filename("comparison", None, log_throughput=log_throughput)
        return [_empty_plot(output_dir, filename=out_filename)]

    paths: list[Path] = []
    for q in qos_order:
        out_path = _generate_comparison_plot_for_qos(
            qos=q,
            parsed=parsed,
            transport_order=transport_order,
            workload_order=workload_order,
            target_rate_order=target_rate_order,
            output_dir=output_dir,
            log_throughput=log_throughput,
        )
        paths.append(out_path)
    return paths


def _generate_latency_cdf_plot_for_qos(
    *,
    qos: int | None,
    parsed: dict[tuple[str, str, int | None, str | None], PerformanceResult],
    transport_order: list[str],
    workload_order: list[str],
    output_dir: Path,
) -> Path:
    """Render the latency-CDF chart for a single QoS level.

    One line per (transport, workload, threading_mode) observed for
    this QoS. Line colour uses the same transport-family-tone scheme
    as the comparison plot; line *style* additionally encodes the
    threading mode (solid for ``multi``/legacy, dashed for ``single``)
    so single and multi curves for the same (transport, workload) are
    distinguishable without doubling the palette.
    """
    out_filename = _qos_filename("latency-cdf", qos, log_throughput=False)
    qos_label = f"qos{qos}" if qos is not None else "n/a"

    fig_width = 14.0
    plot_height = 5.5
    legend_band_height = 1.5
    fig_height = plot_height + legend_band_height
    fig, ax = plt.subplots(1, 1, figsize=(fig_width, fig_height))

    legend_handles: list[matplotlib.lines.Line2D] = []
    plotted_any = False
    for transport in transport_order:
        for workload in workload_order:
            for mode in ("single", "multi", None):
                r = parsed.get((transport, workload, qos, mode))
                if r is None:
                    continue
                samples = r.latency_samples_ms
                if not samples:
                    continue
                x, y = empirical_cdf(samples)
                if x.size == 0:
                    continue
                color = _threading_color(transport, mode)
                linestyle = "--" if mode == "single" else "-"
                ax.plot(x, y, color=color, linewidth=1.4, linestyle=linestyle)
                plotted_any = True
                label_mode = mode if mode is not None else "legacy"
                wl = workload if workload else "-"
                legend_handles.append(
                    matplotlib.lines.Line2D(
                        [],
                        [],
                        color=color,
                        linewidth=1.4,
                        linestyle=linestyle,
                        label=f"{transport} / {wl} / {label_mode}",
                    )
                )

    ax.set_xscale("log")
    ax.set_xlabel("latency (ms, log scale)")
    ax.set_ylabel(f"{qos_label} - empirical CDF")
    ax.set_title(f"{qos_label} - Latency CDF")
    ax.set_ylim(0.0, 1.0)
    ax.grid(True, which="both", linestyle="--", alpha=0.5)
    ax.set_axisbelow(True)
    if not plotted_any:
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

    if legend_handles:
        legend_ncol = max(3, min(6, (len(legend_handles) + 2) // 3))
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
            title="Transport / workload / threading",
            title_fontsize=9,
        )
    else:
        bottom_reserve = 0.1

    top_reserve = max(0.6, fig_height - 0.4) / fig_height
    fig.subplots_adjust(
        bottom=bottom_reserve,
        top=top_reserve,
        left=0.07,
        right=0.98,
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / out_filename
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)
    return out_path


def generate_latency_cdf_plot(
    results: list[PerformanceResult], output_dir: Path
) -> list[Path]:
    """Generate one latency-CDF PNG per observed QoS level.

    Mirror of ``generate_comparison_plot`` for the CDF chart family.
    Each PNG covers exactly one QoS level and carries one CDF line
    per (transport, workload, threading_mode) actually observed for
    that QoS. Threading mode is encoded in line style (solid =
    multi/legacy, dashed = single); the family colormap / tone scheme
    matches the comparison plot so a viewer can correlate distribution
    shape with the percentile bars across the eight files.

    Source: ``PerformanceResult.latency_samples_ms`` -- a downsampled
    per-message latency vector (cap ``LATENCY_SAMPLE_CAP`` per result;
    see ``performance.py``). Spawns with no samples for a given QoS
    contribute no line; an entire empty QoS produces a placeholder
    annotation rather than crashing.

    Returns
    -------
    List of paths to the generated PNGs, one per observed QoS, in
    ascending QoS order (``None`` last for legacy spawns).
    """
    if not results:
        out_filename = _qos_filename("latency-cdf", None, log_throughput=False)
        return [_empty_plot(output_dir, filename=out_filename)]

    parsed = _index_parsed_results(results)
    if not parsed:
        out_filename = _qos_filename("latency-cdf", None, log_throughput=False)
        return [_empty_plot(output_dir, filename=out_filename)]

    parsed_keys = list(parsed.keys())
    transport_order, workload_order, qos_order = _collect_layout_orders(parsed_keys)
    if not transport_order or not workload_order or not qos_order:
        out_filename = _qos_filename("latency-cdf", None, log_throughput=False)
        return [_empty_plot(output_dir, filename=out_filename)]

    paths: list[Path] = []
    for q in qos_order:
        out_path = _generate_latency_cdf_plot_for_qos(
            qos=q,
            parsed=parsed,
            transport_order=transport_order,
            workload_order=workload_order,
            output_dir=output_dir,
        )
        paths.append(out_path)
    return paths
