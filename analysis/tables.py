"""CLI summary table formatting for benchmark analysis."""

from __future__ import annotations

from integrity import IntegrityResult
from performance import PerformanceResult


def _pad(text: str, width: int) -> str:
    """Left-align text within a fixed width."""
    return text.ljust(width)


def _rpad(text: str, width: int) -> str:
    """Right-align text within a fixed width."""
    return text.rjust(width)


def _fmt_pct(value: float) -> str:
    """Format a percentage value."""
    return f"{value:.2f}%"


def _fmt_ms(value: float) -> str:
    """Format a millisecond value."""
    if value < 0.01:
        return f"{value:.4f}ms"
    if value < 1.0:
        return f"{value:.3f}ms"
    if value < 100.0:
        return f"{value:.2f}ms"
    return f"{value:.1f}ms"


def _fmt_rate(value: float) -> str:
    """Format a rate (events per second)."""
    if value >= 1000:
        return f"{value:,.0f}"
    return f"{value:.1f}"


def format_integrity_table(
    results: list[IntegrityResult],
    *,
    late_tail_groups: set[tuple[str, str]] | None = None,
) -> str:
    """Format the integrity report as a CLI table.

    Groups results by (variant, run) and shows one row per writer->receiver pair.

    ``late_tail_groups`` (T11.5): set of ``(variant, run)`` keys for
    which the corresponding ``PerformanceResult.late_receives_tail_pct``
    was > 0. Rows in those groups are annotated with
    ``[late_tail_present]`` so the integrity reader sees the same
    outlier signal that the performance table shows. ``None`` means
    no late-tail information available (legacy callers).
    """
    if not results:
        return "Integrity Report\n(no data)\n"

    lines: list[str] = []
    lines.append("Integrity Report")
    # Wider table to accommodate the ``BP-skip`` column added in
    # T-impl.6 (per-writer count of ``backpressure_skipped`` events),
    # the ``Timeout`` column added in T14.17 (per-spawn failure-cause
    # classification), and the ``Leaves Lost`` column added in T19.6
    # (E19 leaf-level loss accounting).
    sep = "-" * 185
    lines.append(sep)

    # Column widths
    w_variant = 22
    w_run = 16
    w_path = 20
    w_qos = 5
    w_sent = 8
    w_rcvd = 8
    w_deliv = 10
    w_ooo = 14
    w_dupes = 7
    w_gaps = 16
    w_bpskip = 12
    # T19.6 / E19: ``Leaves Lost`` is the scalar-leaf analogue of the
    # op-level loss surfaced by ``Delivery%``. For pre-E19 data where
    # ``leaf_count == 1`` everywhere this equals ``write_count -
    # receive_count``; for block-flood / mixed-types it can be many
    # times larger than the op-count gap. Column is wide enough to
    # render the comma-grouped int with no truncation up to ~9
    # significant digits.
    w_leaves_lost = 12
    # T14.17 / T15.6 / T15.11: ``Timeout`` column holds the longest
    # enum value (``variant_self_killed_idle`` = 25 chars) plus a
    # little padding.
    w_timeout = 27

    header = (
        _pad("Variant", w_variant)
        + _pad("Run", w_run)
        + _pad("Path", w_path)
        + _rpad("QoS", w_qos)
        + _rpad("Sent", w_sent)
        + _rpad("Rcvd", w_rcvd)
        + _rpad("Delivery%", w_deliv)
        + _rpad("Out-of-order", w_ooo)
        + _rpad("Dupes", w_dupes)
        + _rpad("Unresolved gaps", w_gaps)
        + _rpad("BP-skip", w_bpskip)
        + _rpad("Leaves Lost", w_leaves_lost)
        + "  "
        + _pad("Timeout", w_timeout)
    )
    lines.append(header)
    lines.append(sep)

    for r in results:
        path_str = f"{r.writer}->{r.receiver}"
        gaps_str = str(r.unresolved_gaps) if r.unresolved_gaps is not None else "-"

        # Mark errors with [FAIL] suffix
        errors: list[str] = []
        if r.completeness_error:
            errors.append("completeness")
        if r.ordering_error:
            errors.append("ordering")
        if r.duplicate_error:
            errors.append("duplicates")
        if r.gap_error:
            errors.append("gaps")
        # T17.9: ``backpressure_skipped`` at QoS 3/4 violates the
        # strict no-skip contract (DESIGN.md § 6.5). Surfaced as its
        # own annotation so the row makes it clear which contract was
        # broken -- separate from completeness/ordering/duplicates.
        if r.skip_at_reliable_error:
            errors.append("skip-at-reliable")

        # T14.17: build the timeout-classification cell. The base
        # enum value is left-aligned in the narrow column. Any
        # sub-tags are appended after the row's FAIL/late-tail
        # annotations so the column stays narrow.
        if r.timeout_sub_tags:
            sub_tag_suffix = " [" + ", ".join(r.timeout_sub_tags) + "]"
        else:
            sub_tag_suffix = ""

        row = (
            _pad(r.variant, w_variant)
            + _pad(r.run, w_run)
            + _pad(path_str, w_path)
            + _rpad(str(r.qos), w_qos)
            + _rpad(f"{r.write_count:,}", w_sent)
            + _rpad(f"{r.receive_count:,}", w_rcvd)
            + _rpad(_fmt_pct(r.delivery_pct), w_deliv)
            + _rpad(str(r.out_of_order), w_ooo)
            + _rpad(str(r.duplicates), w_dupes)
            + _rpad(gaps_str, w_gaps)
            + _rpad(f"{r.backpressure_skipped_count:,}", w_bpskip)
            + _rpad(f"{r.leaves_lost:,}", w_leaves_lost)
            + "  "
            + _pad(r.timeout_classification, w_timeout)
        )
        if errors:
            row += "  [FAIL: " + ", ".join(errors) + "]"
        if sub_tag_suffix:
            row += sub_tag_suffix
        # T11.5: flag rows whose (variant, run) carries a non-zero
        # late_receives_tail_pct from the performance side. This is
        # a notice, not an error -- it just calls out the latency-tail
        # outlier so the reader doesn't have to cross-reference the
        # performance table to see it.
        if late_tail_groups is not None and (r.variant, r.run) in late_tail_groups:
            row += "  [late_tail_present]"
        lines.append(row)

    lines.append("")
    return "\n".join(lines)


