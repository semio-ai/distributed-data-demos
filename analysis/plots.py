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
(``drop-rate-qos<N>.png``) that renders ``loss_pct`` as an annotated
heatmap matrix: rows are ``(transport, threading_mode)`` pairs and
columns are workloads (T16.15, replacing the T16.14 bar layout).

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

# E19 / T19.6: workload-profile distinguished by matplotlib ``hatch``
# attribute on the bars in the restructured comparison-qos chart and on
# the new throughput-vs-workload-shape chart. The three profiles map
# onto the ``PerformanceResult.shape`` value (canonical contract per
# ``metak-shared/api-contracts/jsonl-log-schema.md`` E19 additions):
#
#   shape "scalar" -> scalar-flood profile -> solid fill (no hatch)
#   shape "array"  -> block-flood profile  -> horizontal-line hatch
#   shape "struct" -> mixed-types profile  -> checkered hatch
#
# Hatch density was picked empirically against the comparison-qos chart
# size at 150 dpi (target ~30 px per bar half): ``"---"`` is dense
# enough to remain visible on small bars without bleeding into the
# adjacent bar; ``"x"`` (single crosshatch) reads as a checker pattern
# at the same scale and contrasts cleanly with the horizontal lines.
# ``"+"`` was rejected because at small bar widths it visually fuses
# into ``"---"`` and the distinction is lost. Legacy / unknown shapes
# fall back to solid (no hatch) so unparseable data still renders.
_WORKLOAD_HATCHES: dict[str, str] = {
    "scalar": "",
    "array": "---",
    "struct": "x",
}

# Display label per shape value -- used in chart legends so the reader
# sees the workload-profile name (the user-facing vocabulary from
# BENCHMARK.md § 6) rather than the analyzer-internal shape token.
_WORKLOAD_LABELS: dict[str, str] = {
    "scalar": "scalar-flood",
    "array": "block-flood",
    "struct": "mixed-types",
}

# Stable ordering of shapes on the throughput-vs-workload-shape chart's
# x-axis. Matches the spec's sort: scalar-flood -> block-flood ->
# mixed-types. Shapes not in this list render after these in
# alphabetical order so a future shape addition still plots cleanly.
_WORKLOAD_SHAPE_ORDER: tuple[str, ...] = ("scalar", "array", "struct")


def _shape_hatch(shape: str | None) -> str:
    """Return the matplotlib hatch pattern for a ``PerformanceResult.shape`` value.

    Unknown / null shapes fall back to the ``scalar`` (solid) hatch so
    legacy / pre-E19 data renders consistently with the scalar-flood
    convention.
    """
    if shape is None:
        return _WORKLOAD_HATCHES["scalar"]
    return _WORKLOAD_HATCHES.get(shape, _WORKLOAD_HATCHES["scalar"])


def _shape_label(shape: str | None) -> str:
    """Return the user-facing workload-profile label for a shape value."""
    if shape is None:
        return _WORKLOAD_LABELS["scalar"]
    return _WORKLOAD_LABELS.get(shape, shape)


