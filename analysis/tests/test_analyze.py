"""Tests for the ``analyze.py`` CLI driver.

Currently focused on the ``--measure-peak-rss`` flag added in T11.2.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

import pytest


_ANALYSIS_ROOT = Path(__file__).resolve().parent.parent
_ANALYZE_PY = _ANALYSIS_ROOT / "analyze.py"


def _has_psutil() -> bool:
    try:
        import psutil  # noqa: F401
    except ImportError:
        return False
    return True


@pytest.mark.skipif(
    not _has_psutil(),
    reason="--measure-peak-rss requires psutil",
)
class TestMeasurePeakRSSFlag:
    """End-to-end round-trip of the ``--measure-peak-rss`` flag."""

    def _run_cli(self, *extra_args: str, cwd: Path) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                sys.executable,
                str(_ANALYZE_PY),
                *extra_args,
            ],
            capture_output=True,
            text=True,
            cwd=str(cwd),
        )

    def test_flag_emits_peak_rss_line(self, tmp_logs: Path) -> None:
        proc = self._run_cli(
            str(tmp_logs),
            "--summary",
            "--measure-peak-rss",
            cwd=_ANALYSIS_ROOT,
        )
        assert proc.returncode == 0, proc.stderr
        # Reporter line on stderr.
        assert "[rss] peak=" in proc.stderr, proc.stderr
        # Peak must be a positive integer count of bytes.
        match = re.search(r"\[rss\] peak=(\d+) bytes", proc.stderr)
        assert match is not None, proc.stderr
        peak_bytes = int(match.group(1))
        assert peak_bytes > 0

    def test_flag_default_off_emits_no_peak_rss_line(self, tmp_logs: Path) -> None:
        proc = self._run_cli(
            str(tmp_logs),
            "--summary",
            cwd=_ANALYSIS_ROOT,
        )
        assert proc.returncode == 0, proc.stderr
        assert "[rss] peak=" not in proc.stderr

    def test_help_lists_flag(self) -> None:
        proc = self._run_cli("--help", cwd=_ANALYSIS_ROOT)
        assert proc.returncode == 0
        assert "--measure-peak-rss" in proc.stdout


class TestRSSSamplerUnit:
    """Direct unit cover on ``_RSSSampler`` so the round-trip can run
    even when ``psutil`` is missing in CI -- the round-trip suite skips,
    but the import-error path here does not require psutil."""

    def test_sampler_raises_clearly_without_psutil(
        self, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
    ) -> None:
        # Force the import to fail.
        import builtins

        real_import = builtins.__import__

        def fake_import(name: str, *args, **kwargs):
            if name == "psutil":
                raise ImportError("psutil not installed")
            return real_import(name, *args, **kwargs)

        monkeypatch.setattr(builtins, "__import__", fake_import)

        from analyze import _RSSSampler

        sampler = _RSSSampler()
        with pytest.raises(SystemExit) as excinfo:
            sampler.start()
        assert excinfo.value.code == 1
        captured = capsys.readouterr()
        assert "psutil" in captured.err
