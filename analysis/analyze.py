"""CLI entry point for the benchmark analysis tool.

Usage::

    python analyze.py <logs-dir> [--clear] [--summary] [--diagrams] \\
                                 [--output <dir>] [--measure-peak-rss]

By default, both the CLI summary tables and comparison diagrams are
produced. Pass ``--summary`` or ``--diagrams`` to produce only one.

Phase 1.5 architecture:

1. Bring the per-shard Parquet cache up to date (see ``cache.py``).
2. Open a polars ``LazyFrame`` over the cache.
3. Discover ``(variant, run)`` groups.
4. For each group run correlation -> integrity -> performance, lazy
   end-to-end, materializing only the per-group delivery DataFrame.
5. Pass the aggregated dataclass results to ``tables.py`` / ``plots.py``
   exactly as Phase 1 did.
"""

from __future__ import annotations

import argparse
import re
import sys
import threading
import time
from pathlib import Path

from cache import discover_groups, scan_group, update_cache
from correlate import correlate_lazy
from incomplete_warnings import (
    collect_incomplete_warnings,
    emit_incomplete_warnings,
    format_incomplete_warnings,
)
from integrity import IntegrityResult, integrity_for_group
from performance import PerformanceResult, performance_for_group
from pivot_tables import export_csv, format_pivot_for_qos, format_pivot_section
from tables import format_integrity_table, format_performance_table


class _RSSSampler:
    """Background thread that polls ``psutil`` for peak RSS.

    Runs only when ``--measure-peak-rss`` is passed. The sampler thread
    polls every ``poll_interval_s`` seconds (default 0.2 s), tracking
    the maximum ``memory_info().rss`` observed. The main thread calls
    ``stop()`` after the analysis is done; the peak is reported on
    stderr.

    Default off so the steady-state pipeline stays
    instrumentation-free. ``psutil`` is only imported when the flag is
    set, so users without the optional dependency installed are not
    blocked from running the tool.
    """

    def __init__(self, poll_interval_s: float = 0.2) -> None:
        self.poll_interval_s = poll_interval_s
        self._peak_bytes = 0
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None
        self._proc = None

    def start(self) -> None:
        try:
            import psutil  # type: ignore[import-not-found]
        except ImportError as exc:
            print(
                "Error: --measure-peak-rss requires psutil. "
                "Install with: pip install psutil",
                file=sys.stderr,
            )
            raise SystemExit(1) from exc

        self._proc = psutil.Process()
        # Seed with the current RSS so an immediate stop still reports
        # something meaningful.
        self._peak_bytes = int(self._proc.memory_info().rss)
        self._thread = threading.Thread(
            target=self._run,
            name="rss-sampler",
            daemon=True,
        )
        self._thread.start()

    def _run(self) -> None:
        proc = self._proc
        if proc is None:
            return
        while not self._stop_event.is_set():
            try:
                rss = int(proc.memory_info().rss)
            except Exception:
                # If the process disappears or psutil errors out, just
                # stop sampling -- don't crash the analysis.
                return
            if rss > self._peak_bytes:
                self._peak_bytes = rss
            self._stop_event.wait(self.poll_interval_s)

    def stop(self) -> int:
        """Stop the sampler thread and return the peak RSS in bytes."""
        self._stop_event.set()
        if self._thread is not None:
            self._thread.join(timeout=2.0)
        # Capture one final reading after the join in case the peak
        # arrived between the last poll and the stop.
        if self._proc is not None:
            try:
                rss_now = int(self._proc.memory_info().rss)
                if rss_now > self._peak_bytes:
                    self._peak_bytes = rss_now
            except Exception:
                pass
        return self._peak_bytes


