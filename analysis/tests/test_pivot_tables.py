"""Tests for the variant x workload pivot tables (T-pivot.*)."""

from __future__ import annotations

import csv
import io
import math

import pytest
from helpers import events_to_lazy, make_event

from correlate import correlate_lazy
from performance import PerformanceResult, _latency_mean_std, performance_for_group
from pivot_tables import (
    PivotCell,
    PivotTable,
    build_pivot_tables,
    export_csv,
    format_pivot_section,
    format_pivot_table,
    parse_spawn_name,
)


# --- Spawn-name parser -------------------------------------------------------


class TestParseSpawnName:
    """Coverage of every shape from the canonical config (configs/two-runner-all-variants.toml)."""

    @pytest.mark.parametrize(
        "name, family, vpt, hz, qos, mode, kind",
        [
            (
                "custom-udp-1000x100hz-qos4-multi",
                "custom-udp",
                1000,
                100,
                4,
                "multi",
                "scalar-flood",
            ),
            (
                "custom-udp-1000x10hz-qos1-single",
                "custom-udp",
                1000,
                10,
                1,
                "single",
                "scalar-flood",
            ),
            (
                "custom-udp-100x1000hz-qos3-multi",
                "custom-udp",
                100,
                1000,
                3,
                "multi",
                "scalar-flood",
            ),
            (
                "custom-udp-100x100hz-qos2-single",
                "custom-udp",
                100,
                100,
                2,
                "single",
                "scalar-flood",
            ),
            (
                "custom-udp-10x100hz-qos4-multi",
                "custom-udp",
                10,
                100,
                4,
                "multi",
                "scalar-flood",
            ),
            (
                "custom-udp-10x1000hz-qos4-multi",
                "custom-udp",
                10,
                1000,
                4,
                "multi",
                "scalar-flood",
            ),
            (
                "custom-udp-max-qos4-multi",
                "custom-udp",
                0,
                0,
                4,
                "multi",
                "max-throughput",
            ),
            (
                "hybrid-1000x100hz-qos4-multi",
                "hybrid",
                1000,
                100,
                4,
                "multi",
                "scalar-flood",
            ),
            (
                "hybrid-max-qos1-single",
                "hybrid",
                0,
                0,
                1,
                "single",
                "max-throughput",
            ),
            (
                "quic-1000x100hz-qos4-multi",
                "quic",
                1000,
                100,
                4,
                "multi",
                "scalar-flood",
            ),
            (
                "quic-max-qos2-multi",
                "quic",
                0,
                0,
                2,
                "multi",
                "max-throughput",
            ),
            (
                "zenoh-100x1000hz-qos3-multi",
                "zenoh",
                100,
                1000,
                3,
                "multi",
                "scalar-flood",
            ),
            (
                "websocket-100x100hz-qos3-single",
                "websocket",
                100,
                100,
                3,
                "single",
                "scalar-flood",
            ),
            (
                "websocket-100x100hz-qos4-multi",
                "websocket",
                100,
                100,
                4,
                "multi",
                "scalar-flood",
            ),
            (
                "webrtc-1000x10hz-qos4-multi",
                "webrtc",
                1000,
                10,
                4,
                "multi",
                "scalar-flood",
            ),
            (
                "webrtc-max-qos4-multi",
                "webrtc",
                0,
                0,
                4,
                "multi",
                "max-throughput",
            ),
        ],
    )
    def test_canonical_spawn_names(
        self,
        name: str,
        family: str,
        vpt: int,
        hz: int,
        qos: int,
        mode: str,
        kind: str,
    ) -> None:
        ident = parse_spawn_name(name)
        assert ident is not None, f"failed to parse {name!r}"
        assert ident.family == family
        assert ident.values_per_tick == vpt
        assert ident.tick_rate_hz == hz
        assert ident.qos == qos
        assert ident.mode == mode
        assert ident.workload_kind == kind

    def test_workload_profile(self) -> None:
        ident = parse_spawn_name("custom-udp-1000x100hz-qos4-multi")
        assert ident is not None
        assert ident.workload_profile == "1000x100hz"

        ident_max = parse_spawn_name("custom-udp-max-qos4-multi")
        assert ident_max is not None
        assert ident_max.workload_profile == "max"

    def test_row_key(self) -> None:
        ident = parse_spawn_name("websocket-100x100hz-qos3-single")
        assert ident is not None
        assert ident.row_key == ("websocket", "single")

    @pytest.mark.parametrize(
        "name",
        [
            "",
            "custom-udp",  # missing suffix
            "custom-udp-1000x100hz",  # missing qos/mode
            "custom-udp-1000x100hz-qos4",  # missing mode
            "custom-udp-1000x100hz-qos4-bogus",  # unknown mode
            "unknown-family-1000x100hz-qos4-multi",
            "clock-sync",
            "custom-udp-foo-qos4-multi",  # bad workload token
            "custom-udp-1000x100-qos4-multi",  # missing hz suffix
            "custom-udp-max-qos0-multi",  # qos 0 is technically parsed
        ],
    )
    def test_invalid_names_return_none(self, name: str) -> None:
        """Names that don't match the canonical shape return None.

        The last case (qos 0) is intentionally permissive -- the regex
        accepts any integer in the qos slot and the caller decides
        whether to filter on the value. We sanity-check the others
        return None.
        """
        result = parse_spawn_name(name)
        if name == "custom-udp-max-qos0-multi":
            # Parser accepts any int; spawn-side gating filters qos.
            assert result is not None
            assert result.qos == 0
        else:
            assert result is None


