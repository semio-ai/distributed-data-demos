"""Pivot tables for benchmark analysis: variant x workload grids per QoS.

Produces a stack of ASCII grids (one per QoS level 1..4). Each grid's
rows are (variant family, threading mode) pairs and each grid's columns
are workload profiles (e.g. ``1000x100hz``, ``100x100hz``, ``max``).
Every cell renders THREE sub-cell lines:

1. ``Delivery%``   -- 100 * receives_per_sec / writes_per_sec (existing
   PerformanceResult-derived metric, same formula as the performance
   table).
2. ``Ratio%``      -- 100 * receives_per_sec / expected_writes_per_sec
   where ``expected_writes_per_sec = tick_rate_hz * values_per_tick``
   parsed from the spawn name. For the unbounded ``max-throughput``
   workload there is no nominal rate; the sub-cell renders ``n/a``.
   For multicast variants where the receiver also gets its own loopback
   writes the ratio can exceed 100% -- this is expected and documented
   in ``metak-shared/ANALYSIS.md``.
3. ``mean +/- std`` ms -- mean and sample std-dev of the per-message
   latency samples already stored on PerformanceResult.

Empty cells (no spawn for this (variant_family, mode, vpt, hz, qos)
combination, or unsupported by the variant family -- e.g. quic-single)
render as a triple of dashes so the grid stays rectangular.

The spawn-name parser is the entry point used by the rest of the
analysis pipeline to pivot any PerformanceResult onto the
(family, mode, vpt, hz, qos, workload_kind) coordinate system. It is
deliberately tolerant: any name that does not match the canonical
``<family>-<vpt>x<hz>hz-qos<N>-<mode>`` (or ``<family>-max-qos<N>-<mode>``)
shape returns ``None`` and the caller is expected to skip that
PerformanceResult from the pivot.
"""

from __future__ import annotations

import csv
import math
import re
from dataclasses import dataclass
from io import StringIO
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    # Avoid an import cycle at module-load time: performance.py imports
    # ``parse_spawn_name`` from this module to compute its new fields,
    # and at type-check time we still want the dataclass references.
    from performance import PerformanceResult


# Canonical spawn-name shapes (T14.8 expansion):
#
#   <family>-<vpt>x<hz>hz-qos<N>-<mode>      scalar-flood
#   <family>-max-qos<N>-<mode>               max-throughput
#
# ``<family>`` is one of the lowercase identifiers from the canonical
# config (custom-udp, hybrid, quic, zenoh, websocket, webrtc). Hyphens
# in the family name make a naive split on ``-`` ambiguous, so we use a
# regex that anchors on the ``-qos<N>-<mode>`` suffix and treats the
# leading run as the family + workload selector.
_FAMILIES: tuple[str, ...] = (
    "custom-udp",
    "hybrid",
    "quic",
    "zenoh",
    "websocket",
    "webrtc",
)

# Pre-compile one regex per family so the alternation does not back-
# track over family boundaries. The ``workload`` group either matches
# ``<vpt>x<hz>hz`` (scalar-flood) or the literal ``max`` token.
_SPAWN_RE = re.compile(
    r"^(?P<family>"
    + "|".join(re.escape(f) for f in _FAMILIES)
    + r")"
    + r"-(?P<workload>(?:\d+x\d+hz)|max)"
    + r"-qos(?P<qos>\d+)"
    + r"-(?P<mode>single|multi)$"
)

_WORKLOAD_RE = re.compile(r"^(?P<vpt>\d+)x(?P<hz>\d+)hz$")


