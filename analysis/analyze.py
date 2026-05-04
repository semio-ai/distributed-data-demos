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
import sys
import threading
import time
from pathlib import Path

from cache import discover_groups, scan_group, update_cache
from correlate import correlate_lazy
from integrity import IntegrityResult, integrity_for_group
from performance import PerformanceResult, performance_for_group
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
            integrity_results.extend(integrity_for_group(group, deliveries))

        performance_results.append(
            performance_for_group(group, deliveries, variant, run)
        )

        # Free per-group materialized data before moving on.
        del deliveries

    return integrity_results, performance_results


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

        # Step 2: per-(variant, run) lazy analysis.
        integrity_results, performance_results = run_analysis(
            logs_dir, do_summary=do_summary
        )

        # Step 3: summary tables.
        if do_summary:
            print(format_integrity_table(integrity_results))
            print(format_performance_table(performance_results))

        # Step 4: diagrams.
        if do_diagrams:
            try:
                from plots import generate_comparison_plot
            except ImportError:
                print(
                    "Error: --diagrams requires matplotlib. "
                    "Install with: pip install matplotlib",
                    file=sys.stderr,
                )
                return 1

            plot_path = generate_comparison_plot(performance_results, output_dir)
            print(f"Plot saved to: {plot_path}", file=sys.stderr)

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
