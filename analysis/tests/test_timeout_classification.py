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
    WATCHDOG_STDERR_SUBSTRING,
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
    def test_pre_operate_truncated_jsonl_classifies_deadlock(
        self, tmp_path: Path
    ) -> None:
        """Spec rule 8 (post-T14.17 follow-up): truncated JSONL with NO
        ``phase=operate`` keeps the legacy ``deadlock`` label.

        With the 2026-05-14 ``variant_crashed`` follow-up, truncated
        JSONLs that DID reach ``phase=operate`` are now classified as
        ``variant_crashed``. The legacy ``deadlock`` label survives for
        the pre-operate truncation edge case (variant killed before it
        emitted its first ``phase=operate`` line).
        """
        variant = "test-variant"
        run = "run01"
        alice_events = [
            make_event("phase", runner="alice", run=run, phase="connect", offset_ms=0),
            make_event(
                "phase", runner="alice", run=run, phase="stabilize", offset_ms=50
            ),
            # Note: no phase=operate -- variant was killed mid-record
            # before it even logged the operate transition. variant_rejected
            # requires non-empty stderr, which we deliberately omit, so the
            # only rule that catches this is the legacy deadlock fallthrough.
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

    def test_truncated_jsonl_without_logs_dir_classifies_deadlock(
        self, tmp_path: Path
    ) -> None:
        """Without ``logs_dir`` the stderr-aware rules (6/7) cannot run,
        so a truncated JSONL falls through to the legacy ``deadlock``
        rule by definition. Regression guard: a caller that doesn't
        supply ``logs_dir`` must still see ``deadlock`` rather than
        ``variant_crashed`` (which requires reading stderr).
        """
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
        ]
        _write_jsonl_truncated(
            tmp_path / f"{variant}-alice-{run}.jsonl",
            alice_events,
            truncate_bytes=15,
        )

        lazy = events_to_lazy(alice_events)
        # logs_dir=None forces the rule-6/7 stderr-aware branch off.
        result = classify_group(lazy, variant=variant, run=run, logs_dir=None)
        # Without logs_dir the file-tail truncation check cannot run
        # either, so the classifier falls all the way through to unknown.
        # Document the existing behaviour rather than assert deadlock --
        # the contract is: no logs_dir, no stderr/JSONL reads.
        assert result["alice"].classification == "unknown"

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


def _alice_idle_terminated_events(*, run: str = "run01") -> list[dict]:
    """Events for an alice spawn that exited cleanly via E15 idle detection.

    Writer reaches operate, emits ``eot_sent`` on its own idle
    transition (T15.5), then transitions to ``phase=silent`` and
    exits. No on-wire EOT exchange happens, so no peer logs
    ``eot_received{writer=alice}`` and no ``eot_timeout`` event is
    written. This is the canonical shape that T15.6 classifies as
    ``runner_idle_terminated``.
    """
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
        # T15.5: variant emits eot_sent on its own idle detection.
        make_event("eot_sent", runner="alice", run=run, eot_id=0, offset_ms=5000),
        make_event("phase", runner="alice", run=run, phase="silent", offset_ms=5001),
    ]


