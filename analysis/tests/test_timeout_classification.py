"""Synthetic-fixture tests for T14.17 timeout-classification rules.

Each test builds a per-spawn fixture (JSONL + optional stderr) under
``tmp_path`` and runs the classifier through :func:`classify_group`,
asserting the resulting label for each runner side.
"""

from __future__ import annotations

import json
from pathlib import Path

from helpers import events_to_lazy, make_event

from timeout_classification import (
    SATURATION_HINT_SUBSTRING,
    classify_group,
    jsonl_ends_mid_record,
)


def _write_jsonl_truncated(path: Path, events: list[dict], truncate_bytes: int) -> None:
    """Write ``events`` as JSONL then truncate the final ``truncate_bytes``.

    Simulates a mid-record process kill: the file's last line is a
    JSON prefix without its closing brace and newline.
    """
    lines = [json.dumps(e) + "\n" for e in events]
    data = "".join(lines).encode("utf-8")
    assert truncate_bytes < len(data)
    with open(path, "wb") as f:
        f.write(data[: len(data) - truncate_bytes])


def _alice_completed_events(*, run: str = "run01") -> list[dict]:
    """Events for a clean completed alice spawn that received bob's EOT."""
    return [
        make_event("phase", runner="alice", run=run, phase="connect", offset_ms=0),
        make_event("phase", runner="alice", run=run, phase="stabilize", offset_ms=10),
        make_event("phase", runner="alice", run=run, phase="operate", offset_ms=100),
        make_event(
            "write",
            runner="alice",
            run=run,
            seq=1,
            path="/k",
            qos=4,
            bytes=8,
            offset_ms=110,
        ),
        make_event("phase", runner="alice", run=run, phase="eot", offset_ms=200),
        make_event("eot_sent", runner="alice", run=run, eot_id=111, offset_ms=210),
        make_event(
            "eot_received",
            runner="alice",
            run=run,
            writer="bob",
            eot_id=222,
            offset_ms=220,
        ),
        make_event("phase", runner="alice", run=run, phase="silent", offset_ms=300),
    ]


def _bob_completed_events(*, run: str = "run01") -> list[dict]:
    """Events for a clean completed bob spawn that received alice's EOT."""
    return [
        make_event("phase", runner="bob", run=run, phase="connect", offset_ms=0),
        make_event("phase", runner="bob", run=run, phase="stabilize", offset_ms=10),
        make_event("phase", runner="bob", run=run, phase="operate", offset_ms=100),
        make_event(
            "write",
            runner="bob",
            run=run,
            seq=1,
            path="/k",
            qos=4,
            bytes=8,
            offset_ms=115,
        ),
        make_event("phase", runner="bob", run=run, phase="eot", offset_ms=200),
        make_event("eot_sent", runner="bob", run=run, eot_id=222, offset_ms=210),
        make_event(
            "eot_received",
            runner="bob",
            run=run,
            writer="alice",
            eot_id=111,
            offset_ms=220,
        ),
        make_event("phase", runner="bob", run=run, phase="silent", offset_ms=300),
    ]


def _write_jsonl_clean(path: Path, events: list[dict]) -> None:
    with open(path, "w", encoding="utf-8") as f:
        for ev in events:
            f.write(json.dumps(ev) + "\n")


