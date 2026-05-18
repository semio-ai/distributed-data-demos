"""Job-run "incomplete samples" warnings (T14.21).

After the integrity and performance tables are produced, scan the
results for any job-run case that hits one of four triggers and emit
``WARN:`` lines on stderr so operators don't have to scan the tables
row-by-row:

1. **Not completed**: ``IntegrityResult.timeout_classification`` is
   anything other than ``"completed"`` or ``"runner_idle_terminated"``
   (both are clean-exit classifications). Granularity: per
   ``(variant, run, writer)`` spawn -- a single non-completed spawn
   is reported once even if the integrity table has multiple
   ``(writer -> receiver)`` rows for it.
2. **Delivery shortfall**: ``IntegrityResult.delivery_pct < 99.995``
   (i.e. anything that would round to ``100.00%`` at 2-decimal display
   precision is treated as complete for warning purposes; the
   ``[FAIL: completeness]`` annotation on the integrity table row is
   unaffected by this threshold). *Including* loss-tolerant QoS 1 and
   2. Granularity: per ``(variant, run, writer, receiver)`` integrity
   row.
3. **Late tail present**: ``PerformanceResult.late_receives_tail_pct
   > 0``. Granularity: per ``(variant, run)``.
4. **Skip-at-reliable (T17.9)**: any ``backpressure_skipped`` event
   emitted at QoS 3 or QoS 4. Per ``DESIGN.md`` § 6.5 ("Strict
   No-Skip Contract for QoS 3/4") the variant MUST block the
   publish call at QoS 3/4 -- a skipped value is a contract
   violation. Granularity: per ``(variant, run, writer, qos)``.

The warnings are grouped by ``(variant, run)`` so an operator can see
the full picture for a given run at a glance. A final aggregate line is
emitted summarising counts per rule. When no triggers fire, nothing is
emitted (no noise on clean runs). The function never changes process
exit code -- a warning is not an error.
"""

from __future__ import annotations

import sys
from dataclasses import dataclass
from typing import TextIO

from integrity import IntegrityResult
from performance import PerformanceResult


@dataclass(frozen=True)
class _NotCompletedWarning:
    variant: str
    run: str
    writer: str
    classification: str


@dataclass(frozen=True)
class _DeliveryShortfallWarning:
    variant: str
    run: str
    writer: str
    receiver: str
    qos: int
    delivery_pct: float
    # T16.7: writer-side shortfall (Ratio%) parsed from the spawn name.
    # ``100 * receives_per_sec / expected_writes_per_sec`` where
    # ``expected_writes_per_sec = tick_rate_hz * values_per_tick``.
    # ``None`` when the spawn name does not parse (e.g. the
    # ``max-throughput`` workload has no nominal rate) -- the formatter
    # omits the annotation entirely in that case.
    ratio_pct: float | None


@dataclass(frozen=True)
class _LateTailWarning:
    variant: str
    run: str
    late_tail_pct: float


@dataclass(frozen=True)
class _SkipAtReliableWarning:
    """T17.9: ``backpressure_skipped`` event at QoS 3/4 (contract violation).

    Per ``DESIGN.md`` § 6.5 ("Strict No-Skip Contract for QoS 3/4")
    and ``api-contracts/jsonl-log-schema.md``, ``backpressure_skipped``
    is valid only at QoS 1/2. Any non-zero count at QoS 3/4 means a
    variant skipped a publish where the contract requires it to block.
    Granularity: per ``(variant, run, writer, qos)`` -- the same
    writer can have separate violations at QoS 3 and QoS 4 in the
    same run.
    """

    variant: str
    run: str
    writer: str
    qos: int
    count: int


@dataclass(frozen=True)
class IncompleteWarnings:
    """Collected warnings, grouped/ordered for stable rendering."""

    not_completed: list[_NotCompletedWarning]
    delivery_shortfall: list[_DeliveryShortfallWarning]
    late_tail: list[_LateTailWarning]
    skip_at_reliable: list[_SkipAtReliableWarning]

    @property
    def total_cases(self) -> int:
        return (
            len(self.not_completed)
            + len(self.delivery_shortfall)
            + len(self.late_tail)
            + len(self.skip_at_reliable)
        )


