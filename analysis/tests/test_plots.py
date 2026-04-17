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


class TestSplitVariant:
    def test_basic_split(self) -> None:
        from plots import _split_variant

        assert _split_variant("custom-udp-10vpt") == ("custom-udp", "10vpt")

    def test_single_hyphen(self) -> None:
        from plots import _split_variant

        assert _split_variant("zenoh-max") == ("zenoh", "max")

    def test_no_hyphen(self) -> None:
        from plots import _split_variant

        assert _split_variant("standalone") == ("standalone", "")

    def test_multiple_hyphens(self) -> None:
        from plots import _split_variant

        assert _split_variant("my-custom-udp-1000vpt") == (
            "my-custom-udp",
            "1000vpt",
        )


class TestGenerateComparisonPlot:
    def test_creates_png(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        results = [
            _make_result("custom-udp-10vpt", writes_per_sec=50.0),
            _make_result("custom-udp-max", writes_per_sec=500.0),
            _make_result("zenoh-10vpt", writes_per_sec=45.0),
            _make_result("zenoh-max", writes_per_sec=480.0),
        ]
        out = generate_comparison_plot(results, tmp_path / "output")
        assert out.exists()
        assert out.name == "comparison.png"
        assert out.parent == tmp_path / "output"
        # File should be non-trivial in size
        assert out.stat().st_size > 1000

    def test_creates_output_dir(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        nested = tmp_path / "a" / "b" / "c"
        results = [_make_result("foo-bar")]
        out = generate_comparison_plot(results, nested)
        assert nested.is_dir()
        assert out.exists()

    def test_empty_results(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        out = generate_comparison_plot([], tmp_path / "empty")
        assert out.exists()

    def test_single_variant(self, tmp_path: Path) -> None:
        from plots import generate_comparison_plot

        results = [_make_result("transport-load")]
        out = generate_comparison_plot(results, tmp_path)
        assert out.exists()

    def test_whisker_values_valid(self) -> None:
        """Verify that p95 - p50 and p99 - p95 are non-negative for the error bars."""
        r = _make_result("x-y", p50=2.0, p95=5.0, p99=8.0)
        assert r.latency_p95_ms - r.latency_p50_ms >= 0
        assert r.latency_p99_ms - r.latency_p95_ms >= 0
