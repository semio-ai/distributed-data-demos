"""Tests for the E19 / T19.6 workload-shape plot additions.

Three slices of the locked spec are pinned here:

1. **Restructured ``comparison-qos`` chart** stacks the throughput
   subplot above the latency subplot (vertical 2x1 layout) and
   subdivides every per-variant slot by ``(shape, threading_mode)`` --
   shape distinguished by ``hatch``, threading_mode by colour.
2. **New ``throughput_vs_workload_shape`` chart** renders a per-variant
   subplot grid with workload-profile on the x-axis and
   ``leaves_per_sec`` on the y-axis.
3. **Hatch / colour conventions** are stable across both charts (a
   visual-regression guard so an accidental palette swap is caught
   loudly).
"""

from __future__ import annotations

from pathlib import Path

import pytest

try:
    import matplotlib  # noqa: F401
    import matplotlib.pyplot as plt  # noqa: F401

    HAS_MATPLOTLIB = True
except ImportError:
    HAS_MATPLOTLIB = False

from performance import PerformanceResult

pytestmark = pytest.mark.skipif(
    not HAS_MATPLOTLIB,
    reason="matplotlib not installed",
)


def _make_result(
    variant: str,
    *,
    run: str = "run01",
    shape: str = "scalar",
    leaves_per_sec: float = 100.0,
    receives_per_sec: float = 100.0,
    writes_per_sec: float = 100.0,
    p50: float = 1.0,
    p95: float = 5.0,
    p99: float = 10.0,
) -> PerformanceResult:
    """Build a PerformanceResult tagged with a workload shape."""
    return PerformanceResult(
        variant=variant,
        run=run,
        connect_mean_ms=1.0,
        connect_max_ms=2.0,
        latency_p50_ms=p50,
        latency_p95_ms=p95,
        latency_p99_ms=p99,
        latency_max_ms=p99 + 1.0,
        writes_per_sec=writes_per_sec,
        receives_per_sec=receives_per_sec,
        jitter_ms=0.1,
        jitter_p95_ms=0.2,
        loss_pct=0.0,
        latency_samples_ms=[],
        shape=shape,
        leaves_per_sec=leaves_per_sec,
        ops_per_sec=receives_per_sec,
        bytes_per_sec=receives_per_sec * 8.0,
    )


def _three_workload_fixture() -> list[PerformanceResult]:
    """Three results for the same variant slot, one per workload profile.

    The locked T19.6 fixture: a single (transport, workload, qos, mode)
    slot that observed all three workload profiles. Used by the smoke
    + visual-regression tests below.
    """
    return [
        _make_result(
            "custom-udp-10x100hz-qos1-multi",
            run="scalar",
            shape="scalar",
            leaves_per_sec=1000.0,
            receives_per_sec=1000.0,
        ),
        _make_result(
            "custom-udp-10x100hz-qos1-multi",
            run="block",
            shape="array",
            leaves_per_sec=100_000.0,
            receives_per_sec=1000.0,
        ),
        _make_result(
            "custom-udp-10x100hz-qos1-multi",
            run="mixed",
            shape="struct",
            leaves_per_sec=50_000.0,
            receives_per_sec=1000.0,
        ),
    ]


class TestWorkloadHatchPalette:
    """Workload-profile hatch mapping is stable and documents the spec."""

    def test_scalar_is_solid(self) -> None:
        from plots import _WORKLOAD_HATCHES, _shape_hatch

        assert _WORKLOAD_HATCHES["scalar"] == ""
        assert _shape_hatch("scalar") == ""

    def test_array_is_horizontal_line_hatch(self) -> None:
        from plots import _WORKLOAD_HATCHES, _shape_hatch

        # The locked spec picks ``"---"`` for density / legibility at
        # ~30 px per bar half. The single character ``"-"`` is also
        # valid per the spec text -- the load-bearing assertion is that
        # the hatch starts with ``-`` so the workload reads as
        # horizontal lines.
        assert _WORKLOAD_HATCHES["array"].startswith("-")
        assert _shape_hatch("array").startswith("-")

    def test_struct_is_checkered_hatch(self) -> None:
        from plots import _WORKLOAD_HATCHES, _shape_hatch

        # The locked spec picks ``"x"`` (single crosshatch) over ``"+"``
        # because at small bar widths ``"+"`` visually fuses into
        # ``"---"`` and the distinction is lost. The load-bearing
        # assertion is that the hatch contains ``x``.
        assert "x" in _WORKLOAD_HATCHES["struct"]
        assert "x" in _shape_hatch("struct")

    def test_unknown_shape_falls_back_to_solid(self) -> None:
        from plots import _shape_hatch

        assert _shape_hatch("nonsense") == ""
        assert _shape_hatch(None) == ""

    def test_labels_use_workload_profile_vocabulary(self) -> None:
        """Legend labels show the BENCHMARK.md profile names, not shape tokens."""
        from plots import _shape_label

        assert _shape_label("scalar") == "scalar-flood"
        assert _shape_label("array") == "block-flood"
        assert _shape_label("struct") == "mixed-types"

    def test_shape_sort_order(self) -> None:
        from plots import _shape_sort_key

        ordered = sorted(["struct", "scalar", "array"], key=_shape_sort_key)
        assert ordered == ["scalar", "array", "struct"]


