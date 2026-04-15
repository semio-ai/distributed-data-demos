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


def format_integrity_table(results: list[IntegrityResult]) -> str:
    """Format the integrity report as a CLI table.

    Groups results by (variant, run) and shows one row per writer->receiver pair.
    """
    if not results:
        return "Integrity Report\n(no data)\n"

    lines: list[str] = []
    lines.append("Integrity Report")
    sep = "-" * 90
    lines.append(sep)

    # Column widths
    w_variant = 22
    w_run = 16
    w_path = 20
    w_qos = 5
    w_deliv = 10
    w_ooo = 14
    w_dupes = 7
    w_gaps = 16

    header = (
        _pad("Variant", w_variant)
        + _pad("Run", w_run)
        + _pad("Path", w_path)
        + _rpad("QoS", w_qos)
        + _rpad("Delivery%", w_deliv)
        + _rpad("Out-of-order", w_ooo)
        + _rpad("Dupes", w_dupes)
        + _rpad("Unresolved gaps", w_gaps)
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

        row = (
            _pad(r.variant, w_variant)
            + _pad(r.run, w_run)
            + _pad(path_str, w_path)
            + _rpad(str(r.qos), w_qos)
            + _rpad(_fmt_pct(r.delivery_pct), w_deliv)
            + _rpad(str(r.out_of_order), w_ooo)
            + _rpad(str(r.duplicates), w_dupes)
            + _rpad(gaps_str, w_gaps)
        )
        if errors:
            row += "  [FAIL: " + ", ".join(errors) + "]"
        lines.append(row)

    lines.append("")
    return "\n".join(lines)


def format_performance_table(results: list[PerformanceResult]) -> str:
    """Format the performance report as a CLI table."""
    if not results:
        return "Performance Report\n(no data)\n"

    lines: list[str] = []
    lines.append("Performance Report")
    sep = "-" * 110
    lines.append(sep)

    # Column widths
    w_variant = 22
    w_run = 16
    w_conn = 13
    w_lat = 12
    w_rate = 10
    w_jitter = 10
    w_loss = 8

    header = (
        _pad("Variant", w_variant)
        + _pad("Run", w_run)
        + _rpad("Connect(ms)", w_conn)
        + _rpad("Lat p50", w_lat)
        + _rpad("p95", w_lat)
        + _rpad("p99", w_lat)
        + _rpad("Max", w_lat)
        + _rpad("Writes/s", w_rate)
        + _rpad("Jitter", w_jitter)
        + _rpad("Loss%", w_loss)
    )
    lines.append(header)
    lines.append(sep)

    for r in results:
        row = (
            _pad(r.variant, w_variant)
            + _pad(r.run, w_run)
            + _rpad(f"{r.connect_mean_ms:.1f}", w_conn)
            + _rpad(_fmt_ms(r.latency_p50_ms), w_lat)
            + _rpad(_fmt_ms(r.latency_p95_ms), w_lat)
            + _rpad(_fmt_ms(r.latency_p99_ms), w_lat)
            + _rpad(_fmt_ms(r.latency_max_ms), w_lat)
            + _rpad(_fmt_rate(r.writes_per_sec), w_rate)
            + _rpad(_fmt_ms(r.jitter_ms), w_jitter)
            + _rpad(_fmt_pct(r.loss_pct), w_loss)
        )
        lines.append(row)

    # Resource usage sub-table
    has_resources = any(r.resources for r in results)
    if has_resources:
        lines.append("")
        lines.append("Resource Usage")
        res_sep = "-" * 80
        lines.append(res_sep)
        w_runner = 12
        w_cpu = 10
        w_mem = 12

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
