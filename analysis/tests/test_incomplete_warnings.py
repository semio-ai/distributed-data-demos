"""Unit tests for T14.21 incomplete-samples warnings.

Synthetic ``IntegrityResult`` / ``PerformanceResult`` fixtures, no real
JSONL round-trip. Assertions use substring presence + line counts so
formatting tweaks don't break the suite.
"""

from __future__ import annotations

import pytest

from incomplete_warnings import (
    collect_incomplete_warnings,
    emit_incomplete_warnings,
    format_incomplete_warnings,
)
from integrity import IntegrityResult
from performance import PerformanceResult


def _ok_integrity(
    *,
    variant: str = "v",
    run: str = "r",
    writer: str = "alice",
    receiver: str = "bob",
    qos: int = 4,
    delivery_pct: float = 100.0,
    classification: str = "completed",
) -> IntegrityResult:
    """Build an IntegrityResult with sensible defaults for a clean row."""
    return IntegrityResult(
        variant=variant,
        run=run,
        writer=writer,
        receiver=receiver,
        qos=qos,
        write_count=100,
        receive_count=int(round(delivery_pct)),
        delivery_pct=delivery_pct,
        out_of_order=0,
        duplicates=0,
        unresolved_gaps=None,
        backpressure_skipped_count=0,
        completeness_error=False,
        ordering_error=False,
        duplicate_error=False,
        gap_error=False,
        timeout_classification=classification,
    )


def _ok_perf(
    *,
    variant: str = "v",
    run: str = "r",
    late_pct: float = 0.0,
) -> PerformanceResult:
    """Build a PerformanceResult with sensible defaults for a clean row."""
    return PerformanceResult(
        variant=variant,
        run=run,
        connect_mean_ms=10.0,
        connect_max_ms=20.0,
        latency_p50_ms=0.5,
        latency_p95_ms=1.0,
        latency_p99_ms=2.0,
        latency_max_ms=3.0,
        writes_per_sec=100.0,
        receives_per_sec=100.0,
        jitter_ms=0.1,
        jitter_p95_ms=0.2,
        loss_pct=0.0,
        late_receives_tail_pct=late_pct,
    )


def _warn_lines(captured: str) -> list[str]:
    """Filter captured stderr down to ``WARN:`` lines (lossless splitlines)."""
    return [ln for ln in captured.splitlines() if ln.startswith("WARN:")]


class TestCleanRun:
    """No triggers fire -> no output, no aggregate line."""

    def test_no_warnings_no_output(self, capsys: pytest.CaptureFixture) -> None:
        ints = [_ok_integrity()]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)

        out = capsys.readouterr()
        assert out.err == ""
        assert out.out == ""
        assert warnings.total_cases == 0