class TestComparisonQosVerticalLayout:
    """Restructured ``comparison-qos`` chart: vertical 2-row layout."""

    def _render(self, results, tmp_path: Path):
        import matplotlib.pyplot as plt

        import plots as plots_module

        captured: list = []
        original_close = plt.close

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_comparison_plot(results, tmp_path)
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured, "expected at least one figure"
        return captured

    def test_throughput_subplot_above_latency_subplot(self, tmp_path: Path) -> None:
        """T19.6: the two metric subplots stack vertically (top=tp, bottom=lat)."""
        results = _three_workload_fixture()
        captured = self._render(results, tmp_path)
        fig = captured[-1]
        # Identify by ylabel: receives/s on top, latency on bottom.
        tp_ax = next(ax for ax in fig.axes if "receives/s" in (ax.get_ylabel() or ""))
        lat_ax = next(
            ax for ax in fig.axes if "latency" in (ax.get_ylabel() or "").lower()
        )
        tp_pos = tp_ax.get_position()
        lat_pos = lat_ax.get_position()
        # The throughput axis's bottom edge must sit ABOVE the latency
        # axis's top edge (vertical stack), not overlap horizontally.
        assert tp_pos.y0 > lat_pos.y1, (
            "expected vertical stack: throughput axis above latency axis; "
            f"tp_y0={tp_pos.y0:.3f}, lat_y1={lat_pos.y1:.3f}"
        )

    def test_smoke_three_workload_fixture_produces_valid_png(
        self, tmp_path: Path
    ) -> None:
        """Smoke test: a fixture with three workload rows produces a PNG."""
        from plots import generate_comparison_plot

        paths = generate_comparison_plot(_three_workload_fixture(), tmp_path)
        assert len(paths) == 1
        assert paths[0].exists()
        assert paths[0].stat().st_size > 1000

    def test_hatch_attribute_matches_workload_shape(self, tmp_path: Path) -> None:
        """T19.6 visual regression: each bar carries the per-shape hatch.

        Loops the throughput axis's patches and asserts that the hatch
        attribute matches the locked mapping for at least one bar of
        each workload profile present in the fixture.
        """
        results = _three_workload_fixture()
        captured = self._render(results, tmp_path)
        fig = captured[-1]
        tp_ax = next(ax for ax in fig.axes if "receives/s" in (ax.get_ylabel() or ""))

        hatches = {patch.get_hatch() for patch in tp_ax.patches}
        # The three workload profiles must each contribute their own
        # distinct hatch (empty string for scalar, ``-``-based for
        # array, ``x``-based for struct). The exact density depends on
        # the locked palette; we assert on the *category* of each
        # hatch so the test stays robust to density tweaks.
        scalar_present = "" in hatches or None in hatches
        array_present = any(h and "-" in h for h in hatches if h)
        struct_present = any(h and "x" in h for h in hatches if h)
        assert scalar_present, (
            f"expected a solid (empty hatch) bar for scalar; got {hatches!r}"
        )
        assert array_present, (
            f"expected a horizontal-line hatch bar for array; got {hatches!r}"
        )
        assert struct_present, (
            f"expected a checkered hatch bar for struct; got {hatches!r}"
        )

    def test_latency_subplot_carries_same_hatch_set(self, tmp_path: Path) -> None:
        """The latency subplot mirrors the throughput hatch palette per-bar."""
        results = _three_workload_fixture()
        captured = self._render(results, tmp_path)
        fig = captured[-1]
        lat_ax = next(
            ax for ax in fig.axes if "latency" in (ax.get_ylabel() or "").lower()
        )
        lat_hatches = {patch.get_hatch() for patch in lat_ax.patches}
        # Same palette must appear on both subplots so the reader can
        # correlate top-row throughput with bottom-row latency.
        assert any(h and "-" in h for h in lat_hatches if h)
        assert any(h and "x" in h for h in lat_hatches if h)

    def test_three_bars_per_slot_when_three_shapes_observed(
        self, tmp_path: Path
    ) -> None:
        """Slot subdivision: three shapes -> three bars in the slot."""
        results = _three_workload_fixture()
        captured = self._render(results, tmp_path)
        fig = captured[-1]
        tp_ax = next(ax for ax in fig.axes if "receives/s" in (ax.get_ylabel() or ""))
        # The three-workload fixture has one (transport, workload, qos,
        # mode) slot. The slot must render exactly 3 bars on the
        # throughput axis (one per observed shape).
        assert len(tp_ax.patches) == 3, (
            f"expected 3 throughput bars, got {len(tp_ax.patches)}"
        )

    def test_legend_separates_workload_from_threading(self, tmp_path: Path) -> None:
        """T19.6 two-strip legend: workload (hatch) + threading (colour)."""
        results = _three_workload_fixture()
        captured = self._render(results, tmp_path)
        fig = captured[-1]
        # Two figure-level legends, titled by dimension.
        assert len(fig.legends) >= 2, (
            f"expected two fig-level legends; got {len(fig.legends)}"
        )
        titles = {legend.get_title().get_text() for legend in fig.legends}
        assert any("Workload" in t or "fill pattern" in t for t in titles)
        assert any("threading" in t.lower() for t in titles)