@dataclass(frozen=True)
class SpawnIdentity:
    """Parsed components of a canonical spawn name.

    For the ``max-throughput`` workload, ``tick_rate_hz`` and
    ``values_per_tick`` are 0; the ``workload_kind`` discriminator is
    the load-bearing field for callers who need to skip the ratio
    metric on that workload.
    """

    family: str
    values_per_tick: int
    tick_rate_hz: int
    qos: int
    mode: str
    # ``workload_kind`` discriminates how to render the pivot column:
    # ``"scalar-flood"`` uses ``<vpt>x<hz>hz``, ``"max-throughput"``
    # uses the literal ``max``, and ``"workload-profile"`` (T19.9)
    # uses the user-facing workload-profile token derived from the
    # data-attached shape (``scalar-flood`` / ``block-flood`` /
    # ``mixed-types``). The latter is produced by
    # :func:`resolve_pivot_identity` for unsuffixed E19-style names.
    workload_kind: str
    # T19.9: when ``workload_kind == "workload-profile"``, this carries
    # the user-facing workload-profile name (e.g. ``"block-flood"``)
    # derived from the data-attached ``shape``. Empty string for the
    # legacy scalar-flood / max-throughput kinds.
    workload_profile_override: str = ""

    @property
    def workload_profile(self) -> str:
        """Pivot-column key, e.g. ``1000x100hz`` or ``max``."""
        if self.workload_kind == "max-throughput":
            return "max"
        if self.workload_kind == "workload-profile":
            return self.workload_profile_override or "workload-profile"
        return f"{self.values_per_tick}x{self.tick_rate_hz}hz"

    @property
    def row_key(self) -> tuple[str, str]:
        """Pivot-row key: (family, threading_mode)."""
        return self.family, self.mode


def parse_spawn_name(name: str) -> SpawnIdentity | None:
    """Parse a canonical spawn name into its pivot coordinates.

    Returns ``None`` when the name does not match the canonical T14.8
    shape (e.g. clock-sync shards, legacy logs predating QoS expansion).
    The caller is expected to skip non-matching PerformanceResults from
    the pivot tables -- they still appear in the existing flat
    Performance Report.

    Examples that match::

        custom-udp-1000x100hz-qos4-multi
        custom-udp-max-qos4-multi
        websocket-100x10hz-qos3-single
        webrtc-1000x10hz-qos2-multi

    Examples that do NOT match (return ``None``)::

        custom-udp                       (no QoS / mode suffix)
        zenoh-1000x10hz                  (no QoS / mode suffix)
        clock-sync                       (not a variant spawn)

    For unsuffixed E19-style names (e.g. ``dummy-block-flood``) the
    pivot builder uses :func:`resolve_pivot_identity` instead, which
    falls back to ``PerformanceResult`` fields (``qos``,
    ``threading_mode``, ``shape``) when this parser returns ``None``.
    """
    m = _SPAWN_RE.match(name)
    if m is None:
        return None
    workload = m["workload"]
    if workload == "max":
        return SpawnIdentity(
            family=m["family"],
            values_per_tick=0,
            tick_rate_hz=0,
            qos=int(m["qos"]),
            mode=m["mode"],
            workload_kind="max-throughput",
        )
    wm = _WORKLOAD_RE.match(workload)
    if wm is None:
        # Defensive: the outer regex only accepts "<vpt>x<hz>hz" or
        # "max", so this branch is unreachable in practice.
        return None
    return SpawnIdentity(
        family=m["family"],
        values_per_tick=int(wm["vpt"]),
        tick_rate_hz=int(wm["hz"]),
        qos=int(m["qos"]),
        mode=m["mode"],
        workload_kind="scalar-flood",
    )


# T19.9: workload-only / unsuffixed spawn names (e.g. dummy-block-flood,
# dummy-mixed-types) do NOT carry the canonical ``-<vpt>x<hz>hz-qos<N>-<mode>``
# suffix, so :func:`parse_spawn_name` returns ``None`` on them. For the
# pivot tables we still want a usable row identity in that case --
# falling back to the dataclass-attached fields (``qos``,
# ``threading_mode``, ``shape``) which the polars pipeline populates
# directly from the per-event data columns. The "family" used for the
# pivot row falls back to the full variant name (so a name like
# ``dummy-block-flood`` lands on a ``dummy-block-flood`` row instead of
# collapsing to ``n/a``); the workload-profile column maps from the
# data-derived shape via :data:`_SHAPE_TO_WORKLOAD_PROFILE`.
_SHAPE_TO_WORKLOAD_PROFILE: dict[str, str] = {
    "scalar": "scalar-flood",
    "array": "block-flood",
    "struct": "mixed-types",
}


