"""Synthetic compact-parquet fixture builder for analyzer round-trip tests.

Builds a ``.compact.parquet`` file with the exact shape produced by
``variant-base/src/compact_writer.rs`` -- one ``compact_events`` columnar
table plus the KV file metadata block carrying spawn identity and the
path/peer intern dictionaries. Used by the T18.4 round-trip tests to
exercise the compact loader without needing a live variant build.

The fixture writer mirrors the contract (see
``metak-shared/api-contracts/compact-log-schema.md``) deliberately --
when the contract changes (additive kinds, new metadata keys), this
file is the only Python-side place that needs updating to keep the
round-trip tests honest.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

import polars as pl

# Mirror the Rust-side EventKind enum. Pinned by the on-disk wire format.
KIND_WRITE = 0
KIND_RECEIVE = 1
KIND_BACKPRESSURE_SKIPPED = 2
KIND_GAP_DETECTED = 3
KIND_GAP_FILLED = 4
KIND_PHASE = 5
KIND_CONNECTED = 6
KIND_EOT_SENT = 7
KIND_EOT_RECEIVED = 8
KIND_EOT_TIMEOUT = 9
KIND_RESOURCE = 10
KIND_CLOCK_SYNC = 11

# Mirror ``variant-base::compact::PEER_SELF``.
PEER_SELF = 255


@dataclass
class CompactFixture:
    """Mutable builder for a synthetic compact-parquet file.

    Accumulates one row per logical event using interned ``path_idx`` /
    ``peer_idx`` indices, then flushes to a Parquet file via
    :meth:`write`. Each push method has the same signature as the
    corresponding ``CompactBuffers::push_*`` in ``variant-base``, so
    porting tests / adding new event kinds is mechanical.
    """

    variant: str
    runner: str
    run: str
    threading_mode: str = "single"
    recv_buffer_kb: int = 4096
    schema_version: int = 1

    ts_ns: list[int] = field(default_factory=list)
    kind: list[int] = field(default_factory=list)
    seq: list[int] = field(default_factory=list)
    path_idx: list[int] = field(default_factory=list)
    peer_idx: list[int] = field(default_factory=list)
    qos: list[int] = field(default_factory=list)
    bytes_col: list[int] = field(default_factory=list)
    extra_f32: list[float | None] = field(default_factory=list)
    extra_f32_b: list[float | None] = field(default_factory=list)
    extra_i64: list[int | None] = field(default_factory=list)
    extra_utf8: list[str | None] = field(default_factory=list)

    paths: list[str] = field(default_factory=list)
    _paths_lookup: dict[str, int] = field(default_factory=dict)
    peers: list[str] = field(default_factory=list)
    _peers_lookup: dict[str, int] = field(default_factory=dict)

    def _intern_path(self, p: str) -> int:
        idx = self._paths_lookup.get(p)
        if idx is None:
            idx = len(self.paths)
            self.paths.append(p)
            self._paths_lookup[p] = idx
        return idx

    def _intern_peer(self, p: str) -> int:
        idx = self._peers_lookup.get(p)
        if idx is None:
            idx = len(self.peers)
            self.peers.append(p)
            self._peers_lookup[p] = idx
        return idx

    def _push_row(
        self,
        *,
        ts_ns: int,
        kind: int,
        seq: int = 0,
        path_idx: int = 0,
        peer_idx: int = PEER_SELF,
        qos: int = 0,
        bytes_n: int = 0,
        extra_f32: float | None = None,
        extra_f32_b: float | None = None,
        extra_i64: int | None = None,
        extra_utf8: str | None = None,
    ) -> None:
        self.ts_ns.append(ts_ns)
        self.kind.append(kind)
        self.seq.append(seq)
        self.path_idx.append(path_idx)
        self.peer_idx.append(peer_idx)
        self.qos.append(qos)
        self.bytes_col.append(bytes_n)
        self.extra_f32.append(extra_f32)
        self.extra_f32_b.append(extra_f32_b)
        self.extra_i64.append(extra_i64)
        self.extra_utf8.append(extra_utf8)

    # ----- Per-kind pushes (mirror CompactBuffers::push_*) -----

    def push_write(
        self, ts_ns: int, path: str, qos: int, seq: int, bytes_n: int
    ) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_WRITE,
            seq=seq,
            path_idx=self._intern_path(path),
            peer_idx=PEER_SELF,
            qos=qos,
            bytes_n=bytes_n,
        )

    def push_receive(
        self,
        ts_ns: int,
        writer: str,
        seq: int,
        path: str,
        qos: int,
        bytes_n: int,
    ) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_RECEIVE,
            seq=seq,
            path_idx=self._intern_path(path),
            peer_idx=self._intern_peer(writer),
            qos=qos,
            bytes_n=bytes_n,
        )

    def push_backpressure_skipped(self, ts_ns: int, path: str, qos: int) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_BACKPRESSURE_SKIPPED,
            path_idx=self._intern_path(path),
            qos=qos,
        )

    def push_gap_detected(self, ts_ns: int, writer: str, missing_seq: int) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_GAP_DETECTED,
            seq=missing_seq,
            peer_idx=self._intern_peer(writer),
            extra_i64=missing_seq,
        )

    def push_gap_filled(self, ts_ns: int, writer: str, recovered_seq: int) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_GAP_FILLED,
            seq=recovered_seq,
            peer_idx=self._intern_peer(writer),
            extra_i64=recovered_seq,
        )

    def push_phase(self, ts_ns: int, phase: str) -> None:
        self._push_row(ts_ns=ts_ns, kind=KIND_PHASE, extra_utf8=phase)

    def push_connected(
        self,
        ts_ns: int,
        peer: str | None,
        elapsed_ms: float,
        threading_mode: str,
    ) -> None:
        peer_idx = self._intern_peer(peer) if peer is not None else PEER_SELF
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_CONNECTED,
            peer_idx=peer_idx,
            extra_f32=elapsed_ms,
            extra_utf8=threading_mode,
        )

    def push_eot_sent(self, ts_ns: int, eot_id: int) -> None:
        self._push_row(ts_ns=ts_ns, kind=KIND_EOT_SENT, extra_i64=eot_id)

    def push_eot_received(self, ts_ns: int, writer: str, eot_id: int) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_EOT_RECEIVED,
            peer_idx=self._intern_peer(writer),
            extra_i64=eot_id,
        )

    def push_eot_timeout(self, ts_ns: int, wait_ms: int, missing_json: str) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_EOT_TIMEOUT,
            extra_i64=wait_ms,
            extra_utf8=missing_json,
        )

    def push_resource(self, ts_ns: int, cpu_percent: float, memory_mb: float) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_RESOURCE,
            extra_f32=cpu_percent,
            extra_f32_b=memory_mb,
        )

    def push_clock_sync(
        self, ts_ns: int, peer: str, offset_ns: int, rtt_ms: float
    ) -> None:
        self._push_row(
            ts_ns=ts_ns,
            kind=KIND_CLOCK_SYNC,
            peer_idx=self._intern_peer(peer),
            extra_i64=offset_ns,
            extra_f32=rtt_ms,
        )

    # ----- Persist -----

    def write(self, path: Path) -> None:
        """Flush the accumulated buffers to a ``.compact.parquet`` file.

        Dtypes match the Rust writer's schema (Parquet 2.0 with
        primitive INT64 / INT32 / FLOAT / UTF8 columns; OPTIONAL on
        the four ``extra_*`` slots). The intern dictionaries and the
        spawn identifying fields go into the Parquet file's KV
        metadata block exactly the way the Rust writer encodes them.
        """
        df = pl.DataFrame(
            {
                "ts_ns": self.ts_ns,
                "kind": self.kind,
                "seq": self.seq,
                "path_idx": self.path_idx,
                "peer_idx": self.peer_idx,
                "qos": self.qos,
                "bytes": self.bytes_col,
                "extra_f32": self.extra_f32,
                "extra_f32_b": self.extra_f32_b,
                "extra_i64": self.extra_i64,
                "extra_utf8": self.extra_utf8,
            },
            schema={
                "ts_ns": pl.Int64,
                "kind": pl.Int32,
                "seq": pl.Int64,
                "path_idx": pl.Int32,
                "peer_idx": pl.Int32,
                "qos": pl.Int32,
                "bytes": pl.Int32,
                "extra_f32": pl.Float32,
                "extra_f32_b": pl.Float32,
                "extra_i64": pl.Int64,
                "extra_utf8": pl.Utf8,
            },
        )
        # KV metadata mirrors what variant-base writes -- the loader
        # decodes these keys back into the projection. Values are all
        # strings (Parquet KV metadata is utf8-only); non-scalar values
        # are JSON-encoded.
        metadata = {
            "schema_version": str(self.schema_version),
            "paths": json.dumps(self.paths),
            "peers": json.dumps(self.peers),
            "variant": self.variant,
            "runner": self.runner,
            "run": self.run,
            "threading_mode": self.threading_mode,
            "recv_buffer_kb": str(self.recv_buffer_kb),
        }
        df.write_parquet(path, compression="snappy", metadata=metadata)