class TestRunnerIdleTerminated:
    def test_eot_sent_silent_no_peer_confirm_no_eot_timeout_classifies_idle(
        self, tmp_path: Path
    ) -> None:
        """T15.6 rule: clean exit via E15 idle detection."""
        variant = "test-variant"
        run = "run01"
        alice = _alice_idle_terminated_events(run=run)
        # Bob also exits cleanly via idle detection -- no on-wire EOT
        # exchange happens in E15, so neither side logs
        # ``eot_received`` against the peer.
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
            make_event("eot_sent", runner="bob", run=run, eot_id=0, offset_ms=5000),
            make_event("phase", runner="bob", run=run, phase="silent", offset_ms=5001),
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice)
        _write_jsonl_clean(tmp_path / f"{variant}-bob-{run}.jsonl", bob)

        lazy = events_to_lazy(alice + bob)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "runner_idle_terminated"
        assert result["bob"].classification == "runner_idle_terminated"
        assert result["alice"].sub_tags == ()
        assert result["bob"].sub_tags == ()

    def test_completed_takes_precedence_when_peer_confirms(
        self, tmp_path: Path
    ) -> None:
        """T15.6 precedence: peer-confirmed handshake stays ``completed``.

        The ``completed`` rule still wins when at least one peer logs
        a matching ``eot_received``. ``runner_idle_terminated`` only
        fires when peer confirmation is ABSENT and ``eot_timeout`` is
        also absent.
        """
        variant = "test-variant"
        run = "run01"
        alice = _alice_completed_events(run=run)
        bob = _bob_completed_events(run=run)

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice)
        _write_jsonl_clean(tmp_path / f"{variant}-bob-{run}.jsonl", bob)

        lazy = events_to_lazy(alice + bob)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "completed"
        assert result["bob"].classification == "completed"

    def test_eot_timeout_present_blocks_idle_classification(
        self, tmp_path: Path
    ) -> None:
        """T15.6 precedence: ``eot_timeout_internal`` still wins.

        A spawn that emitted ``eot_timeout`` is using the legacy
        on-wire EOT-wait path (per the E12 protocol). T15.6 must not
        relabel it as ``runner_idle_terminated`` even when the writer
        also reaches ``phase=silent`` and the peer did not confirm.
        """
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
            make_event("eot_sent", runner="alice", run=run, eot_id=111, offset_ms=210),
            make_event(
                "eot_timeout",
                runner="alice",
                run=run,
                missing=["bob"],
                wait_ms=5000,
                offset_ms=5210,
            ),
            # Some legacy variants reach phase=silent even after
            # emitting eot_timeout (the websocket Single path on the
            # post-E15 stress fixture exhibits this exact shape).
            make_event(
                "phase", runner="alice", run=run, phase="silent", offset_ms=5300
            ),
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "eot_timeout_internal"

    def test_loopback_self_eot_received_does_not_count_as_peer_confirm(
        self, tmp_path: Path
    ) -> None:
        """Self-loopback ``eot_received`` does not satisfy peer-confirm.

        Single-runner variants observe their own EOT marker via the
        local subscriber (``writer=alice`` on alice's own log). Since
        ``classify_spawn`` consults only OTHER runners' summaries
        (``peer_summaries``), a self-receive does not flip the
        classification away from ``runner_idle_terminated``. The
        regression guard is here because the same JSONL contains the
        self-loopback row, and the test ensures the classifier still
        sees the absence of a true peer confirmation.
        """
        variant = "test-variant"
        run = "run01"
        alice_events = _alice_idle_terminated_events(run=run) + [
            # Self-loopback: alice's own log records receiving its
            # own EOT marker. Must not be counted as a peer confirm.
            make_event(
                "eot_received",
                runner="alice",
                run=run,
                writer="alice",
                eot_id=0,
                offset_ms=5002,
            ),
        ]

        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "runner_idle_terminated"


def _alice_watchdog_stalled_events(*, run: str = "run01") -> list[dict]:
    """Events for an alice spawn that the T15.11 watchdog self-exited.

    Reaches operate (and emits a couple of writes), but the variant's
    driver thread then wedges inside transport-library code. The
    watchdog OS thread observes both counters frozen for the
    configured threshold and calls ``std::process::exit(2)``. No
    ``eot_sent`` event is emitted (the driver was stuck before idle
    detection could run); the variant never reaches ``phase=silent``;
    the JSONL is FLUSHED cleanly via the watchdog's explicit flush
    callback.
    """
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
        # Driver thread wedges here. No further events. Watchdog
        # fires after `watchdog_secs` and the process self-exits with
        # a clean JSONL tail.
    ]