def resolve_pivot_identity(result: PerformanceResult) -> SpawnIdentity | None:
    """Return a SpawnIdentity for ``result``, preferring data over name parsing.

    Resolution order (T19.9):

    1. Try :func:`parse_spawn_name` on ``result.variant``; if the name
       matches the canonical ``<family>-<vpt>x<hz>hz-qos<N>-<mode>``
       shape (or the ``-max-qos<N>-<mode>`` variant), use that
       identity verbatim. This preserves pre-T19.9 behaviour for every
       existing canonical config.
    2. Otherwise build an identity from the dataclass fields:
       ``result.qos`` (read from the ``qos`` column on ``write``
       events), ``result.threading_mode`` (read from ``connected``
       events), and ``result.shape`` (the dominant workload shape).
       The family is the full ``result.variant`` so the pivot row
       label reads e.g. ``dummy-block-flood-single`` instead of
       collapsing to ``n/a``. The workload-profile column is derived
       from the shape via :data:`_SHAPE_TO_WORKLOAD_PROFILE`.

    Returns ``None`` only when both paths fail -- specifically when the
    canonical parser fails AND ``result.qos`` is also ``None`` (which
    means the underlying data has no ``write`` events with a populated
    qos column). Such rows still appear in the flat Performance Report;
    they just can't be placed on a per-QoS pivot.
    """
    canonical = parse_spawn_name(result.variant)
    if canonical is not None:
        return canonical
    if result.qos is None:
        # Data has no qos column populated either -- nothing to pivot on.
        return None
    # Workload-profile resolution (T19.9): if the variant name itself
    # contains a canonical workload-profile token (e.g.
    # ``dummy-block-flood``), use that token directly -- it's more
    # accurate than the shape-derived fallback, which collapses
    # ``mixed-types`` onto ``block-flood`` whenever the dominant shape
    # happens to be ``array``. The shape-derived value is the second-
    # choice fallback for spawn names that carry no workload token at
    # all.
    workload_profile = _workload_from_variant_name(result.variant)
    if workload_profile is None:
        shape = result.shape if result.shape else "scalar"
        workload_profile = _SHAPE_TO_WORKLOAD_PROFILE.get(shape, shape)
    return SpawnIdentity(
        family=result.variant,
        values_per_tick=0,
        tick_rate_hz=0,
        qos=int(result.qos),
        mode=result.threading_mode,
        # ``workload_kind`` is the discriminator that controls whether
        # the ratio sub-cell renders ``n/a``. Data-derived rows do NOT
        # have a nominal rate (vpt / hz are zero) and the receives-to-
        # expected ratio is computed upstream as ``None`` for them, so
        # marking the kind as ``"workload-profile"`` (a fresh sentinel)
        # lets renderers / CSV exporters distinguish "no nominal rate
        # because max-throughput" from "no nominal rate because the
        # spawn name didn't encode one". Existing callers that branch
        # on ``"max-throughput"`` continue to fall into the else-branch
        # so behaviour is unchanged for them.
        workload_kind="workload-profile",
        workload_profile_override=workload_profile,
    )


# Canonical workload-profile tokens that may appear in unsuffixed E19
# variant names (e.g. ``dummy-block-flood``). Order is significant: the
# longest prefix is checked first so ``mixed-types`` is matched before
# ``mixed`` (defensive against future tokens).
_WORKLOAD_PROFILE_TOKENS: tuple[str, ...] = (
    "scalar-flood",
    "block-flood",
    "mixed-types",
    "max-throughput",
)


def _workload_from_variant_name(variant: str) -> str | None:
    """Extract a workload-profile token from a variant name, if present.

    Returns one of :data:`_WORKLOAD_PROFILE_TOKENS` when the variant
    name ends with the token (preceded by ``-``), e.g.
    ``dummy-block-flood`` -> ``"block-flood"``. Returns ``None`` when
    no canonical workload-profile token is recognisable -- the caller
    then falls back to the shape-derived mapping.
    """
    for token in _WORKLOAD_PROFILE_TOKENS:
        if variant == token or variant.endswith(f"-{token}"):
            return token
    return None


# Canonical row order across all pivot tables. Asymmetric: families
# that only support ``multi`` (quic, zenoh, webrtc) get one row, the
# others get two rows (single + multi). zenoh has a Single-mode entry
# per T14.9 but is multi-only in the canonical config, so we keep
# zenoh-single OUT of the canonical row order. If a real spawn name
# carries zenoh-single (or any other family/mode pair not listed here)
# the builder appends it after the canonical rows so the data is still
# visible.
_CANONICAL_ROWS: tuple[tuple[str, str], ...] = (
    ("custom-udp", "single"),
    ("custom-udp", "multi"),
    ("hybrid", "single"),
    ("hybrid", "multi"),
    ("websocket", "single"),
    ("websocket", "multi"),
    ("quic", "multi"),
    ("webrtc", "multi"),
    ("zenoh", "multi"),
)

