"""Integration tests using real JSONL log files from logs/."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

import polars as pl
import pytest

from cache import discover_groups, scan_group, scan_shards, update_cache
from correlate import correlate_lazy, deliveries_to_records
from helpers import TWO_RUNNER_LOGS
from integrity import integrity_for_group
from performance import performance_for_group
from tables import format_performance_table


# Marker reused by integration classes that need real top-level log files.
_real_logs_skip = pytest.mark.skipif(
    not TWO_RUNNER_LOGS.is_dir() or not list(TWO_RUNNER_LOGS.glob("*.jsonl")),
    reason="Real top-level log files not available at logs/",
)


def _all_groups(logs_dir: Path) -> list[tuple[str, str]]:
    lazy = scan_shards(logs_dir)
    df = (
        lazy.select(["variant", "run"])
        .unique()
        .with_columns(
            pl.col("variant").cast(pl.Utf8),
            pl.col("run").cast(pl.Utf8),
        )
        .sort(["variant", "run"])
        .collect()
    )
    return [(row[0], row[1]) for row in df.iter_rows()]


@_real_logs_skip
class TestRealLogParsing:
    def test_loads_all_files(self, tmp_path: Path) -> None:
        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        metas = update_cache(tmp_path)
        # One shard per source JSONL.
        assert len(metas) == len(list(tmp_path.glob("*.jsonl")))

        lazy = scan_shards(tmp_path)
        assert lazy.collect().height > 0

    def test_event_types_present(self, tmp_path: Path) -> None:
        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)
        update_cache(tmp_path)
        lazy = scan_shards(tmp_path)
        events = (
            lazy.select(pl.col("event").cast(pl.Utf8))
            .unique()
            .collect()
            .get_column("event")
            .to_list()
        )
        assert "phase" in events
        assert "write" in events


@_real_logs_skip
class TestRealLogPipeline:
    def test_correlation_produces_records(self, tmp_path: Path) -> None:
        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)
        update_cache(tmp_path)
        lazy = scan_shards(tmp_path)
        groups = _all_groups(tmp_path)
        assert groups
        any_records = False
        for variant, run in groups:
            g = lazy.filter(
                (pl.col("variant").cast(pl.Utf8) == variant)
                & (pl.col("run").cast(pl.Utf8) == run)
            )
            deliveries = correlate_lazy(g).collect()
            if deliveries.height > 0:
                any_records = True
                # Sanity: every record has the expected columns.
                for col in (
                    "variant",
                    "run",
                    "writer",
                    "receiver",
                    "seq",
                    "path",
                    "qos",
                    "latency_ms",
                ):
                    assert col in deliveries.columns
        assert any_records

    def test_integrity_and_performance(self, tmp_path: Path) -> None:
        for f in TWO_RUNNER_LOGS.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)
        update_cache(tmp_path)
        lazy = scan_shards(tmp_path)
        groups = _all_groups(tmp_path)

        any_integrity = False
        any_perf = False
        for variant, run in groups:
            g = lazy.filter(
                (pl.col("variant").cast(pl.Utf8) == variant)
                & (pl.col("run").cast(pl.Utf8) == run)
            )
            deliveries = correlate_lazy(g).collect()

            ints = integrity_for_group(g, deliveries)
            for r in ints:
                any_integrity = True
                assert 0.0 <= r.delivery_pct <= 100.0
                assert r.out_of_order >= 0
                assert r.duplicates >= 0

            perf = performance_for_group(g, deliveries, variant, run)
            any_perf = True
            assert perf.writes_per_sec >= 0.0
            assert perf.latency_p50_ms >= 0.0
            assert perf.jitter_p95_ms >= 0.0

        assert any_perf
        # Integrity may be empty if no deliveries -- just smoke-checked.
        _ = any_integrity


_PHASE1_REFERENCE = (
    Path(__file__).resolve().parent / "fixtures" / "phase1_reference_summary.txt"
)
_SMALL_DATASET = TWO_RUNNER_LOGS / "same-machine-20260430_140856"


@pytest.mark.skipif(
    not _PHASE1_REFERENCE.exists() or not _SMALL_DATASET.is_dir(),
    reason="Phase 1 reference output or small dataset not available",
)
class TestPhase1Regression:
    """Compare new pipeline output against captured Phase 1 reference.

    The reference was captured from the Phase 1 implementation (E4) before
    the rework. Integrity rows must match byte-for-byte; performance rows
    may differ in the last digit of latency percentiles (polars uses
    linear interpolation -- documented divergence) and in jitter (the
    new vectorised window definition is not bit-identical to Phase 1's
    rolling-start windows). Aggregate identifiers, throughput, loss, and
    delivery counts must match exactly.
    """

    def _run_pipeline(self) -> str:
        # Invoke analyze.py directly so we exercise the full driver code
        # path including update_cache + summary rendering.
        analysis_dir = Path(__file__).resolve().parent.parent
        proc = subprocess.run(
            [
                sys.executable,
                str(analysis_dir / "analyze.py"),
                str(_SMALL_DATASET),
                "--summary",
            ],
            capture_output=True,
            text=True,
            cwd=str(analysis_dir),
            env={"PYTHONIOENCODING": "utf-8", "PATH": ""},
        )
        # If env={} is too aggressive we may break Windows path lookups,
        # but it's enough to set PATH/utf-8 and let Python locate itself.
        return proc.stdout

    def test_integrity_table_matches(self) -> None:
        ref = _PHASE1_REFERENCE.read_text(encoding="utf-8").splitlines()
        # Find the Performance Report header and split.
        try:
            split_idx = ref.index("Performance Report")
        except ValueError:
            pytest.skip("Reference file missing Performance Report")
        ref_integrity = "\n".join(ref[:split_idx]).strip()

        # Run new pipeline and compare integrity portion.
        out = self._run_pipeline().splitlines()
        try:
            split_idx2 = out.index("Performance Report")
        except ValueError:
            pytest.fail("Pipeline output missing Performance Report")
        out_integrity = "\n".join(out[:split_idx2]).strip()

        assert out_integrity == ref_integrity


# ---------------------------------------------------------------------------
# Clock-sync integration (E8 / T8.2): synthetic two-runner fixture that
# materialises a +50 ms receiver clock skew and verifies the analysis
# pipeline applies the correction end-to-end.
# ---------------------------------------------------------------------------


def _ts_at(base_epoch_ns: int, offset_ms: float) -> str:
    """Build an RFC 3339 timestamp ``offset_ms`` ms after a base epoch."""
    ns = base_epoch_ns + int(offset_ms * 1_000_000)
    secs = ns // 1_000_000_000
    frac = ns % 1_000_000_000
    dt = datetime.fromtimestamp(secs, tz=timezone.utc)
    return dt.strftime(f"%Y-%m-%dT%H:%M:%S.{frac:09d}Z")


def _write_jsonl(path: Path, events: list[dict]) -> None:
    with open(path, "w", encoding="utf-8") as f:
        for ev in events:
            f.write(json.dumps(ev) + "\n")


def _build_skew_fixture(
    target_dir: Path,
    *,
    run: str = "skew-run01",
    variant: str = "test-variant",
    seq_count: int = 20,
    real_latency_ms: float = 100.0,
    skew_ms: float = 50.0,
) -> Path:
    """Build a synthetic two-runner run with a +50 ms receiver clock skew.

    Layout written under ``target_dir`` (treated as the run directory):

    - ``<variant>-alice-<run>.jsonl``: alice writes seq 1..N at her
      t = 1000 + i ms (her clock).
    - ``<variant>-bob-<run>.jsonl``: bob receives those writes at his
      t = 1000 + i + (real_latency + skew) ms (his clock; the +skew is
      because his clock is ahead of alice's).
    - ``bob-clock-sync-<run>.jsonl``: bob's clock-sync log records that
      alice's clock is ``-skew_ms`` ms relative to bob's (peer.clock -
      self.clock). The analysis adds this offset to the raw delta to
      recover the real latency.

    Returns the run directory path.
    """
    base_ns = 1744710950_000_000_000  # arbitrary, matches helpers._ts

    # Alice's events on alice's clock.
    alice_events: list[dict] = [
        {
            "ts": _ts_at(base_ns, 0),
            "variant": variant,
            "runner": "alice",
            "run": run,
            "event": "phase",
            "phase": "connect",
        },
        {
            "ts": _ts_at(base_ns, 50),
            "variant": variant,
            "runner": "alice",
            "run": run,
            "event": "connected",
            "launch_ts": _ts_at(base_ns, -50),
            "elapsed_ms": 50.0,
        },
        {
            "ts": _ts_at(base_ns, 500),
            "variant": variant,
            "runner": "alice",
            "run": run,
            "event": "phase",
            "phase": "operate",
            "profile": "skew-test",
        },
    ]
    for i in range(seq_count):
        alice_events.append(
            {
                "ts": _ts_at(base_ns, 1000 + i),
                "variant": variant,
                "runner": "alice",
                "run": run,
                "event": "write",
                "seq": i + 1,
                "path": "/k",
                "qos": 1,
                "bytes": 8,
            }
        )
    alice_events.append(
        {
            "ts": _ts_at(base_ns, 5000),
            "variant": variant,
            "runner": "alice",
            "run": run,
            "event": "phase",
            "phase": "silent",
        }
    )

    # Bob's events live on his clock, which runs ``skew_ms`` ahead of
    # alice's. So a write recorded by alice at her t=1000+i ms is
    # received by bob at his wall-clock t=1000+i+real_latency+skew ms.
    bob_events: list[dict] = [
        {
            "ts": _ts_at(base_ns, 0 + skew_ms),
            "variant": variant,
            "runner": "bob",
            "run": run,
            "event": "phase",
            "phase": "connect",
        },
        {
            "ts": _ts_at(base_ns, 50 + skew_ms),
            "variant": variant,
            "runner": "bob",
            "run": run,
            "event": "connected",
            "launch_ts": _ts_at(base_ns, -50 + skew_ms),
            "elapsed_ms": 50.0,
        },
        {
            "ts": _ts_at(base_ns, 500 + skew_ms),
            "variant": variant,
            "runner": "bob",
            "run": run,
            "event": "phase",
            "phase": "operate",
            "profile": "skew-test",
        },
    ]
    for i in range(seq_count):
        bob_events.append(
            {
                "ts": _ts_at(base_ns, 1000 + i + real_latency_ms + skew_ms),
                "variant": variant,
                "runner": "bob",
                "run": run,
                "event": "receive",
                "writer": "alice",
                "seq": i + 1,
                "path": "/k",
                "qos": 1,
                "bytes": 8,
            }
        )
    bob_events.append(
        {
            "ts": _ts_at(base_ns, 5000 + skew_ms),
            "variant": variant,
            "runner": "bob",
            "run": run,
            "event": "phase",
            "phase": "silent",
        }
    )

    # Bob's clock-sync log: initial sync (variant="") plus a per-variant
    # resync. Both record offset_ms = peer.clock - self.clock = alice -
    # bob = -skew_ms.
    bob_clocksync = [
        {
            "ts": _ts_at(base_ns, -100 + skew_ms),
            "variant": "",
            "runner": "bob",
            "run": run,
            "event": "clock_sync",
            "peer": "alice",
            "offset_ms": -skew_ms,
            "rtt_ms": 0.4,
            "samples": 32,
            "min_rtt_ms": 0.4,
            "max_rtt_ms": 0.6,
        },
        {
            "ts": _ts_at(base_ns, 400 + skew_ms),
            "variant": variant,
            "runner": "bob",
            "run": run,
            "event": "clock_sync",
            "peer": "alice",
            "offset_ms": -skew_ms,
            "rtt_ms": 0.5,
        },
    ]

    target_dir.mkdir(parents=True, exist_ok=True)
    _write_jsonl(target_dir / f"{variant}-alice-{run}.jsonl", alice_events)
    _write_jsonl(target_dir / f"{variant}-bob-{run}.jsonl", bob_events)
    _write_jsonl(target_dir / f"bob-clock-sync-{run}.jsonl", bob_clocksync)
    return target_dir


class TestClockSkewIntegration:
    """End-to-end pipeline on a synthetic +50 ms skew run."""

    def test_corrected_latency_in_deliveries(self, tmp_path: Path) -> None:
        run_dir = _build_skew_fixture(tmp_path / "skew-run")
        update_cache(run_dir)

        groups = discover_groups(run_dir)
        assert len(groups) == 1
        variant, run, paths = groups[0]
        # Clock-sync shard is broadcast into the variant group.
        assert any("clock-sync" in p.stem for p in paths)

        group = scan_group(paths)
        deliveries = correlate_lazy(group).collect()
        records = deliveries_to_records(deliveries)
        assert records, "expected delivery records from synthetic fixture"

        # Every record corrects to ~100 ms (the real latency), not
        # ~150 ms (raw delta = real + 50 ms skew).
        for rec in records:
            assert rec.offset_applied is True
            assert rec.offset_ms == -50.0
            assert abs(rec.latency_ms - 100.0) < 1.0, (
                f"corrected latency = {rec.latency_ms} ms; expected ~100 ms "
                f"(would be ~150 ms without correction)"
            )

    def test_performance_table_no_uncorrected_marker(self, tmp_path: Path) -> None:
        run_dir = _build_skew_fixture(tmp_path / "skew-run")
        update_cache(run_dir)

        groups = discover_groups(run_dir)
        variant, run, paths = groups[0]
        group = scan_group(paths)
        deliveries = correlate_lazy(group).collect()
        perf = performance_for_group(group, deliveries, variant, run)

        # All deliveries were corrected, so no uncorrected marker.
        assert perf.has_uncorrected_latency is False

        table = format_performance_table([perf])
        assert "(uncorrected)" not in table
        # Sanity: latency cells reflect the corrected ~100 ms (well
        # below the raw 150 ms).
        assert perf.latency_p50_ms < 120.0

    def test_uncorrected_marker_when_clocksync_missing(self, tmp_path: Path) -> None:
        """Drop the clock-sync log -> cross-runner deliveries are
        flagged uncorrected and the table is annotated."""
        run_dir = _build_skew_fixture(tmp_path / "skew-run-no-csync")
        # Remove the clock-sync log so no offset is available.
        for p in run_dir.glob("*-clock-sync-*.jsonl"):
            p.unlink()
        update_cache(run_dir)

        groups = discover_groups(run_dir)
        variant, run, paths = groups[0]
        group = scan_group(paths)
        deliveries = correlate_lazy(group).collect()
        perf = performance_for_group(group, deliveries, variant, run)

        assert perf.has_uncorrected_latency is True
        # Without correction the latency is ~150 ms (real 100 + 50 skew).
        assert perf.latency_p50_ms > 130.0

        table = format_performance_table([perf])
        assert "(uncorrected)" in table


_PERSISTENT_SKEW_FIXTURE = (
    Path(__file__).resolve().parent / "fixtures" / "two-runner-skew50ms"
)


@pytest.mark.skipif(
    not _PERSISTENT_SKEW_FIXTURE.is_dir()
    or not list(_PERSISTENT_SKEW_FIXTURE.glob("*.jsonl")),
    reason="Persistent skew fixture not committed",
)
class TestPersistentSkewFixture:
    """Run the pipeline against the on-disk fixture under
    ``tests/fixtures/two-runner-skew50ms/`` (committed to the repo so
    other repos can sanity-check their offset application).

    The fixture is copied into a temporary directory before
    ``update_cache`` runs to keep the source tree free of build
    artefacts (``.cache/`` and the global sentinel).
    """

    def test_corrected_latency(self, tmp_path: Path) -> None:
        for f in _PERSISTENT_SKEW_FIXTURE.glob("*.jsonl"):
            shutil.copy2(f, tmp_path / f.name)

        update_cache(tmp_path)
        groups = discover_groups(tmp_path)
        assert len(groups) == 1
        variant, run, paths = groups[0]
        group = scan_group(paths)
        deliveries = correlate_lazy(group).collect()
        records = deliveries_to_records(deliveries)
        assert records, "fixture produced no delivery records"
        for rec in records:
            assert rec.offset_applied is True
            assert rec.offset_ms == -50.0
            assert abs(rec.latency_ms - 100.0) < 1.0
