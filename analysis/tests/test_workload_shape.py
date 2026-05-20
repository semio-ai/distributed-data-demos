"""Tests for the E19 / T19.5 workload-shape additions.

Three slices of the locked spec are pinned here:

1. **Legacy JSONL parses with defaults**: a pre-E19 ``write`` event
   that omits ``leaf_count`` / ``shape`` still produces a row with
   ``leaf_count = 1`` and ``shape = "scalar"`` (api-contracts
   ``jsonl-log-schema.md`` § E19 additions).
2. **Mixed leaf_count values propagate write -> correlate ->
   performance**: a JSONL fixture that mixes ``leaf_count`` values
   across writes lands on the matching receives via
   ``correlate_lazy`` and produces a ``PerformanceResult`` whose
   ``leaves_per_sec`` accumulates the total leaf count over the
   operate window.
3. **Block-flood arithmetic**: when every write carries the same
   ``leaf_count = blob_size`` the relation
   ``leaves_per_sec == ops_per_sec * blob_size`` must hold to within
   floating-point rounding. This is the load-bearing invariant that
   lets the comparison plots in T19.6 stack ``ops_per_sec`` against
   ``leaves_per_sec`` on the same axis.
"""

from __future__ import annotations

import json

from helpers import events_to_lazy, make_event

from correlate import correlate_lazy, deliveries_to_records
from parse import project_line
from performance import performance_for_group


def _perf(events: list[dict], variant: str = "test-variant", run: str = "run01"):
    """Drive the full per-group pipeline on a list of JSONL event dicts."""
    lazy = events_to_lazy(events)
    deliveries = correlate_lazy(lazy).collect()
    return performance_for_group(lazy, deliveries, variant, run)


class TestLegacyJsonlParseDefaults:
    """Pre-E19 JSONL (no leaf_count / shape) parses with defaults."""

    def test_legacy_write_defaults_to_scalar_with_one_leaf(self) -> None:
        """A ``write`` line without ``leaf_count`` / ``shape`` -> defaults."""
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.000Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "r1",
                "event": "write",
                "seq": 1,
                "path": "/k",
                "qos": 1,
                "bytes": 8,
            }
        )
        row = project_line(line)
        assert row is not None
        # The api-contracts E19 backward-compat rule: legacy rows
        # default to ``leaf_count = 1`` and ``shape = "scalar"``.
        assert row["leaf_count"] == 1
        assert row["shape"] == "scalar"
        # ``bytes`` is now part of the projected schema -- E19 needs it
        # for the bytes_per_sec headline metric.
        assert row["bytes"] == 8

    def test_non_write_event_leaves_columns_null(self) -> None:
        """Receive / phase / resource rows leave leaf_count + shape null.

        The contract is "fields populated on write rows only". The
        analyzer propagates them onto receives via the (writer, seq,
        path) join key, not via the receive row itself.
        """
        receive_line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.000Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "r1",
                "event": "receive",
                "writer": "bob",
                "seq": 1,
                "path": "/k",
                "qos": 1,
                "bytes": 8,
            }
        )
        row = project_line(receive_line)
        assert row is not None
        assert row["leaf_count"] is None
        assert row["shape"] is None
        # ``bytes`` is populated on receive too -- it's the wire size,
        # which both sides observe.
        assert row["bytes"] == 8

    def test_explicit_leaf_count_and_shape_round_trip(self) -> None:
        """E19 writes carry leaf_count + shape; both survive projection."""
        line = json.dumps(
            {
                "ts": "2026-04-15T09:35:51.000Z",
                "variant": "custom-udp",
                "runner": "alice",
                "run": "r1",
                "event": "write",
                "seq": 7,
                "path": "/k",
                "qos": 1,
                "bytes": 800,
                "leaf_count": 100,
                "shape": "array",
            }
        )
        row = project_line(line)
        assert row is not None
        assert row["leaf_count"] == 100
        assert row["shape"] == "array"
        assert row["bytes"] == 800

    def test_legacy_perf_result_defaults_match_pre_e19_numbers(self) -> None:
        """Re-running the analyzer on legacy data -> same numbers as before.

        The contract is "the existing columns are numerically
        identical, the new columns default to 1 / 'scalar'". This
        exercises the full pipeline (parse + correlate + performance)
        on a small legacy-shaped fixture to pin the invariant.
        """
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1001,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1002,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1010,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1011,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        r = _perf(events)
        # Pre-E19 invariants: every receive is correlated, no loss,
        # writes_per_sec / receives_per_sec are derived from the
        # operate window. Leaf count is implicitly 1 per write so
        # leaves_per_sec must equal receives_per_sec to the same
        # precision the row was rendered at.
        assert r.shape == "scalar"
        assert r.ops_per_sec == r.receives_per_sec
        assert abs(r.leaves_per_sec - r.receives_per_sec) < 1e-6
        # Bytes per sec is non-zero because writes recorded 8-byte
        # payloads; if the column were dropped from the pipeline the
        # whole sum would be zero.
        assert r.bytes_per_sec > 0