# Canonical column order. ``max`` last because it is the outlier
# (unbounded workload). The E19 workload-profile tokens
# (``scalar-flood`` / ``block-flood`` / ``mixed-types``) trail the
# legacy <vpt>x<hz>hz columns and precede ``max`` per T19.9 so the
# pivot reads left-to-right in the canonical workload progression
# (scalar -> block -> mixed -> max).
_CANONICAL_COLUMNS: tuple[str, ...] = (
    "1000x100hz",
    "1000x10hz",
    "100x1000hz",
    "100x100hz",
    "100x10hz",
    "10x100hz",
    "10x1000hz",
    "scalar-flood",
    "block-flood",
    "mixed-types",
    "max",
)


@dataclass(frozen=True)
class PivotCell:
    """The three sub-cell values for one (row, column) intersection.

    ``delivery_pct`` is ``None`` when no spawn populated this cell.
    ``ratio_pct`` is ``None`` either when the cell is unpopulated OR
    when the spawn is a ``max-throughput`` workload (no nominal rate).
    ``latency_mean_ms`` and ``latency_std_ms`` are ``nan`` when the
    cell is unpopulated or when the spawn had no delivery samples.
    """

    delivery_pct: float | None
    ratio_pct: float | None
    latency_mean_ms: float
    latency_std_ms: float


@dataclass(frozen=True)
class PivotTable:
    """A single QoS-level pivot table.

    ``rows`` and ``columns`` are the canonical orderings (extended with
    any non-canonical rows / columns observed in the data).
    ``cells[(row_key, column_key)]`` is the per-cell triple.

    Row-key shape:

    - Default mode (``build_pivot_tables(results)``): the row key is
      the 2-tuple ``(family, threading_mode)``.
    - Shape-aware mode (``build_pivot_tables(results,
      include_shape=True)`` -- T19.6): the row key is the 3-tuple
      ``(family, threading_mode, shape)`` where ``shape`` is
      ``"scalar"`` / ``"array"`` / ``"struct"``.

    Renderers introspect the tuple length so callers don't need to
    pick a different format function per mode.
    """

    qos: int
    rows: tuple[tuple, ...]
    columns: tuple[str, ...]
    cells: dict[tuple[tuple, str], PivotCell]


def _delivery_pct(result: PerformanceResult) -> float | None:
    """Delivery % = 100 * receives/writes. ``None`` for zero writes.

    Mirrors the formula used in ``tables.format_performance_table`` so
    the pivot is consistent with the existing flat report.
    """
    if result.writes_per_sec <= 0:
        return None
    return 100.0 * result.receives_per_sec / result.writes_per_sec