# --- Latency mean/std -------------------------------------------------------


class TestLatencyMeanStd:
    def test_empty_returns_nan(self) -> None:
        mean, std = _latency_mean_std([])
        assert math.isnan(mean)
        assert math.isnan(std)

    def test_single_sample_has_zero_std(self) -> None:
        mean, std = _latency_mean_std([7.5])
        assert mean == 7.5
        assert std == 0.0

    def test_multi_sample_matches_statistics(self) -> None:
        import statistics

        samples = [1.0, 2.0, 3.0, 4.0, 5.0]
        mean, std = _latency_mean_std(samples)
        assert mean == statistics.mean(samples)
        assert std == statistics.stdev(samples)

    def test_attached_to_performance_result(self) -> None:
        """The new fields are exposed on PerformanceResult."""
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1001,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1011,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        r = performance_for_group(
            lazy, deliveries, "custom-udp-100x100hz-qos1-multi", "run01"
        )
        # mean & std defined (one sample -> 0.0 std)
        assert not math.isnan(r.latency_mean_ms)
        assert not math.isnan(r.latency_std_ms)
        # parser populates expected_writes_per_sec = 100 * 100 = 10000
        assert r.expected_writes_per_sec == 10000.0


# --- Expected writes & ratio ------------------------------------------------


class TestExpectedWritesPerSec:
    def test_scalar_flood_computed(self) -> None:
        """100x100hz -> 10K expected writes/sec."""
        ident = parse_spawn_name("custom-udp-100x100hz-qos4-multi")
        assert ident is not None
        # The caller multiplies vpt * hz to get the expected rate.
        assert ident.values_per_tick * ident.tick_rate_hz == 10_000

    def test_max_workload_has_no_expected_rate(self) -> None:
        ident = parse_spawn_name("custom-udp-max-qos4-multi")
        assert ident is not None
        assert ident.workload_kind == "max-throughput"
        # vpt and hz are 0 for max-throughput by the parser contract.
        assert ident.values_per_tick == 0
        assert ident.tick_rate_hz == 0

    def test_ratio_none_for_max_workload_in_perf_result(self) -> None:
        """A max-workload spawn carries None for the ratio field."""
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1001,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1011,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        r = performance_for_group(
            lazy, deliveries, "custom-udp-max-qos4-multi", "run01"
        )
        assert r.expected_writes_per_sec is None
        assert r.receives_to_expected_ratio_pct is None


# --- Pivot table builder & renderer -----------------------------------------


def _synthetic_result(
    variant: str,
    receives_per_sec: float,
    writes_per_sec: float,
    expected_wps: float | None,
    ratio_pct: float | None,
    latency_mean_ms: float,
    latency_std_ms: float,
) -> PerformanceResult:
    """Build a PerformanceResult by hand, bypassing the polars pipeline."""
    return PerformanceResult(
        variant=variant,
        run="run01",
        connect_mean_ms=0.0,
        connect_max_ms=0.0,
        latency_p50_ms=latency_mean_ms,
        latency_p95_ms=latency_mean_ms,
        latency_p99_ms=latency_mean_ms,
        latency_max_ms=latency_mean_ms,
        writes_per_sec=writes_per_sec,
        receives_per_sec=receives_per_sec,
        jitter_ms=0.0,
        jitter_p95_ms=0.0,
        loss_pct=0.0,
        threading_mode="multi",
        latency_mean_ms=latency_mean_ms,
        latency_std_ms=latency_std_ms,
        expected_writes_per_sec=expected_wps,
        receives_to_expected_ratio_pct=ratio_pct,
    )