def build_parser() -> argparse.ArgumentParser:
    """Build the CLI argument parser."""
    parser = argparse.ArgumentParser(
        description="Analyze benchmark JSONL logs: integrity verification "
        "and performance metrics.",
    )
    parser.add_argument(
        "logs_dir",
        type=Path,
        help="Directory containing .jsonl log files.",
    )
    parser.add_argument(
        "--clear",
        action="store_true",
        help="Delete the .cache/ directory and rebuild from all JSONL files.",
    )
    parser.add_argument(
        "--summary",
        action="store_true",
        help="Print CLI summary tables only (no diagrams).",
    )
    parser.add_argument(
        "--diagrams",
        action="store_true",
        help="Generate diagrams only (no CLI output).",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Directory for generated diagrams and reports. "
        "Defaults to <logs-dir>/analysis/.",
    )
    parser.add_argument(
        "--measure-peak-rss",
        action="store_true",
        help="Sample process RSS every 200ms during the run and print "
        "the peak to stderr. Requires psutil. Default off so the "
        "steady-state pipeline stays instrumentation-free.",
    )
    parser.add_argument(
        "--csv-out",
        type=Path,
        default=None,
        help="Write a long-form CSV (one row per (variant, run)) with the "
        "pivot-table columns plus all existing PerformanceResult columns. "
        "Operators can pivot this in Excel/Sheets if they want a custom "
        "slice that the built-in pivot tables don't cover.",
    )
    parser.add_argument(
        "--dump",
        action="store_true",
        help="Write the full --summary output to a set of markdown files in "
        "the output directory (one file per section: integrity, performance, "
        "pivot per QoS, warnings, and an index). Implies --summary "
        "computation (the stdout summary print is unchanged).",
    )
    parser.add_argument(
        "--log-throughput",
        action="store_true",
        help="Render the throughput panels of comparison.png on a log "
        "y-axis. Useful when one variant (e.g. WebRTC max-qos4 at "
        "~400 K receives/s) dwarfs slower transports (e.g. custom-udp "
        "qos4 at ~10 K receives/s) in the same panel. Bars with zero "
        "receives are skipped (NaN) rather than clamped. Default off.",
    )
    return parser


def resolve_logs_dir(logs_dir: Path) -> Path:
    """If ``logs_dir`` has no .jsonl files, auto-select the latest sub-run."""
    if list(logs_dir.glob("*.jsonl")):
        return logs_dir
    candidates = sorted(
        d for d in logs_dir.iterdir() if d.is_dir() and list(d.glob("*.jsonl"))
    )
    if candidates:
        selected = candidates[-1]
        print(f"Auto-selected latest run: {selected.name}", file=sys.stderr)
        return selected
    return logs_dir


def run_analysis(
    logs_dir: Path,
    *,
    do_summary: bool,
) -> tuple[list[IntegrityResult], list[PerformanceResult]]:
    """Run the full per-group analysis pipeline.

    Returns (integrity_results, performance_results). Integrity is only
    computed when ``do_summary`` is true, since the diagrams currently
    do not consume integrity output.

    Per-group execution: each ``(variant, run)`` group is scanned only
    over the Parquet shards that belong to it (see
    ``cache.discover_groups``). This bounds the working set to one
    group at a time -- correlation, integrity and performance all
    operate on the per-group lazy frame.
    """
    groups = discover_groups(logs_dir)

    integrity_results: list[IntegrityResult] = []
    performance_results: list[PerformanceResult] = []

    for variant, run, shard_paths in groups:
        group = scan_group(shard_paths)

        # Materialize delivery records once per group.
        deliveries = correlate_lazy(group).collect()

        if do_summary:
            # T14.17: pass logs_dir/variant/run so integrity_for_group
            # can attach the per-spawn timeout_classification field
            # to each IntegrityResult row.
            integrity_results.extend(
                integrity_for_group(
                    group,
                    deliveries,
                    logs_dir=logs_dir,
                    variant=variant,
                    run=run,
                )
            )

        performance_results.append(
            performance_for_group(group, deliveries, variant, run)
        )

        # Free per-group materialized data before moving on.
        del deliveries

    return integrity_results, performance_results


def _md_section(title: str, context: str, body: str) -> str:
    """Render a single dump-file body: H1 title, context paragraph, fenced body.

    The body is wrapped in a ``text``-fenced code block so the existing
    monospace ASCII alignment renders correctly in markdown viewers
    without losing column information.
    """
    return f"# {title}\n\n{context}\n\n```text\n{body}\n```\n"


def _qos_rank_for_image_path(path: Path) -> int:
    """Return the integer QoS bucket for a per-QoS plot filename.

    Used to group ``comparison-qos<N>.png`` and
    ``latency-cdf-qos<N>.png`` under the same ``## QoS <N>`` header
    inside ``summary_performance.md``. Legacy ``qosNA`` files get a
    large sentinel so they sort last.
    """
    stem = path.stem
    # Pull out the ``qos<N>`` segment, allowing a trailing ``-log`` qualifier.
    m = re.search(r"-qos(\d+)(?:-log)?$", stem)
    if m is None:
        return 10**9
    return int(m.group(1))