def build_pivot_tables(
    results: list[PerformanceResult],
    *,
    include_shape: bool = False,
) -> list[PivotTable]:
    """Build one PivotTable per QoS level present in ``results``.

    Spawns whose name does not parse via ``parse_spawn_name`` are
    silently skipped -- they still appear in the flat Performance
    Report. The returned list is sorted by QoS level ascending.

    Rows and columns are the canonical orderings extended with any
    family/mode or workload-profile observed in the data but missing
    from the canonical lists. This keeps the tables usable when run
    against partial datasets (e.g. a smoke config that only spawns
    websocket variants) AND when run against future configs that add
    new workload profiles or threading-mode combinations.

    E19 / T19.6: when ``include_shape`` is ``True``, the row key is
    extended with the workload-shape value (``"scalar"`` /
    ``"array"`` / ``"struct"``) read from ``PerformanceResult.shape``.
    A single (family, mode) pair that ran multiple shape profiles
    therefore produces one row per shape. Default behaviour
    (``include_shape=False``) is unchanged so existing callers, dumps
    and tests see the pre-E19 (family, mode) row grouping.

    The row-key tuple shape is the documented coupling point: in the
    default mode it stays ``(family, mode)``; in shape-aware mode it
    becomes ``(family, mode, shape)``. Renderers downstream
    (:func:`format_pivot_table`, :func:`_row_label`) handle both via
    tuple-length introspection so no caller has to update.
    """
    by_qos: dict[int, list[tuple[SpawnIdentity, PerformanceResult]]] = {}
    for r in results:
        # T19.9: prefer the data-derived identity so unsuffixed E19-style
        # names (e.g. ``dummy-block-flood``) still land on the correct
        # QoS bucket. :func:`resolve_pivot_identity` falls back to the
        # canonical name parser when the name matches.
        identity = resolve_pivot_identity(r)
        if identity is None:
            continue
        by_qos.setdefault(identity.qos, []).append((identity, r))

    tables: list[PivotTable] = []
    for qos in sorted(by_qos.keys()):
        entries = by_qos[qos]

        observed_rows: list[tuple] = []
        seen_rows: set[tuple] = set()
        observed_cols: list[str] = []
        seen_cols: set[str] = set()
        for identity, r in entries:
            if include_shape:
                shape = r.shape if r.shape else "scalar"
                row_key: tuple = (identity.family, identity.mode, shape)
            else:
                row_key = identity.row_key
            if row_key not in seen_rows:
                seen_rows.add(row_key)
                observed_rows.append(row_key)
            col = identity.workload_profile
            if col not in seen_cols:
                seen_cols.add(col)
                observed_cols.append(col)

        if include_shape:
            # Shape-aware mode: expand each canonical (family, mode)
            # row into one row per observed shape, preserving the
            # canonical (family, mode) ordering and putting shapes in
            # the locked scalar / array / struct order from the
            # workload-profile glossary.
            shape_order_local = ("scalar", "array", "struct")
            rows: list[tuple] = []
            for fm in _CANONICAL_ROWS:
                for shape in shape_order_local:
                    candidate = (fm[0], fm[1], shape)
                    if candidate in seen_rows:
                        rows.append(candidate)
            for r in observed_rows:
                if r not in rows:
                    rows.append(r)
        else:
            rows = [r for r in _CANONICAL_ROWS if r in seen_rows]
            for r in observed_rows:
                if r not in rows:
                    rows.append(r)

        columns: list[str] = [c for c in _CANONICAL_COLUMNS if c in seen_cols]
        for c in observed_cols:
            if c not in columns:
                columns.append(c)

        cells: dict[tuple[tuple, str], PivotCell] = {}
        for identity, r in entries:
            if include_shape:
                shape = r.shape if r.shape else "scalar"
                row_key = (identity.family, identity.mode, shape)
            else:
                row_key = identity.row_key
            key = (row_key, identity.workload_profile)
            # If two runs land in the same cell (e.g. multiple ``run``
            # values for the same spawn), take the LAST one. In the
            # canonical dataset each (variant, run) pair is unique, so
            # this branch is rarely exercised; for repeat runs the
            # operator can filter on ``run`` upstream.
            cells[key] = PivotCell(
                delivery_pct=_delivery_pct(r),
                ratio_pct=r.receives_to_expected_ratio_pct,
                latency_mean_ms=r.latency_mean_ms,
                latency_std_ms=r.latency_std_ms,
            )

        tables.append(
            PivotTable(
                qos=qos,
                rows=tuple(rows),
                columns=tuple(columns),
                cells=cells,
            )
        )

    return tables


# --- Rendering ---------------------------------------------------------------

# Fixed-width per-cell rendering. 14 chars accommodates the longest
# expected formatted value (e.g. "1234.5+/-99.9" = 13 chars).
_CELL_WIDTH: int = 14
# Row-label width: longest canonical row is "custom-udp-single" (17).
_ROW_LABEL_WIDTH: int = 18
# Sub-cell empty marker. Plain ASCII so the table renders on any
# terminal without Unicode font support.
_EMPTY: str = "-"
_NA: str = "n/a"


def _fmt_pct_cell(value: float | None) -> str:
    """Format a percentage value for a sub-cell, or ``-`` if ``None``."""
    if value is None:
        return _EMPTY
    if math.isnan(value):
        return _EMPTY
    return f"{value:.1f}%"


def _fmt_ratio_cell(value: float | None) -> str:
    """Format the ratio sub-cell; ``n/a`` distinguishes max-throughput.

    The Ratio% sub-cell intentionally uses ``n/a`` (not ``-``) for the
    max-throughput workload so the reader can tell at a glance that the
    cell is populated by a real spawn but the metric is undefined.
    """
    if value is None:
        return _NA
    if math.isnan(value):
        return _NA
    return f"{value:.1f}%"