class TestPivotBuilder:
    def test_groups_by_qos(self) -> None:
        results = [
            _synthetic_result(
                "custom-udp-100x100hz-qos1-multi", 9500, 10000, 10000.0, 95.0, 1.2, 0.3
            ),
            _synthetic_result(
                "custom-udp-100x100hz-qos4-multi", 9900, 10000, 10000.0, 99.0, 1.5, 0.4
            ),
            _synthetic_result(
                "quic-1000x100hz-qos4-multi", 99500, 100000, 100000.0, 99.5, 0.5, 0.1
            ),
        ]
        tables = build_pivot_tables(results)
        assert len(tables) == 2
        assert tables[0].qos == 1
        assert tables[1].qos == 4

    def test_canonical_row_and_column_order(self) -> None:
        """Canonical rows come first, in the documented order."""
        results = [
            _synthetic_result(
                "quic-1000x100hz-qos4-multi", 99500, 100000, 100000.0, 99.5, 0.5, 0.1
            ),
            _synthetic_result(
                "custom-udp-100x100hz-qos4-multi", 9900, 10000, 10000.0, 99.0, 1.5, 0.4
            ),
        ]
        tables = build_pivot_tables(results)
        assert len(tables) == 1
        table = tables[0]
        # custom-udp-multi appears before quic-multi in the canonical
        # row order, even though quic was inserted first in `results`.
        assert ("custom-udp", "multi") in table.rows
        assert ("quic", "multi") in table.rows
        assert table.rows.index(("custom-udp", "multi")) < table.rows.index(
            ("quic", "multi")
        )

    def test_3x2_grid_synthetic(self) -> None:
        """Pivot grid with 3 rows and 2 columns has 6 populated cells."""
        results = [
            _synthetic_result(
                "custom-udp-100x100hz-qos4-multi", 9900, 10000, 10000.0, 99.0, 1.5, 0.4
            ),
            _synthetic_result(
                "custom-udp-1000x100hz-qos4-multi",
                99000,
                100000,
                100000.0,
                99.0,
                2.5,
                0.5,
            ),
            _synthetic_result(
                "hybrid-100x100hz-qos4-multi", 9800, 10000, 10000.0, 98.0, 1.8, 0.6
            ),
            _synthetic_result(
                "hybrid-1000x100hz-qos4-multi",
                97000,
                100000,
                100000.0,
                97.0,
                3.2,
                0.7,
            ),
            _synthetic_result(
                "quic-100x100hz-qos4-multi", 9990, 10000, 10000.0, 99.9, 0.3, 0.05
            ),
            _synthetic_result(
                "quic-1000x100hz-qos4-multi",
                99950,
                100000,
                100000.0,
                99.95,
                0.4,
                0.06,
            ),
        ]
        tables = build_pivot_tables(results)
        assert len(tables) == 1
        table = tables[0]
        assert len(table.rows) == 3  # custom-udp, hybrid, quic (all multi)
        assert len(table.columns) == 2  # 100x100hz, 1000x100hz
        assert len(table.cells) == 6
        # Sub-cells in the expected order: every cell has 3 sub-fields.
        for cell in table.cells.values():
            assert cell.delivery_pct is not None
            assert cell.ratio_pct is not None
            assert not math.isnan(cell.latency_mean_ms)
            assert not math.isnan(cell.latency_std_ms)

    def test_skips_unparseable_variants(self) -> None:
        """Variants that don't match the canonical shape are filtered out."""
        results = [
            _synthetic_result(
                "custom-udp-100x100hz-qos4-multi", 9900, 10000, 10000.0, 99.0, 1.5, 0.4
            ),
            # legacy / sentinel variant name without QoS+mode suffix
            _synthetic_result("dummy", 0, 0, None, None, float("nan"), float("nan")),
        ]
        tables = build_pivot_tables(results)
        assert len(tables) == 1
        # The dummy spawn does not contribute a row.
        assert all(family != "dummy" for family, _ in tables[0].rows)