def _build_performance_md(
    *,
    logs_dir: Path,
    performance_results: list[PerformanceResult],
    comparison_paths: list[Path] | None,
    cdf_paths: list[Path] | None,
    timestamp: str,
) -> str:
    """Build the ``summary_performance.md`` content.

    The body is: H1 title, context block, fenced performance table,
    and one ``## QoS <N>`` section per observed QoS embedding the
    matching comparison and CDF PNGs (when diagrams were generated).
    Image links are relative to the markdown file (so the file is
    portable as long as the PNGs sit alongside it -- the default
    ``output_dir`` layout). If a QoS has no diagram (diagrams were
    not generated this invocation) the image section is skipped
    silently so the file stays useful as a tables-only dump.
    """
    performance_body = format_performance_table(performance_results)
    ctx = (
        f"Dataset: `{logs_dir}`\n\n"
        f"Generated: {timestamp}\n\n"
        f"Rows: {len(performance_results)}"
    )
    body = _md_section("Performance Report", ctx, performance_body)

    if not comparison_paths and not cdf_paths:
        return body

    # Group PNG paths by QoS so the markdown renders one section per
    # observed QoS level. Comparison and CDF filenames share the same
    # ``-qos<N>`` segment so the same numeric rank groups them.
    grouped: dict[int, dict[str, Path]] = {}
    for p in comparison_paths or []:
        rank = _qos_rank_for_image_path(p)
        grouped.setdefault(rank, {})["comparison"] = p
    for p in cdf_paths or []:
        rank = _qos_rank_for_image_path(p)
        grouped.setdefault(rank, {})["cdf"] = p

    image_chunks: list[str] = []
    for rank in sorted(grouped.keys()):
        if rank >= 10**9:
            header = "## Legacy spawns (no QoS) - throughput, latency, CDF"
        else:
            header = f"## QoS {rank} - throughput, latency, CDF"
        lines = [header, ""]
        section = grouped[rank]
        if "comparison" in section:
            lines.append(f"![]({section['comparison'].name})")
            lines.append("")
        if "cdf" in section:
            lines.append(f"![]({section['cdf'].name})")
            lines.append("")
        image_chunks.append("\n".join(lines))

    return body + "\n" + "\n".join(image_chunks)


def _write_dump_files(
    *,
    output_dir: Path,
    logs_dir: Path,
    integrity_results: list[IntegrityResult],
    performance_results: list[PerformanceResult],
    late_tail_groups: set[tuple[str, str]],
    comparison_paths: list[Path] | None = None,
    cdf_paths: list[Path] | None = None,
) -> list[Path]:
    """Write the per-section markdown dump files into ``output_dir``.

    Returns the list of paths written, in the canonical order used by
    ``summary_index.md``. The summary stdout print is the caller's
    responsibility; this function only re-renders the same content into
    files (using the same formatter helpers) so the dump matches stdout
    byte-for-byte modulo the markdown frame.

    ``comparison_paths``/``cdf_paths`` are forwarded to
    ``_build_performance_md`` so the per-QoS PNGs generated by
    ``generate_comparison_plot``/``generate_latency_cdf_plot`` get
    embedded under per-QoS markdown headers in
    ``summary_performance.md`` -- T16.13.
    """
    output_dir.mkdir(parents=True, exist_ok=True)
    timestamp = time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime())

    integrity_body = format_integrity_table(
        integrity_results, late_tail_groups=late_tail_groups
    )

    integrity_ctx = (
        f"Dataset: `{logs_dir}`\n\n"
        f"Generated: {timestamp}\n\n"
        f"Rows: {len(integrity_results)}"
    )

    integrity_path = output_dir / "summary_integrity.md"
    integrity_path.write_text(
        _md_section("Integrity Report", integrity_ctx, integrity_body),
        encoding="utf-8",
    )

    performance_path = output_dir / "summary_performance.md"
    performance_path.write_text(
        _build_performance_md(
            logs_dir=logs_dir,
            performance_results=performance_results,
            comparison_paths=comparison_paths,
            cdf_paths=cdf_paths,
            timestamp=timestamp,
        ),
        encoding="utf-8",
    )

    pivot_paths: list[Path] = []
    for qos in (1, 2, 3, 4):
        pivot_body = format_pivot_for_qos(performance_results, qos)
        pivot_ctx = (
            f"Dataset: `{logs_dir}`\n\nGenerated: {timestamp}\n\nQoS level: {qos}"
        )
        pivot_path = output_dir / f"summary_pivot_qos{qos}.md"
        pivot_path.write_text(
            _md_section(f"Pivot Table (QoS {qos})", pivot_ctx, pivot_body),
            encoding="utf-8",
        )
        pivot_paths.append(pivot_path)

    # Warnings: re-use the same formatter the stderr emitter uses so the
    # dump always matches what was (or would have been) shown to the
    # operator. When no warnings fire, the file carries a single
    # explicit "no incomplete samples" line so the operator knows the
    # dump was generated and the run was clean (vs the file just being
    # missing).
    warnings = collect_incomplete_warnings(integrity_results, performance_results)
    warning_lines = format_incomplete_warnings(warnings)
    if not warning_lines:
        warnings_body = "No incomplete samples."
    else:
        warnings_body = "\n".join(warning_lines)
    warnings_ctx = (
        f"Dataset: `{logs_dir}`\n\n"
        f"Generated: {timestamp}\n\n"
        f"Total cases: {warnings.total_cases}"
    )
    warnings_path = output_dir / "summary_warnings.md"
    warnings_path.write_text(
        _md_section("Incomplete Sample Warnings", warnings_ctx, warnings_body),
        encoding="utf-8",
    )

    # Index file: title, brief context, bullet list of relative links.
    ordered_paths: list[Path] = [
        integrity_path,
        performance_path,
        *pivot_paths,
        warnings_path,
    ]
    bullets = "\n".join(f"- [{p.name}](./{p.name})" for p in ordered_paths)
    index_body = (
        f"# Summary Index\n\n"
        f"Dataset: `{logs_dir}`\n\n"
        f"Generated: {timestamp}\n\n"
        f"## Sections\n\n"
        f"{bullets}\n"
    )
    index_path = output_dir / "summary_index.md"
    index_path.write_text(index_body, encoding="utf-8")
    ordered_paths.append(index_path)
    return ordered_paths