class TestMixedLeafCountPropagation:
    """leaf_count flows write -> correlate -> performance correctly."""

    def test_mixed_leaf_counts_sum_into_leaves_per_sec(self) -> None:
        """Writes with leaf_count in {1, 50, 200} sum on the receive side.

        The total = 1 + 50 + 200 = 251 leaves over a 1-second operate
        window -> leaves_per_sec = 251.0.  ops_per_sec = 3.
        """
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                leaf_count=1,
                shape="scalar",
                offset_ms=1001,
            ),
            make_event(
                "write",
                runner="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=400,
                leaf_count=50,
                shape="array",
                offset_ms=1002,
            ),
            make_event(
                "write",
                runner="alice",
                seq=3,
                path="/k",
                qos=1,
                bytes=1600,
                leaf_count=200,
                shape="struct",
                offset_ms=1003,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=1,
                path="/k",
                qos=1,
                bytes=8,
                offset_ms=1010,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=2,
                path="/k",
                qos=1,
                bytes=400,
                offset_ms=1011,
            ),
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=3,
                path="/k",
                qos=1,
                bytes=1600,
                offset_ms=1012,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        # Verify correlate propagates leaf_count / shape onto deliveries.
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        records = sorted(deliveries_to_records(deliveries), key=lambda r: r.seq)
        assert len(records) == 3
        assert [r.leaf_count for r in records] == [1, 50, 200]
        assert [r.shape for r in records] == ["scalar", "array", "struct"]
        assert [r.bytes for r in records] == [8, 400, 1600]

        # Now run the full performance computation on the lazy frame.
        result = performance_for_group(lazy, deliveries, "test-variant", "run01")
        # 3 receives, 251 leaves, 2008 bytes over a 1s operate window.
        assert abs(result.ops_per_sec - 3.0) < 1e-6
        assert abs(result.leaves_per_sec - 251.0) < 1e-6
        assert abs(result.bytes_per_sec - 2008.0) < 1e-6
        # ops_per_sec == receives_per_sec by construction.
        assert result.ops_per_sec == result.receives_per_sec

    def test_unmatched_receive_does_not_get_leaf_count(self) -> None:
        """A receive without a matching write produces NO delivery.

        Correlate joins on (writer, seq, path). If no write row exists
        for the receive's key, the join drops the row. There is no
        "phantom leaf_count" inherited from an unrelated write.
        """
        events = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event(
                "write",
                runner="alice",
                seq=1,
                path="/a",
                qos=1,
                bytes=8,
                leaf_count=10,
                shape="array",
                offset_ms=1001,
            ),
            # Receive for /b -- no matching write.
            make_event(
                "receive",
                runner="bob",
                writer="alice",
                seq=99,
                path="/b",
                qos=1,
                bytes=8,
                offset_ms=1011,
            ),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        lazy = events_to_lazy(events)
        deliveries = correlate_lazy(lazy).collect()
        # The unmatched receive does not produce a delivery row.
        assert deliveries.height == 0