class TestCompleted:
    def test_two_sided_clean_run_classifies_both_as_completed(
        self, tmp_path: Path
    ) -> None:
        """Spec rule 1: eot_sent + matching peer eot_received -> completed."""
        variant = "test-variant"
        run = "run01"
        alice = _alice_completed_events(run=run)
        bob = _bob_completed_events(run=run)

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice)
        _write_jsonl_clean(tmp_path / f"{variant}-bob-{run}.jsonl", bob)

        events = alice + bob
        lazy = events_to_lazy(events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "completed"
        assert result["bob"].classification == "completed"
        assert result["alice"].sub_tags == ()
        assert result["bob"].sub_tags == ()


class TestDeadlock:
    def test_no_eot_sent_truncated_jsonl_classifies_deadlock(
        self, tmp_path: Path
    ) -> None:
        """Spec rule 2: no eot_sent + truncated JSONL tail -> deadlock."""
        variant = "test-variant"
        run = "run01"
        alice_events = [
            make_event("phase", runner="alice", run=run, phase="connect", offset_ms=0),
            make_event(
                "phase", runner="alice", run=run, phase="operate", offset_ms=100
            ),
            make_event(
                "write",
                runner="alice",
                run=run,
                seq=1,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=110,
            ),
            # Last event will be truncated.
            make_event(
                "write",
                runner="alice",
                run=run,
                seq=2,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=120,
            ),
        ]
        # Write everything but lop off the trailing bytes so the final
        # line is missing its closing brace and newline.
        _write_jsonl_truncated(
            tmp_path / f"{variant}-alice-{run}.jsonl",
            alice_events,
            truncate_bytes=15,
        )

        # Only the in-memory events drive the classifier; the file is
        # read separately for the truncation check.
        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "deadlock"

    def test_jsonl_ends_mid_record_helper(self, tmp_path: Path) -> None:
        """Truncation-detection helper recognises an incomplete final record."""
        path = tmp_path / "truncated.jsonl"
        _write_jsonl_truncated(
            path,
            [
                make_event(
                    "phase",
                    runner="alice",
                    phase="operate",
                    offset_ms=0,
                ),
                make_event(
                    "write",
                    runner="alice",
                    seq=1,
                    path="/k",
                    qos=4,
                    bytes=8,
                    offset_ms=10,
                ),
            ],
            truncate_bytes=10,
        )
        assert jsonl_ends_mid_record(path) is True

    def test_jsonl_clean_helper_returns_false(self, tmp_path: Path) -> None:
        """Truncation-detection helper accepts a clean trailing newline."""
        path = tmp_path / "clean.jsonl"
        _write_jsonl_clean(
            path,
            [
                make_event(
                    "phase",
                    runner="alice",
                    phase="operate",
                    offset_ms=0,
                ),
                make_event(
                    "phase",
                    runner="alice",
                    phase="silent",
                    offset_ms=10,
                ),
            ],
        )
        assert jsonl_ends_mid_record(path) is False


def _alice_timed_out_with_eot_sent(*, run: str = "run01") -> list[dict]:
    """Events for an alice spawn that emitted eot_sent then timed out.

    Reaches operate + eot_sent but NEVER logs phase=silent -- the
    runner external-killed alice before the EOT phase completed. This
    is the canonical ``eot_lost`` shape: writer published its EOT but
    something on the other side prevented the spawn from completing
    cleanly.
    """
    return [
        make_event("phase", runner="alice", run=run, phase="connect", offset_ms=0),
        make_event("phase", runner="alice", run=run, phase="operate", offset_ms=100),
        make_event(
            "write",
            runner="alice",
            run=run,
            seq=1,
            path="/k",
            qos=4,
            bytes=8,
            offset_ms=110,
        ),
        make_event("phase", runner="alice", run=run, phase="eot", offset_ms=200),
        make_event("eot_sent", runner="alice", run=run, eot_id=111, offset_ms=210),
        # No phase=silent -- killed by runner timeout before EOT
        # handshake completed.
    ]


class TestEotLost:
    def test_writer_eot_sent_but_no_phase_silent_classifies_eot_lost(
        self, tmp_path: Path
    ) -> None:
        """Spec rule 3: eot_sent on timed-out side -> eot_lost."""
        variant = "test-variant"
        run = "run01"
        alice = _alice_timed_out_with_eot_sent(run=run)
        # Bob's apparently-successful spawn (reached phase=silent). It
        # may or may not have observed alice's EOT -- the eot_lost
        # rule is keyed on alice's own asymmetry, not on the peer
        # confirmation.
        bob = [
            make_event("phase", runner="bob", run=run, phase="operate", offset_ms=100),
            make_event(
                "write",
                runner="bob",
                run=run,
                seq=1,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=115,
            ),
            make_event("eot_sent", runner="bob", run=run, eot_id=222, offset_ms=210),
            make_event("phase", runner="bob", run=run, phase="silent", offset_ms=300),
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice)
        _write_jsonl_clean(tmp_path / f"{variant}-bob-{run}.jsonl", bob)

        events = alice + bob
        lazy = events_to_lazy(events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "eot_lost"
        # No stderr -> no saturation sub-tag.
        assert result["alice"].sub_tags == ()

    def test_eot_lost_with_saturation_sub_tag(self, tmp_path: Path) -> None:
        """Spec rule 4: ``reader channel full`` on success side -> sub-tag."""
        variant = "test-variant"
        run = "run01"
        alice = _alice_timed_out_with_eot_sent(run=run)
        bob = [
            make_event("phase", runner="bob", run=run, phase="operate", offset_ms=100),
            make_event("eot_sent", runner="bob", run=run, eot_id=222, offset_ms=210),
            make_event("phase", runner="bob", run=run, phase="silent", offset_ms=300),
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice)
        _write_jsonl_clean(tmp_path / f"{variant}-bob-{run}.jsonl", bob)

        # Bob is the apparently-successful peer; saturation hint goes
        # on the SUCCESS side's stderr capture per the spec.
        (tmp_path / f"{variant}-bob-stderr.txt").write_text(
            f"{SATURATION_HINT_SUBSTRING}\n"
            f"{SATURATION_HINT_SUBSTRING}\n"
            f"{SATURATION_HINT_SUBSTRING}\n",
            encoding="utf-8",
        )

        events = alice + bob
        lazy = events_to_lazy(events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "eot_lost"
        assert result["alice"].sub_tags == ("eot_lost_likely_saturation",)


class TestVariantRejected:
    def test_no_operate_phase_with_rejection_stderr_classifies_rejected(
        self, tmp_path: Path
    ) -> None:
        """Spec rule 5: no phase=operate + non-empty rejection stderr."""
        variant = "test-variant"
        run = "run01"
        alice_events = [
            make_event("phase", runner="alice", run=run, phase="connect", offset_ms=0),
            # No phase=operate -- variant exited before reaching it.
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)
        (tmp_path / f"{variant}-alice-stderr.txt").write_text(
            "Error: variant does not support single-threaded mode\n",
            encoding="utf-8",
        )

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "variant_rejected"


class TestEotTimeoutInternal:
    def test_eot_sent_plus_eot_timeout_classifies_internal(
        self, tmp_path: Path
    ) -> None:
        """Spec rule 6: eot_sent + eot_timeout -> eot_timeout_internal."""
        variant = "test-variant"
        run = "run01"
        alice_events = [
            make_event(
                "phase", runner="alice", run=run, phase="operate", offset_ms=100
            ),
            make_event(
                "write",
                runner="alice",
                run=run,
                seq=1,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=110,
            ),
            make_event(
                "eot_sent",
                runner="alice",
                run=run,
                eot_id=111,
                offset_ms=210,
            ),
            make_event(
                "eot_timeout",
                runner="alice",
                run=run,
                missing=["bob"],
                wait_ms=5000,
                offset_ms=5210,
            ),
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "eot_timeout_internal"


class TestUnknown:
    def test_no_signal_matches_classifies_unknown(self, tmp_path: Path) -> None:
        """Spec rule 7: nothing matches -> unknown."""
        variant = "test-variant"
        run = "run01"
        # Reached operate, has no eot_sent, no eot_timeout, no
        # phase=silent, JSONL ends with a CLEAN line, no stderr.
        # Doesn't match deadlock (clean JSONL), doesn't match
        # variant_rejected (operate reached + no stderr), doesn't
        # match eot_lost (no eot_sent), doesn't match completed
        # (no eot_sent), doesn't match eot_timeout_internal.
        alice_events = [
            make_event(
                "phase", runner="alice", run=run, phase="operate", offset_ms=100
            ),
            make_event(
                "write",
                runner="alice",
                run=run,
                seq=1,
                path="/k",
                qos=4,
                bytes=8,
                offset_ms=110,
            ),
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "unknown"