class TestNotCompletedSpawn:
    """Rule 1: timeout_classification != 'completed'."""

    def test_single_not_completed_spawn(self, capsys: pytest.CaptureFixture) -> None:
        ints = [
            _ok_integrity(classification="deadlock"),
        ]
        perfs = [_ok_perf()]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        # One per-case line + one aggregate line.
        assert len(lines) == 2
        case_line, agg_line = lines
        assert "not completed" in case_line
        assert "deadlock" in case_line
        assert "1 not-completed" in agg_line
        assert "0 delivery shortfall" in agg_line
        assert "0 late tail" in agg_line

    def test_spawn_with_two_receivers_dedupes(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """Rule 1 collapses to one warning per (variant, run, writer)."""
        ints = [
            _ok_integrity(
                receiver="bob", classification="eot_lost", delivery_pct=100.0
            ),
            _ok_integrity(
                receiver="carol", classification="eot_lost", delivery_pct=100.0
            ),
        ]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        # One per-spawn warning + aggregate; not two.
        assert len(warnings.not_completed) == 1
        assert len(lines) == 2
        assert "1 not-completed" in lines[-1]


class TestDeliveryShortfall:
    """Rule 2: delivery_pct < 100.0 on ANY QoS, including 1 and 2."""

    @pytest.mark.parametrize("qos", [1, 2, 3, 4])
    def test_shortfall_each_qos(self, capsys: pytest.CaptureFixture, qos: int) -> None:
        ints = [_ok_integrity(qos=qos, delivery_pct=87.3)]
        perfs = [_ok_perf()]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(lines) == 2
        case_line, agg_line = lines
        assert f"qos{qos}" in case_line
        assert "87.3%" in case_line
        assert "1 delivery shortfall" in agg_line

    def test_shortfall_includes_loss_tolerant_qos1(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """Loss-tolerant QoS must NOT be filtered out (spec rule 2)."""
        ints = [_ok_integrity(qos=1, delivery_pct=42.0)]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(warnings.delivery_shortfall) == 1
        # The case line must surface the qos1 row.
        assert any("qos1" in ln and "42.0" in ln for ln in lines)

    def test_two_receivers_both_shortfall_produces_two_warnings(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        ints = [
            _ok_integrity(receiver="bob", qos=3, delivery_pct=95.0),
            _ok_integrity(receiver="carol", qos=3, delivery_pct=90.0),
        ]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(warnings.delivery_shortfall) == 2
        # Two per-row warnings + aggregate.
        assert len(lines) == 3
        assert "2 delivery shortfall" in lines[-1]


class TestLateTail:
    """Rule 3: PerformanceResult.late_receives_tail_pct > 0."""

    def test_single_late_tail(self, capsys: pytest.CaptureFixture) -> None:
        ints = [_ok_integrity()]
        perfs = [_ok_perf(late_pct=0.42)]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(lines) == 2
        assert "late-tail" in lines[0]
        assert "0.42" in lines[0]
        assert "1 late tail" in lines[-1]


class TestCombinedCase:
    """All three rules firing for one (variant, run) -> grouped output."""

    def test_grouping_and_aggregate_counts(self, capsys: pytest.CaptureFixture) -> None:
        ints = [
            _ok_integrity(
                variant="v1",
                run="r1",
                writer="alice",
                receiver="bob",
                qos=2,
                delivery_pct=80.0,
                classification="eot_lost",
            ),
        ]
        perfs = [_ok_perf(variant="v1", run="r1", late_pct=1.5)]

        emit_incomplete_warnings(ints, perfs)
        all_lines = _warn_lines(capsys.readouterr().err)

        # 1 not-completed + 1 shortfall + 1 late tail + 1 aggregate.
        assert len(all_lines) == 4

        case_lines = all_lines[:-1]
        agg_line = all_lines[-1]

        # All case lines must share the same (variant, run) tag --
        # grouping puts adjacent.
        for ln in case_lines:
            assert "[v1 / r1]" in ln

        # Within the group, rule 1 (not-completed) -> rule 2
        # (delivery) -> rule 3 (late tail).
        assert "not completed" in case_lines[0]
        assert "delivery" in case_lines[1]
        assert "late-tail" in case_lines[2]

        # Aggregate counts.
        assert "3 job-run case(s)" in agg_line
        assert "1 not-completed" in agg_line
        assert "1 delivery shortfall" in agg_line
        assert "1 late tail" in agg_line


class TestGroupingAcrossMultipleRuns:
    """Multiple groups -> warnings clustered per (variant, run)."""

    def test_two_groups_clustered(self, capsys: pytest.CaptureFixture) -> None:
        ints = [
            _ok_integrity(variant="v1", run="r1", classification="deadlock"),
            _ok_integrity(variant="v2", run="r1", delivery_pct=70.0),
        ]
        perfs = [
            _ok_perf(variant="v1", run="r1"),
            _ok_perf(variant="v2", run="r1"),
        ]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        # 2 case warnings + aggregate.
        assert len(lines) == 3

        # Sorted by (variant, run): v1/r1 first, then v2/r1.
        assert "[v1 / r1]" in lines[0]
        assert "[v2 / r1]" in lines[1]


class TestStdoutSilent:
    """Warnings must not bleed onto stdout."""

    def test_only_stderr_used(self, capsys: pytest.CaptureFixture) -> None:
        ints = [_ok_integrity(classification="unknown")]
        perfs = [_ok_perf()]

        emit_incomplete_warnings(ints, perfs)
        out = capsys.readouterr()

        assert out.out == ""
        assert "WARN:" in out.err


class TestCollectorUnit:
    """The pure collector (no I/O) is independently exercised."""

    def test_collector_returns_structured_results(self) -> None:
        ints = [
            _ok_integrity(classification="eot_lost"),
            _ok_integrity(receiver="carol", delivery_pct=99.0),
        ]
        perfs = [_ok_perf(late_pct=0.1)]

        warnings = collect_incomplete_warnings(ints, perfs)

        assert len(warnings.not_completed) == 1
        assert len(warnings.delivery_shortfall) == 1
        assert len(warnings.late_tail) == 1
        assert warnings.total_cases == 3

    def test_formatter_empty_on_clean(self) -> None:
        warnings = collect_incomplete_warnings([_ok_integrity()], [_ok_perf()])
        assert format_incomplete_warnings(warnings) == []
