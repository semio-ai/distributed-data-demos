"""Write-receive correlation to produce delivery records."""

from __future__ import annotations

from parse import DeliveryRecord, Event


def correlate(events: list[Event]) -> list[DeliveryRecord]:
    """Join write events with receive events to produce delivery records.

    Correlation key: ``(variant, run, seq, path)`` where the receive
    event's ``writer`` field matches the write event's ``runner`` field.

    For single-runner loopback (e.g. VariantDummy), the writer and
    receiver are the same runner. This is valid and produces delivery
    records with near-zero latency.
    """
    # Index write events by (variant, run, writer_runner, seq, path)
    writes: dict[tuple[str, str, str, int, str], Event] = {}
    receives: list[Event] = []

    for ev in events:
        if ev.event == "write":
            seq = ev.data.get("seq")
            path = ev.data.get("path")
            if seq is not None and path is not None:
                key = (ev.variant, ev.run, ev.runner, int(seq), str(path))
                writes[key] = ev
        elif ev.event == "receive":
            receives.append(ev)

    records: list[DeliveryRecord] = []
    for recv_ev in receives:
        writer = recv_ev.data.get("writer")
        seq = recv_ev.data.get("seq")
        path = recv_ev.data.get("path")
        qos = recv_ev.data.get("qos", 1)

        if writer is None or seq is None or path is None:
            continue

        key = (recv_ev.variant, recv_ev.run, str(writer), int(seq), str(path))
        write_ev = writes.get(key)
        if write_ev is None:
            continue

        delta = (recv_ev.ts - write_ev.ts).total_seconds() * 1000.0
        records.append(
            DeliveryRecord(
                variant=recv_ev.variant,
                run=recv_ev.run,
                path=str(path),
                seq=int(seq),
                qos=int(qos),
                writer=str(writer),
                receiver=recv_ev.runner,
                write_ts=write_ev.ts,
                receive_ts=recv_ev.ts,
                latency_ms=delta,
            )
        )

    return records
