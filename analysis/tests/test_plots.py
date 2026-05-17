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
    latency_samples_ms: list[float] | None = None,
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
        latency_samples_ms=latency_samples_ms or [],
    )


class TestSplitVariantName:
    def test_custom_udp_with_qos_legacy(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("custom-udp-1000x100hz-qos1") == (
            "custom-udp",
            "1000x100hz",
            1,
            None,
        )

    def test_hybrid_with_qos_legacy(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("hybrid-100x10hz-qos4") == (
            "hybrid",
            "100x10hz",
            4,
            None,
        )

    def test_quic_with_qos_legacy(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("quic-10x100hz-qos2") == (
            "quic",
            "10x100hz",
            2,
            None,
        )

    def test_zenoh_with_qos_legacy(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("zenoh-max-qos3") == ("zenoh", "max", 3, None)

    def test_no_qos_legacy_shape(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("zenoh-max") == ("zenoh", "max", None, None)

    def test_unknown_prefix_falls_back_to_other(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("weird-name-qos1") == (
            "other",
            "weird-name",
            1,
            None,
        )

    def test_unknown_prefix_no_qos(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("standalone") == (
            "other",
            "standalone",
            None,
            None,
        )

    # T16.13: post-E14 variant names end in ``-single`` or ``-multi``.
    # The threading suffix must be stripped *before* the qos regex
    # runs, otherwise every post-E14 spawn collapses into the
    # ``qos=None`` bucket and the per-QoS chart row layout breaks.

    def test_custom_udp_single_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("custom-udp-100x100hz-qos1-single") == (
            "custom-udp",
            "100x100hz",
            1,
            "single",
        )

    def test_custom_udp_multi_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("custom-udp-100x100hz-qos1-multi") == (
            "custom-udp",
            "100x100hz",
            1,
            "multi",
        )

    def test_hybrid_single_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("hybrid-10x100hz-qos3-single") == (
            "hybrid",
            "10x100hz",
            3,
            "single",
        )

    def test_hybrid_multi_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("hybrid-10x100hz-qos3-multi") == (
            "hybrid",
            "10x100hz",
            3,
            "multi",
        )

    def test_websocket_single_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("websocket-max-qos2-single") == (
            "websocket",
            "max",
            2,
            "single",
        )

    def test_websocket_multi_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("websocket-max-qos2-multi") == (
            "websocket",
            "max",
            2,
            "multi",
        )

    # Natively-multi-only transports (QUIC, WebRTC, Zenoh per E14) only
    # ship in ``-multi`` form. The parser does not special-case them;
    # the layout code handles the single-bar-per-slot rendering.
    def test_quic_multi_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("quic-1000x100hz-qos4-multi") == (
            "quic",
            "1000x100hz",
            4,
            "multi",
        )

    def test_zenoh_multi_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("zenoh-max-qos1-multi") == (
            "zenoh",
            "max",
            1,
            "multi",
        )

    def test_webrtc_multi_post_e14(self) -> None:
        from plots import _split_variant_name

        assert _split_variant_name("webrtc-100x10hz-qos2-multi") == (
            "webrtc",
            "100x10hz",
            2,
            "multi",
        )

    def test_threading_suffix_only_no_qos(self) -> None:
        """Pathological but legal: threading suffix without a qos."""
        from plots import _split_variant_name

        assert _split_variant_name("custom-udp-max-multi") == (
            "custom-udp",
            "max",
            None,
            "multi",
        )


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

    def test_log_throughput_off_keeps_linear_yscale(self, tmp_path: Path) -> None:
        """Default (``log_throughput=False``) keeps throughput on linear scale."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1", writes_per_sec=50.0),
            _make_result("zenoh-10x100hz-qos1", writes_per_sec=480.0),
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
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes, "expected at least one throughput axis"
        for ax in tp_axes:
            assert ax.get_yscale() == "linear", (
                f"throughput axis should be linear by default, got {ax.get_yscale()}"
            )
        for f in captured:
            original_close(f)

    def test_log_throughput_on_sets_log_yscale_on_throughput_axes_only(
        self, tmp_path: Path
    ) -> None:
        """``log_throughput=True`` switches throughput panels to log; latency stays log."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        # Four QoS rows -> four throughput axes and four latency axes.
        results = []
        for q in (1, 2, 3, 4):
            results.append(
                _make_result(
                    f"custom-udp-10x100hz-qos{q}",
                    writes_per_sec=50.0 * q,
                )
            )
            results.append(
                _make_result(
                    f"zenoh-10x100hz-qos{q}",
                    writes_per_sec=480.0 * q,
                )
            )
        original_close = plt.close
        captured: list = []

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_comparison_plot(
                results, tmp_path, log_throughput=True
            )
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured
        fig = captured[-1]
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        lat_axes = [
            ax for ax in fig.axes if "latency" in (ax.get_ylabel() or "").lower()
        ]
        assert len(tp_axes) == 4, f"expected 4 throughput axes, got {len(tp_axes)}"
        assert len(lat_axes) == 4, f"expected 4 latency axes, got {len(lat_axes)}"
        for ax in tp_axes:
            assert ax.get_yscale() == "log", (
                f"throughput axis should be log with log_throughput=True, "
                f"got {ax.get_yscale()}"
            )
        for ax in lat_axes:
            assert ax.get_yscale() == "log", (
                f"latency axis should remain log, got {ax.get_yscale()}"
            )
        for f in captured:
            original_close(f)

    def test_log_throughput_filename_suffix(self, tmp_path: Path) -> None:
        """``log_throughput`` toggles the output filename so both flavours coexist.

        Without the suffix the log-scale run would overwrite the linear-scale
        ``comparison.png`` in the same ``--output`` dir. The returned ``Path``
        must reflect the filename actually written, since the CLI prints it.
        """
        from plots import generate_comparison_plot

        results = [
            _make_result("custom-udp-10x100hz-qos1", writes_per_sec=50.0),
            _make_result("zenoh-10x100hz-qos1", writes_per_sec=480.0),
        ]

        linear_out = generate_comparison_plot(
            results, tmp_path / "out", log_throughput=False
        )
        assert linear_out.name == "comparison.png"
        assert linear_out.exists()

        log_out = generate_comparison_plot(
            results, tmp_path / "out", log_throughput=True
        )
        assert log_out.name == "comparison-log.png"
        assert log_out.exists()

        # Both files coexist in the same directory.
        assert linear_out.parent == log_out.parent
        assert linear_out.exists()

    def test_log_throughput_zero_writes_skipped_not_clamped(
        self, tmp_path: Path
    ) -> None:
        """A 0-writes spawn renders as NaN under log scale, not a tiny visible bar."""
        import math

        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1", writes_per_sec=0.0),
            _make_result("zenoh-10x100hz-qos1", writes_per_sec=480.0),
        ]
        original_close = plt.close
        captured: list = []

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_comparison_plot(
                results, tmp_path, log_throughput=True
            )
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured
        fig = captured[-1]
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes, "expected a throughput axis"
        heights = [b.get_height() for b in tp_axes[0].patches]
        assert any(math.isnan(h) for h in heights), (
            f"expected zero-writes bar to be NaN (skipped), got {heights}"
        )
        assert any(math.isfinite(h) and h > 0 for h in heights), (
            f"expected the non-zero bar to render finite, got {heights}"
        )

        for f in captured:
            original_close(f)

    def test_target_lines_drawn_at_unique_workload_rates(self, tmp_path: Path) -> None:
        """One horizontal line per unique ``vpt * hz`` target rate.

        Three synthesized workloads at 10x100hz, 100x100hz, 1000x100hz
        produce targets {1_000, 10_000, 100_000}. The throughput
        subplot should carry exactly those three axhline objects --
        same set on every QoS row, but here we only have one row so
        we inspect that single throughput axis.
        """
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1", writes_per_sec=900.0),
            _make_result("custom-udp-100x100hz-qos1", writes_per_sec=9_500.0),
            _make_result("custom-udp-1000x100hz-qos1", writes_per_sec=95_000.0),
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
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes, "expected at least one throughput axis"
        ax_tp = tp_axes[0]
        # axhline objects are constant-y Line2Ds with both ydata points
        # equal. Collect their y values and check the unique set.
        target_ys: set[float] = set()
        for line in ax_tp.lines:
            ydata = line.get_ydata()
            if len(ydata) >= 2 and ydata[0] == ydata[-1]:
                target_ys.add(float(ydata[0]))
        assert target_ys == {1_000.0, 10_000.0, 100_000.0}, (
            f"expected three unique target y values, got {target_ys}"
        )
        for f in captured:
            original_close(f)

    def test_target_lines_skip_max_workload(self, tmp_path: Path) -> None:
        """The ``max`` workload has no fixed target -> no axhline drawn."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        # Only a max-workload result. No target rate should be derived.
        results = [
            _make_result("custom-udp-max-qos1", writes_per_sec=400_000.0),
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
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes, "expected at least one throughput axis"
        ax_tp = tp_axes[0]
        horizontal_ys = [
            float(line.get_ydata()[0])
            for line in ax_tp.lines
            if len(line.get_ydata()) >= 2
            and line.get_ydata()[0] == line.get_ydata()[-1]
        ]
        assert horizontal_ys == [], (
            f"expected no target lines for a max-only panel, got {horizontal_ys}"
        )
        for f in captured:
            original_close(f)

    def test_target_line_labels_use_si_suffix(self, tmp_path: Path) -> None:
        """Labels for 1 K / 10 K / 100 K targets use SI suffixes."""
        import re

        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1", writes_per_sec=900.0),
            _make_result("custom-udp-100x100hz-qos1", writes_per_sec=9_500.0),
            _make_result("custom-udp-1000x100hz-qos1", writes_per_sec=95_000.0),
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
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes
        ax_tp = tp_axes[0]
        si_pattern = re.compile(r"^\d+(\.\d+)?\s*[KMG]/s$")
        # Pull all text artists on the axis with content matching the
        # SI suffix pattern; group by the integer y position so the
        # assertion does not depend on the order matplotlib stored
        # them in.
        si_label_ys: set[float] = set()
        for txt in ax_tp.texts:
            if si_pattern.match(txt.get_text() or ""):
                si_label_ys.add(float(txt.get_position()[1]))
        assert {1_000.0, 10_000.0, 100_000.0}.issubset(si_label_ys), (
            f"expected SI labels at 1 K / 10 K / 100 K targets, got {si_label_ys}"
        )
        for f in captured:
            original_close(f)

    def test_tier_markers_one_star_for_1k_target(self, tmp_path: Path) -> None:
        """A 10x100hz workload targets 1 K/s -> the throughput bar carries ``*``.

        Single bar -> exactly one ``*`` text artist on the qos1
        throughput axis. The latency axis must not be annotated.
        """
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1", writes_per_sec=900.0),
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
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes, "expected at least one throughput axis"
        ax_tp = tp_axes[0]
        star_texts = [t for t in ax_tp.texts if t.get_text() == "*"]
        assert len(star_texts) == 1, (
            f"expected exactly one '*' marker on throughput axis, "
            f"got {[t.get_text() for t in ax_tp.texts]}"
        )
        # Latency axis must not carry star annotations.
        lat_axes = [
            ax for ax in fig.axes if "latency" in (ax.get_ylabel() or "").lower()
        ]
        assert lat_axes
        for ax_lat in lat_axes:
            lat_stars = [t for t in ax_lat.texts if set(t.get_text() or "") == {"*"}]
            assert lat_stars == [], (
                f"latency axis should not carry tier markers, found {lat_stars}"
            )
        for f in captured:
            original_close(f)

    def test_tier_markers_three_stars_for_100k_target(self, tmp_path: Path) -> None:
        """A 1000x100hz workload targets 100 K/s -> the bar carries ``***``."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-1000x100hz-qos1", writes_per_sec=95_000.0),
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
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes
        ax_tp = tp_axes[0]
        triple_star_texts = [t for t in ax_tp.texts if t.get_text() == "***"]
        assert len(triple_star_texts) >= 1, (
            f"expected '***' marker on throughput axis, "
            f"got {[t.get_text() for t in ax_tp.texts]}"
        )
        for f in captured:
            original_close(f)

    def test_tier_markers_omitted_for_max(self, tmp_path: Path) -> None:
        """The ``max`` workload has no fixed target -> no star annotation."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-max-qos1", writes_per_sec=400_000.0),
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
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes
        ax_tp = tp_axes[0]
        star_only_texts = [
            t for t in ax_tp.texts if t.get_text() and set(t.get_text()) == {"*"}
        ]
        assert star_only_texts == [], (
            f"expected no '*' / '**' / '***' marker for max workload, "
            f"got {[t.get_text() for t in star_only_texts]}"
        )
        for f in captured:
            original_close(f)

    def test_tier_markers_present_on_log_scale_too(self, tmp_path: Path) -> None:
        """Under ``log_throughput=True`` the ``***`` marker still appears."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-1000x100hz-qos1", writes_per_sec=95_000.0),
        ]
        original_close = plt.close
        captured: list = []

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_comparison_plot(
                results, tmp_path, log_throughput=True
            )
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured
        fig = captured[-1]
        tp_axes = [ax for ax in fig.axes if "writes/s" in (ax.get_ylabel() or "")]
        assert tp_axes
        ax_tp = tp_axes[0]
        assert ax_tp.get_yscale() == "log"
        triple_star_texts = [t for t in ax_tp.texts if t.get_text() == "***"]
        assert len(triple_star_texts) >= 1, (
            f"expected '***' marker on log-scale throughput axis, "
            f"got {[t.get_text() for t in ax_tp.texts]}"
        )
        for f in captured:
            original_close(f)

    def test_nonpositive_p95_renders_as_nan_bar(self, tmp_path: Path) -> None:
        """A percentile <= 0 (clock-noise artifact) is dropped to NaN.

        Regression test for the relaxed epsilon clamp: previously a
        non-positive p95 was clamped to ``_LATENCY_EPSILON_MS`` and
        rendered as a visible "1 us" bar that misled the reader. After
        the relaxation it should disappear entirely (height NaN).
        """
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = [
            _make_result("custom-udp-10x100hz-qos1", p50=-1.0, p95=-0.5, p99=0.0),
            _make_result("zenoh-10x100hz-qos1", p50=1.0, p95=2.0, p99=3.0),
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
        # Find the latency axis. Pull bar heights -- the custom-udp bar
        # should be NaN, the zenoh bar finite.
        import math

        lat_axes = [
            ax for ax in fig.axes if "latency" in (ax.get_ylabel() or "").lower()
        ]
        assert lat_axes
        bars = [p for p in lat_axes[0].patches]
        heights = [b.get_height() for b in bars]
        assert any(math.isnan(h) for h in heights), (
            f"expected at least one NaN bar height, got {heights}"
        )
        assert any(math.isfinite(h) and h > 0 for h in heights), (
            f"expected at least one positive finite bar, got {heights}"
        )
        for f in captured:
            original_close(f)


class TestEmpiricalCdf:
    """Unit tests for the CDF computation used by ``generate_latency_cdf_plot``.

    Acceptance criteria from T11.4:
    * ``y`` monotonic non-decreasing
    * ``y`` bounded in [0, 1]
    * Output length matches the count of positive finite samples
    """

    def test_returns_empty_for_empty_input(self) -> None:
        from plots import empirical_cdf

        x, y = empirical_cdf([])
        assert x.size == 0
        assert y.size == 0

    def test_y_monotonic_non_decreasing(self) -> None:
        from plots import empirical_cdf

        samples = [0.5, 0.1, 1.0, 0.3, 0.8, 0.2, 0.7]
        _, y = empirical_cdf(samples)
        for i in range(1, len(y)):
            assert y[i] >= y[i - 1], (
                f"CDF y[{i}]={y[i]} < y[{i - 1}]={y[i - 1]}; not monotonic"
            )

    def test_y_bounded_in_unit_interval(self) -> None:
        from plots import empirical_cdf

        samples = [10.0, 0.001, 5.0, 50.0, 0.5, 100.0]
        _, y = empirical_cdf(samples)
        assert y.size > 0
        assert y.min() > 0.0
        assert y.max() == 1.0
        assert (y <= 1.0).all()
        assert (y >= 0.0).all()

    def test_output_length_matches_positive_samples(self) -> None:
        from plots import empirical_cdf

        # Mix of positive, zero, negative, NaN, inf -- only positives kept.
        samples = [1.0, 2.0, 0.0, -1.0, float("nan"), float("inf"), 3.0, 0.5]
        x, y = empirical_cdf(samples)
        assert x.size == 4
        assert y.size == 4
        # And x is sorted.
        assert list(x) == sorted(x)

    def test_step_size_is_one_over_n(self) -> None:
        """``y[i+1] - y[i]`` is ``1/n`` for distinct samples."""
        from plots import empirical_cdf

        samples = [1.0, 2.0, 3.0, 4.0, 5.0]
        _, y = empirical_cdf(samples)
        n = len(samples)
        assert abs(y[0] - 1.0 / n) < 1e-12
        for i in range(1, n):
            assert abs((y[i] - y[i - 1]) - (1.0 / n)) < 1e-12


class TestGenerateLatencyCdfPlot:
    def test_creates_png(self, tmp_path: Path) -> None:
        from plots import generate_latency_cdf_plot

        # Ten samples per result; enough to draw a visible CDF.
        results = [
            _make_result(
                "custom-udp-10x100hz-qos1",
                latency_samples_ms=[
                    0.001,
                    0.002,
                    0.005,
                    0.01,
                    0.05,
                    0.1,
                    0.5,
                    1.0,
                    5.0,
                    10.0,
                ],
            ),
            _make_result(
                "zenoh-10x100hz-qos1",
                latency_samples_ms=[
                    0.5,
                    1.0,
                    1.5,
                    2.0,
                    3.0,
                    5.0,
                    8.0,
                    12.0,
                    20.0,
                    50.0,
                ],
            ),
        ]
        out = generate_latency_cdf_plot(results, tmp_path / "out")
        assert out.exists()
        assert out.name == "latency_cdf.png"
        assert out.stat().st_size > 1000

    def test_creates_output_dir(self, tmp_path: Path) -> None:
        from plots import generate_latency_cdf_plot

        nested = tmp_path / "a" / "b"
        out = generate_latency_cdf_plot(
            [_make_result("zenoh-max-qos1", latency_samples_ms=[0.1, 0.2, 0.3])],
            nested,
        )
        assert nested.is_dir()
        assert out.exists()

    def test_empty_results(self, tmp_path: Path) -> None:
        from plots import generate_latency_cdf_plot

        out = generate_latency_cdf_plot([], tmp_path)
        assert out.exists()
        assert out.name == "latency_cdf.png"

    def test_no_samples_renders_placeholder_per_row(self, tmp_path: Path) -> None:
        """A QoS row with no positive samples still renders without crashing."""
        from plots import generate_latency_cdf_plot

        results = [
            _make_result("custom-udp-10x100hz-qos1", latency_samples_ms=[]),
            _make_result(
                "zenoh-10x100hz-qos1",
                latency_samples_ms=[0.1, 0.2, 0.5, 1.0],
            ),
        ]
        out = generate_latency_cdf_plot(results, tmp_path)
        assert out.exists()
        assert out.stat().st_size > 1000

    def test_multi_qos_rows(self, tmp_path: Path) -> None:
        """Four QoS rows should render four subplots."""
        import matplotlib.pyplot as plt

        import plots as plots_module

        results = []
        for q in (1, 2, 3, 4):
            results.append(
                _make_result(
                    f"custom-udp-10x100hz-qos{q}",
                    latency_samples_ms=[0.001 * q, 0.01 * q, 0.1 * q, 1.0 * q],
                )
            )

        original_close = plt.close
        captured: list = []

        def capture_close(fig=None) -> None:
            if fig is not None:
                captured.append(fig)

        plt.close = capture_close  # type: ignore[assignment]
        try:
            plots_module.generate_latency_cdf_plot(results, tmp_path)
        finally:
            plt.close = original_close  # type: ignore[assignment]

        assert captured
        fig = captured[-1]
        # Four QoS rows -> four subplot axes (legend lives on fig, not ax).
        assert len(fig.axes) == 4
        for ax in fig.axes:
            assert ax.get_xscale() == "log"
            ymin, ymax = ax.get_ylim()
            assert ymin == 0.0 and ymax == 1.0
        for f in captured:
            original_close(f)