_UNCORRECTED_SUFFIX: str = " (uncorrected)"


def format_performance_table(results: list[PerformanceResult]) -> str:
    """Format the performance report as a CLI table.

    Column order (T11.5): receive throughput leads as the headline
    metric -- the project goal "keep peers in sync under huge change
    diffs" is bottlenecked by receivers, not writers, so the receive
    rate is what decides "in sync". Write rate is shown next as the
    "requested rate" context, followed by delivery percentage
    (receives / writes), then latency percentiles, jitter, loss and
    other existing columns. No metric is removed; only the ORDER and
    EMPHASIS change relative to pre-T11.5 output.

    When a ``PerformanceResult`` carries
    ``has_uncorrected_latency = True`` (at least one underlying delivery
    record had ``offset_applied == False`` because no ``clock_sync``
    measurement was available for the cross-runner pair), the row's
    latency cells are appended with ``(uncorrected)`` so the operator
    can tell at a glance that the cross-machine latency may be
    contaminated by clock skew. See E8 / clock-sync.md for the protocol.
    """
    if not results:
        return "Performance Report\n(no data)\n"

    lines: list[str] = []
    lines.append("Performance Report")
    # Widen the table to accommodate the (uncorrected) annotation on
    # any of the four latency cells, plus the new ``Late`` column, plus
    # the E19 / T19.5 ``Shape`` / ``Leaves/s`` / ``Bytes/s`` columns.
    sep = "-" * 250
    lines.append(sep)

    # Column widths. The latency columns are widened so that
    # "12.34ms (uncorrected)" still fits without ragging the rest of
    # the table.
    w_variant = 22
    w_run = 16
    w_thread = 8
    w_shape = 8
    w_rate = 14
    w_deliv = 11
    w_conn = 13
    w_lat = 25
    w_jitter = 12
    w_loss = 9
    w_late = 9
    w_tail = 11

    # Column layout (T19.5):
    # The headline ``Receives/s`` column (== ops/sec on the receive
    # side; also exposed as the canonical ``ops_per_sec`` field on
    # PerformanceResult) keeps its T11.5 leading position. ``Leaves/s``
    # and ``Bytes/s`` are E19 additions surfacing the workload-shape
    # throughput numbers introduced by the block-flood / mixed-types
    # workloads. ``Shape`` carries the dominant shape value for the
    # group (defaults to ``"scalar"`` for legacy / pre-E19 data).
    header = (
        _pad("Variant", w_variant)
        + _pad("Run", w_run)
        + _pad("Thread", w_thread)
        + _pad("Shape", w_shape)
        + _rpad("Receives/s", w_rate)
        + _rpad("Leaves/s", w_rate)
        + _rpad("Bytes/s", w_rate)
        + _rpad("Writes/s(req)", w_rate)
        + _rpad("Delivery%", w_deliv)
        + _rpad("Connect(ms)", w_conn)
        + _rpad("Lat p50", w_lat)
        + _rpad("p95", w_lat)
        + _rpad("p99", w_lat)
        + _rpad("Max", w_lat)
        + _rpad("Jitter avg", w_jitter)
        + _rpad("Jitter p95", w_jitter)
        + _rpad("Loss%", w_loss)
        + _rpad("Late", w_late)
        + _rpad("LateTail%", w_tail)
    )
    lines.append(header)
    lines.append(sep)

    for r in results:
        suffix = _UNCORRECTED_SUFFIX if r.has_uncorrected_latency else ""
        late_str = "-" if r.late_receives is None else f"{r.late_receives:,}"
        # Delivery % derived from existing throughput numbers. The
        # values themselves do not change vs pre-T11.5; only the
        # ordering does.
        if r.writes_per_sec > 0:
            delivery_pct = 100.0 * r.receives_per_sec / r.writes_per_sec
        else:
            delivery_pct = 0.0
        # T11.5 late-tail rendering: ``<count> (<pct>%)`` so a glance
        # at the column shows both the absolute outlier count and its
        # share of receives. When count is zero, render a plain ``0``
        # so the column doesn't clutter for well-behaved groups.
        if r.late_receives_tail_count == 0:
            tail_str = "0"
        else:
            tail_str = (
                f"{r.late_receives_tail_count:,} ({r.late_receives_tail_pct:.2f}%)"
            )
        row = (
            _pad(r.variant, w_variant)
            + _pad(r.run, w_run)
            + _pad(r.threading_mode, w_thread)
            + _pad(r.shape, w_shape)
            + _rpad(_fmt_rate(r.receives_per_sec), w_rate)
            + _rpad(_fmt_rate(r.leaves_per_sec), w_rate)
            + _rpad(_fmt_rate(r.bytes_per_sec), w_rate)
            + _rpad(_fmt_rate(r.writes_per_sec), w_rate)
            + _rpad(_fmt_pct(delivery_pct), w_deliv)
            + _rpad(f"{r.connect_mean_ms:.1f}", w_conn)
            + _rpad(_fmt_ms(r.latency_p50_ms) + suffix, w_lat)
            + _rpad(_fmt_ms(r.latency_p95_ms) + suffix, w_lat)
            + _rpad(_fmt_ms(r.latency_p99_ms) + suffix, w_lat)
            + _rpad(_fmt_ms(r.latency_max_ms) + suffix, w_lat)
            + _rpad(_fmt_ms(r.jitter_ms), w_jitter)
            + _rpad(_fmt_ms(r.jitter_p95_ms), w_jitter)
            + _rpad(_fmt_pct(r.loss_pct), w_loss)
            + _rpad(late_str, w_late)
            + _rpad(tail_str, w_tail)
        )
        lines.append(row)

    # Resource usage sub-table
    has_resources = any(r.resources for r in results)
    if has_resources:
        lines.append("")
        lines.append("Resource Usage")
        res_sep = "-" * 100
        lines.append(res_sep)
        w_runner = 12
        w_cpu = 12
        w_mem = 13

        res_header = (
            _pad("Variant", w_variant)
            + _pad("Run", w_run)
            + _pad("Runner", w_runner)
            + _rpad("Mean CPU%", w_cpu)
            + _rpad("Peak CPU%", w_cpu)
            + _rpad("Mean Mem(MB)", w_mem)
            + _rpad("Peak Mem(MB)", w_mem)
        )
        lines.append(res_header)
        lines.append(res_sep)

        for r in results:
            for res in r.resources:
                row = (
                    _pad(res.variant, w_variant)
                    + _pad(res.run, w_run)
                    + _pad(res.runner, w_runner)
                    + _rpad(f"{res.mean_cpu_pct:.1f}", w_cpu)
                    + _rpad(f"{res.peak_cpu_pct:.1f}", w_cpu)
                    + _rpad(f"{res.mean_memory_mb:.1f}", w_mem)
                    + _rpad(f"{res.peak_memory_mb:.1f}", w_mem)
                )
                lines.append(row)

    lines.append("")
    return "\n".join(lines)
