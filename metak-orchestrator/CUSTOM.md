# Orchestrator Custom Instructions

## Planning Priorities

1. **API contracts first.** The four contracts in `metak-shared/api-contracts/`
   (variant-cli, jsonl-log-schema, runner-coordination, toml-config-schema)
   must be reviewed and agreed before spawning any implementation workers.

2. **Runner before variants.** E1 (runner) must be functional before E2/E4
   (variants) can be tested end-to-end. The runner is the harness; variants
   are the systems under test.

3. **One workload profile at a time.** Start with `scalar-flood` everywhere.
   Additional profiles (`multi-writer`, `mixed-types`, etc.) are stretch
   goals.

## Review Gates

- Always ask the user to review `overview.md` and `architecture.md` before
  moving to task breakdown.
- Always ask the user to review API contracts before spawning implementation
  workers.

## Repo Scaffolding

When creating sub-repos (`runner/`, `variants/zenoh/`, etc.), use `metak add`
if available. Each sub-repo needs:
- `AGENTS.md` (from template or custom)
- `CUSTOM.md` with project-specific worker context
- `STRUCT.md` describing initial file layout
- `.claude/CLAUDE.md` scoped to the worker role

## Design Documents

The three design documents (`DESIGN.md`, `BENCHMARK.md`, `ANALYSIS.md`) live
in `metak-shared/` and are the authoritative requirements. The orchestrator
planning files (overview, architecture, epics, contracts) are derived from
them. If a conflict arises, the design documents win.
