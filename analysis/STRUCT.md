# analysis/ -- File Layout

```
analysis/
  AGENTS.md            -- Agent rules for this directory
  CUSTOM.md            -- Detailed build instructions and design guidance
  STRUCT.md            -- This file
  analyze.py           -- CLI entry point (argparse, pipeline orchestration)
  cache.py             -- Pickle caching pipeline (load, detect changes, save)
  parse.py             -- JSONL parsing, Event and DeliveryRecord dataclasses
  correlate.py         -- Write-receive correlation producing delivery records
  integrity.py         -- Integrity verification (completeness, ordering, dupes, gaps)
  performance.py       -- Performance metrics (latency, throughput, jitter, loss, resources)
  tables.py            -- CLI summary table formatting
  tests/
    conftest.py        -- Pytest fixtures (tmp_logs with synthetic two-runner data)
    helpers.py         -- Shared test helpers (make_event, write_jsonl, path constants)
    test_parse.py      -- Unit tests for JSONL parsing and timestamp handling
    test_correlate.py  -- Unit tests for write-receive correlation
    test_integrity.py  -- Unit tests for integrity checks across QoS levels 1-4
    test_performance.py -- Unit tests for performance metric computation
    test_cache.py      -- Unit tests for pickle caching pipeline
    test_integration.py -- Integration tests using real logs from ../logs/
    fixtures/          -- Directory for synthetic JSONL fixture files
  .claude/
    CLAUDE.md          -- Worker agent instructions
```

## Dependencies

- Standard library only: json, pickle, pathlib, statistics, dataclasses, argparse, datetime, collections
- Test: pytest
- Lint: ruff

## Data flow

```
JSONL files --> parse.py --> cache.py --> correlate.py --> integrity.py --> tables.py --> stdout
                                                     \--> performance.py --/
```