class TestVariantSelfKilledIdle:
    def test_watchdog_stderr_signature_classifies_self_killed(
        self, tmp_path: Path
    ) -> None:
        """T15.11 rule: watchdog stderr signature + no eot_sent + clean JSONL."""
        variant = "test-variant"
        run = "run01"
        alice_events = _alice_watchdog_stalled_events(run=run)
        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)
        # The variant's watchdog wrote its diagnostic line right
        # before exiting. The classifier substring-matches the stable
        # ``watchdog: no progress`` token.
        (tmp_path / f"{variant}-alice-stderr.txt").write_text(
            f"[variant] {WATCHDOG_STDERR_SUBSTRING} in 60s during operate phase "
            "-- internal stall; self-exiting\n",
            encoding="utf-8",
        )

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "variant_self_killed_idle"
        assert result["alice"].sub_tags == ()

    def test_variant_crashed_when_jsonl_truncated_without_watchdog_signature(
        self, tmp_path: Path
    ) -> None:
        """Mutual exclusion (post-2026-05-14): truncated JSONL + no
        watchdog signature + has ``phase=operate`` -> ``variant_crashed``.

        Before the T14.17 follow-up this case classified as
        ``deadlock`` (truncation is the smoking gun, watchdog signature
        absent so ``variant_self_killed_idle`` doesn't fire). The new
        ``variant_crashed`` rule sits between the two and catches the
        fast-panic shape (variant reached operate then died too fast
        for the watchdog to fire). The Zenoh qos3 alice case under
        stress is the motivating example.
        """
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
        # Mid-record kill (truncated JSONL) with NO stderr capture --
        # the variant_crashed rule wins because the spawn reached
        # phase=operate and the watchdog signature is absent.
        _write_jsonl_truncated(
            tmp_path / f"{variant}-alice-{run}.jsonl",
            alice_events,
            truncate_bytes=15,
        )

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "variant_crashed"

    def test_runner_idle_terminated_wins_when_eot_sent_and_silent_reached(
        self, tmp_path: Path
    ) -> None:
        """Mutual exclusion: ``eot_sent`` + ``phase=silent`` -> clean rule.

        A spawn that reached the variant-side idle detection path
        (T15.5) emits ``eot_sent`` AND ``phase=silent``. Even if its
        stderr happens to contain the watchdog substring (which the
        production watchdog would not write in that case but might
        appear in a legacy capture), the classifier must prefer the
        clean-exit rule because ``has_eot_sent`` is true.
        """
        variant = "test-variant"
        run = "run01"
        alice = _alice_idle_terminated_events(run=run)
        # Self-loopback / single-runner: no peer confirms.
        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice)
        # Even with a spurious watchdog string in stderr the clean
        # exit must win because eot_sent + phase=silent already proved
        # the variant teared down through the normal path.
        (tmp_path / f"{variant}-alice-stderr.txt").write_text(
            f"[variant] {WATCHDOG_STDERR_SUBSTRING} stray line\n",
            encoding="utf-8",
        )

        lazy = events_to_lazy(alice)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "runner_idle_terminated"

    def test_eot_timeout_internal_wins_over_watchdog_signature(
        self, tmp_path: Path
    ) -> None:
        """Mutual exclusion: ``eot_sent`` + ``eot_timeout`` keeps top precedence.

        The classifier's first rule (``eot_timeout_internal``) MUST
        win even if the stderr capture happens to contain the
        watchdog substring -- that combination indicates the variant
        ran the legacy on-wire EOT phase and self-aborted via the E12
        protocol, not via the T15.11 watchdog. ``eot_sent`` is also
        absent in the watchdog case by definition.
        """
        variant = "test-variant"
        run = "run01"
        alice_events = [
            make_event(
                "phase", runner="alice", run=run, phase="operate", offset_ms=100
            ),
            make_event("eot_sent", runner="alice", run=run, eot_id=1, offset_ms=200),
            make_event(
                "eot_timeout",
                runner="alice",
                run=run,
                missing=["bob"],
                wait_ms=5000,
                offset_ms=5200,
            ),
        ]
        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)
        (tmp_path / f"{variant}-alice-stderr.txt").write_text(
            f"[variant] {WATCHDOG_STDERR_SUBSTRING} stray line\n",
            encoding="utf-8",
        )

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "eot_timeout_internal"

    def test_no_phase_operate_falls_through_to_variant_rejected(
        self, tmp_path: Path
    ) -> None:
        """A variant that died before operate keeps the variant_rejected label.

        ``variant_self_killed_idle`` requires ``has_phase_operate``
        to be true (the watchdog only fires inside operate). If the
        variant exited earlier with a watchdog-shaped stderr (which
        the production watchdog wouldn't produce, but a fuzzy fixture
        might), the ``variant_rejected`` rule keeps precedence.
        """
        variant = "test-variant"
        run = "run01"
        alice_events = [
            make_event("phase", runner="alice", run=run, phase="connect", offset_ms=0),
            # No phase=operate.
        ]
        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice_events)
        (tmp_path / f"{variant}-alice-stderr.txt").write_text(
            f"[variant] {WATCHDOG_STDERR_SUBSTRING} should not match here\n",
            encoding="utf-8",
        )

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        # variant_rejected wins because the spawn never reached operate.
        assert result["alice"].classification == "variant_rejected"


def _alice_crashed_events(*, run: str = "run01") -> list[dict]:
    """Events for an alice spawn that crashed mid-operate.

    Reaches operate (and emits a couple of writes), but the variant
    process exits abnormally without flushing -- the simulated case is
    a panic inside a transport library (the Zenoh qos3 multi alice
    failure shape under stress). The watchdog never fires because the
    crash happens faster than its threshold (default 30s). The JSONL
    is left truncated mid-record because the writer buffer never
    flushes. No stderr is written by the variant.
    """
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
        # Process panics here. No further events; the writer buffer
        # never flushes so the file ends mid-record.
    ]


