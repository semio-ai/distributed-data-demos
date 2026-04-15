"""Pickle caching pipeline for parsed JSONL log data."""

from __future__ import annotations

import pickle
from dataclasses import dataclass, field
from pathlib import Path

from parse import Event, parse_file

CACHE_FILENAME = ".analysis_cache.pkl"


@dataclass
class FileEntry:
    """Cached metadata and events for a single JSONL file."""

    mtime: float
    events: list[Event]


@dataclass
class Cache:
    """In-memory representation of the analysis cache.

    Keys in ``files`` are the file stem (e.g. ``custom-udp-alice-local-test-01``).
    """

    files: dict[str, FileEntry] = field(default_factory=dict)

    def all_events(self) -> list[Event]:
        """Return a flat list of all cached events, sorted by timestamp."""
        events: list[Event] = []
        for entry in self.files.values():
            events.extend(entry.events)
        events.sort(key=lambda e: e.ts)
        return events


def _cache_path(logs_dir: Path) -> Path:
    return logs_dir / CACHE_FILENAME


def load_cache(logs_dir: Path, *, clear: bool = False) -> Cache:
    """Load the pickle cache from disk.

    If ``clear`` is True or the cache file does not exist, returns an empty
    cache.
    """
    path = _cache_path(logs_dir)
    if clear and path.exists():
        path.unlink()

    if path.exists():
        with open(path, "rb") as f:
            try:
                return pickle.load(f)  # noqa: S301
            except (pickle.UnpicklingError, EOFError, ModuleNotFoundError):
                # Corrupted cache -- start fresh
                return Cache()
    return Cache()


def save_cache(logs_dir: Path, cache: Cache) -> None:
    """Write the cache to disk as a pickle file."""
    path = _cache_path(logs_dir)
    with open(path, "wb") as f:
        pickle.dump(cache, f, protocol=pickle.HIGHEST_PROTOCOL)


def update_cache(logs_dir: Path, cache: Cache) -> bool:
    """Scan for new or changed JSONL files and parse them into the cache.

    Returns True if any files were added or updated.
    """
    changed = False
    jsonl_files = sorted(logs_dir.glob("*.jsonl"))

    # Detect files that are new or have a newer mtime
    current_stems: set[str] = set()
    for jsonl_path in jsonl_files:
        stem = jsonl_path.stem
        current_stems.add(stem)
        mtime = jsonl_path.stat().st_mtime

        existing = cache.files.get(stem)
        if existing is not None and existing.mtime >= mtime:
            continue  # unchanged

        # Parse and insert/replace
        events = parse_file(jsonl_path)
        cache.files[stem] = FileEntry(mtime=mtime, events=events)
        changed = True

    # Remove cache entries for files that no longer exist
    stale = set(cache.files.keys()) - current_stems
    for stem in stale:
        del cache.files[stem]
        changed = True

    return changed


def load_and_update(logs_dir: Path, *, clear: bool = False) -> Cache:
    """Full caching pipeline: load, detect changes, parse, save.

    This is the main entry point for the caching subsystem.
    """
    cache = load_cache(logs_dir, clear=clear)
    changed = update_cache(logs_dir, cache)
    if changed:
        save_cache(logs_dir, cache)
    return cache