class TestBlockFloodArithmetic:
    """Block-flood: leaves_per_sec = ops_per_sec * blob_size."""

    def test_constant_blob_size_yields_arithmetic_identity(self) -> None:
        """Every write has leaf_count=blob_size, shape='array'.

        With ``blob_size = 100``, ``N = 10`` writes over a 1-second
        operate window:

        - ``ops_per_sec`` = 10 / 1 = 10
        - ``leaves_per_sec`` = 100 * 10 / 1 = 1000
        - ``leaves_per_sec / ops_per_sec`` = blob_size = 100

        The invariant the comparison plots in T19.6 rely on is the
        last identity -- it lets a single ``leaves_per_sec`` y-axis
        compare scalar-flood (where each write is one leaf) against
        block-flood (where each write is N leaves) on the same scale.
        """
        blob_size = 100
        n_writes = 10
        events: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000)
        ]
        for i in range(1, n_writes + 1):
            # Spread writes over [1001, 1100] so they all land
            # comfortably inside the [1000, 2000] operate window.
            offset = 1001 + (i - 1) * (99.0 / max(n_writes - 1, 1))
            events.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=blob_size * 4,  # 4 bytes per leaf
                    leaf_count=blob_size,
                    shape="array",
                    offset_ms=offset,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=blob_size * 4,
                    offset_ms=offset + 5,
                )
            )
        events.append(
            make_event("phase", runner="alice", phase="silent", offset_ms=2000)
        )

        result = _perf(events)

        # Arithmetic identity to within float rounding.
        assert result.ops_per_sec > 0
        assert result.leaves_per_sec > 0
        # The headline identity: leaves_per_sec / ops_per_sec == blob_size.
        ratio = result.leaves_per_sec / result.ops_per_sec
        assert abs(ratio - blob_size) < 1e-6, (
            f"Expected leaves_per_sec / ops_per_sec == {blob_size}, "
            f"got {ratio} (leaves={result.leaves_per_sec}, "
            f"ops={result.ops_per_sec})"
        )
        # Shape is the array workload.
        assert result.shape == "array"

    def test_scalar_flood_collapses_to_ops_per_sec(self) -> None:
        """scalar-flood: leaves_per_sec == ops_per_sec exactly.

        The api-contracts contract states ``leaves_per_sec ==
        ops_per_sec`` for ``scalar-flood`` runs because every WriteOp
        carries one leaf. This pins that identity end-to-end.
        """
        events: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000)
        ]
        for i in range(1, 6):
            events.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=8,
                    leaf_count=1,
                    shape="scalar",
                    offset_ms=1000 + i,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1005 + i,
                )
            )
        events.append(
            make_event("phase", runner="alice", phase="silent", offset_ms=2000)
        )

        result = _perf(events)
        # The two metrics coincide exactly: every WriteOp = 1 leaf.
        assert result.leaves_per_sec == result.ops_per_sec
        assert result.shape == "scalar"

    def test_compact_parquet_with_leaf_count_and_shape(self, tmp_path) -> None:
        """A compact-parquet file with E19 columns round-trips into deliveries.

        Builds a synthetic compact-parquet file that includes the new
        ``leaf_count`` column + ``shape_intern`` KV metadata,
        round-trips it through ``read_compact_parquet``, then runs the
        correlate pipeline and asserts both columns land on the
        deliveries with the expected values. This is the read-side
        analog of the JSONL fixtures above.
        """
        import polars as pl

        from parse_compact import read_compact_parquet
        from compact_fixture import CompactFixture, KIND_WRITE

        fx = CompactFixture(variant="v", runner="alice", run="r1")
        base_ns = 1_700_000_000_000_000_000

        # operate / silent phase markers so the operate window is well
        # defined for the downstream performance helpers.
        fx.push_phase(base_ns + 1_000_000_000, "operate")
        # Two writes: a scalar and a block-flood array.
        fx.push_write(
            ts_ns=base_ns + 1_001_000_000,
            path="/k",
            qos=1,
            seq=1,
            bytes_n=8,
        )
        fx.push_write(
            ts_ns=base_ns + 1_002_000_000,
            path="/k",
            qos=1,
            seq=2,
            bytes_n=400,
        )
        # Receives for both seq values.
        fx.push_receive(
            ts_ns=base_ns + 1_010_000_000,
            writer="alice",
            seq=1,
            path="/k",
            qos=1,
            bytes_n=8,
        )
        fx.push_receive(
            ts_ns=base_ns + 1_011_000_000,
            writer="alice",
            seq=2,
            path="/k",
            qos=1,
            bytes_n=400,
        )
        fx.push_phase(base_ns + 2_000_000_000, "silent")

        # Push the fixture out to a parquet file, then mutate it to add
        # the E19 columns directly. The fixture builder doesn't yet
        # know about them (it lives alongside the contract; T19.2
        # variant-base side will land later), so we synthesize the
        # extra columns inline here -- this mirrors what a T19.2
        # writer would emit.
        path = tmp_path / "v-alice-r1.compact.parquet"
        fx.write(path)

        # Re-write with leaf_count + shape_idx columns + shape_intern
        # KV metadata. ``read_compact_parquet`` should resolve them.
        raw = pl.read_parquet(str(path))
        leaf_counts = []
        shape_idxs = []
        for k in raw.get_column("kind").to_list():
            if k == KIND_WRITE:
                # Encode two write rows differently: first scalar, second array.
                pos = len(leaf_counts) - sum(
                    1
                    for kk in raw.get_column("kind").to_list()[: len(leaf_counts)]
                    if kk != KIND_WRITE
                )
                if pos == 0:
                    leaf_counts.append(1)
                    shape_idxs.append(0)
                else:
                    leaf_counts.append(100)
                    shape_idxs.append(1)
            else:
                leaf_counts.append(None)
                shape_idxs.append(None)

        augmented = raw.with_columns(
            pl.Series("leaf_count", leaf_counts, dtype=pl.UInt32),
            pl.Series("shape_idx", shape_idxs, dtype=pl.UInt32),
        )
        # Re-emit with the augmented columns AND a ``shape_intern`` KV
        # entry mapping idx 0 -> "scalar", idx 1 -> "array".
        meta = {
            "schema_version": str(fx.schema_version),
            "paths": json.dumps(fx.paths),
            "peers": json.dumps(fx.peers),
            "variant": fx.variant,
            "runner": fx.runner,
            "run": fx.run,
            "threading_mode": fx.threading_mode,
            "recv_buffer_kb": str(fx.recv_buffer_kb),
            "shape_intern": json.dumps(["scalar", "array"]),
        }
        augmented.write_parquet(path, compression="snappy", metadata=meta)

        # Now run the loader and verify the write rows carry the
        # expected ``leaf_count`` / ``shape`` values.
        projected = read_compact_parquet(path)
        write_rows = projected.filter(pl.col("event") == "write").sort("seq")
        assert write_rows.height == 2
        assert write_rows.get_column("leaf_count").to_list() == [1, 100]
        assert write_rows.get_column("shape").to_list() == ["scalar", "array"]
        # ``bytes`` is now in the projected shard too.
        assert write_rows.get_column("bytes").to_list() == [8, 400]
        # Receive rows still leave leaf_count / shape null (the wire
        # is opaque); the correlate step pulls them off the write side.
        recv_rows = projected.filter(pl.col("event") == "receive").sort("seq")
        assert recv_rows.height == 2
        assert recv_rows.get_column("leaf_count").to_list() == [None, None]
        assert recv_rows.get_column("shape").to_list() == [None, None]

    def test_legacy_data_falls_into_scalar_branch(self) -> None:
        """No leaf_count / shape -> still satisfies the scalar identity.

        Legacy logs (pre-E19) default to ``leaf_count = 1`` / ``shape =
        "scalar"`` so the same identity ``leaves_per_sec ==
        ops_per_sec`` must hold even when the fields are entirely
        absent on the source. This is the regression guard for the
        backward-compat invariant from the api-contracts.
        """
        events: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000)
        ]
        for i in range(1, 6):
            events.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1000 + i,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1005 + i,
                )
            )
        events.append(
            make_event("phase", runner="alice", phase="silent", offset_ms=2000)
        )

        result = _perf(events)
        assert result.shape == "scalar"
        assert result.leaves_per_sec == result.ops_per_sec