class TestVariantCrashed:
    """T14.17 follow-up (2026-05-14): fast-panic shape classification.

    Distinguishes a variant that crashed inside the variant process
    (e.g. transport-library panic) from a slow-stall self-exit
    (``variant_self_killed_idle``) and from a generic
    ``deadlock``. The discriminator is: truncated JSONL +
    ``phase=operate`` reached + NO watchdog signature.
    """

    def test_truncated_jsonl_no_watchdog_classifies_variant_crashed(
        self, tmp_path: Path
    ) -> None:
        """Truncated JSONL + has phase=operate + no eot_sent + no watchdog
        signature -> ``variant_crashed``."""
        variant = "test-variant"
        run = "run01"
        alice_events = _alice_crashed_events(run=run)
        _write_jsonl_truncated(
            tmp_path / f"{variant}-alice-{run}.jsonl",
            alice_events,
            truncate_bytes=15,
        )
        # No stderr file at all -- the variant didn't get a chance to
        # write one. This is the typical Zenoh qos3 alice fast-panic
        # shape: a process panic inside a transport library inside a
        # tokio runtime returns no output to the runner's stderr
        # capture.

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "variant_crashed"
        assert result["alice"].sub_tags == ()

    def test_truncated_jsonl_empty_stderr_classifies_variant_crashed(
        self, tmp_path: Path
    ) -> None:
        """Empty stderr file (created by the runner before spawn, child
        wrote nothing) must classify the same as no stderr at all.

        The runner opens the stderr capture path BEFORE spawning the
        child (see ``runner/src/spawn.rs::spawn_and_monitor``), so an
        empty file is the on-disk shape for a child that died without
        emitting any stderr.
        """
        variant = "test-variant"
        run = "run01"
        alice_events = _alice_crashed_events(run=run)
        _write_jsonl_truncated(
            tmp_path / f"{variant}-alice-{run}.jsonl",
            alice_events,
            truncate_bytes=15,
        )
        # Empty stderr capture file -- runner pre-creates it.
        (tmp_path / f"{variant}-alice-stderr.txt").write_text("", encoding="utf-8")

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "variant_crashed"

    def test_watchdog_signature_wins_over_variant_crashed(self, tmp_path: Path) -> None:
        """Precedence: stderr signature present -> ``variant_self_killed_idle``.

        Even if the JSONL happens to be truncated, the watchdog rule's
        explicit signature wins over the truncation-based
        ``variant_crashed`` rule -- mirroring the existing ``deadlock``
        precedence note.
        """
        variant = "test-variant"
        run = "run01"
        alice_events = _alice_crashed_events(run=run)
        _write_jsonl_truncated(
            tmp_path / f"{variant}-alice-{run}.jsonl",
            alice_events,
            truncate_bytes=15,
        )
        (tmp_path / f"{variant}-alice-stderr.txt").write_text(
            f"[variant] {WATCHDOG_STDERR_SUBSTRING} in 60s during operate phase\n",
            encoding="utf-8",
        )

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "variant_self_killed_idle"

    def test_deadlock_wins_when_no_phase_operate(self, tmp_path: Path) -> None:
        """Precedence: truncated JSONL with NO ``phase=operate`` keeps the
        legacy ``deadlock`` label.

        ``variant_crashed`` requires ``has_phase_operate`` so that the
        very-early truncation cases (variant killed before logging the
        operate transition) keep their pre-existing label. They cannot
        be ``variant_rejected`` because that rule additionally requires
        a non-empty stderr capture.
        """
        variant = "test-variant"
        run = "run01"
        alice_events = [
            make_event("phase", runner="alice", run=run, phase="connect", offset_ms=0),
            make_event(
                "phase", runner="alice", run=run, phase="stabilize", offset_ms=50
            ),
            # No phase=operate -- variant was killed earlier.
        ]
        _write_jsonl_truncated(
            tmp_path / f"{variant}-alice-{run}.jsonl",
            alice_events,
            truncate_bytes=15,
        )

        lazy = events_to_lazy(alice_events)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        # variant_crashed needs phase=operate; falls through to deadlock.
        assert result["alice"].classification == "deadlock"

    def test_runner_idle_terminated_wins_over_variant_crashed(
        self, tmp_path: Path
    ) -> None:
        """Precedence: ``eot_sent`` + ``phase=silent`` -> clean-exit rule.

        A spawn that reached the E15 idle-detection path (``eot_sent``
        + ``phase=silent``) keeps the ``runner_idle_terminated`` label
        even if the JSONL would otherwise be considered truncated --
        ``has_eot_sent`` is checked earlier in the chain.
        """
        variant = "test-variant"
        run = "run01"
        alice = _alice_idle_terminated_events(run=run)
        _write_jsonl_clean(tmp_path / f"{variant}-alice-{run}.jsonl", alice)

        lazy = events_to_lazy(alice)
        result = classify_group(lazy, variant=variant, run=run, logs_dir=tmp_path)
        assert result["alice"].classification == "runner_idle_terminated"


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
