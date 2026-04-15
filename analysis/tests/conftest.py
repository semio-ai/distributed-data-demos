"""Shared fixtures for analysis tests."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

# Add the analysis package root to sys.path so imports work without install
_ANALYSIS_ROOT = Path(__file__).resolve().parent.parent
if str(_ANALYSIS_ROOT) not in sys.path:
    sys.path.insert(0, str(_ANALYSIS_ROOT))

from helpers import _ts, make_event, write_jsonl  # noqa: E402


@pytest.fixture
def tmp_logs(tmp_path: Path) -> Path:
    """Create a temporary logs directory with a minimal two-runner scenario.

    Returns the logs directory path.
    """
    # Alice: writer that also receives from Bob
    alice_events = [
        make_event("phase", runner="alice", phase="connect", offset_ms=0),
        make_event(
            "connected",
            runner="alice",
            launch_ts=_ts(-50),
            elapsed_ms=50.0,
            offset_ms=50,
        ),
        make_event("phase", runner="alice", phase="stabilize", offset_ms=51),
        make_event(
            "phase",
            runner="alice",
            phase="operate",
            profile="scalar-flood",
            offset_ms=1000,
        ),
        # Alice writes seq 1-5
        make_event(
            "write",
            runner="alice",
            seq=1,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1001,
        ),
        make_event(
            "write",
            runner="alice",
            seq=2,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1002,
        ),
        make_event(
            "write",
            runner="alice",
            seq=3,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1003,
        ),
        make_event(
            "write",
            runner="alice",
            seq=4,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1004,
        ),
        make_event(
            "write",
            runner="alice",
            seq=5,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1005,
        ),
        # Alice receives from Bob (seq 1-5)
        make_event(
            "receive",
            runner="alice",
            writer="bob",
            seq=1,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1010,
        ),
        make_event(
            "receive",
            runner="alice",
            writer="bob",
            seq=2,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1011,
        ),
        make_event(
            "receive",
            runner="alice",
            writer="bob",
            seq=3,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1012,
        ),
        make_event(
            "receive",
            runner="alice",
            writer="bob",
            seq=4,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1013,
        ),
        make_event(
            "receive",
            runner="alice",
            writer="bob",
            seq=5,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1014,
        ),
        make_event(
            "resource",
            runner="alice",
            cpu_percent=5.0,
            memory_mb=10.0,
            offset_ms=1100,
        ),
        make_event("phase", runner="alice", phase="silent", offset_ms=2000),
    ]

    # Bob: writer that also receives from Alice
    bob_events = [
        make_event("phase", runner="bob", phase="connect", offset_ms=0),
        make_event(
            "connected",
            runner="bob",
            launch_ts=_ts(-50),
            elapsed_ms=55.0,
            offset_ms=55,
        ),
        make_event("phase", runner="bob", phase="stabilize", offset_ms=56),
        make_event(
            "phase",
            runner="bob",
            phase="operate",
            profile="scalar-flood",
            offset_ms=1000,
        ),
        # Bob writes seq 1-5
        make_event(
            "write",
            runner="bob",
            seq=1,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1001,
        ),
        make_event(
            "write",
            runner="bob",
            seq=2,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1002,
        ),
        make_event(
            "write",
            runner="bob",
            seq=3,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1003,
        ),
        make_event(
            "write",
            runner="bob",
            seq=4,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1004,
        ),
        make_event(
            "write",
            runner="bob",
            seq=5,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1005,
        ),
        # Bob receives from Alice (seq 1-5)
        make_event(
            "receive",
            runner="bob",
            writer="alice",
            seq=1,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1010,
        ),
        make_event(
            "receive",
            runner="bob",
            writer="alice",
            seq=2,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1011,
        ),
        make_event(
            "receive",
            runner="bob",
            writer="alice",
            seq=3,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1012,
        ),
        make_event(
            "receive",
            runner="bob",
            writer="alice",
            seq=4,
            path="/bench/1",
            qos=1,
            bytes=8,
            offset_ms=1013,
        ),
        make_event(
            "receive",
            runner="bob",
            writer="alice",
            seq=5,
            path="/bench/0",
            qos=1,
            bytes=8,
            offset_ms=1014,
        ),
        make_event(
            "resource",
            runner="bob",
            cpu_percent=7.0,
            memory_mb=12.0,
            offset_ms=1100,
        ),
        make_event("phase", runner="bob", phase="silent", offset_ms=2000),
    ]

    write_jsonl(tmp_path / "test-variant-alice-run01.jsonl", alice_events)
    write_jsonl(tmp_path / "test-variant-bob-run01.jsonl", bob_events)
    return tmp_path