def main(argv: list[str] | None = None) -> int:
    """Run the analysis tool."""
    parser = build_parser()
    args = parser.parse_args(argv)

    logs_dir: Path = args.logs_dir.resolve()
    if not logs_dir.is_dir():
        print(f"Error: {logs_dir} is not a directory.", file=sys.stderr)
        return 1

    logs_dir = resolve_logs_dir(logs_dir)

    any_flag = args.summary or args.diagrams
    do_summary = args.summary or not any_flag
    do_diagrams = args.diagrams or not any_flag

    # --dump is additive: it requires the summary computation but does
    # NOT suppress diagrams. If the user invoked --diagrams alone, we
    # still need to compute the summary so the dump has something to
    # write. The stdout summary print still gates on the original
    # ``do_summary`` decision (see below) so we don't regress the
    # diagrams-only behaviour for stdout output.
    if args.dump:
        do_summary = True

    output_dir: Path = args.output.resolve() if args.output else logs_dir / "analysis"

    sampler: _RSSSampler | None = None
    started_at: float | None = None
    if args.measure_peak_rss:
        sampler = _RSSSampler()
        sampler.start()
        started_at = time.monotonic()

    try:
        # Step 1: per-shard Parquet cache (build / refresh).
        update_cache(logs_dir, clear=args.clear)

        if not list((logs_dir / ".cache").glob("*.parquet")):
            print("No events found in log files.", file=sys.stderr)
            return 1

        # Step 2: per-(variant, run) lazy analysis. Force-enable the
        # integrity computation when --csv-out is requested without
        # --summary so the CSV gets the same data the pivot tables
        # would. Actually the CSV is computed from performance_results
        # alone (which is always populated), so no force is needed --
        # this comment documents the intent.
        integrity_results, performance_results = run_analysis(
            logs_dir, do_summary=do_summary
        )

        # T-pivot.4: long-form CSV export. Emitted before the summary
        # tables so that piping the CLI output to a file still leaves
        # the CSV file intact regardless of stdout truncation.
        if args.csv_out is not None:
            csv_text = export_csv(performance_results)
            args.csv_out.parent.mkdir(parents=True, exist_ok=True)
            args.csv_out.write_text(csv_text, encoding="utf-8")
            print(f"CSV export written to: {args.csv_out}", file=sys.stderr)

        # Step 3: summary tables.
        if do_summary:
            # T11.5: pass the (variant, run) keys of groups with a
            # non-zero late-tail percentage so the integrity rows can
            # carry a ``[late_tail_present]`` notice alongside the
            # performance-table column.
            late_tail_groups: set[tuple[str, str]] = {
                (p.variant, p.run)
                for p in performance_results
                if p.late_receives_tail_pct > 0
            }
            print(
                format_integrity_table(
                    integrity_results, late_tail_groups=late_tail_groups
                )
            )
            print(format_performance_table(performance_results))

            # T-pivot.3: variant x workload pivot tables, one per QoS
            # level. Rendered after the three flat reports so the
            # operator gets the existing scan first and then a
            # cross-cut view at the bottom.
            print(format_pivot_section(performance_results))

            # T14.21: surface job-run cases with incomplete samples
            # (non-completed spawn / delivery shortfall / late tail)
            # on stderr so the operator doesn't have to scan the
            # tables row-by-row. Exit code unchanged.
            emit_incomplete_warnings(integrity_results, performance_results)

        # Step 4: diagrams. Run before the markdown summary writes so
        # ``summary_performance.md`` can embed the generated PNGs
        # under per-QoS headers -- T16.13.
        comparison_paths: list[Path] = []
        cdf_paths: list[Path] = []
        if do_diagrams:
            try:
                from plots import generate_comparison_plot, generate_latency_cdf_plot
            except ImportError:
                print(
                    "Error: --diagrams requires matplotlib. "
                    "Install with: pip install matplotlib",
                    file=sys.stderr,
                )
                return 1

            # T16.13: comparison + CDF plots now return one PNG per
            # observed QoS level. Print each path so the operator
            # sees the full set without having to inspect the
            # analysis/ directory.
            comparison_paths = generate_comparison_plot(
                performance_results,
                output_dir,
                log_throughput=args.log_throughput,
            )
            for p in comparison_paths:
                print(f"Plot saved to: {p}", file=sys.stderr)

            cdf_paths = generate_latency_cdf_plot(performance_results, output_dir)
            for p in cdf_paths:
                print(f"Plot saved to: {p}", file=sys.stderr)

        # Step 5: markdown summary. ``summary_performance.md`` is the
        # operator's one-file walkthrough of the dataset: performance
        # table plus per-QoS image embeds. We write it whenever the
        # summary computation ran (regardless of --dump) so the
        # T16.13 acceptance test (--summary --diagrams produces
        # summary_performance.md with embedded images) passes without
        # the user having to remember --dump. Other dump sections
        # (integrity, pivot, warnings, index) still require --dump.
        if do_summary:
            output_dir.mkdir(parents=True, exist_ok=True)
            timestamp = time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime())
            performance_md_path = output_dir / "summary_performance.md"
            performance_md_path.write_text(
                _build_performance_md(
                    logs_dir=logs_dir,
                    performance_results=performance_results,
                    comparison_paths=comparison_paths if do_diagrams else None,
                    cdf_paths=cdf_paths if do_diagrams else None,
                    timestamp=timestamp,
                ),
                encoding="utf-8",
            )
            print(
                f"Summary written to: {performance_md_path}",
                file=sys.stderr,
            )

        # Step 5.5: full markdown dump. Runs after the stdout summary
        # print and the targeted summary_performance.md write so a
        # regression in the dump cannot block the operator from
        # seeing the tables. Always lands in ``output_dir``.
        if args.dump:
            late_tail_groups_for_dump: set[tuple[str, str]] = {
                (p.variant, p.run)
                for p in performance_results
                if p.late_receives_tail_pct > 0
            }
            dump_paths = _write_dump_files(
                output_dir=output_dir,
                logs_dir=logs_dir,
                integrity_results=integrity_results,
                performance_results=performance_results,
                late_tail_groups=late_tail_groups_for_dump,
                comparison_paths=comparison_paths if do_diagrams else None,
                cdf_paths=cdf_paths if do_diagrams else None,
            )
            print(
                f"Dump written: {len(dump_paths)} files in {output_dir}",
                file=sys.stderr,
            )

        return 0
    finally:
        if sampler is not None:
            peak_bytes = sampler.stop()
            elapsed = (
                f"{time.monotonic() - started_at:.2f}s"
                if started_at is not None
                else "n/a"
            )
            peak_mib = peak_bytes / (1024.0 * 1024.0)
            peak_gib = peak_bytes / (1024.0 * 1024.0 * 1024.0)
            print(
                f"[rss] peak={peak_bytes} bytes ({peak_mib:.1f} MiB / "
                f"{peak_gib:.3f} GiB) wall={elapsed}",
                file=sys.stderr,
            )


if __name__ == "__main__":
    sys.exit(main())
