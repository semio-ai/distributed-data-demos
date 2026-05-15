"""Tests for the ``analyze --dump`` flag.

Validates that ``--dump`` is additive (stdout summary still printed),
writes one markdown file per summary section into the resolved output
directory, links them from ``summary_index.md`` and tags each section
with its H1 title.

Uses the existing ``tmp_logs`` fixture (see ``conftest.py``) for a
minimal two-runner scenario that exercises the full summary pipeline.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

_ANALYSIS_ROOT = Path(__file__).resolve().parent.parent
if str(_ANALYSIS_ROOT) not in sys.path:
    sys.path.insert(0, str(_ANALYSIS_ROOT))

from analyze import main  # noqa: E402


_EXPECTED_FILES: tuple[str, ...] = (
    "summary_integrity.md",
    "summary_performance.md",
    "summary_pivot_qos1.md",
    "summary_pivot_qos2.md",
    "summary_pivot_qos3.md",
    "summary_pivot_qos4.md",
    "summary_warnings.md",
    "summary_index.md",
)


_EXPECTED_H1: dict[str, str] = {
    "summary_integrity.md": "# Integrity Report",
    "summary_performance.md": "# Performance Report",
    "summary_pivot_qos1.md": "# Pivot Table (QoS 1)",
    "summary_pivot_qos2.md": "# Pivot Table (QoS 2)",
    "summary_pivot_qos3.md": "# Pivot Table (QoS 3)",
    "summary_pivot_qos4.md": "# Pivot Table (QoS 4)",
    "summary_warnings.md": "# Incomplete Sample Warnings",
    "summary_index.md": "# Summary Index",
}


class TestDumpFlag:
    """End-to-end coverage of ``--dump`` writing per-section markdown files."""

    def test_dump_writes_every_section_file(
        self, tmp_logs: Path, tmp_path: Path, capsys: pytest.CaptureFixture
    ) -> None:
        out_dir = tmp_path / "out"
        rc = main([str(tmp_logs), "--dump", "--output", str(out_dir)])
        assert rc == 0
        # Confirm the stdout summary print is still happening (additive
        # contract: --dump does not regress the stdout output).
        captured = capsys.readouterr()
        assert "Integrity Report" in captured.out
        assert "Performance Report" in captured.out

        for name in _EXPECTED_FILES:
            assert (out_dir / name).is_file(), f"missing dump file {name}"

    def test_each_file_carries_expected_h1(
        self, tmp_logs: Path, tmp_path: Path
    ) -> None:
        out_dir = tmp_path / "out"
        rc = main([str(tmp_logs), "--dump", "--output", str(out_dir)])
        assert rc == 0
        for name, expected_h1 in _EXPECTED_H1.items():
            content = (out_dir / name).read_text(encoding="utf-8")
            assert content.startswith(expected_h1), (
                f"{name} does not start with {expected_h1!r}; got {content[:80]!r}"
            )

    def test_index_references_every_section_file(
        self, tmp_logs: Path, tmp_path: Path
    ) -> None:
        out_dir = tmp_path / "out"
        rc = main([str(tmp_logs), "--dump", "--output", str(out_dir)])
        assert rc == 0
        index = (out_dir / "summary_index.md").read_text(encoding="utf-8")
        # The index must list every other section file as a relative
        # link so the operator can navigate the dump in any markdown
        # viewer.
        for name in _EXPECTED_FILES:
            if name == "summary_index.md":
                continue
            assert f"./{name}" in index, f"index missing link to {name}"

    def test_dump_with_diagrams_only_still_produces_dump(
        self, tmp_logs: Path, tmp_path: Path
    ) -> None:
        """``--diagrams --dump`` must force summary computation for the dump.

        Brief: "If --diagrams was passed without --summary, that's fine:
        --dump should still cause the summary computation."
        """
        out_dir = tmp_path / "out"
        rc = main(
            [
                str(tmp_logs),
                "--diagrams",
                "--dump",
                "--output",
                str(out_dir),
            ]
        )
        assert rc == 0
        # The dump files exist regardless of which output flag was used.
        for name in _EXPECTED_FILES:
            assert (out_dir / name).is_file(), f"missing dump file {name}"

    def test_warnings_file_clean_run_states_no_incomplete(self, tmp_path: Path) -> None:
        """On a clean run the warnings file must carry the explicit no-warning line.

        Build a minimal logs dir with no writes / no integrity rows --
        the warnings collector sees zero offending cases and the dump
        writer takes the ``No incomplete samples.`` branch. We do not
        run ``--diagrams`` on this fixture because the throughput plot
        rejects an empty (all-zero) dataset; the dump path is what we
        exercise here.
        """
        # Use the same helpers the canonical conftest fixture uses so
        # the fixture stays in sync with the JSONL schema.
        from helpers import (  # type: ignore[import-not-found]
            _ts,
            make_event,
            write_jsonl,
        )

        clean_out = tmp_path / "out"
        clean_logs = tmp_path / "logs"
        clean_logs.mkdir()

        events = [
            make_event("phase", runner="alice", phase="connect", offset_ms=0),
            make_event(
                "connected",
                runner="alice",
                launch_ts=_ts(-10),
                elapsed_ms=10.0,
                offset_ms=10,
            ),
            make_event("phase", runner="alice", phase="operate", offset_ms=100),
            make_event("phase", runner="alice", phase="silent", offset_ms=200),
        ]
        write_jsonl(clean_logs / "test-variant-alice-run01.jsonl", events)

        rc = main(
            [
                str(clean_logs),
                "--summary",
                "--dump",
                "--output",
                str(clean_out),
            ]
        )
        assert rc == 0
        body = (clean_out / "summary_warnings.md").read_text(encoding="utf-8")
        assert "No incomplete samples." in body

    def test_no_dump_flag_writes_no_dump_files(
        self, tmp_logs: Path, tmp_path: Path
    ) -> None:
        """Without ``--dump`` the dump files must not be created."""
        out_dir = tmp_path / "out"
        rc = main([str(tmp_logs), "--summary", "--output", str(out_dir)])
        assert rc == 0
        for name in _EXPECTED_FILES:
            assert not (out_dir / name).exists(), (
                f"{name} should not exist without --dump"
            )