def _fmt_latency_cell(mean_ms: float, std_ms: float) -> str:
    """Format ``mean+/-std`` in ms, or ``-`` if unavailable."""
    if math.isnan(mean_ms) or math.isnan(std_ms):
        return _EMPTY
    if mean_ms < 1.0:
        mean_str = f"{mean_ms:.2f}"
    else:
        mean_str = f"{mean_ms:.1f}"
    if std_ms < 1.0:
        std_str = f"{std_ms:.2f}"
    else:
        std_str = f"{std_ms:.1f}"
    return f"{mean_str}+/-{std_str}ms"


def _row_label(row_key: tuple) -> str:
    """Format a row label.

    Accepts either the default 2-tuple ``(family, mode)`` or the
    shape-aware 3-tuple ``(family, mode, shape)`` (E19 / T19.6). The
    shape token is appended with a slash so the resulting label reads
    e.g. ``custom-udp-multi/array``.
    """
    if len(row_key) == 2:
        family, mode = row_key
        return f"{family}-{mode}"
    family, mode, shape = row_key
    return f"{family}-{mode}/{shape}"


def format_pivot_table(table: PivotTable) -> str:
    """Render one PivotTable as a fixed-width ASCII grid.

    Each cell is rendered across THREE lines so the row height is 3 +
    a separator line. The grid header carries the QoS level so a stack
    of four tables in a single output is unambiguous.

    Layout::

        QoS <N>
        ----...
        <row label>        | col1         | col2         | ...
                           | Delivery%    | Delivery%    |
                           | Ratio%       | Ratio%       |
                           | mean+/-std   | mean+/-std   |
        ----...

    The column headers are followed by a separator line. The cell
    sub-cells are separated by ``|`` so the grid is unambiguous even
    when a cell value is empty (``-``).
    """
    n_cols = len(table.columns)
    # T19.6: shape-aware row keys (3-tuple) produce wider labels like
    # ``custom-udp-multi/struct`` (~23 chars). Widen the row-label
    # column dynamically so those labels still fit; the default 2-tuple
    # row keys fall comfortably within the original 18-char column.
    row_label_width = _ROW_LABEL_WIDTH
    for row_key in table.rows:
        candidate = len(_row_label(row_key)) + 1
        if candidate > row_label_width:
            row_label_width = candidate
    total_width = row_label_width + (_CELL_WIDTH + 3) * n_cols + 1

    lines: list[str] = []
    lines.append(f"QoS {table.qos}")
    lines.append("-" * total_width)

    # Header row.
    header = " " * row_label_width
    for col in table.columns:
        header += " | " + col.ljust(_CELL_WIDTH)
    lines.append(header)
    lines.append("-" * total_width)

    for row_key in table.rows:
        # Build the three sub-lines, accumulating each column's cell
        # rendering into the corresponding line.
        line_a = _row_label(row_key).ljust(row_label_width)
        line_b = " " * row_label_width
        line_c = " " * row_label_width
        for col in table.columns:
            cell = table.cells.get((row_key, col))
            if cell is None:
                # Unpopulated cell: render a triple of dashes.
                a = _EMPTY
                b = _EMPTY
                c = _EMPTY
            else:
                a = _fmt_pct_cell(cell.delivery_pct)
                b = _fmt_ratio_cell(cell.ratio_pct)
                c = _fmt_latency_cell(cell.latency_mean_ms, cell.latency_std_ms)
            line_a += " | " + a.ljust(_CELL_WIDTH)
            line_b += " | " + b.ljust(_CELL_WIDTH)
            line_c += " | " + c.ljust(_CELL_WIDTH)
        lines.append(line_a)
        lines.append(line_b)
        lines.append(line_c)
        lines.append("-" * total_width)

    return "\n".join(lines)


_PIVOT_SECTION_HEADER: str = "Pivot Tables (variant x workload, one per QoS)"
_PIVOT_SECTION_LEGEND: str = (
    "Each cell: line 1 = Delivery%, line 2 = Ratio% (receives/expected; "
    "may exceed 100% for multicast loopback), line 3 = latency mean+/-std ms"
)