class TestThroughputVsWorkloadShapeChart:
    """E19 / T19.6: new per-variant throughput-vs-workload-shape chart."""

    def test_smoke_three_workload_fixture_produces_valid_png(
        self, tmp_path: Path
    ) -> None:
        """Smoke test: three-workload fixture renders a valid PNG."""
        from plots import generate_throughput_vs_workload_shape_plot

        path = generate_throughput_vs_workload_shape_plot(
            _three_workload_fixture(), tmp_path
        )
        assert path.exists()
        assert path.name == "throughput-vs-workload-shape.png"
        assert path.stat().st_size > 1000

    def test_empty_results_emits_placeholder(self, tmp_path: Path) -> None:
        from plots import generate_throughput_vs_workload_shape_plot

        path = generate_throughput_vs_workload_shape_plot([], tmp_path)
        assert path.exists()

    def test_one_subplot_per_variant_axis(self, tmp_path: Path) -> None:
        """Three different (transport, workload, mode) axes -> 3 subplots."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1-multi", shape="scalar", run="r1"),
            _make_result("hybrid-100x10hz-qos1-multi", shape="array", run="r2"),
            _make_result("zenoh-max-qos1-multi", shape="struct", run="r3"),
        ]
        captured: list = []
        original_close = plt.close

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_throughput_vs_workload_shape_plot(results, tmp_path)
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured
        fig = captured[-1]
        # Each variant axis becomes a subplot; the active ones are
        # those whose ``get_title()`` is non-empty. Unused grid slots
        # are turned off via ``set_axis_off`` and produce no title.
        titled_axes = [ax for ax in fig.axes if ax.get_title()]
        # The figure's suptitle does not count as a subplot title; the
        # three variant axes do.
        variant_titles = [
            ax.get_title()
            for ax in titled_axes
            if "Throughput vs" not in (ax.get_title() or "")
        ]
        assert len(variant_titles) == 3, (
            f"expected 3 variant subplots, got {len(variant_titles)} "
            f"with titles {variant_titles}"
        )

    def test_y_axis_label_is_leaves_per_sec(self, tmp_path: Path) -> None:
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = _three_workload_fixture()
        captured: list = []
        original_close = plt.close

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_throughput_vs_workload_shape_plot(results, tmp_path)
        finally:
            plt.close = original_close  # type: ignore[assignment]

        fig = captured[-1]
        # At least one subplot must carry the leaves/s axis label.
        leaves_axes = [
            ax for ax in fig.axes if "leaves" in (ax.get_ylabel() or "").lower()
        ]
        assert leaves_axes, "expected at least one subplot axis with a leaves/s ylabel"

    def test_bar_hatch_consistent_with_comparison_chart(self, tmp_path: Path) -> None:
        """Cross-chart palette consistency: same hatch mapping per shape."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = _three_workload_fixture()
        captured: list = []
        original_close = plt.close

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_throughput_vs_workload_shape_plot(results, tmp_path)
        finally:
            plt.close = original_close  # type: ignore[assignment]

        fig = captured[-1]
        all_hatches = set()
        for ax in fig.axes:
            for patch in ax.patches:
                all_hatches.add(patch.get_hatch())
        # All three workload-profile hatches must be present.
        assert "" in all_hatches or None in all_hatches  # scalar
        assert any(h and "-" in h for h in all_hatches if h)
        assert any(h and "x" in h for h in all_hatches if h)
