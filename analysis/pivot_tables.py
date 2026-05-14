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
    workload_kind: str  # "scalar-flood" or "max-throughput"

    @property
    def workload_profile(self) -> str:
        """Pivot-column key, e.g. ``1000x100hz`` or ``max``."""
        if self.workload_kind == "max-throughput":
            return "max"
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
# (unbounded workload).
_CANONICAL_COLUMNS: tuple[str, ...] = (
    "1000x100hz",
    "1000x10hz",
    "100x1000hz",
    "100x100hz",
    "100x10hz",
    "10x100hz",
    "10x1000hz",
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
    """

    qos: int
    rows: tuple[tuple[str, str], ...]
    columns: tuple[str, ...]
    cells: dict[tuple[tuple[str, str], str], PivotCell]


def _delivery_pct(result: PerformanceResult) -> float | None:
    """Delivery % = 100 * receives/writes. ``None`` for zero writes.

    Mirrors the formula used in ``tables.format_performance_table`` so
    the pivot is consistent with the existing flat report.
    """
    if result.writes_per_sec <= 0:
        return None
    return 100.0 * result.receives_per_sec / result.writes_per_sec


def build_pivot_tables(results: list[PerformanceResult]) -> list[PivotTable]:
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
    """
    by_qos: dict[int, list[tuple[SpawnIdentity, PerformanceResult]]] = {}
    for r in results:
        identity = parse_spawn_name(r.variant)
        if identity is None:
            continue
        by_qos.setdefault(identity.qos, []).append((identity, r))

    tables: list[PivotTable] = []
    for qos in sorted(by_qos.keys()):
        entries = by_qos[qos]

        observed_rows: list[tuple[str, str]] = []
        seen_rows: set[tuple[str, str]] = set()
        observed_cols: list[str] = []
        seen_cols: set[str] = set()
        for identity, _r in entries:
            row_key = identity.row_key
            if row_key not in seen_rows:
                seen_rows.add(row_key)
                observed_rows.append(row_key)
            col = identity.workload_profile
            if col not in seen_cols:
                seen_cols.add(col)
                observed_cols.append(col)

        rows: list[tuple[str, str]] = [r for r in _CANONICAL_ROWS if r in seen_rows]
        for r in observed_rows:
            if r not in rows:
                rows.append(r)

        columns: list[str] = [c for c in _CANONICAL_COLUMNS if c in seen_cols]
        for c in observed_cols:
            if c not in columns:
                columns.append(c)

        cells: dict[tuple[tuple[str, str], str], PivotCell] = {}
        for identity, r in entries:
            key = (identity.row_key, identity.workload_profile)
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


def _row_label(row_key: tuple[str, str]) -> str:
    family, mode = row_key
    return f"{family}-{mode}"


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
    total_width = _ROW_LABEL_WIDTH + (_CELL_WIDTH + 3) * n_cols + 1

    lines: list[str] = []
    lines.append(f"QoS {table.qos}")
    lines.append("-" * total_width)

    # Header row.
    header = " " * _ROW_LABEL_WIDTH
    for col in table.columns:
        header += " | " + col.ljust(_CELL_WIDTH)
    lines.append(header)
    lines.append("-" * total_width)

    for row_key in table.rows:
        # Build the three sub-lines, accumulating each column's cell
        # rendering into the corresponding line.
        line_a = _row_label(row_key).ljust(_ROW_LABEL_WIDTH)
        line_b = " " * _ROW_LABEL_WIDTH
        line_c = " " * _ROW_LABEL_WIDTH
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


def format_pivot_section(results: list[PerformanceResult]) -> str:
    """Render the full pivot-tables section: one table per QoS level.

    Begins with a section header that documents the 3-sub-cell format
    so the reader does not have to cross-reference the docs while
    scanning the output.
    """
    tables = build_pivot_tables(results)
    if not tables:
        return "Pivot Tables (variant x workload, one per QoS)\n(no data)\n"

    lines: list[str] = []
    lines.append("Pivot Tables (variant x workload, one per QoS)")
    lines.append(
        "Each cell: line 1 = Delivery%, line 2 = Ratio% (receives/expected; "
        "may exceed 100% for multicast loopback), line 3 = latency mean+/-std ms"
    )
    lines.append("")
    for table in tables:
        lines.append(format_pivot_table(table))
        lines.append("")
    return "\n".join(lines)


# --- CSV export --------------------------------------------------------------

# Long-form CSV column order. Keeps the pivot-relevant columns first
# so operators can pivot in Excel/Sheets without re-arranging, then
# appends the existing PerformanceResult columns for completeness.
_CSV_COLUMNS: tuple[str, ...] = (
    "variant",
    "run",
    "family",
    "threading_mode",
    "values_per_tick",
    "tick_rate_hz",
    "qos",
    "workload_kind",
    "delivery_pct",
    "ratio_pct",
    "expected_writes_per_sec",
    "receives_per_sec",
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
    identity = parse_spawn_name(result.variant)
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

    row = {
        "variant": result.variant,
        "run": result.run,
        "family": family,
        "threading_mode": mode,
        "values_per_tick": vpt,
        "tick_rate_hz": hz,
        "qos": qos,
        "workload_kind": workload_kind,
        "delivery_pct": delivery_pct,
        "ratio_pct": result.receives_to_expected_ratio_pct,
        "expected_writes_per_sec": result.expected_writes_per_sec,
        "receives_per_sec": result.receives_per_sec,
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