def format_pivot_for_qos(
    results: list[PerformanceResult],
    qos: int,
    *,
    include_shape: bool = False,
) -> str:
    """Render the pivot block for a single QoS level.

    Returns the same header + legend used by :func:`format_pivot_section`
    followed by the one :class:`PivotTable` whose ``qos`` matches the
    argument. When no spawn in ``results`` matches the requested QoS
    level a placeholder ``(no data)`` block is returned so the caller
    (e.g. the ``--dump`` writer) always produces a well-formed file.

    E19 / T19.6: pass ``include_shape=True`` to expand each
    (family, mode) row into one row per observed workload shape.
    Default (``False``) preserves the pre-E19 (family, mode) row
    grouping.
    """
    tables = build_pivot_tables(results, include_shape=include_shape)
    table = next((t for t in tables if t.qos == qos), None)

    lines: list[str] = []
    lines.append(_PIVOT_SECTION_HEADER)
    lines.append(_PIVOT_SECTION_LEGEND)
    lines.append("")
    if table is None:
        lines.append(f"QoS {qos}")
        lines.append("(no data)")
        lines.append("")
        return "\n".join(lines)
    lines.append(format_pivot_table(table))
    lines.append("")
    return "\n".join(lines)


def format_pivot_section(
    results: list[PerformanceResult],
    *,
    include_shape: bool = False,
) -> str:
    """Render the full pivot-tables section: one table per QoS level.

    Begins with a section header that documents the 3-sub-cell format
    so the reader does not have to cross-reference the docs while
    scanning the output.

    E19 / T19.6: pass ``include_shape=True`` to expand each
    (family, mode) row into one row per observed workload shape.
    """
    tables = build_pivot_tables(results, include_shape=include_shape)
    if not tables:
        return "Pivot Tables (variant x workload, one per QoS)\n(no data)\n"

    lines: list[str] = []
    lines.append(_PIVOT_SECTION_HEADER)
    lines.append(_PIVOT_SECTION_LEGEND)
    lines.append("")
    for table in tables:
        lines.append(format_pivot_table(table))
        lines.append("")
    return "\n".join(lines)


# --- CSV export --------------------------------------------------------------

# Long-form CSV column order. Keeps the pivot-relevant columns first
# so operators can pivot in Excel/Sheets without re-arranging, then
# appends the existing PerformanceResult columns for completeness.
#
# E19 / T19.6: ``workload`` (the canonical user-facing workload-
# profile token) and ``shape`` (the analyzer-internal shape value)
# are surfaced as optional pivot dimensions so an external pivot can
# slice on the workload-shape axis. Both columns are unconditionally
# populated -- defaults are ``"scalar-flood"`` / ``"scalar"`` for
# pre-E19 data per the api-contracts backward-compat rule -- so the
# CSV stays stable across datasets that mix legacy and E19+ spawns.
# ``leaves_per_sec`` and ``bytes_per_sec`` are surfaced alongside
# the existing ``receives_per_sec`` so a spreadsheet pivot can render
# the canonical cross-workload comparable metric (``leaves_per_sec``)
# without re-deriving it.
_CSV_COLUMNS: tuple[str, ...] = (
    "variant",
    "run",
    "family",
    "threading_mode",
    "values_per_tick",
    "tick_rate_hz",
    "qos",
    "workload_kind",
    "workload",
    "shape",
    "delivery_pct",
    "ratio_pct",
    "expected_writes_per_sec",
    "receives_per_sec",
    "leaves_per_sec",
    "bytes_per_sec",
    "writes_per_sec",
    "latency_mean_ms",
    "latency_std_ms",
    "latency_p50_ms",
    "latency_p95_ms",
    "latency_p99_ms",
    "latency_max_ms",
    "jitter_ms",
    "jitter_p95_ms",
    "loss_pct",
    "connect_mean_ms",
    "connect_max_ms",
    "late_receives",
    "late_receives_tail_count",
    "late_receives_tail_pct",
    "has_uncorrected_latency",
)

# Mapping from the analyzer-internal ``shape`` token to the user-facing
# workload-profile name used by BENCHMARK.md § 6 and the CLI variant
# matrix. Kept here -- not duplicated in plots.py -- because the CSV
# is the cleanest place to surface a stable column name that
# downstream pivot tooling can rely on.
_SHAPE_TO_WORKLOAD: dict[str, str] = {
    "scalar": "scalar-flood",
    "array": "block-flood",
    "struct": "mixed-types",
}


