"""Job-run "incomplete samples" warnings (T14.21).

After the integrity and performance tables are produced, scan the
results for any job-run case that hits one of three triggers and emit
``WARN:`` lines on stderr so operators don't have to scan the tables
row-by-row:

1. **Not completed**: ``IntegrityResult.timeout_classification`` is
   anything other than ``"completed"`` or ``"runner_idle_terminated"``
   (both are clean-exit classifications). Granularity: per
   ``(variant, run, writer)`` spawn -- a single non-completed spawn
   is reported once even if the integrity table has multiple
   ``(writer -> receiver)`` rows for it.
2. **Delivery shortfall**: ``IntegrityResult.delivery_pct < 100.0``,
   *including* loss-tolerant QoS 1 and 2. Granularity: per
   ``(variant, run, writer, receiver)`` integrity row.
3. **Late tail present**: ``PerformanceResult.late_receives_tail_pct
   > 0``. Granularity: per ``(variant, run)``.

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


@dataclass(frozen=True)
class _LateTailWarning:
    variant: str
    run: str
    late_tail_pct: float


@dataclass(frozen=True)
class IncompleteWarnings:
    """Collected warnings, grouped/ordered for stable rendering."""

    not_completed: list[_NotCompletedWarning]
    delivery_shortfall: list[_DeliveryShortfallWarning]
    late_tail: list[_LateTailWarning]

    @property
    def total_cases(self) -> int:
        return (
            len(self.not_completed) + len(self.delivery_shortfall) + len(self.late_tail)
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
    """
    not_completed: dict[tuple[str, str, str], _NotCompletedWarning] = {}
    delivery_shortfall: list[_DeliveryShortfallWarning] = []

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
        if r.delivery_pct < 100.0:
            delivery_shortfall.append(
                _DeliveryShortfallWarning(
                    variant=r.variant,
                    run=r.run,
                    writer=r.writer,
                    receiver=r.receiver,
                    qos=r.qos,
                    delivery_pct=r.delivery_pct,
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

    return IncompleteWarnings(
        not_completed=list(not_completed.values()),
        delivery_shortfall=delivery_shortfall,
        late_tail=late_tail,
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
            lines.append(
                f"WARN: [{w.variant} / {w.run}] {w.writer}->{w.receiver} qos{w.qos} "
                f"delivery {w.delivery_pct:.1f}% (<100.0%)"
            )
        for w in sorted(
            (w for w in warnings.late_tail if _group_key(w) == group),
            key=lambda x: x.late_tail_pct,
            reverse=True,
        ):
            lines.append(
                f"WARN: [{w.variant} / {w.run}] late-tail "
                f"{w.late_tail_pct:.2f}% of receives beyond 10x p99"
            )

    lines.append(
        f"WARN: {warnings.total_cases} job-run case(s) with incomplete samples "
        f"({len(warnings.not_completed)} not-completed, "
        f"{len(warnings.delivery_shortfall)} delivery shortfall, "
        f"{len(warnings.late_tail)} late tail)."
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
