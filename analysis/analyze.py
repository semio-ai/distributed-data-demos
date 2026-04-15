"""CLI entry point for the benchmark analysis tool.

Usage:
    python analyze.py <logs-dir> [--clear] [--summary] [--diagrams] [--output <dir>]

When neither --summary nor --diagrams is given, both are produced.
For Phase 1, --diagrams prints a placeholder message and skips.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from cache import load_and_update
from correlate import correlate
from integrity import verify_integrity
from performance import compute_performance
from tables import format_integrity_table, format_performance_table


def build_parser() -> argparse.ArgumentParser:
    """Build the CLI argument parser."""
    parser = argparse.ArgumentParser(
        description="Analyze benchmark JSONL logs: integrity verification "
        "and performance metrics.",
    )
    parser.add_argument(
        "logs_dir",
        type=Path,
        help="Directory containing .jsonl log files (and the pickle cache).",
    )
    parser.add_argument(
        "--clear",
        action="store_true",
        help="Delete the pickle cache and rebuild from all JSONL files.",
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
    return parser


def main(argv: list[str] | None = None) -> int:
    """Run the analysis tool."""
    parser = build_parser()
    args = parser.parse_args(argv)

    logs_dir: Path = args.logs_dir.resolve()
    if not logs_dir.is_dir():
        print(f"Error: {logs_dir} is not a directory.", file=sys.stderr)
        return 1

    # Determine what to produce
    do_summary = args.summary or (not args.summary and not args.diagrams)
    do_diagrams = args.diagrams or (not args.summary and not args.diagrams)

    # Output directory for diagrams
    output_dir: Path = args.output.resolve() if args.output else logs_dir / "analysis"

    # Step 1: Caching pipeline
    cache = load_and_update(logs_dir, clear=args.clear)
    events = cache.all_events()

    if not events:
        print("No events found in log files.", file=sys.stderr)
        return 1

    # Step 2: Correlation
    records = correlate(events)

    # Step 3: Summary tables
    if do_summary:
        integrity_results = verify_integrity(events, records)
        performance_results = compute_performance(events, records)

        print(format_integrity_table(integrity_results))
        print(format_performance_table(performance_results))

    # Step 4: Diagrams (Phase 1 placeholder)
    if do_diagrams:
        print(f"Diagrams not yet implemented (E5) -- output dir: {output_dir}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