def _csv_value(value: object) -> str:
    """Render a value for the CSV cell.

    ``None`` -> empty string (sheet-friendly). ``nan`` -> empty string
    (same -- distinguishes "no data" from a real zero). Other floats
    use the platform repr; csv.writer handles quoting.
    """
    if value is None:
        return ""
    if isinstance(value, float) and math.isnan(value):
        return ""
    if isinstance(value, bool):
        # Render booleans as ``true`` / ``false`` so the column is
        # unambiguous in a spreadsheet.
        return "true" if value else "false"
    return str(value)


def _csv_row(result: PerformanceResult) -> dict[str, str]:
    """Build the per-(variant, run) CSV row dict."""
    # T19.9: use the data-aware resolver so unsuffixed E19-style spawn
    # names still get populated qos / mode / workload_kind columns.
    identity = resolve_pivot_identity(result)
    if identity is not None:
        family = identity.family
        vpt = identity.values_per_tick
        hz = identity.tick_rate_hz
        qos: int | None = identity.qos
        mode = identity.mode
        workload_kind = identity.workload_kind
    else:
        family = ""
        vpt = 0
        hz = 0
        qos = None
        mode = result.threading_mode
        workload_kind = ""

    delivery_pct = _delivery_pct(result)

    # E19 / T19.6: surface workload + shape as first-class CSV
    # columns. ``shape`` defaults to ``"scalar"`` for legacy data per
    # the api-contracts backward-compat rule; the matching workload
    # name falls back via :data:`_SHAPE_TO_WORKLOAD` so the column
    # value remains the user-facing profile token from BENCHMARK.md
    # § 6 rather than the analyzer-internal shape value.
    shape_value = result.shape if result.shape else "scalar"
    workload_value = _SHAPE_TO_WORKLOAD.get(shape_value, shape_value)

    row = {
        "variant": result.variant,
        "run": result.run,
        "family": family,
        "threading_mode": mode,
        "values_per_tick": vpt,
        "tick_rate_hz": hz,
        "qos": qos,
        "workload_kind": workload_kind,
        "workload": workload_value,
        "shape": shape_value,
        "delivery_pct": delivery_pct,
        "ratio_pct": result.receives_to_expected_ratio_pct,
        "expected_writes_per_sec": result.expected_writes_per_sec,
        "receives_per_sec": result.receives_per_sec,
        "leaves_per_sec": result.leaves_per_sec,
        "bytes_per_sec": result.bytes_per_sec,
        "writes_per_sec": result.writes_per_sec,
        "latency_mean_ms": result.latency_mean_ms,
        "latency_std_ms": result.latency_std_ms,
        "latency_p50_ms": result.latency_p50_ms,
        "latency_p95_ms": result.latency_p95_ms,
        "latency_p99_ms": result.latency_p99_ms,
        "latency_max_ms": result.latency_max_ms,
        "jitter_ms": result.jitter_ms,
        "jitter_p95_ms": result.jitter_p95_ms,
        "loss_pct": result.loss_pct,
        "connect_mean_ms": result.connect_mean_ms,
        "connect_max_ms": result.connect_max_ms,
        "late_receives": result.late_receives,
        "late_receives_tail_count": result.late_receives_tail_count,
        "late_receives_tail_pct": result.late_receives_tail_pct,
        "has_uncorrected_latency": result.has_uncorrected_latency,
    }
    return {k: _csv_value(v) for k, v in row.items()}


def export_csv(results: list[PerformanceResult]) -> str:
    """Serialize ``results`` to a long-form CSV string.

    One row per (variant, run); columns documented in ``_CSV_COLUMNS``.
    Rows are emitted in the input order (which is the per-group order
    from ``analyze.run_analysis`` -- i.e. variant ASC, run ASC). The
    header row is always emitted, even when ``results`` is empty, so
    the file is well-formed CSV in every case.
    """
    buf = StringIO()
    writer = csv.DictWriter(buf, fieldnames=list(_CSV_COLUMNS))
    writer.writeheader()
    for r in results:
        writer.writerow(_csv_row(r))
    return buf.getvalue()