def _shape_sort_key(shape: str) -> tuple[int, str]:
    """Sort key putting scalar/array/struct in canonical order, others last."""
    if shape in _WORKLOAD_SHAPE_ORDER:
        return (_WORKLOAD_SHAPE_ORDER.index(shape), shape)
    return (len(_WORKLOAD_SHAPE_ORDER), shape)


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

    Note: callers that need to distinguish ``shape`` (E19 / T19.6 -- the
    restructured comparison-qos chart and the new throughput-vs-
    workload-shape chart) should use :func:`_index_parsed_results_with_shape`
    instead. That index keys on a 5-tuple including the workload-shape
    so a (transport, workload, qos, mode) slot can hold one bar per
    observed shape. The 4-tuple version here is retained for the
    drop-rate heatmap and the latency-CDF charts which intentionally
    collapse the shape dimension.
    """
    parsed: dict[tuple[str, str, int | None, str | None], PerformanceResult] = {}
    for r in results:
        transport, workload, qos, threading_mode = _split_variant_name(r.variant)
        key = (transport, workload, qos, threading_mode)
        parsed.setdefault(key, r)
    return parsed


def _index_parsed_results_with_shape(
    results: list[PerformanceResult],
) -> dict[tuple[str, str, int | None, str | None, str], PerformanceResult]:
    """Index results by ``(transport, workload, qos, threading_mode, shape)``.

    E19 / T19.6 index used by the restructured comparison-qos chart and
    the new throughput-vs-workload-shape chart. Adds the workload shape
    (``scalar``/``array``/``struct``, defaulting to ``"scalar"`` for
    legacy data per the api-contracts backward-compat rule) as a fifth
    key component so a single (transport, workload, qos, mode) slot can
    hold one bar per observed shape. First entry per key wins -- matches
    the existing collapse convention.
    """
    parsed: dict[tuple[str, str, int | None, str | None, str], PerformanceResult] = {}
    for r in results:
        transport, workload, qos, threading_mode = _split_variant_name(r.variant)
        shape = r.shape if r.shape else "scalar"
        key = (transport, workload, qos, threading_mode, shape)
        parsed.setdefault(key, r)
    return parsed


def _qos_sort_key(q: int | None) -> tuple[int, int]:
    """Sort key that puts numeric QoS first ascending, ``None`` last."""
    if q is None:
        return (1, 0)
    return (0, q)


def _collect_layout_orders(
    parsed_keys: list[tuple],
) -> tuple[list[str], list[str], list[int | None]]:
    """Return ordered transports, workloads, and QoS levels.

    Accepts any tuple key whose first three components are
    ``(transport, workload, qos)`` -- both the 4-tuple key from
    :func:`_index_parsed_results` and the 5-tuple key from
    :func:`_index_parsed_results_with_shape` are supported. Mirrors the
    pre-T16.13 ordering: ``TRANSPORT_FAMILIES`` order for transports
    (with ``other`` appended if present), load-intensity order for
    workloads, and ascending order for QoS (``None`` last).
    """
    transports_seen = {k[0] for k in parsed_keys}
    transport_order: list[str] = [t for t in TRANSPORT_FAMILIES if t in transports_seen]
    if "other" in transports_seen:
        transport_order.append("other")

    workload_set: set[str] = {k[1] for k in parsed_keys}
    workload_order: list[str] = sorted(workload_set, key=_workload_load_rank)

    qos_values_seen: set[int | None] = {k[2] for k in parsed_keys}
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


def _slot_subbars_with_shape(
    transport: str,
    workload: str,
    qos: int | None,
    parsed: dict[tuple[str, str, int | None, str | None, str], PerformanceResult],
) -> list[tuple[str, str | None]]:
    """Return the ordered ``(shape, threading_mode)`` sub-bars for a slot.

    E19 / T19.6 expansion of :func:`_slot_threading_modes`. Each
    sub-bar represents one observed ``(shape, threading_mode)``
    combination for the given (transport, workload, qos) slot. The
    ordering is: shapes in canonical order (``scalar`` -> ``array`` ->
    ``struct``; unknown shapes last) and, within each shape, threading
    modes in ``single`` -> ``multi`` -> legacy order. Multi-only
    transports (per E14) still emit only multi bars; the threading
    suppression rule from :func:`_slot_threading_modes` is preserved.

    Slots with no observations return an empty list.
    """
    observed_pairs: set[tuple[str, str | None]] = {
        (shape, mode)
        for (t, w, q, mode, shape) in parsed.keys()
        if t == transport and w == workload and q == qos
    }
    if not observed_pairs:
        return []

    shapes_seen = sorted({s for (s, _m) in observed_pairs}, key=_shape_sort_key)

    is_multi_only = transport in _MULTI_ONLY_TRANSPORTS
    ordered: list[tuple[str, str | None]] = []
    for shape in shapes_seen:
        modes_for_shape = {m for (s, m) in observed_pairs if s == shape}
        if is_multi_only and "multi" in modes_for_shape:
            ordered.append((shape, "multi"))
            continue
        for mode in ("single", "multi"):
            if mode in modes_for_shape:
                ordered.append((shape, mode))
        if None in modes_for_shape:
            ordered.append((shape, None))
    return ordered


def _slot_subbar_layout(
    n_subbars: int,
) -> tuple[list[float], float]:
    """Compute x-offsets and bar width for ``n_subbars`` inside a slot.

    The slot occupies ``[-0.5, +0.5]`` around its centre. Bars are
    spread evenly with a small inter-bar margin so an empty band
    separates adjacent slots. Bar width is constant within a slot so a
    visual grouping of slot members is unambiguous.
    """
    if n_subbars <= 0:
        return [], 0.0
    if n_subbars == 1:
        return [0.0], 0.78
    if n_subbars == 2:
        return [-0.22, 0.22], 0.40
    # General N-bar spreading. Leave ~30% of slot width as margins so
    # bars in adjacent slots stay visibly separate; bars are evenly
    # spaced across the remaining 70%.
    step = 0.70 / n_subbars
    start = -0.35 + 0.5 * step
    return [start + i * step for i in range(n_subbars)], step * 0.85


def _collect_target_rates(
    parsed_keys: list[tuple],
) -> list[int]:
    """Compute the sorted unique ``vpt * hz`` target rates across workloads.

    Accepts any tuple key whose second component is the workload string
    (i.e. both the 4-tuple and 5-tuple key formats). ``max`` and
    unparseable workloads contribute nothing.
    """
    target_rates: set[int] = set()
    for key in parsed_keys:
        workload = key[1]
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
    parsed: dict[tuple[str, str, int | None, str | None, str], PerformanceResult],
    transport_order: list[str],
    workload_order: list[str],
    target_rate_order: list[int],
    output_dir: Path,
    log_throughput: bool,
) -> Path:
    """Render the comparison chart for a single QoS level (T19.6 layout).

    Slots are (transport, workload) pairs in family-block x
    load-intensity order. Each slot holds one bar per observed
    ``(shape, threading_mode)`` pair (E19 / T19.6 axis expansion); the
    workload-profile shape (``scalar``/``array``/``struct``) is
    distinguished by matplotlib ``hatch`` and the threading_mode is
    distinguished by the family colormap tone (preserving the
    pre-T19.6 ``single``/``multi`` palette). Variants that don't
    support multi threading_mode simply emit fewer bars in their group
    -- no placeholder bars per the locked spec.

    Layout: the two subplots are stacked **vertically** (``nrows=2,
    ncols=1``) -- receive throughput on top, latency on bottom -- so
    every slot's x-position aligns across the two metric views and a
    reader can scan a single column to see both the throughput bar
    and its corresponding latency-p95 bar without cross-eyeing two
    side-by-side panels. The pre-T19.6 1x2 horizontal layout is
    retired (and now unreadable past ~3 workload profiles per slot).

    The throughput column reads ``PerformanceResult.receives_per_sec``
    -- per T16.14 the project headline metric is *receive* throughput,
    not write throughput. The target-rate horizontal lines and tier
    markers still encode the intended *write* rate; the gap between a
    bar (receives) and its target line (writes) is the visible
    delivery shortfall. Returns the path of the PNG written.

    Figure-width formula: ``max(14.0, 0.55 * n_slots + 4.0)``. The
    coefficient is tuned for the post-E14 6-family x 8-workload (~48
    slots) matrix so each slot gets roughly 30 px of x-axis space at
    150 dpi -- wide enough to read the tick label without zooming.
    Slots with 4-6 sub-bars therefore get ~5-7 px per bar; the hatch
    + colour combination remains legible at that size.
    """
    out_filename = _qos_filename("comparison", qos, log_throughput=log_throughput)
    qos_label = f"qos{qos}" if qos is not None else "n/a"

    # Slots in family-block x load-intensity order. Drop slots that
    # have no observation for this QoS at all (saves x-axis width on
    # sparse QoS levels).
    slots: list[tuple[str, str]] = []
    slot_subbars: list[list[tuple[str, str | None]]] = []
    for t in transport_order:
        for w in workload_order:
            subbars = _slot_subbars_with_shape(t, w, qos, parsed)
            if subbars:
                slots.append((t, w))
                slot_subbars.append(subbars)

    if not slots:
        return _empty_plot(output_dir, filename=out_filename)

    n_slots = len(slots)
    x_centres = np.arange(n_slots, dtype=float)

    # Figure size: width grows with slot count (see docstring); height
    # accommodates two stacked metric panels plus a legend band. The
    # 2-row vertical layout (T19.6) is the load-bearing structural
    # change from the pre-T19.6 1x2 layout.
    fig_width = max(14.0, 0.55 * n_slots + 4.0)
    panel_height = 4.0
    legend_band_height = 1.6
    fig_height = 2 * panel_height + legend_band_height
    fig, axes = plt.subplots(
        nrows=2, ncols=1, figsize=(fig_width, fig_height), squeeze=False
    )
    ax_tp = axes[0][0]
    ax_lat = axes[1][0]

    # Per-bar data. Build flat lists of (x_pos, color, hatch, value)
    # so we can attribute hatch + colour per bar without batching by
    # group -- matplotlib's ``ax.bar`` accepts a single hatch per call,
    # so we render slot-by-slot (one ``bar()`` per (shape, mode) group)
    # and the resulting Patch objects retain their hatch attribute for
    # the visual-regression test.
    shapes_present: set[str] = set()
    modes_present: set[str | None] = set()

    # Track bar tier markers (target write-rate annotations).
    tp_markers: list[tuple[float, float, str]] = []

    for slot_idx, ((transport, workload), subbars) in enumerate(
        zip(slots, slot_subbars)
    ):
        offsets, bar_w = _slot_subbar_layout(len(subbars))
        slot_centre = x_centres[slot_idx]
        marker = _bar_tier_marker(workload)
        for (shape, mode), off in zip(subbars, offsets):
            bar_x = slot_centre + off
            color = _threading_color(transport, mode)
            hatch = _shape_hatch(shape)
            shapes_present.add(shape)
            modes_present.add(mode)
            r = parsed.get((transport, workload, qos, mode, shape))

            # Throughput (receives_per_sec -- T16.14 headline).
            if r is None:
                tp_val = float("nan")
            else:
                tp_val = float(r.receives_per_sec)
            if log_throughput and (np.isnan(tp_val) or tp_val <= 0.0):
                tp_val = float("nan")

            # Each bar is rendered as its own ax.bar() call so the
            # resulting Patch carries the per-(shape) hatch attribute
            # uniformly -- batching by ax.bar([x1, x2], [y1, y2], ...)
            # forces a single hatch across the batch. The overhead is
            # negligible at the ~50-200 sub-bar scale of this chart.
            ax_tp.bar(
                [bar_x],
                [tp_val],
                bar_w,
                color=[color],
                edgecolor="black",
                linewidth=0.3,
                hatch=hatch,
            )

            if marker is not None:
                tp_markers.append((bar_x, tp_val, marker))

            # Latency p95 + p50/p99 whiskers.
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
            ax_lat.bar(
                [bar_x],
                [lat_val],
                bar_w,
                color=[color],
                edgecolor="black",
                linewidth=0.3,
                yerr=[[lower], [upper]],
                capsize=2,
                ecolor="black",
                error_kw={"linewidth": 0.6},
                hatch=hatch,
            )

    # x-axis: slot centres carry the tick labels (both panels).
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

    # Legend: two parallel legend strips so each dimension reads
    # independently. The first (workload profile, by hatch) sits above
    # the second (transport / threading, by colour). Two separate
    # ``fig.legend`` calls are used instead of a single combined legend
    # because matplotlib's per-legend ``title=`` argument is the
    # cleanest way to call out the dimension each strip encodes.
    workload_handles: list[matplotlib.patches.Patch] = []
    for shape in sorted(shapes_present, key=_shape_sort_key):
        workload_handles.append(
            matplotlib.patches.Patch(
                facecolor="white",
                edgecolor="black",
                linewidth=0.5,
                hatch=_shape_hatch(shape),
                label=_shape_label(shape),
            )
        )

    threading_handles: list[matplotlib.patches.Patch] = []
    seen_legend_keys: set[tuple[str, str | None]] = set()
    for transport in transport_order:
        for mode in ("single", "multi", None):
            if mode not in modes_present:
                continue
            key = (transport, mode)
            if key in seen_legend_keys:
                continue
            present = any(
                t == transport and any(m == mode for (_s, m) in subbars)
                for (t, _w), subbars in zip(slots, slot_subbars)
            )
            if not present:
                continue
            color = _threading_color(transport, mode)
            label_mode = mode if mode is not None else "legacy"
            threading_handles.append(
                matplotlib.patches.Patch(
                    facecolor=color,
                    edgecolor="black",
                    linewidth=0.3,
                    label=f"{transport} / {label_mode}",
                )
            )
            seen_legend_keys.add(key)

    # Reserve the bottom legend band proportional to the number of
    # threading rows (the larger of the two legends).
    threading_ncol = max(3, min(6, (len(threading_handles) + 2) // 3))
    threading_rows = (
        (len(threading_handles) + threading_ncol - 1) // threading_ncol
        if threading_handles
        else 0
    )
    workload_rows = 1 if workload_handles else 0
    row_height_in = 0.22
    legend_band_in = max(0.8, 0.4 + row_height_in * (threading_rows + workload_rows))
    bottom_reserve = min(0.30, legend_band_in / fig_height)

    if workload_handles:
        # Workload (hatch) legend sits just above the threading legend.
        wl_y = 0.005 + (0.005 + row_height_in * threading_rows / fig_height)
        fig.legend(
            handles=workload_handles,
            loc="lower center",
            bbox_to_anchor=(0.5, wl_y),
            ncol=min(3, len(workload_handles)),
            frameon=True,
            fontsize=8,
            title="Workload profile (fill pattern)",
            title_fontsize=9,
        )

    if threading_handles:
        fig.legend(
            handles=threading_handles,
            loc="lower center",
            bbox_to_anchor=(0.5, 0.005),
            ncol=threading_ncol,
            frameon=True,
            fontsize=8,
            title="Transport / threading (colour)",
            title_fontsize=9,
        )

    top_reserve = max(0.6, fig_height - 0.4) / fig_height
    fig.subplots_adjust(
        bottom=bottom_reserve,
        top=top_reserve,
        left=0.07,
        right=0.98,
        hspace=0.45,
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

    # T19.6: index by 5-tuple including the workload shape so a single
    # (transport, workload, qos, mode) slot can hold one bar per
    # observed shape.
    parsed = _index_parsed_results_with_shape(results)
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


# Drop-rate heatmap colormap (T16.15). ``RdYlGn_r`` runs green -> yellow
# -> red, so when used as the cell-fill colormap with the data range
# locked to [0, 100] loss %, "low loss is good (green)" and "high loss
# is bad (red)" matches the operator's intuition without any extra
# annotation. The reversed (``_r``) variant is the one that puts green
# at the *low* end -- matplotlib's base ``RdYlGn`` runs red -> green
# which is the wrong direction for a "lower is better" metric.
_DROP_RATE_CMAP: str = "RdYlGn_r"

# Drop-rate heatmap missing-cell rendering (T16.15). Cells with no
# observation for the (row, workload, qos) tuple render with a light-
# grey fill and a diagonal hatch pattern, plus the literal text
# ``n/a``. The hatch makes the missing-data state legible at a glance
# even if the viewer's eye reads the grey as "neutral colour from the
# bottom of the colormap"; the explicit ``n/a`` text removes any
# residual ambiguity.
_DROP_RATE_MISSING_FILL: str = "#dddddd"
_DROP_RATE_MISSING_HATCH: str = "///"

# Drop-rate heatmap luminance threshold for picking annotation text
# colour (T16.15). The threshold is computed from the cell's RGBA fill
# colour via the standard Rec. 601 luma weights
# (``0.299 R + 0.587 G + 0.114 B``). Values brighter than 0.55 get
# black text, darker than 0.55 get white text. The threshold is
# slightly above 0.5 so the bright yellow midband of ``RdYlGn_r``
# (~0.7-0.85 luminance) reliably picks black, and the saturated red
# top end (~0.4 luminance) reliably picks white. Picking from
# luminance -- not a hard-coded percentage cutoff -- keeps the
# behaviour correct if the colormap is swapped later.
_DROP_RATE_TEXT_LUMA_THRESHOLD: float = 0.55


def _drop_rate_row_order(
    parsed_keys: list[tuple[str, str, int | None, str | None]],
    qos: int | None,
) -> list[tuple[str, str | None]]:
    """Return the ordered list of ``(transport, threading_mode)`` rows for a QoS.

    Rows follow ``TRANSPORT_FAMILIES`` order (with ``other`` appended
    when present). Within a family the threading modes that exist for
    that family are listed ``single`` before ``multi`` so a family
    that has both shows ``<family>-single`` above ``<family>-multi`` as
    adjacent rows. A multi-only family (QUIC / WebRTC / Zenoh per E14)
    contributes a single row whose mode is ``"multi"``. Legacy spawns
    with ``threading_mode=None`` contribute a row labelled ``legacy``
    placed after both threading rows for the family.
    """
    observed: dict[str, set[str | None]] = {}
    for transport, _workload, q, mode in parsed_keys:
        if q != qos:
            continue
        observed.setdefault(transport, set()).add(mode)

    ordered: list[tuple[str, str | None]] = []
    transport_lookup = list(TRANSPORT_FAMILIES) + ["other"]
    for transport in transport_lookup:
        if transport not in observed:
            continue
        modes_present = observed[transport]
        for mode in ("single", "multi", None):
            if mode in modes_present:
                ordered.append((transport, mode))
    return ordered


def _row_label(transport: str, mode: str | None) -> str:
    """Format a row label as ``<transport>-<mode>`` with a ``legacy`` fallback."""
    if mode is None:
        return f"{transport}-legacy"
    return f"{transport}-{mode}"


def _relative_luminance(rgba: tuple[float, ...]) -> float:
    """Return the Rec. 601 relative luminance of an RGBA colour in [0, 1]."""
    r, g, b = rgba[0], rgba[1], rgba[2]
    return 0.299 * r + 0.587 * g + 0.114 * b


def _generate_drop_rate_plot_for_qos(
    *,
    qos: int | None,
    parsed: dict[tuple[str, str, int | None, str | None], PerformanceResult],
    workload_order: list[str],
    parsed_keys: list[tuple[str, str, int | None, str | None]],
    output_dir: Path,
) -> Path:
    """Render the drop-rate heatmap for a single QoS level (T16.15).

    Rows are ``(transport, threading_mode)`` pairs in
    ``TRANSPORT_FAMILIES`` order with ``single`` listed above ``multi``
    within each family. Columns are observed workloads in
    ``_workload_load_rank`` order. Cells encode
    ``PerformanceResult.loss_pct`` (0-100 linear) via the
    ``RdYlGn_r`` colormap -- green at 0 % loss, yellow in the middle,
    red at 100 % -- with the percentage stamped inside each cell as
    ``XX.X%``. Missing (row, workload) cells render as a grey hatched
    patch with the literal text ``n/a``; they are *not* drawn as 0 %
    (which would falsely suggest a successful spawn).

    Text colour is auto-selected per cell from the cell's relative
    luminance: black on cells brighter than
    ``_DROP_RATE_TEXT_LUMA_THRESHOLD``, white on darker cells. This
    keeps the threshold tied to the colormap rather than to a fixed
    percentage cutoff, so the labels remain legible even if the
    colormap is later swapped.
    """
    out_filename = _qos_filename("drop-rate", qos, log_throughput=False)
    qos_label = f"qos{qos}" if qos is not None else "n/a"

    rows = _drop_rate_row_order(parsed_keys, qos)
    cols = [
        w for w in workload_order if any((t, w, qos, m) in parsed for (t, m) in rows)
    ]

    if not rows or not cols:
        return _empty_plot(output_dir, filename=out_filename)

    n_rows = len(rows)
    n_cols = len(cols)

    # Build the value matrix. NaN marks "no observation" -- the
    # missing-cell rendering pass paints those slots with the grey
    # hatched patch and the ``n/a`` label.
    data = np.full((n_rows, n_cols), np.nan, dtype=float)
    for i, (transport, mode) in enumerate(rows):
        for j, workload in enumerate(cols):
            r = parsed.get((transport, workload, qos, mode))
            if r is not None:
                data[i, j] = float(r.loss_pct)

    # Figure size: ~0.8 in per column for the heatmap body plus 3 in
    # of slack for row labels and the colour-bar; ~0.4 in per row plus
    # 1 in for the title and column labels. Matches the suggested
    # cell-aspect target (cell ~80 px wide, ~30 px tall at 150 dpi).
    fig_width = 0.8 * n_cols + 3.0
    fig_height = 0.4 * n_rows + 1.5
    fig, ax = plt.subplots(figsize=(fig_width, fig_height))

    cmap = plt.get_cmap(_DROP_RATE_CMAP)
    norm = matplotlib.colors.Normalize(vmin=0.0, vmax=100.0)
    masked = np.ma.masked_invalid(data)
    im = ax.imshow(
        masked,
        cmap=cmap,
        norm=norm,
        aspect="auto",
        origin="upper",
        interpolation="nearest",
    )

    # Axis ticks: column labels on the bottom, row labels on the
    # left. Column labels are rotated 45 degrees to match the
    # comparison-chart convention; row labels stay horizontal so the
    # ``<transport>-<mode>`` text is easy to scan top-to-bottom.
    ax.set_xticks(np.arange(n_cols))
    ax.set_xticklabels(cols, rotation=45, ha="right", fontsize=8)
    ax.set_yticks(np.arange(n_rows))
    ax.set_yticklabels([_row_label(t, m) for (t, m) in rows], fontsize=8)

    # Thin grid between cells: emphasise the matrix structure without
    # competing with the in-cell text.
    ax.set_xticks(np.arange(-0.5, n_cols, 1), minor=True)
    ax.set_yticks(np.arange(-0.5, n_rows, 1), minor=True)
    ax.grid(which="minor", color="white", linestyle="-", linewidth=1)
    ax.tick_params(which="minor", bottom=False, left=False)

    # Per-cell annotation pass. Each cell is stamped with either the
    # ``XX.X%`` value (over the colormap fill from ``imshow``) or, for
    # missing cells, a grey hatched overlay patch plus the literal
    # ``n/a`` text.
    for i in range(n_rows):
        for j in range(n_cols):
            val = data[i, j]
            if np.isnan(val):
                # Missing cell: overlay a grey hatched patch on top of
                # whatever ``imshow`` rendered for the masked entry,
                # then stamp ``n/a``. The hatch's edge stays visible
                # in black for clarity even at small sizes.
                patch = matplotlib.patches.Rectangle(
                    (j - 0.5, i - 0.5),
                    1.0,
                    1.0,
                    facecolor=_DROP_RATE_MISSING_FILL,
                    edgecolor="black",
                    linewidth=0.0,
                    hatch=_DROP_RATE_MISSING_HATCH,
                    zorder=2,
                )
                ax.add_patch(patch)
                ax.text(
                    j,
                    i,
                    "n/a",
                    ha="center",
                    va="center",
                    fontsize=8,
                    color="black",
                    zorder=3,
                )
                continue
            cell_rgba = cmap(norm(val))
            luma = _relative_luminance(cell_rgba)
            text_colour = "black" if luma > _DROP_RATE_TEXT_LUMA_THRESHOLD else "white"
            ax.text(
                j,
                i,
                f"{val:.1f}%",
                ha="center",
                va="center",
                fontsize=8,
                color=text_colour,
                zorder=3,
            )

    ax.set_title(f"{qos_label} - Drop rate (loss %)")

    # Colour-bar to the right with explicit 0/25/50/75/100 ticks. Size
    # the bar narrowly so the heatmap body stays the dominant visual
    # element.
    cbar = fig.colorbar(im, ax=ax, fraction=0.04, pad=0.02)
    cbar.set_ticks([0, 25, 50, 75, 100])
    cbar.set_ticklabels(["0%", "25%", "50%", "75%", "100%"])
    cbar.ax.tick_params(labelsize=8)

    fig.tight_layout()

    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / out_filename
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)
    return out_path


def generate_drop_rate_plot(
    results: list[PerformanceResult], output_dir: Path
) -> list[Path]:
    """Generate one drop-rate heatmap PNG per observed QoS level (T16.15).

    Filename convention: ``drop-rate-qos<N>.png`` (or
    ``drop-rate-qosNA.png`` for legacy spawns with no qos suffix).
    Each PNG is an annotated heatmap: rows are
    ``(transport, threading_mode)`` pairs in ``TRANSPORT_FAMILIES``
    order with ``single`` listed above ``multi`` within each family;
    columns are workloads ordered by ``_workload_load_rank``; cells
    encode ``PerformanceResult.loss_pct`` via the ``RdYlGn_r``
    colormap with the value stamped inside each cell as ``XX.X%``.
    Missing ``(row, workload, qos)`` combinations render as a grey
    hatched cell with the literal text ``n/a``; they are *not* drawn
    as a green 0 % cell (which would falsely imply a successful
    spawn). This is the visual analogue of the
    ``summary_pivot_qos<N>.md`` pivot tables, restricted to the
    drop-rate metric.

    Returns
    -------
    List of paths to the generated PNGs, one per observed QoS, in
    ascending QoS order (``None`` last for legacy spawns). When
    ``results`` is empty a single placeholder PNG is generated and
    returned in a one-element list so callers (and the CLI status
    print) keep working.
    """
    if not results:
        out_filename = _qos_filename("drop-rate", None, log_throughput=False)
        return [_empty_plot(output_dir, filename=out_filename)]

    parsed = _index_parsed_results(results)
    if not parsed:
        out_filename = _qos_filename("drop-rate", None, log_throughput=False)
        return [_empty_plot(output_dir, filename=out_filename)]

    parsed_keys = list(parsed.keys())
    _transport_order, workload_order, qos_order = _collect_layout_orders(parsed_keys)
    if not workload_order or not qos_order:
        out_filename = _qos_filename("drop-rate", None, log_throughput=False)
        return [_empty_plot(output_dir, filename=out_filename)]

    paths: list[Path] = []
    for q in qos_order:
        out_path = _generate_drop_rate_plot_for_qos(
            qos=q,
            parsed=parsed,
            workload_order=workload_order,
            parsed_keys=parsed_keys,
            output_dir=output_dir,
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


# --- E19 / T19.6: throughput-vs-workload-shape chart --------------------------

# Filename emitted by :func:`generate_throughput_vs_workload_shape_plot`.
# Single PNG: per-variant subplot grid stacks all variants in one image.
_THROUGHPUT_VS_SHAPE_FILENAME: str = "throughput-vs-workload-shape.png"


def _group_results_by_variant_axis(
    results: list[PerformanceResult],
) -> dict[tuple[str, str, str | None], dict[tuple[str, int | None], PerformanceResult]]:
    """Bucket performance results into per-variant-axis cells.

    The "variant axis" key is ``(transport, workload, threading_mode)``
    -- everything that identifies a variant *except* QoS and the
    workload-shape profile, both of which become axes within each
    subplot of the throughput-vs-workload-shape chart. The inner dict
    keys on ``(shape, qos)`` so a single subplot can carry up to
    ``len(shapes) * len(qos)`` bars.

    First entry per ``(shape, qos)`` within a variant wins, matching
    the existing collapse convention in :func:`_index_parsed_results`.
    """
    buckets: dict[
        tuple[str, str, str | None],
        dict[tuple[str, int | None], PerformanceResult],
    ] = {}
    for r in results:
        transport, workload, qos, threading_mode = _split_variant_name(r.variant)
        key = (transport, workload, threading_mode)
        shape = r.shape if r.shape else "scalar"
        inner = buckets.setdefault(key, {})
        inner.setdefault((shape, qos), r)
    return buckets


def _variant_axis_label(key: tuple[str, str, str | None]) -> str:
    """Render a ``(transport, workload, threading_mode)`` tuple as a label."""
    transport, workload, mode = key
    parts = [transport]
    if workload:
        parts.append(workload)
    if mode is not None:
        parts.append(mode)
    return "-".join(parts)


def _generate_throughput_vs_workload_shape_for_axis(
    *,
    ax,
    variant_key: tuple[str, str, str | None],
    cells: dict[tuple[str, int | None], PerformanceResult],
    qos_order: list[int | None],
    shape_order: list[str],
) -> None:
    """Render one subplot of the throughput-vs-workload-shape chart.

    The subplot has one bar group per workload shape on the x-axis and
    one bar per QoS within each group. Bar hatch encodes the shape
    (consistent with the comparison-qos restructure for cross-chart
    legibility) and the bar's edge / colour-tone band can be re-used
    later if a QoS-by-colour scheme is added. For T19.6 the y-axis is
    locked to ``leaves_per_sec`` (the canonical cross-workload
    comparable metric per the api-contracts E19 additions).
    """
    transport, _workload, threading_mode = variant_key
    base_color = _threading_color(transport, threading_mode)

    n_shapes = len(shape_order)
    n_qos = len(qos_order)
    shape_centres = np.arange(n_shapes, dtype=float)

    # Sub-bar layout per shape group: spread the per-QoS bars evenly
    # across [-0.4, +0.4] around the shape's centre. Bar width matches
    # the inter-bar step minus a small margin so adjacent groups stay
    # visibly separated.
    if n_qos == 1:
        per_qos_offsets = [0.0]
        bar_w = 0.6
    else:
        step = 0.8 / n_qos
        start = -0.4 + 0.5 * step
        per_qos_offsets = [start + i * step for i in range(n_qos)]
        bar_w = step * 0.85

    for shape_idx, shape in enumerate(shape_order):
        hatch = _shape_hatch(shape)
        for qos_idx, qos in enumerate(qos_order):
            r = cells.get((shape, qos))
            if r is None:
                value = float("nan")
            else:
                value = float(r.leaves_per_sec)
            ax.bar(
                [shape_centres[shape_idx] + per_qos_offsets[qos_idx]],
                [value],
                bar_w,
                color=[base_color],
                edgecolor="black",
                linewidth=0.3,
                hatch=hatch,
            )

    ax.set_xticks(shape_centres)
    ax.set_xticklabels([_shape_label(s) for s in shape_order], rotation=0, fontsize=8)
    ax.set_ylabel("leaves/s")
    ax.set_title(_variant_axis_label(variant_key), fontsize=9)
    ax.yaxis.grid(True, linestyle="--", alpha=0.5)
    ax.set_axisbelow(True)


def generate_throughput_vs_workload_shape_plot(
    results: list[PerformanceResult],
    output_dir: Path,
) -> Path:
    """Render the per-variant throughput-vs-workload-shape chart (T19.6).

    Layout:

    - Per-variant subplot grid (one subplot per unique
      ``(transport, workload, threading_mode)`` tuple in the dataset).
    - X-axis on each subplot: workload profile, sorted scalar-flood ->
      block-flood -> mixed-types (per :data:`_WORKLOAD_SHAPE_ORDER`).
    - Y-axis: ``PerformanceResult.leaves_per_sec`` -- the canonical
      cross-workload comparable metric per the api-contracts E19
      additions.
    - One bar per QoS within each workload group. The per-shape hatch
      mirrors the comparison-qos chart so a reader can correlate bars
      across the two outputs.

    Empty datasets render a single placeholder PNG.
    """
    if not results:
        return _empty_plot(output_dir, filename=_THROUGHPUT_VS_SHAPE_FILENAME)

    buckets = _group_results_by_variant_axis(results)
    if not buckets:
        return _empty_plot(output_dir, filename=_THROUGHPUT_VS_SHAPE_FILENAME)

    # Determine global axis orderings from the union of every variant
    # axis's observations so each subplot's x-axis covers the same
    # ordered set of shapes (gaps show up as missing bars, not as a
    # shorter x-axis).
    shape_set: set[str] = set()
    qos_set: set[int | None] = set()
    for cells in buckets.values():
        for shape, qos in cells.keys():
            shape_set.add(shape)
            qos_set.add(qos)
    shape_order = sorted(shape_set, key=_shape_sort_key)
    qos_order = sorted(qos_set, key=_qos_sort_key)

    # Stable variant-axis ordering: family order, then workload load-
    # intensity, then ``single`` before ``multi`` before legacy.
    def variant_sort_key(
        key: tuple[str, str, str | None],
    ) -> tuple[int, tuple[int, int, str], int]:
        transport, workload, mode = key
        family_rank = (
            TRANSPORT_FAMILIES.index(transport)
            if transport in TRANSPORT_FAMILIES
            else len(TRANSPORT_FAMILIES)
        )
        mode_rank = {"single": 0, "multi": 1, None: 2}.get(mode, 3)
        return family_rank, _workload_load_rank(workload), mode_rank

    variant_keys = sorted(buckets.keys(), key=variant_sort_key)
    n_variants = len(variant_keys)

    # Grid shape: prefer a square-ish layout with a width cap so the
    # subplots stay legible on dense datasets. 3-wide is the sweet spot
    # for the ~6-9 variant axes in the canonical T19.6 fixture; the
    # cap auto-extends to 4 cols on larger datasets.
    ncols = min(4, max(1, n_variants if n_variants <= 3 else 3))
    nrows = (n_variants + ncols - 1) // ncols

    subplot_w = 4.5
    subplot_h = 3.2
    legend_band_height = 1.2
    fig_width = max(8.0, subplot_w * ncols)
    fig_height = subplot_h * nrows + legend_band_height
    fig, axes = plt.subplots(
        nrows=nrows, ncols=ncols, figsize=(fig_width, fig_height), squeeze=False
    )

    for idx, variant_key in enumerate(variant_keys):
        r_idx = idx // ncols
        c_idx = idx % ncols
        ax = axes[r_idx][c_idx]
        _generate_throughput_vs_workload_shape_for_axis(
            ax=ax,
            variant_key=variant_key,
            cells=buckets[variant_key],
            qos_order=qos_order,
            shape_order=shape_order,
        )

    # Hide unused subplot slots so the figure doesn't render empty
    # axes on the right of the last row.
    for idx in range(n_variants, nrows * ncols):
        r_idx = idx // ncols
        c_idx = idx % ncols
        axes[r_idx][c_idx].set_axis_off()

    fig.suptitle("Throughput vs workload shape (per variant, leaves/s)", fontsize=11)

    # Two-strip legend: workload (hatch) above QoS (text legend).
    workload_handles: list[matplotlib.patches.Patch] = []
    for shape in shape_order:
        workload_handles.append(
            matplotlib.patches.Patch(
                facecolor="white",
                edgecolor="black",
                linewidth=0.5,
                hatch=_shape_hatch(shape),
                label=_shape_label(shape),
            )
        )

    qos_handles: list[matplotlib.patches.Patch] = []
    for qos in qos_order:
        label = f"qos{qos}" if qos is not None else "qosNA"
        qos_handles.append(
            matplotlib.patches.Patch(
                facecolor="lightgrey",
                edgecolor="black",
                linewidth=0.3,
                label=label,
            )
        )

    bottom_reserve = min(0.20, legend_band_height / fig_height)
    if workload_handles:
        fig.legend(
            handles=workload_handles,
            loc="lower center",
            bbox_to_anchor=(0.5, 0.05),
            ncol=min(3, len(workload_handles)),
            frameon=True,
            fontsize=8,
            title="Workload profile (fill pattern)",
            title_fontsize=9,
        )
    if qos_handles:
        fig.legend(
            handles=qos_handles,
            loc="lower center",
            bbox_to_anchor=(0.5, 0.005),
            ncol=min(4, len(qos_handles)),
            frameon=True,
            fontsize=8,
            title="QoS (one bar per QoS within each workload group)",
            title_fontsize=9,
        )

    top_reserve = max(0.6, fig_height - 0.4) / fig_height
    fig.subplots_adjust(
        bottom=bottom_reserve,
        top=top_reserve * 0.96,
        left=0.07,
        right=0.98,
        hspace=0.55,
        wspace=0.3,
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    out_path = output_dir / _THROUGHPUT_VS_SHAPE_FILENAME
    fig.savefig(str(out_path), dpi=150)
    plt.close(fig)
    return out_path
