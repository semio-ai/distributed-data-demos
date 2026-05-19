"""Per-shard Parquet cache for analysis logs.

Replaces the Phase 1 monolithic pickle cache with one Parquet shard
plus a JSON sidecar per source file. Layout::

    <logs-dir>/
        <name>-<runner>-<run>.jsonl              # lifecycle JSONL source (untouched)
        <name>-<runner>-<run>.compact.parquet    # T18.2 compact source (untouched)
        ...
        .cache/
            <name>-<runner>-<run>.parquet
            <name>-<runner>-<run>.meta.json
            ...
            _cache_schema_version.json

A shard is **stale** when any of:

* sidecar missing or malformed
* ``schema_version`` mismatch with ``schema.SCHEMA_VERSION``
* sidecar ``mtime`` < source ``mtime``
* shard parquet missing

Stale shards are rebuilt by streaming the source file (JSONL
line-by-line, or compact-parquet via a single columnar read) through
the appropriate loader, then writing the projected
``SHARD_SCHEMA``-shaped DataFrame as a Parquet shard. For JSONL the
loader runs in fixed-size row-group batches so peak memory is bounded
by the batch buffer rather than by the file size.

Post-E19 cleanup (T19.10): each spawn writes a **per-spawn file pair**
to its log directory -- a lifecycle-only ``<stem>.jsonl`` and a
per-event ``<stem>.compact.parquet``. Both are merged into the same
shard by ``_build_shard`` -- the JSONL contributes lifecycle rows
(``phase`` / ``connected`` / ``eot_*`` / ``resource`` / ``clock_sync``)
and the compact-Parquet contributes per-event rows (``write`` /
``receive`` / ``backpressure_skipped`` / ``gap_*``). Pre-T18.2
datasets that contain per-event rows directly in JSONL are no longer
supported -- :func:`parse.iter_rows` warns once per such file and
skips those rows.

For each ``<stem>``, when both ``<stem>.compact.parquet`` and
``<stem>.jsonl`` exist (the post-cleanup norm), the compact file wins
for shard derivation. Lifecycle events are mirrored into both files
by variant-base since T18.2b, so picking one source still yields a
complete row set for the spawn.

Orphan shards (no matching source file in either format) are removed.
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

from parse import COLUMN_ORDER, SourceFormat, detect_source_format, iter_rows
from parse_compact import read_compact_parquet
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

    The ``args`` tuple is
    ``(stem, source_path, source_format_value, parquet_path, meta_path, batch_rows)``.
    The source format is passed as the ``SourceFormat.value`` string so
    the pickle is small and resilient to enum-import order across
    worker processes.
    """
    stem, source_str, source_format_value, parquet_str, meta_str, batch_rows = args
    source_format = SourceFormat(source_format_value)
    meta = _build_shard(
        Path(source_str),
        Path(parquet_str),
        Path(meta_str),
        source_format=source_format,
        batch_rows=batch_rows,
    )
    return stem, meta


def _build_shard(
    source_path: Path,
    parquet_path: Path,
    meta_path: Path,
    *,
    source_format: SourceFormat,
    batch_rows: int = DEFAULT_BATCH_ROWS,
) -> ShardMeta:
    """Build the Parquet shard for ``source_path`` (JSONL or compact).

    Dispatches by ``source_format``:

    - :class:`SourceFormat.JSONL` -- stream-parse the JSONL file
      line-by-line through ``parse.iter_rows``, batching into typed
      polars DataFrames sized by ``batch_rows`` before a single
      concat + write.
    - :class:`SourceFormat.COMPACT` -- read the entire compact-parquet
      file in one shot via ``parse_compact.read_compact_parquet``;
      ``batch_rows`` is unused on this path because the columnar
      compact format is already compact in memory (the file is sized
      so the whole thing fits comfortably under the analyzer's RSS
      budget -- the compact format exists precisely to compress what
      JSONL bloats).

    Both paths produce the same ``SHARD_SCHEMA``-shaped Parquet shard,
    so the analyzer's downstream lazy frame doesn't see the difference.
    The sidecar is flushed only after the Parquet write fully
    succeeds.
    """
    cache_dir_path = parquet_path.parent
    cache_dir_path.mkdir(parents=True, exist_ok=True)

    variant: str | None = None
    run: str | None = None
    is_clocksync: bool | None = None
    row_count = 0

    if source_format is SourceFormat.JSONL:
        typed_batches: list[pl.DataFrame] = []
        with open(source_path, encoding="utf-8") as stream:
            for batch in _batches(
                iter_rows(stream, source_path=source_path), batch_rows
            ):
                df = pl.DataFrame(batch, schema=SHARD_SCHEMA, orient="row")
                if variant is None and df.height > 0:
                    # Recover (variant, run) + clock-sync flag once
                    # from the first non-empty batch's first row.
                    first = df.row(0, named=True)
                    v = first.get("variant")
                    r = first.get("run")
                    e = first.get("event")
                    if v is not None:
                        variant = str(v)
                    if r is not None:
                        run = str(r)
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

        del typed_batches

    elif source_format is SourceFormat.COMPACT:
        # The compact loader returns the full projected DataFrame in
        # one go; there is no streaming API at this layer because the
        # compact format itself is the compression step the streaming
        # JSONL path was trying to bound. Even a 30 s / 100K msg/s
        # spawn is on the order of a few MB on disk and ~100 MB
        # expanded, well within the analyzer's per-shard budget.
        combined = read_compact_parquet(source_path)
        combined = combined.select(list(COLUMN_ORDER))
        row_count = combined.height
        if row_count > 0:
            first = combined.row(0, named=True)
            v = first.get("variant")
            r = first.get("run")
            e = first.get("event")
            if v is not None:
                variant = str(v)
            if r is not None:
                run = str(r)
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
        combined.write_parquet(parquet_path, compression="snappy")

    else:  # pragma: no cover -- exhaustive enum check
        raise ValueError(f"unsupported source format: {source_format!r}")

    source_mtime = source_path.stat().st_mtime
    meta = ShardMeta(
        mtime=source_mtime,
        row_count=row_count,
        schema_version=SCHEMA_VERSION,
        variant=variant,
        run=run,
        is_clocksync=is_clocksync,
    )
    _write_meta(meta_path, meta)
    return meta