def collect_incomplete_warnings(
    integrity_results: list[IntegrityResult],
    performance_results: list[PerformanceResult],
) -> IncompleteWarnings:
    """Walk results, collecting offending cases for the three rules.

    Rule 1 deduplicates across receivers: each ``(variant, run, writer)``
    spawn produces at most one ``not_completed`` warning even if the
    integrity table contains multiple ``(writer -> receiver)`` rows for
    the same writer-side spawn.

    Rule 2 (T16.7) attaches the writer-side Ratio% parsed from the
    matching ``PerformanceResult`` (one per ``(variant, run)``). When
    no matching ``PerformanceResult`` exists or its
    ``receives_to_expected_ratio_pct`` is ``None`` (e.g. max-throughput
    workload), the ratio annotation is dropped by the formatter.
    """
    not_completed: dict[tuple[str, str, str], _NotCompletedWarning] = {}
    delivery_shortfall: list[_DeliveryShortfallWarning] = []

    # Build a (variant, run) -> ratio_pct lookup so the per-integrity-row
    # delivery shortfall can pick up the writer-side Ratio% without
    # duplicating the pivot-table formula. ``PerformanceResult`` already
    # carries ``receives_to_expected_ratio_pct`` computed from the same
    # spawn-name parser used by ``pivot_tables`` (see performance.py).
    ratio_by_group: dict[tuple[str, str], float | None] = {
        (p.variant, p.run): p.receives_to_expected_ratio_pct
        for p in performance_results
    }

    # Both "completed" (peer-confirmed E12 handshake) and the new
    # "runner_idle_terminated" (T15.6; E15 variant-side idle-detection
    # path) are clean-exit classifications -- neither triggers a
    # not-completed warning.
    _CLEAN_EXIT_CLASSIFICATIONS = frozenset({"completed", "runner_idle_terminated"})

    for r in integrity_results:
        # Rule 1: spawn did not finish gracefully.
        if r.timeout_classification not in _CLEAN_EXIT_CLASSIFICATIONS:
            key = (r.variant, r.run, r.writer)
            # Keep the first occurrence -- they all share the same
            # writer-side classification by construction (T14.17).
            not_completed.setdefault(
                key,
                _NotCompletedWarning(
                    variant=r.variant,
                    run=r.run,
                    writer=r.writer,
                    classification=r.timeout_classification,
                ),
            )

        # Rule 2: delivery shortfall on ANY QoS (including 1, 2).
        # Threshold is 99.995% (not 100.0%) so rows that round to
        # ``100.00%`` at 2-decimal display precision -- e.g.
        # 99.99988% (875999/876000) -- don't fire a warning that would
        # be reported alongside an integrity table row already showing
        # ``100.00%`` (T16.1). The integrity table's
        # ``[FAIL: completeness]`` annotation is independent and
        # unaffected.
        if r.delivery_pct < 99.995:
            delivery_shortfall.append(
                _DeliveryShortfallWarning(
                    variant=r.variant,
                    run=r.run,
                    writer=r.writer,
                    receiver=r.receiver,
                    qos=r.qos,
                    delivery_pct=r.delivery_pct,
                    ratio_pct=ratio_by_group.get((r.variant, r.run)),
                )
            )

    late_tail: list[_LateTailWarning] = []
    for p in performance_results:
        if p.late_receives_tail_pct > 0:
            late_tail.append(
                _LateTailWarning(
                    variant=p.variant,
                    run=p.run,
                    late_tail_pct=p.late_receives_tail_pct,
                )
            )

    # T17.9: collect skip-at-reliable contract violations. Dedupe by
    # ``(variant, run, writer, qos)`` -- the same writer's count is
    # replicated onto every (writer -> receiver) integrity row, but
    # we want to emit one WARN line per writer/qos, not per receiver.
    skip_at_reliable_index: dict[tuple[str, str, str, int], _SkipAtReliableWarning] = {}
    for r in integrity_results:
        if r.skip_at_reliable_error and r.skip_at_reliable_count > 0:
            key = (r.variant, r.run, r.writer, r.qos)
            skip_at_reliable_index.setdefault(
                key,
                _SkipAtReliableWarning(
                    variant=r.variant,
                    run=r.run,
                    writer=r.writer,
                    qos=r.qos,
                    count=r.skip_at_reliable_count,
                ),
            )

    return IncompleteWarnings(
        not_completed=list(not_completed.values()),
        delivery_shortfall=delivery_shortfall,
        late_tail=late_tail,
        skip_at_reliable=list(skip_at_reliable_index.values()),
    )


def _group_key(item: object) -> tuple[str, str]:
    """Return the ``(variant, run)`` group key for any warning record."""
    # All three warning dataclasses carry ``variant`` and ``run``.
    return (getattr(item, "variant"), getattr(item, "run"))


