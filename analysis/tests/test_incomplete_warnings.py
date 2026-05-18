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
    skip_at_reliable_count: int = 0,
    skip_at_reliable_error: bool = False,
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
        skip_at_reliable_count=skip_at_reliable_count,
        skip_at_reliable_error=skip_at_reliable_error,
        timeout_classification=classification,
    )


def _ok_perf(
    *,
    variant: str = "v",
    run: str = "r",
    late_pct: float = 0.0,
    expected_writes_per_sec: float | None = None,
    receives_to_expected_ratio_pct: float | None = None,
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
        expected_writes_per_sec=expected_writes_per_sec,
        receives_to_expected_ratio_pct=receives_to_expected_ratio_pct,
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
        assert "87.30%" in case_line
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

    def test_rounds_to_100_does_not_trigger(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """T16.1: rows that round to ``100.00%`` at 2-decimal display
        precision are treated as complete; no warning is emitted.

        99.999% rounds to ``100.00%`` in the integrity table, so the
        warning ``delivery 100.0% (<100.0%)`` would be pure noise.
        """
        ints = [_ok_integrity(qos=3, delivery_pct=99.999)]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(warnings.delivery_shortfall) == 0
        assert lines == []

    def test_99_pct_still_triggers(self, capsys: pytest.CaptureFixture) -> None:
        """T16.1: well below the new threshold still fires (regression
        guard against an over-eager threshold lift)."""
        ints = [_ok_integrity(qos=3, delivery_pct=99.0)]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(warnings.delivery_shortfall) == 1
        # Per-row warning + aggregate.
        assert len(lines) == 2
        assert "99.00%" in lines[0]

    def test_threshold_boundary(self, capsys: pytest.CaptureFixture) -> None:
        """Just under 99.995 triggers; at/above does not."""
        # Just below threshold -> warn.
        ints_below = [_ok_integrity(qos=3, delivery_pct=99.994)]
        warnings_below = emit_incomplete_warnings(ints_below, [_ok_perf()])
        capsys.readouterr()  # drain.
        assert len(warnings_below.delivery_shortfall) == 1

        # Exactly at threshold -> no warn (it would round to 100.00%).
        ints_at = [_ok_integrity(qos=3, delivery_pct=99.995)]
        warnings_at = emit_incomplete_warnings(ints_at, [_ok_perf()])
        capsys.readouterr()
        assert len(warnings_at.delivery_shortfall) == 0


class TestDeliveryShortfallRatioAnnotation:
    """T16.7: surface writer-side Ratio% on delivery-shortfall warnings."""

    def test_ratio_below_50_annotates(self, capsys: pytest.CaptureFixture) -> None:
        """Spawn parses, ratio < 50% -> annotation appears on the line."""
        ints = [
            _ok_integrity(
                variant="websocket-1000x100hz-qos3-multi",
                run="all-variants-01",
                qos=3,
                delivery_pct=99.50,
            ),
        ]
        perfs = [
            _ok_perf(
                variant="websocket-1000x100hz-qos3-multi",
                run="all-variants-01",
                expected_writes_per_sec=100_000.0,
                receives_to_expected_ratio_pct=9.3,
            ),
        ]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        case_line = lines[0]
        assert "delivery 99.50% (<100%)" in case_line
        assert "ratio 9.3%" in case_line
        assert "writer-side shortfall" in case_line

    def test_ratio_at_or_above_50_no_annotation(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """Spawn parses, ratio >= 50% -> NO annotation (avoid noise)."""
        ints = [
            _ok_integrity(
                variant="custom-udp-100x100hz-qos3-multi",
                run="r",
                qos=3,
                delivery_pct=95.00,
            ),
        ]
        perfs = [
            _ok_perf(
                variant="custom-udp-100x100hz-qos3-multi",
                run="r",
                expected_writes_per_sec=10_000.0,
                receives_to_expected_ratio_pct=87.5,
            ),
        ]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        case_line = lines[0]
        assert "delivery 95.00% (<100%)" in case_line
        assert "writer-side shortfall" not in case_line
        assert "ratio" not in case_line

    def test_ratio_exactly_50_no_annotation(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """Boundary: ratio == 50% -> NO annotation (strict ``<`` cutoff)."""
        ints = [
            _ok_integrity(
                variant="custom-udp-100x100hz-qos3-multi",
                run="r",
                qos=3,
                delivery_pct=95.00,
            ),
        ]
        perfs = [
            _ok_perf(
                variant="custom-udp-100x100hz-qos3-multi",
                run="r",
                expected_writes_per_sec=10_000.0,
                receives_to_expected_ratio_pct=50.0,
            ),
        ]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert "writer-side shortfall" not in lines[0]

    def test_max_throughput_spawn_no_annotation(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """``max-throughput`` workload has no nominal rate -> no annotation.

        ``receives_to_expected_ratio_pct`` is ``None`` for max-throughput
        spawns (see performance.py); the formatter must NOT render
        ``ratio n/a`` (per the T16.7 spec: omit entirely).
        """
        ints = [
            _ok_integrity(
                variant="custom-udp-max-qos3-multi",
                run="r",
                qos=3,
                delivery_pct=50.0,
            ),
        ]
        perfs = [
            _ok_perf(
                variant="custom-udp-max-qos3-multi",
                run="r",
                expected_writes_per_sec=None,
                receives_to_expected_ratio_pct=None,
            ),
        ]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        case_line = lines[0]
        assert "delivery 50.00% (<100%)" in case_line
        assert "ratio" not in case_line
        assert "writer-side shortfall" not in case_line
        assert "n/a" not in case_line

    def test_unparsable_spawn_no_annotation(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """Legacy / unparsable spawn name + no perf ratio -> no annotation."""
        ints = [
            _ok_integrity(variant="legacy-spawn", run="r", qos=3, delivery_pct=70.0),
        ]
        perfs = [
            _ok_perf(
                variant="legacy-spawn",
                run="r",
                expected_writes_per_sec=None,
                receives_to_expected_ratio_pct=None,
            ),
        ]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert "ratio" not in lines[0]
        assert "writer-side shortfall" not in lines[0]


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


class TestSkipAtReliable:
    """T17.9: rule 4 -- ``backpressure_skipped`` at QoS 3/4.

    The integrity row's ``skip_at_reliable_error`` flag drives the
    warning. The warning's wording must explicitly cite DESIGN.md § 6.5
    so operators can find the contract.
    """

    @pytest.mark.parametrize("qos", [3, 4])
    def test_violation_emits_warning(
        self, capsys: pytest.CaptureFixture, qos: int
    ) -> None:
        ints = [
            _ok_integrity(
                qos=qos,
                skip_at_reliable_count=5,
                skip_at_reliable_error=True,
            )
        ]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(warnings.skip_at_reliable) == 1
        # Per-violation line + aggregate.
        assert len(lines) == 2
        case_line, agg_line = lines
        assert f"qos{qos}" in case_line
        assert "5 backpressure_skipped" in case_line
        assert "contract violation" in case_line
        assert "DESIGN.md" in case_line
        assert "6.5" in case_line
        assert "1 skip-at-reliable" in agg_line

    @pytest.mark.parametrize("qos", [1, 2])
    def test_qos_one_or_two_skip_emits_no_violation(
        self, capsys: pytest.CaptureFixture, qos: int
    ) -> None:
        """At QoS 1/2 ``backpressure_skipped`` is the contractual signal,
        not a violation. The warning rule must stay silent.

        We construct an integrity row that *would* fire the warning if
        the rule had been keyed on raw ``backpressure_skipped_count``;
        the row's ``skip_at_reliable_*`` fields are the correct gating
        signal and stay False/0.
        """
        ints = [
            _ok_integrity(
                qos=qos,
                skip_at_reliable_count=0,
                skip_at_reliable_error=False,
            )
        ]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        out = capsys.readouterr()

        assert len(warnings.skip_at_reliable) == 0
        assert warnings.total_cases == 0
        assert out.err == ""

    def test_multiple_receivers_deduped_to_one_per_writer_qos(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """The same (writer, qos) appearing on two integrity rows
        (writer publishes to two receivers) emits ONE WARN line, not
        two -- the violation is a property of the writer's publish
        path, not the (writer, receiver) pair.
        """
        ints = [
            _ok_integrity(
                receiver="bob",
                qos=4,
                skip_at_reliable_count=3,
                skip_at_reliable_error=True,
            ),
            _ok_integrity(
                receiver="carol",
                qos=4,
                skip_at_reliable_count=3,
                skip_at_reliable_error=True,
            ),
        ]
        perfs = [_ok_perf()]

        warnings = emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        assert len(warnings.skip_at_reliable) == 1
        # 1 warning + 1 aggregate.
        assert len(lines) == 2
        assert "1 skip-at-reliable" in lines[-1]

    def test_warning_grouped_with_other_warnings_for_same_run(
        self, capsys: pytest.CaptureFixture
    ) -> None:
        """A skip-at-reliable warning lands inside the same
        ``[variant / run]`` block as other warnings for that group.
        """
        ints = [
            _ok_integrity(
                variant="v1",
                run="r1",
                qos=3,
                delivery_pct=80.0,
                skip_at_reliable_count=7,
                skip_at_reliable_error=True,
            )
        ]
        perfs = [_ok_perf(variant="v1", run="r1")]

        emit_incomplete_warnings(ints, perfs)
        lines = _warn_lines(capsys.readouterr().err)

        # 1 delivery shortfall + 1 skip-at-reliable + aggregate.
        assert len(lines) == 3
        for ln in lines[:-1]:
            assert "[v1 / r1]" in ln
        # Skip-at-reliable is the last per-case line (after delivery).
        assert "DESIGN.md" in lines[1]
        agg = lines[-1]
        assert "1 delivery shortfall" in agg
        assert "1 skip-at-reliable" in agg

    def test_clean_run_aggregate_still_mentions_skip_at_reliable_count(
        self,
    ) -> None:
        """Sanity: the aggregate string includes the new bucket even
        when zero -- format_incomplete_warnings returns empty on clean,
        but collect returns a structured zero result that callers can
        inspect.
        """
        warnings = collect_incomplete_warnings([_ok_integrity()], [_ok_perf()])
        assert warnings.skip_at_reliable == []
        assert warnings.total_cases == 0