def discover_sources(logs_dir: Path) -> dict[str, tuple[Path, SourceFormat]]:
    """Discover per-spawn source files under ``logs_dir``.

    Returns a dict mapping ``stem`` -> ``(source_path, source_format)``.
    A stem is the canonical ``<variant>-<runner>-<run>`` triple shared
    by both formats:

    - ``<stem>.jsonl`` (lifecycle-only JSONL since the E19 cleanup --
      ``phase`` / ``connected`` / ``eot_*`` / ``resource`` /
      ``clock_sync``)
    - ``<stem>.compact.parquet`` (T18.2 columnar compact format,
      per-event observations + mirrored lifecycle rows)

    When **both** files exist for the same stem (the post-cleanup norm
    -- variant-base now always writes the pair), the compact file
    wins for shard derivation: lifecycle events are mirrored into
    compact-Parquet by variant-base (T18.2b) so one source still
    yields a complete row set. The lifecycle JSONL is left on disk
    for live-debugging / tail-f purposes but is not cached.

    Files in any other format are silently skipped. The function is
    name-based (does not open the files) so it stays fast on the
    multi-thousand-file two-machine 40 GB scenario.
    """
    sources: dict[str, tuple[Path, SourceFormat]] = {}
    # Two passes: JSONL first, then compact. The compact-overrides-jsonl
    # rule falls out naturally because the second pass simply replaces
    # whatever the first pass put down.
    for entry in sorted(logs_dir.iterdir() if logs_dir.exists() else ()):
        if not entry.is_file():
            continue
        fmt = detect_source_format(entry)
        if fmt is not SourceFormat.JSONL:
            continue
        sources[entry.stem] = (entry, fmt)
    for entry in sorted(logs_dir.iterdir() if logs_dir.exists() else ()):
        if not entry.is_file():
            continue
        fmt = detect_source_format(entry)
        if fmt is not SourceFormat.COMPACT:
            continue
        stem = entry.name[: -len(".compact.parquet")]
        sources[stem] = (entry, fmt)
    return sources


# Back-compat shim. Some callers (older tests, third-party scripts)
# imported the previous name. Keep the alias pointing at the modern
# implementation so import sites don't break; the alias intentionally
# returns a JSONL-only view to preserve the original signature.
def discover_jsonl(logs_dir: Path) -> list[Path]:
    """Return JSONL source files under ``logs_dir`` (back-compat shim).

    Prefer :func:`discover_sources` for new code -- it also surfaces
    compact-parquet sources. This shim is kept so any import-by-name
    site that predates T18.4 keeps working without a search/replace.
    """
    return [
        path
        for path, fmt in discover_sources(logs_dir).values()
        if fmt is SourceFormat.JSONL
    ]


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

    # Discover per-spawn source files in BOTH formats (JSONL + compact).
    # ``discover_sources`` already implements the
    # "compact wins when both exist" rule, so callers downstream see
    # one source file per stem.
    sources = discover_sources(logs_dir)
    valid_stems: set[str] = set(sources.keys())

    metas: dict[str, ShardMeta] = {}
    stale_jobs: list[tuple[str, str, str, str, str, int]] = []

    for stem, (source_path, source_format) in sources.items():
        parquet_path, meta_path = shard_paths(logs_dir, stem)

        existing_meta: ShardMeta | None = indexed_metas.get(stem)
        if existing_meta is None:
            # Index miss -- fall back to the per-sidecar read path so
            # caches built before the index extension still work.
            existing_meta = _read_meta(meta_path)

        if _is_stale(source_path, existing_meta, parquet_path):
            stale_jobs.append(
                (
                    stem,
                    str(source_path),
                    source_format.value,
                    str(parquet_path),
                    str(meta_path),
                    batch_rows,
                )
            )
        else:
            metas[stem] = existing_meta  # type: ignore[assignment]

    if stale_jobs:
        n_workers = workers if workers is not None else _default_workers()
        # Single-job or single-worker path: skip the process pool
        # overhead. Useful for tests and small datasets.
        if n_workers <= 1 or len(stale_jobs) == 1:
            for job in stale_jobs:
                # job[0] is the stem (canonical <variant>-<runner>-<run>
                # triple, format-agnostic).
                if on_progress is not None:
                    on_progress(job[0])
                stem, meta = _build_shard_worker(job)
                metas[stem] = meta
        else:
            with ProcessPoolExecutor(max_workers=n_workers) as pool:
                futures = {
                    pool.submit(_build_shard_worker, job): job for job in stale_jobs
                }
                if on_progress is not None:
                    for job in stale_jobs:
                        on_progress(job[0])
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