def format_incomplete_warnings(warnings: IncompleteWarnings) -> list[str]:
    """Render warnings as a list of stderr-ready lines.

    Returns an empty list when no triggers fired (caller emits nothing,
    no aggregate line, no noise).

    Lines are grouped by ``(variant, run)``; within a group rule 1
    (not-completed spawns) is printed first, then rule 2 (delivery
    shortfalls), then rule 3 (late tail). The aggregate line is always
    last.
    """
    if warnings.total_cases == 0:
        return []

    # Collect every ``(variant, run)`` group that has at least one
    # warning, sorted for stable output.
    groups: set[tuple[str, str]] = set()
    for w in warnings.not_completed:
        groups.add(_group_key(w))
    for w in warnings.delivery_shortfall:
        groups.add(_group_key(w))
    for w in warnings.late_tail:
        groups.add(_group_key(w))
    for w in warnings.skip_at_reliable:
        groups.add(_group_key(w))

    lines: list[str] = []
    for group in sorted(groups):
        for w in sorted(
            (w for w in warnings.not_completed if _group_key(w) == group),
            key=lambda x: x.writer,
        ):
            lines.append(
                f"WARN: [{w.variant} / {w.run}] spawn {w.writer!r} not completed "
                f"(classification={w.classification})"
            )
        for w in sorted(
            (w for w in warnings.delivery_shortfall if _group_key(w) == group),
            key=lambda x: (x.writer, x.receiver, x.qos),
        ):
            # Format with 2 decimals to match the integrity table's
            # display precision -- a 1-decimal format would render a
            # legitimate ``99.99%`` row as the misleading ``100.0%
            # (<100.0%)`` (T16.1). The trigger threshold (99.995%)
            # already suppresses rows that round to ``100.00%`` here.
            #
            # T16.7: append the writer-side Ratio% when it parses AND is
            # low enough to be load-bearing context. A low ratio means
            # the writer didn't attempt the requested rate, which is a
            # qualitatively different failure mode from a transport
            # losing packets at line rate. We only annotate below 50%
            # so healthy-but-shy-of-100% rows don't get a noisy ``ratio
            # 99.x%`` tag. ``None`` ratio (unparsable spawn name or
            # max-throughput workload with no nominal rate) -> omit
            # entirely (no ``ratio n/a`` noise).
            line = (
                f"WARN: [{w.variant} / {w.run}] {w.writer}->{w.receiver} qos{w.qos} "
                f"delivery {w.delivery_pct:.2f}% (<100%)"
            )
            if w.ratio_pct is not None and w.ratio_pct < 50.0:
                line += f" ratio {w.ratio_pct:.1f}% (writer-side shortfall)"
            lines.append(line)
        for w in sorted(
            (w for w in warnings.late_tail if _group_key(w) == group),
            key=lambda x: x.late_tail_pct,
            reverse=True,
        ):
            lines.append(
                f"WARN: [{w.variant} / {w.run}] late-tail "
                f"{w.late_tail_pct:.2f}% of receives beyond 10x p99"
            )
        # T17.9: skip-at-reliable contract violation. Rendered last
        # within the group so the line stands out and references the
        # design contract explicitly. ``writer`` is included so the
        # operator can find the spawn in the integrity table.
        for w in sorted(
            (w for w in warnings.skip_at_reliable if _group_key(w) == group),
            key=lambda x: (x.writer, x.qos),
        ):
            lines.append(
                f"WARN: [{w.variant} / {w.run}] {w.writer} {w.count} "
                f"backpressure_skipped events at qos{w.qos} -- "
                f"contract violation per DESIGN.md § 6.5"
            )

    lines.append(
        f"WARN: {warnings.total_cases} job-run case(s) with incomplete samples "
        f"({len(warnings.not_completed)} not-completed, "
        f"{len(warnings.delivery_shortfall)} delivery shortfall, "
        f"{len(warnings.late_tail)} late tail, "
        f"{len(warnings.skip_at_reliable)} skip-at-reliable)."
    )
    return lines


def emit_incomplete_warnings(
    integrity_results: list[IntegrityResult],
    performance_results: list[PerformanceResult],
    *,
    stream: TextIO | None = None,
) -> IncompleteWarnings:
    """Collect, render and write incomplete-samples warnings to ``stream``.

    ``stream`` defaults to ``sys.stderr``. On a clean run nothing is
    written. The collected :class:`IncompleteWarnings` is returned so
    callers (and tests) can introspect it without re-parsing the
    stream output.
    """
    out = sys.stderr if stream is None else stream
    warnings = collect_incomplete_warnings(integrity_results, performance_results)
    for line in format_incomplete_warnings(warnings):
        print(line, file=out)
    return warnings