class TestPivotRendering:
    def test_empty_cells_dont_crash(self) -> None:
        """A grid with mixed populated/empty cells renders without raising."""
        results = [
            _synthetic_result(
                "custom-udp-100x100hz-qos4-multi", 9900, 10000, 10000.0, 99.0, 1.5, 0.4
            ),
            # only one cell populated -- the others are empty
        ]
        tables = build_pivot_tables(results)
        rendered = format_pivot_table(tables[0])
        assert "QoS 4" in rendered
        assert "custom-udp-multi" in rendered
        # The 3 sub-cell lines all appear for the populated cell.
        assert "99.0%" in rendered  # delivery + ratio share the same value here
        # mean+/-std rendering
        assert "+/-" in rendered

    def test_max_workload_renders_na_for_ratio(self) -> None:
        results = [
            _synthetic_result(
                "custom-udp-max-qos4-multi", 50000, 50000, None, None, 5.0, 1.2
            ),
        ]
        tables = build_pivot_tables(results)
        rendered = format_pivot_table(tables[0])
        # The ratio cell should be n/a for max-workload
        assert "n/a" in rendered
        # But delivery and latency are still shown
        assert "100.0%" in rendered  # delivery
        assert "+/-" in rendered

    def test_format_pivot_section_no_data(self) -> None:
        rendered = format_pivot_section([])
        assert "Pivot Tables" in rendered
        assert "(no data)" in rendered

    def test_format_pivot_section_full(self) -> None:
        results = [
            _synthetic_result(
                "custom-udp-100x100hz-qos1-multi", 9500, 10000, 10000.0, 95.0, 1.2, 0.3
            ),
            _synthetic_result(
                "custom-udp-100x100hz-qos4-multi", 9900, 10000, 10000.0, 99.0, 1.5, 0.4
            ),
        ]
        rendered = format_pivot_section(results)
        # One header line + two QoS tables -> both QoS labels appear.
        assert "QoS 1" in rendered
        assert "QoS 4" in rendered
        # The format-documenting hint is present.
        assert "Delivery%" in rendered
        assert "Ratio%" in rendered


# --- CSV export -------------------------------------------------------------


class TestCsvExport:
    def test_round_trip(self) -> None:
        results = [
            _synthetic_result(
                "custom-udp-100x100hz-qos4-multi", 9900, 10000, 10000.0, 99.0, 1.5, 0.4
            ),
            _synthetic_result(
                "custom-udp-max-qos4-multi", 50000, 50000, None, None, 5.0, 1.2
            ),
        ]
        text = export_csv(results)
        rows = list(csv.DictReader(io.StringIO(text)))
        assert len(rows) == 2

        scalar = rows[0]
        assert scalar["variant"] == "custom-udp-100x100hz-qos4-multi"
        assert scalar["family"] == "custom-udp"
        # The variant name encodes vpt=100, hz=100 (100x100hz).
        assert scalar["values_per_tick"] == "100"
        assert scalar["tick_rate_hz"] == "100"
        assert scalar["qos"] == "4"
        assert scalar["threading_mode"] == "multi"
        assert scalar["workload_kind"] == "scalar-flood"
        assert scalar["expected_writes_per_sec"] == "10000.0"
        assert scalar["ratio_pct"] == "99.0"

        max_row = rows[1]
        assert max_row["workload_kind"] == "max-throughput"
        # Empty cells for nan / None values.
        assert max_row["expected_writes_per_sec"] == ""
        assert max_row["ratio_pct"] == ""

    def test_empty_input_emits_header_only(self) -> None:
        text = export_csv([])
        rows = list(csv.DictReader(io.StringIO(text)))
        assert rows == []
        # Header still present.
        assert text.startswith("variant,run,family,")

    def test_unparseable_variant_emits_blank_family(self) -> None:
        results = [
            _synthetic_result("dummy", 0, 0, None, None, float("nan"), float("nan")),
        ]
        text = export_csv(results)
        rows = list(csv.DictReader(io.StringIO(text)))
        assert len(rows) == 1
        assert rows[0]["variant"] == "dummy"
        assert rows[0]["family"] == ""
        assert rows[0]["qos"] == ""


# --- Smoke: directly construct PivotTable & verify dataclass surface --------


class TestPivotDataclasses:
    def test_pivot_cell_dataclass(self) -> None:
        cell = PivotCell(
            delivery_pct=99.0,
            ratio_pct=98.5,
            latency_mean_ms=1.2,
            latency_std_ms=0.3,
        )
        assert cell.delivery_pct == 99.0
        assert cell.ratio_pct == 98.5

    def test_pivot_table_dataclass(self) -> None:
        table = PivotTable(
            qos=4,
            rows=(("custom-udp", "multi"),),
            columns=("100x100hz",),
            cells={
                (("custom-udp", "multi"), "100x100hz"): PivotCell(99.0, 99.0, 1.2, 0.3),
            },
        )
        assert table.qos == 4
        assert table.rows[0] == ("custom-udp", "multi")
