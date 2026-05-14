"""Per-spawn timeout-cause classification (T14.17).

A spawn is one ``(variant, run, runner)`` triple -- one variant process
on one runner during one run. The runner spawns the variant and
collects either a graceful exit, a timeout kill, or a non-zero exit.
Currently the analysis pipeline does not have access to the runner's
own spawn-status sidecar (no such file is written), so this module
INFERS the spawn outcome from the per-spawn JSONL events plus the
``<log_subdir>/<variant>-<runner>-stderr.txt`` capture written by the
runner (per ``runner/src/spawn.rs::stderr_capture_path``).

The classification taxonomy and rules are documented in
``metak-shared/ANALYSIS.md`` -- see the "Timeout classification" section.
Nine values, plus an optional ``eot_lost_likely_saturation`` sub-tag:

``completed``
    Spawn ran to graceful exit (``phase=silent`` reached) and the
    writer's ``eot_sent`` is matched by a peer ``eot_received``.
``runner_idle_terminated`` (T15.6)
    Spawn ran to graceful exit (``phase=silent`` reached) via the
    E15 variant-side idle-detection path: the writer emitted
    ``eot_sent`` to its own JSONL on idle, no ``eot_timeout`` event
    is present, but no peer logged a matching ``eot_received`` --
    consistent with the post-E15 architecture where no on-wire EOT
    exchange happens. This is a clean exit, distinct from the
    failure-mode ``eot_lost`` (which requires the writer to NOT
    reach ``phase=silent``).
``deadlock``
    Killed mid-operate: no ``eot_sent`` AND the JSONL ends with an
    incomplete record (last line not valid JSON). Post-T15.11 the
    ``variant_self_killed_idle`` and ``variant_crashed`` rules
    siphon off the two practical sub-cases; ``deadlock`` keeps its
    label for the legacy edge cases (truncation before
    ``phase=operate``, or ``logs_dir`` absent so stderr-aware rules
    cannot run).
``eot_lost``
    Writer reached ``eot_sent`` but never reached ``phase=silent``.
    Legacy E12/E14 failure shape: writer published EOT but
    something on the other side prevented the spawn from
    completing cleanly. If the asymmetric side's stderr capture
    has ``reader channel full`` lines, attach the
    ``eot_lost_likely_saturation`` sub-tag.
``variant_rejected``
    Variant exited before reaching ``phase=operate``; stderr capture
    is non-empty.
``variant_self_killed_idle`` (T15.11)
    Variant reached ``phase=operate`` but did NOT emit ``eot_sent``
    and did NOT reach ``phase=silent``; the in-process watchdog
    thread (T15.11) detected no progress on either counter for the
    configured threshold and called ``std::process::exit`` after
    flushing the JSONL. The stderr capture contains the
    ``watchdog: no progress`` substring -- the load-bearing signal
    that distinguishes a clean self-exit from a runner-side kill.
    Mutually exclusive with ``deadlock`` (whose JSONL is truncated
    mid-record) and ``eot_lost`` (which requires ``eot_sent`` to be
    present). Precedence: above ``deadlock`` so the watchdog's
    explicit diagnostic wins over the generic truncation heuristic.
``variant_crashed`` (T14.17 follow-up, 2026-05-14)
    Variant reached ``phase=operate`` but did NOT emit ``eot_sent``
    and did NOT reach ``phase=silent``; stderr capture lacks the
    watchdog signature AND the JSONL ends mid-record. The
    fast-panic shape: variant crashed (likely inside a transport
    library) too quickly for T15.11's watchdog to fire (default
    threshold 30s). The Zenoh qos3 alice case observed under stress
    is the motivating example. Mutually exclusive with
    ``variant_self_killed_idle`` (which requires the signature) and
    sits above ``deadlock`` so the ``has_phase_operate`` predicate
    distinguishes fast-panic from generic truncation.
``eot_timeout_internal``
    Variant emitted ``eot_sent`` AND ``eot_timeout`` -- decided to
    give up waiting for peer EOTs per the E12 EOT protocol. Post-E15
    this fires only for legacy code paths that still run the
    on-wire EOT phase (e.g. websocket Single before T15.8 cleanup).
``unknown``
    None of the above. Operator must inspect manually.

Stderr capture reads are LAZY: only spawns whose JSONL-derived state
is ambiguous (``variant_rejected`` candidates, ``eot_lost`` candidates
for the saturation sub-tag, and ``variant_self_killed_idle`` /
``variant_crashed`` candidates after T15.11) trigger a read. Stderr
files are not loaded unconditionally. Spawns that classify as
``completed`` / ``runner_idle_terminated`` / ``eot_timeout_internal`` /
``deadlock`` / ``unknown`` without first being a watchdog candidate
never touch stderr.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

import polars as pl

# Classification values. Single-token enum strings so they fit a narrow
# table column without quoting.
Classification = Literal[
    "completed",
    "runner_idle_terminated",
    "deadlock",
    "eot_lost",
    "variant_rejected",
    "variant_self_killed_idle",
    "variant_crashed",
    "eot_timeout_internal",
    "unknown",
]

#: Substring patterns that indicate a known variant-rejection cause.
#: Matched case-sensitively against the spawn's stderr capture.
KNOWN_REJECTION_PATTERNS: tuple[str, ...] = (
    "does not support single-threaded mode",
    "does not support QoS",
    "port collision",
    "unsupported",
)

#: Substring whose presence in the asymmetric (success-side) stderr
#: capture upgrades an ``eot_lost`` classification with the saturation
#: sub-tag. Sourced from the user-reported failure on the
#: ``custom-udp-1000x100hz-qos2-multi`` spawn that motivated T14.17.
SATURATION_HINT_SUBSTRING: str = "reader channel full"

#: Substring whose presence in the SAME spawn's stderr capture
#: classifies the row as ``variant_self_killed_idle`` (T15.11). The
#: variant-side watchdog (see ``variant-base/src/watchdog.rs``,
#: ``WATCHDOG_STDERR_PREFIX``) writes a line starting with this exact
#: prefix immediately before calling ``std::process::exit(2)``. The
#: prefix is part of the analysis contract -- changing it on the
#: variant side without updating this constant is a pipeline-breaking
#: change.
WATCHDOG_STDERR_SUBSTRING: str = "watchdog: no progress"

#: Number of bytes from the END of a JSONL file to consider when
#: deciding whether the file ends mid-record. 4 KiB is enough to
#: capture even a long single-line tail (write events are well under
#: 500 bytes).
_TAIL_READ_BYTES: int = 4096


@dataclass(frozen=True)
class SpawnEventSummary:
    """Boolean / count summary of per-spawn events for classification.

    Populated by :func:`summarise_spawn_events` from the per-group
    ``LazyFrame`` exactly once per ``(variant, run)`` group. The
    ``eot_received_writers`` set is the set of ``writer`` values seen
    in this runner's ``eot_received`` events; the classification logic
    consults it on a different runner's summary to decide whether the
    peer confirmed THIS writer's EOT.
    """

    runner: str
    has_phase_operate: bool
    has_phase_silent: bool
    has_eot_sent: bool
    has_eot_timeout: bool
    eot_received_writers: frozenset[str]


@dataclass(frozen=True)
class SpawnClassification:
    """Classification result for one spawn.

    ``sub_tags`` is empty unless the classifier upgraded the row with
    a refinement (currently only ``eot_lost_likely_saturation`` on
    ``eot_lost``).
    """

    variant: str
    run: str
    runner: str
    classification: Classification
    sub_tags: tuple[str, ...] = ()

    def render(self) -> str:
        """Render as ``"<classification>"`` or ``"<classification> [tag]"``."""
        if not self.sub_tags:
            return self.classification
        return f"{self.classification} [{', '.join(self.sub_tags)}]"


def summarise_spawn_events(group: pl.LazyFrame) -> dict[str, SpawnEventSummary]:
    """Build per-runner event summaries for one ``(variant, run)`` group.

    A single ``collect()`` of the small event-presence projection drives
    every per-spawn classifier in the group, so we never re-scan the
    underlying shard.
    """
    # Project only the columns the classifier needs. Filter to the
    # handful of event types that participate.
    summary_lazy = group.filter(
        pl.col("event").is_in(["phase", "eot_sent", "eot_received", "eot_timeout"])
    ).select(
        pl.col("runner").cast(pl.Utf8),
        pl.col("event").cast(pl.Utf8),
        pl.col("phase").cast(pl.Utf8),
        pl.col("writer").cast(pl.Utf8),
    )
    summary_df = summary_lazy.collect()

    # Bucket per-runner records into the booleans / writer set the
    # classifier needs.
    per_runner: dict[str, dict] = {}
    if not summary_df.is_empty():
        for row in summary_df.iter_rows(named=True):
            runner = row["runner"]
            if runner is None:
                continue
            slot = per_runner.setdefault(
                runner,
                {
                    "has_phase_operate": False,
                    "has_phase_silent": False,
                    "has_eot_sent": False,
                    "has_eot_timeout": False,
                    "eot_received_writers": set(),
                },
            )
            event = row["event"]
            if event == "phase":
                phase_val = row["phase"]
                if phase_val == "operate":
                    slot["has_phase_operate"] = True
                elif phase_val == "silent":
                    slot["has_phase_silent"] = True
            elif event == "eot_sent":
                slot["has_eot_sent"] = True
            elif event == "eot_timeout":
                slot["has_eot_timeout"] = True
            elif event == "eot_received":
                writer = row["writer"]
                if writer is not None:
                    slot["eot_received_writers"].add(writer)

    out: dict[str, SpawnEventSummary] = {}
    for runner, slot in per_runner.items():
        out[runner] = SpawnEventSummary(
            runner=runner,
            has_phase_operate=slot["has_phase_operate"],
            has_phase_silent=slot["has_phase_silent"],
            has_eot_sent=slot["has_eot_sent"],
            has_eot_timeout=slot["has_eot_timeout"],
            eot_received_writers=frozenset(slot["eot_received_writers"]),
        )
    return out


def _jsonl_path(logs_dir: Path, variant: str, run: str, runner: str) -> Path:
    """Return the conventional JSONL path for a spawn.

    Mirrors the variant-base writer convention
    ``<variant>-<runner>-<run>.jsonl``.
    """
    return logs_dir / f"{variant}-{runner}-{run}.jsonl"


def _stderr_path(logs_dir: Path, variant: str, runner: str) -> Path:
    """Return the conventional stderr-capture path for a spawn.

    Mirrors ``runner/src/spawn.rs::stderr_capture_path`` --
    ``<log_subdir>/<effective_name>-<runner_name>-stderr.txt``. The
    ``effective_name`` is the variant name in the analysis pipeline's
    usage (the per-spawn JSONL filename uses the same name).
    """
    return logs_dir / f"{variant}-{runner}-stderr.txt"


def jsonl_ends_mid_record(jsonl_path: Path) -> bool:
    """Return True if the JSONL file ends with an incomplete final line.

    Reads the last :data:`_TAIL_READ_BYTES` bytes from the file and
    checks whether the final non-empty line parses as JSON. Used by the
    ``deadlock`` classification rule. If the file does not exist or is
    empty, returns ``False`` -- the caller falls back to other signals
    (likely ``variant_rejected`` when stderr is non-empty, otherwise
    ``unknown``).
    """
    if not jsonl_path.is_file():
        return False
    try:
        size = jsonl_path.stat().st_size
    except OSError:
        return False
    if size == 0:
        return False
    read_n = min(size, _TAIL_READ_BYTES)
    try:
        with open(jsonl_path, "rb") as f:
            f.seek(size - read_n)
            tail_bytes = f.read(read_n)
    except OSError:
        return False
    # Decode tolerantly: a mid-record byte cut may produce a partial
    # UTF-8 sequence on the BOUNDARY of our 4 KiB read. Replace
    # undecodable bytes; we only need to find newline boundaries and
    # parse complete lines.
    tail = tail_bytes.decode("utf-8", errors="replace")
    # Drop everything up to the first newline -- the first "line" in
    # our slice is almost certainly partial just because of where the
    # tail window landed, not because the writer truncated.
    if "\n" in tail:
        tail = tail.split("\n", 1)[1]
    else:
        # The whole tail is a single (potentially huge) record; if it
        # doesn't parse, the file was truncated mid-record.
        try:
            json.loads(tail)
            return False
        except json.JSONDecodeError:
            return True
    # Strip the trailing newline if present so we don't see an empty
    # final segment from a clean writer.
    if tail.endswith("\n"):
        # Clean trailing newline -- last record is complete.
        return False
    # The tail does NOT end with a newline. The final segment is the
    # candidate incomplete record. Try to parse it.
    last_line = tail.rsplit("\n", 1)[-1].strip()
    if not last_line:
        # Trailing whitespace only -- treat as a clean end.
        return False
    try:
        json.loads(last_line)
        return False
    except json.JSONDecodeError:
        return True


def read_stderr_capture(stderr_path: Path) -> str | None:
    """Read the stderr capture file if it exists, else ``None``.

    Capped at a generous slice (effectively unbounded for our captures,
    which are at most a few MB even in pathological logs) -- we
    substring-search only, so the cost is linear in file size and
    bounded by the read budget.
    """
    if not stderr_path.is_file():
        return None
    try:
        return stderr_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def _stderr_is_nonempty(stderr_text: str | None) -> bool:
    return stderr_text is not None and stderr_text.strip() != ""


def _has_saturation_hint(stderr_text: str | None) -> bool:
    return stderr_text is not None and SATURATION_HINT_SUBSTRING in stderr_text


def _has_watchdog_signature(stderr_text: str | None) -> bool:
    """Return True if the stderr capture contains the T15.11 prefix.

    The variant-side watchdog (``variant-base/src/watchdog.rs``) writes
    a line starting with ``[variant] watchdog: no progress`` immediately
    before calling ``std::process::exit(2)``. This helper checks for
    the stable substring ``watchdog: no progress`` -- the leading
    ``[variant]`` is dropped so other prefixes (e.g. a future
    sidecar-emitted variant) can still match cleanly.
    """
    return stderr_text is not None and WATCHDOG_STDERR_SUBSTRING in stderr_text


def _matches_known_rejection(stderr_text: str | None) -> bool:
    if stderr_text is None:
        return False
    return any(pat in stderr_text for pat in KNOWN_REJECTION_PATTERNS)


def classify_spawn(
    *,
    variant: str,
    run: str,
    runner: str,
    summary: SpawnEventSummary,
    peer_summaries: dict[str, SpawnEventSummary],
    logs_dir: Path | None,
) -> SpawnClassification:
    """Classify a single spawn given event summaries and ``logs_dir``.

    ``peer_summaries`` maps every OTHER runner in the group to its
    :class:`SpawnEventSummary`; the function consults each peer's
    ``eot_received_writers`` to decide whether the peer confirmed
    this writer's ``eot_sent``.

    ``logs_dir`` is required to read the JSONL tail (deadlock check)
    and stderr capture (variant_rejected / saturation sub-tag). When
    ``None``, the deadlock / variant_rejected / saturation checks
    return their negative defaults and the classifier falls through
    to whichever rule the remaining JSONL signals can prove.
    """
    # Rule precedence is documented in
    # metak-shared/ANALYSIS.md -- specific rules first, ``unknown``
    # only as a last resort.

    # 1. eot_timeout_internal: variant declared it gave up waiting.
    #    Takes precedence over completed because both eot_sent AND
    #    eot_timeout coexist on a self-aborted spawn -- per the EOT
    #    protocol the variant logs eot_sent first, then eot_timeout
    #    if the wait_for_peer_eots exhausted the deadline.
    if summary.has_eot_sent and summary.has_eot_timeout:
        return SpawnClassification(
            variant=variant,
            run=run,
            runner=runner,
            classification="eot_timeout_internal",
        )

    # 2. completed: this writer reached phase=silent AND emitted
    #    eot_sent AND at least one peer confirmed it. The peer
    #    confirmation is the contract-bound signal that the EOT
    #    handshake closed cleanly per E12 -- without it we'd mark a
    #    spawn "completed" that may actually have dropped its EOT and
    #    been kept alive only by silent_secs grace.
    peer_confirmed = any(
        runner in peer.eot_received_writers for peer in peer_summaries.values()
    )
    if summary.has_eot_sent and summary.has_phase_silent and peer_confirmed:
        return SpawnClassification(
            variant=variant,
            run=run,
            runner=runner,
            classification="completed",
        )

    # 3. runner_idle_terminated (T15.6): writer emitted eot_sent on
    #    its own idle detection (E15 architecture, T15.5), reached
    #    phase=silent cleanly, and did NOT log eot_timeout. No peer
    #    confirmation is required -- E15 no longer runs an on-wire
    #    EOT exchange, so peer eot_received events only appear for
    #    legacy code paths. Precedence: this rule sits AFTER
    #    ``completed`` (peer-confirmed handshake wins when it
    #    happens, e.g. websocket multi which still observes the
    #    on-wire EOT marker) and BEFORE ``eot_lost`` (which requires
    #    the writer to NOT reach phase=silent and therefore cannot
    #    coexist with this rule).
    if (
        summary.has_eot_sent
        and summary.has_phase_silent
        and not summary.has_eot_timeout
        and not peer_confirmed
    ):
        return SpawnClassification(
            variant=variant,
            run=run,
            runner=runner,
            classification="runner_idle_terminated",
        )

    # 4. eot_lost: writer reached eot_sent but never reached
    #    phase=silent. Spec rule (T14.17): the timed-out side IS the
    #    side with eot_sent in its own JSONL -- strong signal the
    #    writer published EOT but the peer never confirmed back in
    #    time (or the writer never observed the peer's own EOT and
    #    the EOT phase deadline expired with an external kill).
    #
    #    We do NOT gate this on "peer did not confirm" because the
    #    timed-out side's pain is the missing reverse-direction EOT,
    #    not whether the peer received THIS side's EOT. The motivating
    #    custom-udp-qos2-multi case has the peer confirming this
    #    writer's EOT yet this writer still timed out.
    if summary.has_eot_sent and not summary.has_phase_silent:
        sub_tags: tuple[str, ...] = ()
        if logs_dir is not None and peer_summaries:
            # Check whether any peer that DID reach phase=silent (the
            # asymmetric / apparently-successful side) has the
            # saturation hint in its stderr capture. Fall back to
            # checking THIS side's stderr too -- single-runner
            # loopback spawns have no peer to inspect.
            scanned_any_peer = False
            for peer_runner, peer in peer_summaries.items():
                if not peer.has_phase_silent:
                    continue
                scanned_any_peer = True
                peer_stderr = read_stderr_capture(
                    _stderr_path(logs_dir, variant, peer_runner)
                )
                if _has_saturation_hint(peer_stderr):
                    sub_tags = ("eot_lost_likely_saturation",)
                    break
            if not scanned_any_peer:
                self_stderr = read_stderr_capture(
                    _stderr_path(logs_dir, variant, runner)
                )
                if _has_saturation_hint(self_stderr):
                    sub_tags = ("eot_lost_likely_saturation",)
        return SpawnClassification(
            variant=variant,
            run=run,
            runner=runner,
            classification="eot_lost",
            sub_tags=sub_tags,
        )

    # 5. variant_rejected: never reached operate phase, stderr present.
    if not summary.has_phase_operate:
        stderr_text = (
            read_stderr_capture(_stderr_path(logs_dir, variant, runner))
            if logs_dir is not None
            else None
        )
        if _stderr_is_nonempty(stderr_text):
            # Match known patterns; whether the pattern is recognised
            # or not, the row is still classified as variant_rejected
            # (the spec calls out the unknown-pattern case as "still
            # classify"). The match informs the operator but doesn't
            # change the bucket.
            _matches_known_rejection(stderr_text)  # informative only
            return SpawnClassification(
                variant=variant,
                run=run,
                runner=runner,
                classification="variant_rejected",
            )

    # 6. variant_self_killed_idle (T15.11): variant reached operate
    #    but no eot_sent / no phase=silent, AND stderr capture
    #    contains the watchdog's diagnostic substring -- the
    #    variant's in-process watchdog thread observed no progress
    #    on either counter and called `std::process::exit(2)` after
    #    flushing the JSONL.
    #
    #    Mutually exclusive with `deadlock` below (which keys on
    #    JSONL truncation): the watchdog flushes the buffer before
    #    exit, so a clean self-exit produces a complete JSONL tail.
    #    Placed BEFORE the deadlock rule so the explicit watchdog
    #    signal wins over the generic truncation heuristic in the
    #    edge case where both could match (e.g. a watchdog exit
    #    racing with an unlikely partial flush failure).
    #
    #    This rule fires regardless of JSONL truncation state -- a
    #    clean watchdog exit usually leaves a clean JSONL, but the
    #    stderr signature is the load-bearing signal. Falls back to
    #    the deadlock rule (then to ``unknown``) if no stderr file
    #    exists.
    #
    #    Note: this branch also drives the (T14.17 follow-up,
    #    2026-05-14) ``variant_crashed`` distinction. Both rules
    #    consult the same stderr capture; the watchdog substring
    #    discriminates between a slow stall (signature present ->
    #    self-killed-idle) and a fast crash/panic (signature absent
    #    -> variant_crashed, handled by rule 7 below).
    if (
        summary.has_phase_operate
        and not summary.has_eot_sent
        and not summary.has_phase_silent
        and logs_dir is not None
    ):
        stderr_text = read_stderr_capture(_stderr_path(logs_dir, variant, runner))
        if _has_watchdog_signature(stderr_text):
            return SpawnClassification(
                variant=variant,
                run=run,
                runner=runner,
                classification="variant_self_killed_idle",
            )

        # 7. variant_crashed (T14.17 follow-up, 2026-05-14): variant
        #    reached operate, did NOT emit eot_sent, did NOT reach
        #    phase=silent, stderr capture lacks the watchdog
        #    signature, AND the JSONL ends mid-record. This is the
        #    fast-panic shape -- the variant entered operate then
        #    crashed abnormally within the variant process (e.g.
        #    transport-library panic on Zenoh qos3 alice under flood)
        #    too fast for the T15.11 watchdog (default 30s) to
        #    observe and fire. The JSONL tail is truncated because
        #    the process died before flushing its writer buffer.
        #
        #    Mutually exclusive with:
        #      - ``variant_self_killed_idle`` -- requires watchdog
        #        signature; this rule fires only in the absence.
        #      - ``deadlock`` (below) -- both rules key on truncated
        #        JSONL, but ``variant_crashed`` adds the
        #        ``has_phase_operate`` predicate so deadlock keeps
        #        its precedence only for the legacy pre-operate
        #        truncation edge cases.
        #      - ``eot_lost`` / ``runner_idle_terminated`` /
        #        ``completed`` -- all require ``has_eot_sent``,
        #        which is false here.
        #      - ``variant_rejected`` -- requires
        #        ``not has_phase_operate``.
        #
        #    Precedence note: placed between
        #    ``variant_self_killed_idle`` and ``deadlock`` so the
        #    watchdog signal (when present) keeps top precedence,
        #    crashes (truncated + no signature + reached operate)
        #    take the next slot, and the legacy ``deadlock`` label
        #    survives for truncated pre-operate cases or when
        #    ``logs_dir`` is unavailable for stderr reads.
        #
        #    Spawn-status caveat: ideally this rule would also gate
        #    on the runner's ``ChildOutcome::Failed(_)`` (non-zero,
        #    non-timeout exit code). The analysis pipeline does not
        #    have access to the runner's spawn-status sidecar (no
        #    such file is written today), so the rule infers the
        #    crash shape from the JSONL + stderr signals alone. In
        #    practice with T15.11 active the only way to land in
        #    this branch is a crash inside the variant -- the runner
        #    safety-net would have first triggered the watchdog or
        #    let it self-exit. If a future change writes a status
        #    sidecar, this rule can tighten by also requiring
        #    ``status == failed``.
        if jsonl_ends_mid_record(_jsonl_path(logs_dir, variant, run, runner)):
            return SpawnClassification(
                variant=variant,
                run=run,
                runner=runner,
                classification="variant_crashed",
            )

    # 8. deadlock: no eot_sent, no graceful silent, JSONL ends mid-record.
    #    With T15.11 + the T14.17 follow-up ``variant_crashed`` rule
    #    above, the only paths that still land here are: (a) the
    #    variant truncated its JSONL BEFORE reaching ``phase=operate``,
    #    or (b) ``logs_dir`` was not supplied so the stderr-aware
    #    rules 6 and 7 short-circuited. Operator reading: the variant
    #    was killed mid-record by something the classifier can't see;
    #    inspect the run manually.
    if not summary.has_eot_sent and not summary.has_phase_silent:
        if logs_dir is not None and jsonl_ends_mid_record(
            _jsonl_path(logs_dir, variant, run, runner)
        ):
            return SpawnClassification(
                variant=variant,
                run=run,
                runner=runner,
                classification="deadlock",
            )

    # 9. Fallthrough.
    return SpawnClassification(
        variant=variant,
        run=run,
        runner=runner,
        classification="unknown",
    )


def classify_group(
    group: pl.LazyFrame,
    *,
    variant: str,
    run: str,
    logs_dir: Path | None,
) -> dict[str, SpawnClassification]:
    """Classify every spawn in a ``(variant, run)`` group.

    Returns a dict mapping ``runner -> SpawnClassification``. Each
    runner's classification is computed from its own event summary
    plus the OTHER runners' summaries (for the peer-confirmed-eot
    check).
    """
    summaries = summarise_spawn_events(group)
    if not summaries:
        return {}

    out: dict[str, SpawnClassification] = {}
    for runner, summary in summaries.items():
        peers = {r: s for r, s in summaries.items() if r != runner}
        out[runner] = classify_spawn(
            variant=variant,
            run=run,
            runner=runner,
            summary=summary,
            peer_summaries=peers,
            logs_dir=logs_dir,
        )
    return out
