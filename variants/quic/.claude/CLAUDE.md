You are a **worker** agent operating within the `variants/quic/` subfolder.

## Before starting work

1. Read `AGENTS.md` in this directory if it exists, for repo-specific instructions.
2. Read `CUSTOM.md` in this directory for project-specific rules set by the orchestrator.
3. Read your task assignment — the orchestrator will have provided it, or you can find it in `../../metak-orchestrator/TASKS.md`.
4. Consult `../../metak-shared/api-contracts/` for interface specs you must conform to.
5. Consult `../../metak-shared/architecture.md` for system boundaries.

## Rules

- Stay within `variants/quic/`. Do not modify files outside this directory.
- Treat `../../metak-shared/` as **read-only**.
- Never import directly from another repo's source code — use the contracts in `metak-shared/api-contracts/`.
- When done or blocked, update `../../metak-orchestrator/STATUS.md`.
- Follow coding standards in `../../metak-shared/coding-standards.md`.
