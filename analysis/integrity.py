"""Integrity verification for benchmark delivery records."""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass

from parse import DeliveryRecord, Event


@dataclass
class IntegrityResult:
    """Integrity check result for one (variant, run, writer -> receiver) pair."""

    variant: str
    run: str
    writer: str
    receiver: str
    qos: int
    write_count: int
    receive_count: int
    delivery_pct: float
    out_of_order: int
    duplicates: int
    unresolved_gaps: int | None  # None when gap checking does not apply
    # Error flags (True means the integrity check failed)
    completeness_error: bool
    ordering_error: bool
    duplicate_error: bool
    gap_error: bool


# Type alias for the grouping key
_PairKey = tuple[str, str, str, str]  # (variant, run, writer, receiver)


def _group_deliveries(
    records: list[DeliveryRecord],
) -> dict[_PairKey, list[DeliveryRecord]]:
    """Group delivery records by (variant, run, writer, receiver)."""
    groups: dict[_PairKey, list[DeliveryRecord]] = defaultdict(list)
    for rec in records:
        key = (rec.variant, rec.run, rec.writer, rec.receiver)
        groups[key].append(rec)
    return groups


def _count_writes(
    events: list[Event],
) -> dict[tuple[str, str, str], int]:
    """Count write events per (variant, run, writer).

    Returns a dict mapping (variant, run, writer) to write count.
    """
    counts: dict[tuple[str, str, str], int] = defaultdict(int)
    for ev in events:
        if ev.event == "write":
            counts[(ev.variant, ev.run, ev.runner)] += 1
    return counts


def _get_receivers(
    records: list[DeliveryRecord],
) -> dict[tuple[str, str, str], set[str]]:
    """Find all receivers for each (variant, run, writer)."""
    result: dict[tuple[str, str, str], set[str]] = defaultdict(set)
    for rec in records:
        result[(rec.variant, rec.run, rec.writer)].add(rec.receiver)
    return result


def _check_ordering(records: list[DeliveryRecord]) -> int:
    """Count out-of-order receives (non-decreasing seq expected).

    Records must already be sorted by receive timestamp.
    """
    sorted_recs = sorted(records, key=lambda r: r.receive_ts)
    count = 0
    prev_seq = -1
    for rec in sorted_recs:
        if rec.seq < prev_seq:
            count += 1
        prev_seq = rec.seq
    return count


def _check_duplicates(records: list[DeliveryRecord]) -> int:
    """Count duplicate receives (same writer, seq, path on same receiver)."""
    seen: set[tuple[str, int, str]] = set()
    dupes = 0
    for rec in records:
        key = (rec.writer, rec.seq, rec.path)
        if key in seen:
            dupes += 1
        else:
            seen.add(key)
    return dupes


def _check_gaps(
    events: list[Event], variant: str, run: str, writer: str, receiver: str
) -> int | None:
    """Check gap_detected vs gap_filled events for QoS 3.

    Returns the number of unresolved gaps, or None if no gap events exist.
    """
    detected: set[int] = set()
    filled: set[int] = set()

    for ev in events:
        if ev.variant != variant or ev.run != run or ev.runner != receiver:
            continue
        writer_field = ev.data.get("writer")
        if writer_field != writer:
            continue

        if ev.event == "gap_detected":
            missing_seq = ev.data.get("missing_seq")
            if missing_seq is not None:
                detected.add(int(missing_seq))
        elif ev.event == "gap_filled":
            recovered_seq = ev.data.get("recovered_seq")
            if recovered_seq is not None:
                filled.add(int(recovered_seq))

    if not detected and not filled:
        return None

    return len(detected - filled)


def verify_integrity(
    events: list[Event],
    records: list[DeliveryRecord],
) -> list[IntegrityResult]:
    """Run integrity checks on all (variant, run, writer -> receiver) pairs.

    Returns a list of IntegrityResult objects, one per pair.
    """
    write_counts = _count_writes(events)
    receivers_map = _get_receivers(records)
    grouped = _group_deliveries(records)

    # Build the set of all pairs we need to check. This includes pairs
    # where a writer has writes but a known receiver got zero receives
    # (would show as missing from grouped).
    all_pairs: set[_PairKey] = set(grouped.keys())
    for (variant, run, writer), recv_set in receivers_map.items():
        for receiver in recv_set:
            all_pairs.add((variant, run, writer, receiver))

    results: list[IntegrityResult] = []

    for variant, run, writer, receiver in sorted(all_pairs):
        pair_records = grouped.get((variant, run, writer, receiver), [])
        w_count = write_counts.get((variant, run, writer), 0)
        r_count = len(pair_records)

        # Determine QoS from records (use the first record's qos)
        qos = pair_records[0].qos if pair_records else 1

        delivery_pct = (r_count / w_count * 100.0) if w_count > 0 else 0.0
        out_of_order = _check_ordering(pair_records) if pair_records else 0
        duplicates = _check_duplicates(pair_records) if pair_records else 0

        # Gap checking only applies to QoS 3
        if qos == 3:
            unresolved_gaps = _check_gaps(events, variant, run, writer, receiver)
            if unresolved_gaps is None:
                unresolved_gaps = 0
        else:
            unresolved_gaps = None

        # Error flags based on QoS level
        completeness_error = False
        ordering_error = False
        duplicate_error = False
        gap_error = False

        if qos >= 3:
            # QoS 3-4: 100% delivery required
            completeness_error = r_count < w_count
            # QoS 3-4: strict ordering required
            ordering_error = out_of_order > 0
            # QoS 3-4: duplicates are errors
            duplicate_error = duplicates > 0
        elif qos == 2:
            # QoS 2: ordering required, delivery is loss-tolerant
            ordering_error = out_of_order > 0

        # QoS 1: no error flags (fully loss-tolerant, unordered)

        if qos == 3 and unresolved_gaps is not None:
            gap_error = unresolved_gaps > 0

        results.append(
            IntegrityResult(
                variant=variant,
                run=run,
                writer=writer,
                receiver=receiver,
                qos=qos,
                write_count=w_count,
                receive_count=r_count,
                delivery_pct=delivery_pct,
                out_of_order=out_of_order,
                duplicates=duplicates,
                unresolved_gaps=unresolved_gaps,
                completeness_error=completeness_error,
                ordering_error=ordering_error,
                duplicate_error=duplicate_error,
                gap_error=gap_error,
            )
        )

    return results