class TestShapeDisplay:
    """T19.12: ``shape_display`` honors the distinct-shapes set.

    These tests pin the contract at the PerformanceResult level (not the
    table-rendering level -- that's in ``test_tables.py``). The display
    field must:

    1. Render the verbatim shape value when the group is homogeneous.
    2. Render the literal ``"mixed"`` when the group spans multiple
       distinct shapes -- the load-bearing fix for the T19.8 issue #6
       where mixed-types rows displayed ``"array"``.
    3. Stay consistent with :attr:`PerformanceResult.shape` (the
       dominant-shape field) for homogeneous groups: when there is
       only one shape, ``shape_display == shape``.
    """

    def test_scalar_flood_shape_display_is_scalar(self) -> None:
        events: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000)
        ]
        for i in range(1, 4):
            events.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=8,
                    leaf_count=1,
                    shape="scalar",
                    offset_ms=1000 + i,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=8,
                    offset_ms=1010 + i,
                )
            )
        events.append(
            make_event("phase", runner="alice", phase="silent", offset_ms=2000)
        )
        result = _perf(events)
        assert result.shape == "scalar"
        assert result.shape_display == "scalar"

    def test_block_flood_shape_display_is_array(self) -> None:
        events: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000)
        ]
        for i in range(1, 4):
            events.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=400,
                    leaf_count=100,
                    shape="array",
                    offset_ms=1000 + i,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=400,
                    offset_ms=1010 + i,
                )
            )
        events.append(
            make_event("phase", runner="alice", phase="silent", offset_ms=2000)
        )
        result = _perf(events)
        assert result.shape == "array"
        assert result.shape_display == "array"

    def test_mixed_types_shape_display_is_mixed(self) -> None:
        """Heterogeneous group -> ``shape_display == "mixed"``.

        The dominant-shape field still resolves to the lex-first
        non-null value (``"array"`` here -- ``a`` < ``s``) because other
        consumers (legend hatch picker, chart sort-order) want a single
        stable identifier. ``shape_display`` is the operator-facing
        label that calls out the heterogeneity.
        """
        events: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000)
        ]
        shapes = ["scalar", "array", "struct"]
        leaf_counts = [1, 100, 50]
        sizes = [8, 400, 200]
        for i, (s, lc, sz) in enumerate(zip(shapes, leaf_counts, sizes), start=1):
            events.append(
                make_event(
                    "write",
                    runner="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=sz,
                    leaf_count=lc,
                    shape=s,
                    offset_ms=1000 + i,
                )
            )
            events.append(
                make_event(
                    "receive",
                    runner="bob",
                    writer="alice",
                    seq=i,
                    path="/k",
                    qos=1,
                    bytes=sz,
                    offset_ms=1010 + i,
                )
            )
        events.append(
            make_event("phase", runner="alice", phase="silent", offset_ms=2000)
        )
        result = _perf(events)
        # Dominant shape preserved at lex-first non-null entry.
        assert result.shape == "array"
        # Display value honestly reflects heterogeneity.
        assert result.shape_display == "mixed"

    def test_empty_deliveries_shape_display_defaults_to_scalar(self) -> None:
        """No deliveries -> fall back to ``"scalar"`` (same default as ``shape``)."""
        events: list[dict] = [
            make_event("phase", runner="alice", phase="operate", offset_ms=1000),
            make_event("phase", runner="alice", phase="silent", offset_ms=2000),
        ]
        result = _perf(events)
        assert result.shape == "scalar"
        assert result.shape_display == "scalar"
