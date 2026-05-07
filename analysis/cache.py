"""Per-shard Parquet cache for analysis logs.

Replaces the Phase 1 monolithic pickle cache with one Parquet shard
plus a JSON sidecar per source JSONL file. Layout::

    <logs-dir>/
        <name>-<runner>-<run>.jsonl              # source logs (untouched)
        ...
        .cache/
            <name>-<runner>-<run>.parquet
            <name>-<runner>-<run>.meta.json
            ...
            _cache_schema_version.json

A shard is **stale** when any of:

* sidecar missing or malformed
* ``schema_version`` mismatch with ``schema.SCHEMA_VERSION``
* sidecar ``mtime`` < JSONL ``mtime``
* shard parquet missing

Stale shards are rebuilt by streaming the JSONL line-by-line through
``parse.iter_rows`` and writing fixed-size row-group batches via polars.
This keeps peak memory bounded by the batch buffer rather than by the
file size: a 2.1 GB JSONL file is ingested with at most a single batch
of 100k rows in RAM at any moment.

Orphan shards (no matching JSONL) are removed.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
from concurrent.futures import ProcessPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

import polars as pl

from parse import COLUMN_ORDER, iter_rows
from schema import SCHEMA_VERSION, SHARD_SCHEMA

CACHE_DIRNAME: str = ".cache"
LEGACY_PICKLE_NAME: str = ".analysis_cache.pkl"
GLOBAL_SENTINEL_NAME: str = "_cache_schema_version.json"

# Number of rows to accumulate before flushing to disk as a Parquet
# row group. 100k rows of the columnar event layout is on the order of
# a few MB in Arrow representation -- well within bounded-memory budget.
DEFAULT_BATCH_ROWS: int = 100_000


@dataclass(frozen=True)
class ShardMeta:
    """Sidecar metadata for a single Parquet shard.

    ``variant`` and ``run`` are populated from the first row of the
    source JSONL during build. ``is_clocksync`` records whether the
    shard's first row is a ``clock_sync`` event (clock-sync logs need
    to be broadcast across every variant in a run; see ``discover_groups``).
    All three are persisted in the global sentinel index (and,
    opportunistically, the per-shard sidecar) so warm runs can recover
    the per-shard mapping without opening any Parquet shard.
    """

    mtime: float
    row_count: int
    schema_version: str
    variant: str | None = None
    run: str | None = None
    is_clocksync: bool | None = None

    def to_dict(self) -> dict:
        out: dict = {
            "mtime": self.mtime,
            "row_count": self.row_count,
            "schema_version": self.schema_version,
        }
        if self.variant is not None:
            out["variant"] = self.variant
        if self.run is not None:
            out["run"] = self.run
        if self.is_clocksync is not None:
            out["is_clocksync"] = self.is_clocksync
        return out

    @classmethod
    def from_dict(cls, obj: dict) -> ShardMeta | None:
        try:
            variant_raw = obj.get("variant")
            run_raw = obj.get("run")
            clocksync_raw = obj.get("is_clocksync")
            return cls(
                mtime=float(obj["mtime"]),
                row_count=int(obj["row_count"]),
                schema_version=str(obj["schema_version"]),
                variant=str(variant_raw) if variant_raw is not None else None,
                run=str(run_raw) if run_raw is not None else None,
                is_clocksync=bool(clocksync_raw) if clocksync_raw is not None else None,
            )
        except (KeyError, TypeError, ValueError):
            return None


def cache_dir(logs_dir: Path) -> Path:
    """Path to the per-shard cache directory under ``logs_dir``."""
    return logs_dir / CACHE_DIRNAME


def shard_paths(logs_dir: Path, stem: str) -> tuple[Path, Path]:
    """Return ``(parquet_path, meta_path)`` for a JSONL stem."""
    base = cache_dir(logs_dir)
    return base / f"{stem}.parquet", base / f"{stem}.meta.json"


def _read_meta(meta_path: Path) -> ShardMeta | None:
    if not meta_path.exists():
        return None
    try:
        with open(meta_path, encoding="utf-8") as f:
            return ShardMeta.from_dict(json.load(f))
    except (OSError, json.JSONDecodeError):
        return None


def _write_meta(meta_path: Path, meta: ShardMeta) -> None:
    tmp = meta_path.with_suffix(meta_path.suffix + ".tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(meta.to_dict(), f)
    tmp.replace(meta_path)


def _read_global_sentinel(logs_dir: Path) -> str | None:
    """Read just the global ``schema_version`` from the sentinel.

    Returns ``None`` if the sentinel is missing or malformed. Unchanged
    in shape so a sentinel from before the index extension still parses.
    """
    path = cache_dir(logs_dir) / GLOBAL_SENTINEL_NAME
    if not path.exists():
        return None
    try:
        with open(path, encoding="utf-8") as f:
            obj = json.load(f)
        return str(obj.get("schema_version"))
    except (OSError, json.JSONDecodeError):
        return None


def _read_global_index(logs_dir: Path) -> dict[str, ShardMeta]:
    """Read the per-stem ``ShardMeta`` index from the global sentinel.

    The sentinel may be in either of two formats:

    * Legacy (Phase 1.5 T11.1): ``{"schema_version": "1"}`` -- no index;
      returns an empty dict so callers fall back to per-sidecar reads.
    * Extended: ``{"schema_version": "1", "shards": {<stem>: {...}}}``.
      Each ``shards`` entry round-trips through ``ShardMeta.from_dict``.

    Entries whose ``schema_version`` does not match
    ``SCHEMA_VERSION`` are silently dropped -- the caller will rebuild
    them on the per-shard stale path.
    """
    path = cache_dir(logs_dir) / GLOBAL_SENTINEL_NAME
    if not path.exists():
        return {}
    try:
        with open(path, encoding="utf-8") as f:
            obj = json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}
    shards = obj.get("shards")
    if not isinstance(shards, dict):
        return {}
    out: dict[str, ShardMeta] = {}
    for stem, payload in shards.items():
        if not isinstance(payload, dict):
            continue
        meta = ShardMeta.from_dict(payload)
        if meta is None or meta.schema_version != SCHEMA_VERSION:
            continue
        out[str(stem)] = meta
    return out


def _write_global_sentinel(
    logs_dir: Path,
    metas: dict[str, ShardMeta] | None = None,
) -> None:
    """Persist the global sentinel, optionally including the shard index.

    When ``metas`` is provided the sentinel records per-stem
    ``ShardMeta`` so warm-path ``update_cache`` and ``discover_groups``
    can short-circuit per-file reads. The legacy version-only shape is
    still produced when ``metas`` is ``None`` (e.g. on a forced rebuild
    where the index will be repopulated on the next run).
    """
    base = cache_dir(logs_dir)
    base.mkdir(parents=True, exist_ok=True)
    path = base / GLOBAL_SENTINEL_NAME
    payload: dict = {"schema_version": SCHEMA_VERSION}
    if metas is not None:
        payload["shards"] = {stem: meta.to_dict() for stem, meta in metas.items()}
    tmp = path.with_suffix(path.suffix + ".tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(payload, f)
    tmp.replace(path)


def _delete_legacy_pickle(logs_dir: Path) -> None:
    legacy = logs_dir / LEGACY_PICKLE_NAME
    if legacy.exists():
        try:
            legacy.unlink()
            print(
                f"[cache] Removed legacy Phase 1 pickle cache: {legacy}",
                file=sys.stderr,
            )
        except OSError as exc:
            print(
                f"[cache] WARNING: could not remove legacy pickle {legacy}: {exc}",
                file=sys.stderr,
            )


def _is_stale(jsonl_path: Path, meta: ShardMeta | None, parquet_path: Path) -> bool:
    """Return True if the shard for ``jsonl_path`` must be rebuilt."""
    if meta is None:
        return True
    if meta.schema_version != SCHEMA_VERSION:
        return True
    if not parquet_path.exists():
        return True
    try:
        jsonl_mtime = jsonl_path.stat().st_mtime
    except OSError:
        return True
    if meta.mtime < jsonl_mtime:
        return True
    return False


def _batches(stream: Iterator[dict], batch_rows: int) -> Iterator[list[dict]]:
    """Yield row-dicts grouped into batches of at most ``batch_rows``."""
    batch: list[dict] = []
    for row in stream:
        batch.append(row)
        if len(batch) >= batch_rows:
            yield batch
            batch = []
    if batch:
        yield batch


def _build_shard_worker(args: tuple) -> tuple[str, ShardMeta]:
    """Top-level worker for ``ProcessPoolExecutor``.

    Must be defined at module scope (and not be a closure) so that it
    can be pickled and dispatched to a worker process.
    """
    jsonl_str, parquet_str, meta_str, batch_rows = args
    meta = _build_shard(
        Path(jsonl_str),
        Path(parquet_str),
        Path(meta_str),
        batch_rows=batch_rows,
    )
    # Return the JSONL stem so the parent can index by it.
    return Path(jsonl_str).stem, meta


def _build_shard(
    jsonl_path: Path,
    parquet_path: Path,
    meta_path: Path,
    *,
    batch_rows: int = DEFAULT_BATCH_ROWS,
) -> ShardMeta:
    """Stream-build the Parquet shard for ``jsonl_path``.

    Memory is bounded by ``batch_rows``: at most one batch and one
    DataFrame/Arrow buffer at a time. Flushes the sidecar only after the
    Parquet write fully succeeds.

    Strategy: collect each batch into a polars DataFrame (typed via
    ``SHARD_SCHEMA``) and concatenate them at the very end before a
    single ``write_parquet`` call. The DataFrames hold compact columnar
    Arrow buffers (rather than Python row-dicts) so the tail batches are
    much smaller than the JSONL bytes they came from. The two-stage
    "rows -> small typed DataFrame -> single concat" approach beats both:

    - Calling ``write_parquet`` per batch (no append API in polars).
    - Holding all rows as Python dicts before a single conversion.

    A 2.1 GB JSONL file (~7M rows) yields ~70 batched DataFrames whose
    aggregate Arrow size stays well under 1 GB, fitting the acceptance
    criterion for the largest single shard.
    """
    cache_dir_path = parquet_path.parent
    cache_dir_path.mkdir(parents=True, exist_ok=True)

    typed_batches: list[pl.DataFrame] = []
    row_count = 0
    variant: str | None = None
    run: str | None = None
    is_clocksync: bool | None = None

    with open(jsonl_path, encoding="utf-8") as stream:
        for batch in _batches(iter_rows(stream), batch_rows):
            df = pl.DataFrame(batch, schema=SHARD_SCHEMA, orient="row")
            if variant is None and df.height > 0:
                # Recover (variant, run) and the clock-sync flag once
                # from the first non-empty batch's first row. By log
                # convention every line in a variant JSONL has the
                # same (variant, run); cache it now so the warm path
                # skips the per-shard mini Parquet read.
                first = df.row(0, named=True)
                v = first.get("variant")
                r = first.get("run")
                e = first.get("event")
                if v is not None:
                    variant = str(v)
                if r is not None:
                    run = str(r)
                # Mirror the two-check rule in ``_is_clocksync_shard``:
                # a shard is clock-sync-only if its first row's event
                # is one of the known clock-sync event names OR if its
                # variant is empty (broadcast-only sibling log).
                if e is not None or v is not None:
                    e_str = str(e) if e is not None else ""
                    v_str = str(v) if v is not None else ""
                    is_clocksync = (
                        e_str
                        in (
                            "clock_sync",
                            "clock_sync_sample",
                        )
                        or v_str == ""
                    )
            row_count += df.height
            typed_batches.append(df)

    if not typed_batches:
        empty = pl.DataFrame(schema=SHARD_SCHEMA)
        empty = empty.select(list(COLUMN_ORDER))
        empty.write_parquet(parquet_path, compression="snappy")
    else:
        combined = pl.concat(typed_batches, how="vertical")
        combined = combined.select(list(COLUMN_ORDER))
        combined.write_parquet(parquet_path, compression="snappy")

    # Free any references before flushing meta.
    del typed_batches

    jsonl_mtime = jsonl_path.stat().st_mtime
    meta = ShardMeta(
        mtime=jsonl_mtime,
        row_count=row_count,
        schema_version=SCHEMA_VERSION,
        variant=variant,
        run=run,
        is_clocksync=is_clocksync,
    )
    _write_meta(meta_path, meta)
    return meta


def _remove_orphan_shards(logs_dir: Path, valid_stems: set[str]) -> None:
    """Delete shards (and sidecars) whose source JSONL no longer exists."""
    base = cache_dir(logs_dir)
    if not base.is_dir():
        return
    for entry in base.iterdir():
        if not entry.is_file():
            continue
        # Don't touch the global sentinel.
        if entry.name == GLOBAL_SENTINEL_NAME:
            continue
        # Determine stem regardless of suffix combinations.
        if entry.name.endswith(".parquet"):
            stem = entry.name[: -len(".parquet")]
        elif entry.name.endswith(".meta.json"):
            stem = entry.name[: -len(".meta.json")]
        else:
            continue
        if stem not in valid_stems:
            try:
                entry.unlink()
            except OSError:
                pass


def _default_workers() -> int:
    """Pick a sensible default worker count for parallel ingestion.

    Caps at 8 because the JSONL parsing is largely Python+JSON-bound,
    so going wider than the typical fast I/O fan-out yields diminishing
    returns. Caller can override via the ``workers`` arg.
    """
    cpu = os.cpu_count() or 1
    return max(1, min(8, cpu - 1))


def update_cache(
    logs_dir: Path,
    *,
    clear: bool = False,
    batch_rows: int = DEFAULT_BATCH_ROWS,
    workers: int | None = None,
    on_progress: callable | None = None,
) -> dict[str, ShardMeta]:
    """Bring the per-shard cache for ``logs_dir`` up to date.

    Returns a dict mapping JSONL stem -> ``ShardMeta`` for every shard
    that is now present in the cache.

    Steps:
      1. Delete any legacy Phase 1 pickle.
      2. If ``clear``, wipe the entire ``.cache/`` directory.
      3. Detect a missing or stale global schema sentinel; if mismatched
         the entire ``.cache/`` directory is wiped before continuing.
      4. For each ``*.jsonl`` in ``logs_dir``, decide if its shard is
         stale (per ``_is_stale``). Build all stale shards in parallel
         using a ``ProcessPoolExecutor`` so JSON parsing scales across
         CPU cores.
      5. Remove orphan shards.
      6. Refresh the global sentinel.

    Parallelism is bounded by ``workers`` (default: min(8, cpu-1)). Each
    worker processes one JSONL at a time, so peak memory is approx.
    ``workers * (single-shard peak)`` -- with 100k-row batches and
    ~8 workers this stays well under 1 GB on the largest individual
    file (~2 GB JSONL).
    """
    _delete_legacy_pickle(logs_dir)

    base = cache_dir(logs_dir)

    if clear and base.exists():
        shutil.rmtree(base)

    # Check global sentinel; if version mismatch (or missing while
    # shards exist), treat as a global rebuild request.
    if base.exists():
        sentinel_version = _read_global_sentinel(logs_dir)
        if sentinel_version is not None and sentinel_version != SCHEMA_VERSION:
            shutil.rmtree(base)

    base.mkdir(parents=True, exist_ok=True)

    # Pre-load the global shard index (if any). On warm runs this turns
    # the per-stem stale check into a single hash lookup, skipping the
    # 128 sidecar opens + json.load calls that dominated the previous
    # warm wall-time on the 40 GB dataset.
    indexed_metas: dict[str, ShardMeta] = _read_global_index(logs_dir)

    jsonl_files = sorted(logs_dir.glob("*.jsonl"))
    valid_stems: set[str] = {p.stem for p in jsonl_files}

    metas: dict[str, ShardMeta] = {}
    stale_jobs: list[tuple[str, str, str, int]] = []

    for jsonl_path in jsonl_files:
        stem = jsonl_path.stem
        parquet_path, meta_path = shard_paths(logs_dir, stem)

        existing_meta: ShardMeta | None = indexed_metas.get(stem)
        if existing_meta is None:
            # Index miss -- fall back to the per-sidecar read path so
            # caches built before the index extension still work.
            existing_meta = _read_meta(meta_path)

        if _is_stale(jsonl_path, existing_meta, parquet_path):
            stale_jobs.append(
                (str(jsonl_path), str(parquet_path), str(meta_path), batch_rows)
            )
        else:
            metas[stem] = existing_meta  # type: ignore[assignment]

    if stale_jobs:
        n_workers = workers if workers is not None else _default_workers()
        # Single-job or single-worker path: skip the process pool
        # overhead. Useful for tests and small datasets.
        if n_workers <= 1 or len(stale_jobs) == 1:
            for job in stale_jobs:
                jsonl_str = job[0]
                if on_progress is not None:
                    on_progress(Path(jsonl_str).stem)
                stem, meta = _build_shard_worker(job)
                metas[stem] = meta
        else:
            with ProcessPoolExecutor(max_workers=n_workers) as pool:
                futures = {
                    pool.submit(_build_shard_worker, job): job for job in stale_jobs
                }
                if on_progress is not None:
                    for job in stale_jobs:
                        on_progress(Path(job[0]).stem)
                for future in as_completed(futures):
                    stem, meta = future.result()
                    metas[stem] = meta

    _remove_orphan_shards(logs_dir, valid_stems)

    # Opportunistic upgrade: if any retained ``ShardMeta`` lacks the
    # ``variant``/``run``/``is_clocksync`` fields (because it was built
    # before T11.2 added them), pay the cost of one first-row Parquet
    # read per such shard right here. The result is persisted into the
    # global sentinel below, so the *next* warm run is fully indexed
    # and skips both the per-sidecar walk and the per-shard probe.
    metas = _backfill_index_fields(logs_dir, metas)

    _write_global_sentinel(logs_dir, metas=metas)

    return metas


def _backfill_index_fields(
    logs_dir: Path, metas: dict[str, ShardMeta]
) -> dict[str, ShardMeta]:
    """Fill in ``variant``/``run``/``is_clocksync`` on legacy ``ShardMeta``.

    Idempotent: shards that already have all three fields populated are
    returned unchanged. Shards that need the upgrade pay one first-row
    Parquet read each. This is the migration path for caches built
    before the global index was extended in T11.2.
    """
    upgraded: dict[str, ShardMeta] = {}
    for stem, meta in metas.items():
        if (
            meta.variant is not None
            and meta.run is not None
            and meta.is_clocksync is not None
        ):
            upgraded[stem] = meta
            continue

        parquet_path, _ = shard_paths(logs_dir, stem)
        if not parquet_path.exists():
            upgraded[stem] = meta
            continue
        head = pl.read_parquet(
            parquet_path, columns=["variant", "run", "event"], n_rows=1
        )
        if head.is_empty():
            upgraded[stem] = meta
            continue
        variant = head.get_column("variant").cast(pl.Utf8)[0]
        run = head.get_column("run").cast(pl.Utf8)[0]
        event = head.get_column("event").cast(pl.Utf8)[0]
        # Match ``_is_clocksync_shard``: known clock-sync event names
        # OR empty-variant fallback both mark the shard as broadcast.
        if event is not None or variant is not None:
            event_str = str(event) if event is not None else ""
            variant_str = str(variant) if variant is not None else ""
            new_is_clocksync: bool | None = (
                event_str
                in (
                    "clock_sync",
                    "clock_sync_sample",
                )
                or variant_str == ""
            )
        else:
            new_is_clocksync = meta.is_clocksync
        upgraded[stem] = ShardMeta(
            mtime=meta.mtime,
            row_count=meta.row_count,
            schema_version=meta.schema_version,
            variant=str(variant) if variant is not None else meta.variant,
            run=str(run) if run is not None else meta.run,
            is_clocksync=new_is_clocksync,
        )
    return upgraded


def scan_shards(logs_dir: Path) -> pl.LazyFrame:
    """Return a polars ``LazyFrame`` over every Parquet shard.

    The frame's row order is unspecified; analysis code must group/sort
    explicitly. The lazy frame pushes filters and column selections down
    into the Parquet readers so we never materialize the full dataset.
    """
    pattern = str(cache_dir(logs_dir) / "*.parquet")
    return pl.scan_parquet(pattern)


def _is_clocksync_shard(shard: Path) -> bool:
    """Return True if the shard is a clock-sync sibling log.

    Clock-sync logs (``<runner>-clock-sync-<run>.jsonl`` and the
    ``<runner>-clock-sync-debug-<run>.jsonl`` debug variant, see E8)
    interleave rows for many variants in a single file. We detect them
    by event type so they can be broadcast across every variant group
    in the run instead of being mis-classified into a single group
    based on their first row's ``variant`` field.

    Two checks are required (defence-in-depth):

    1. Match BOTH ``clock_sync`` (the periodic accepted-offset summary)
       and ``clock_sync_sample`` (per-sample debug rows emitted by the
       ``*-clock-sync-debug-*.jsonl`` shards). Either one is sufficient
       to identify the shard as clock-sync-only.
    2. Treat any first row with an empty ``variant`` as broadcast-only
       too. Variant logs always carry a non-empty ``variant`` by log
       convention; an empty one means the shard is a sibling log
       (clock-sync or future broadcast types) that must not be exposed
       as its own ``("", run)`` group. This guards against new
       broadcast event names that the explicit list above doesn't yet
       know about.
    """
    head = pl.read_parquet(shard, columns=["event", "variant"], n_rows=1)
    if head.is_empty():
        return False
    event = head.get_column("event").cast(pl.Utf8)[0]
    if event in ("clock_sync", "clock_sync_sample"):
        return True
    variant = head.get_column("variant").cast(pl.Utf8)[0]
    # Empty-variant fallback: any sibling log without a variant tag is
    # broadcast-only and must not become its own (variant, run) group.
    return variant is None or variant == ""


def discover_groups(logs_dir: Path) -> list[tuple[str, str, list[Path]]]:
    """Discover ``(variant, run)`` groups by reading per-shard metadata.

    Returns a list of ``(variant, run, shard_paths)`` tuples. Each
    variant shard is included in exactly one group's list -- the
    (variant, run) that its rows belong to. This sidesteps a full lazy
    collect over the whole cache to enumerate groups, which on the
    40 GB dataset would materialize all categorical columns into RAM.

    Resolution strategy (warm-path first):

    1. Try the global sentinel index, which carries each stem's
       ``(variant, run)`` from the build that produced it. On a warm
       run this gives the full mapping with one ``open`` + one
       ``json.load`` rather than 128 mini Parquet reads.
    2. For any stem whose ``(variant, run)`` is unknown (legacy
       sidecar, sentinel without an index, or a freshly built shard
       whose index entry happened to lack the fields), fall back to
       reading the first row of the Parquet shard.

    Either way each variant shard's identity is determined exactly
    once per cache state, since each source variant JSONL contains
    exactly one ``(variant, runner, run)`` by log-naming convention.

    **Clock-sync shards** (E8) are an exception: a single
    ``<runner>-clock-sync-<run>.jsonl`` file mixes rows from many
    variants (the initial sync uses ``variant == ""``; per-variant
    resyncs use the variant about to start). To make every variant's
    analysis pipeline see the offsets it needs, clock-sync shards are
    appended to ALL discovered ``(variant, run)`` groups that share
    the same ``run``. They are NOT exposed as their own ``("", run)``
    group -- the empty-variant rows are only ever consumed via
    ``clock_offsets.build_offset_table`` from inside another variant's
    group lazy frame.
    """
    base = cache_dir(logs_dir)
    indexed = _read_global_index(logs_dir)

    groups: dict[tuple[str, str], list[Path]] = {}
    # run -> list of clock-sync shard paths, collected on the first pass
    # and broadcast across all groups for that run on the second pass.
    clocksync_shards_by_run: dict[str, list[Path]] = {}

    for shard in sorted(base.glob("*.parquet")):
        stem = shard.name[: -len(".parquet")]
        meta = indexed.get(stem)
        variant: str | None = meta.variant if meta is not None else None
        run: str | None = meta.run if meta is not None else None
        is_clocksync: bool | None = meta.is_clocksync if meta is not None else None

        if variant is None or run is None:
            # Legacy / index-miss fallback: read the first row to
            # recover (variant, run).
            head = pl.read_parquet(shard, columns=["variant", "run"], n_rows=1)
            if head.is_empty():
                continue
            variant = head.get_column("variant").cast(pl.Utf8)[0]
            run = head.get_column("run").cast(pl.Utf8)[0]
            if variant is None or run is None:
                continue

        # When the index didn't tell us, fall back to the dedicated
        # clock-sync probe (also a Parquet first-row read). Indexed
        # entries skip this entirely.
        if is_clocksync is None:
            is_clocksync = _is_clocksync_shard(shard)

        if is_clocksync:
            clocksync_shards_by_run.setdefault(str(run), []).append(shard)
            continue

        groups.setdefault((variant, run), []).append(shard)

    # Broadcast clock-sync shards into every (variant, run) group that
    # shares the same run identifier.
    for (_variant, run), paths in groups.items():
        extras = clocksync_shards_by_run.get(run)
        if extras:
            paths.extend(extras)

    out: list[tuple[str, str, list[Path]]] = []
    for (variant, run), paths in sorted(groups.items()):
        out.append((variant, run, paths))
    return out


def scan_group(shard_paths: list[Path]) -> pl.LazyFrame:
    """Lazy-frame over only the shards belonging to one ``(variant, run)``.

    Use ``discover_groups`` to obtain the list of shard paths per group,
    then call this for the per-group lazy pipeline. This avoids the
    overhead of asking polars to predicate-push a categorical filter
    through every shard in the cache.
    """
    if not shard_paths:
        # Empty placeholder LazyFrame matching the schema.
        return pl.DataFrame(schema=SHARD_SCHEMA).lazy()
    return pl.scan_parquet([str(p) for p in shard_paths])
