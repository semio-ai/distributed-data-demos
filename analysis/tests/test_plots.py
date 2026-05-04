"""Tests for the plots module."""

from __future__ import annotations

from pathlib import Path

import pytest

try:
    import matplotlib  # noqa: F401

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
    run: str = "run01",
    writes_per_sec: float = 100.0,
    p50: float = 1.0,
    p95: float = 5.0,
    p99: float = 10.0,
) -> PerformanceResult:
    return PerformanceResult(
        variant=variant,
        run=run,
        connect_mean_ms=10.0,
        connect_max_ms=20.0,
        latency_p50_ms=p50,
        latency_p95_ms=p95,
        latency_p99_ms=p99,
        latency_max_ms=p99 + 5.0,
        writes_per_sec=writes_per_sec,
        receives_per_sec=writes_per_sec,
        jitter_ms=0.5,
        jitter_p95_ms=1.0,
        loss_pct=0.0,
    )


class TestSplitVariantName:
    def test_custom_udp_with_qos(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("custom-udp-1000x100hz-qos1") == (
            "custom-udp",
            "1000x100hz",
            1,
        )

    def test_hybrid_with_qos(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("hybrid-100x10hz-qos4") == (
            "hybrid",
            "100x10hz",
            4,
        )

    def test_quic_with_qos(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("quic-10x100hz-qos2") == (
            "quic",
            "10x100hz",
            2,
        )

    def test_zenoh_with_qos(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("zenoh-max-qos3") == ("zenoh", "max", 3)

    def test_no_qos_legacy_shape(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("zenoh-max") == ("zenoh", "max", None)

    def test_unknown_prefix_falls_back_to_other(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("weird-name-qos1") == (
            "other",
            "weird-name",
            1,
        )

    def test_unknown_prefix_no_qos(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("standalone") == ("other", "standalone", None)


class TestWorkloadLoadOrdering:
    def test_orders_known_workloads_by_load_intensity(self) -> None:
        from plots import _workload_load_rank

        workloads = [
            "1000x100hz",
            "100x100hz",
            "max",
            "10x100hz",
            "100x1000hz",
        ]
        ordered = sorted(workloads, key=_workload_load_rank)
        assert ordered == [
            "10x100hz",
            "100x100hz",
            "100x1000hz",
            "1000x100hz",
            "max",
        ]

    def test_unknown_workload_falls_back_alphabetically(self) -> None:
        from plots import _workload_load_rank

        workloads = ["bogus", "10x100hz"]
        ordered = sorted(workloads, key=_workload_load_rank)
        # Unknown workloads (rank -1) sort before known ones.
        assert ordered == ["bogus", "10x100hz"]

    def test_max_is_last(self) -> None:
        from plots import _workload_load_rank

        assert _workload_load_rank("max")[0] > _workload_load_rank("1000x1000hz")[0]


class TestFamilyPalette:
    def test_returns_distinct_tones_per_workload(self) -> None:
        from plots import _family_palette

        workloads = ["10x100hz", "100x100hz", "100x1000hz", "1000x100hz"]
        palette = _family_palette("zenoh", workloads)
        assert len(palette) == 4
        rgbas = list(palette.values())
        # All tones must be distinct.
        assert len({tuple(c) for c in rgbas}) == 4

    def test_tone_positions_in_expected_range(self) -> None:
        """Sampled positions are spread across [0.4, 0.95]."""
        import numpy as np

        from plots import _TONE_RANGE, _family_palette

        workloads = ["a", "b", "c", "d"]
        palette = _family_palette("custom-udp", workloads)
        # Reverse-engineer the colormap to confirm sample positions are
        # within the configured tone range. We do this by checking the
        # colours match cmap(p) for some p in [_TONE_RANGE[0], _TONE_RANGE[1]].
        import matplotlib.pyplot as plt

        cmap = plt.get_cmap("Oranges")
        for w, rgba in palette.items():
            # Search for a position p in the tone range that produces
            # this colour. With 256-bin colormaps a coarse 0.001 grid is
            # plenty.
            best_p = None
            best_dist = float("inf")
            for p in np.linspace(_TONE_RANGE[0], _TONE_RANGE[1], 1024):
                dist = sum((a - b) ** 2 for a, b in zip(cmap(p), rgba))
                if dist < best_dist:
                    best_dist = dist
                    best_p = p
            assert best_p is not None
            assert _TONE_RANGE[0] - 1e-3 <= best_p <= _TONE_RANGE[1] + 1e-3, (
                f"workload {w} sampled at p={best_p}, outside {_TONE_RANGE}"
            )

    def test_unknown_transport_uses_fallback_colormap(self) -> None:
        from plots import _family_palette

        palette = _family_palette("not-a-real-family", ["x"])
        # Should not crash and should yield one tone.
        assert len(palette) == 1


class TestGenerateComparisonPlot:
    def test_creates_png(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        results = [
            _make_result("custom-udp-10x100hz-qos1", writes_per_sec=50.0),
            _make_result("custom-udp-max-qos1", writes_per_sec=500.0),
            _make_result("zenoh-10x100hz-qos1", writes_per_sec=45.0),
            _make_result("zenoh-max-qos1", writes_per_sec=480.0),
        ]
        out = generate_comparison_plot(results, tmp_path / "output")
        assert out.exists()
        assert out.name == "comparison.png"
        assert out.parent == tmp_path / "output"
        assert out.stat().st_size > 1000

    def test_creates_output_dir(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        nested = tmp_path / "a" / "b" / "c"
        results = [_make_result("zenoh-max-qos1")]
        out = generate_comparison_plot(results, nested)
        assert nested.is_dir()
        assert out.exists()

    def test_empty_results(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        out = generate_comparison_plot([], tmp_path / "empty")
        assert out.exists()

    def test_single_variant(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        results = [_make_result("quic-100x100hz-qos2")]
        out = generate_comparison_plot(results, tmp_path)
        assert out.exists()

    def test_legacy_no_qos_still_renders(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        results = [
            _make_result("custom-udp-10x100hz", writes_per_sec=50.0),
            _make_result("zenoh-max", writes_per_sec=480.0),
        ]
        out = generate_comparison_plot(results, tmp_path)
        assert out.exists()
        assert out.stat().st_size > 1000

    def test_with_qos_expansion_data(self, tmp_path: Path) -> None:
        """Synthetic 4 transports x 2 workloads x 4 qos = 32 entries."""
        from plots import generate_comparison_plot

        transports = ["custom-udp", "hybrid", "quic", "zenoh"]
        workloads = ["10x100hz", "1000x100hz"]
        qos_levels = [1, 2, 3, 4]

        results: list[PerformanceResult] = []
        for t in transports:
            for w in workloads:
                for q in qos_levels:
                    results.append(
                        _make_result(
                            f"{t}-{w}-qos{q}",
                            writes_per_sec=100.0 + q * 10.0,
                            p50=0.5 * q,
                            p95=2.0 * q,
                            p99=5.0 * q,
                        )
                    )

        assert len(results) == 32
        out = generate_comparison_plot(results, tmp_path)
        assert out.exists()
        assert out.stat().st_size > 5000

    def test_handles_missing_qos(self, tmp_path: Path) -> None:
        """A subset of (transport, workload, qos) combinations should
        render as gaps without exception."""
        from plots import generate_comparison_plot

        # Only qos3 and qos4 for one variant; full set for another.
        results = [
            _make_result("custom-udp-10x100hz-qos3", writes_per_sec=50.0),
            _make_result("custom-udp-10x100hz-qos4", writes_per_sec=55.0),
            _make_result("zenoh-10x100hz-qos1", writes_per_sec=80.0),
            _make_result("zenoh-10x100hz-qos2", writes_per_sec=82.0),
            _make_result("zenoh-10x100hz-qos3", writes_per_sec=85.0),
            _make_result("zenoh-10x100hz-qos4", writes_per_sec=86.0),
        ]
        out = generate_comparison_plot(results, tmp_path)
        assert out.exists()
        assert out.stat().st_size > 1000

    def test_legend_outside_axes(self, tmp_path: Path) -> None:
        """The shared legend should live on the figure, not on either axis."""
        import matplotlib.pyplot as plt

        results = [
            _make_result("custom-udp-10x100hz-qos1"),
            _make_result("zenoh-10x100hz-qos1"),
        ]
        # We need the figure object that ``generate_comparison_plot``
        # produced; since it closes its figure, we re-render here using
        # the same code path then inspect the latest figure on the
        # plt-managed stack via a side-effect probe. Instead, peek at
        # the image content by rendering and reopening, OR -- simpler --
        # render once, then re-create a probe figure and assert the
        # plots module's intent by reading the saved bytes is
        # impractical. Use a manual reproduction of the call here so we
        # have a live figure to inspect.
        import plots as plots_module

        # Re-run via an internal hook: monkey-patch ``plt.close`` so the
        # figure stays alive long enough to inspect.
        original_close = plt.close
        captured: list = []

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)
            # Don't actually close.

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_comparison_plot(results, tmp_path)
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured, "expected generate_comparison_plot to produce a figure"
        fig = captured[-1]
        assert len(fig.legends) >= 1, "expected fig.legend(...) to be set, none found"
        for ax in fig.axes:
            assert ax.get_legend() is None, (
                f"per-axis legend should not exist; found one on {ax}"
            )
        # Clean up the figures we held open.
        for f in captured:
            original_close(f)

    def test_latency_axis_is_log_scale(self, tmp_path: Path) -> None:
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1", p50=0.1, p95=0.3, p99=0.6),
            _make_result("zenoh-10x100hz-qos1", p50=5.0, p95=20.0, p99=40.0),
        ]
        original_close = plt.close
        captured: list = []

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_comparison_plot(results, tmp_path)
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured
        fig = captured[-1]
        # The latency subplot is the second of the two created by the
        # 1x2 layout. Identify it by y-axis label.
        lat_axes = [
            ax for ax in fig.axes if "latency" in (ax.get_ylabel() or "").lower()
        ]
        assert lat_axes, "expected a latency axis"
        assert lat_axes[0].get_yscale() == "log"
        for f in captured:
            original_close(f)

    def test_whisker_values_valid(self) -> None:
        """Verify that p95 - p50 and p99 - p95 are non-negative for the error bars."""
        r = _make_result("custom-udp-10x100hz-qos1", p50=2.0, p95=5.0, p99=8.0)
        assert r.latency_p95_ms - r.latency_p50_ms >= 0
        assert r.latency_p99_ms - r.latency_p95_ms >= 0
