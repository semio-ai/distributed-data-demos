# variants/websocket Agent Guide

Repo-specific agent instructions for `variants/websocket`.
Read the root `AGENTS.md` first for global rules, project structure, and coding standards.

## Repo Overview

Rust binary implementing the `Variant` trait from `variant-base` using
WebSocket as the reliable transport. Reliable QoS only (3-4) — UDP
QoS 1-2 are intentionally not implemented; that role belongs to the
Hybrid variant.

## Agent Rules

1. Follow all rules in the root `AGENTS.md`.
2. **Do not modify `metak-shared/`.** Propose changes via the orchestrator for user review.
3. Read your assignments from `metak-orchestrator/TASKS.md` and update `metak-orchestrator/STATUS.md` when done or blocked.
4. Consult `metak-shared/LEARNED.md` for useful methods, procedures, and tricks discovered during the project. Add new learnings as you discover them.
5. The Hybrid variant's TCP design (`variants/hybrid/CUSTOM.md`) is the
   reference implementation for the blocking-write + `SO_RCVTIMEO`
   read polling pattern. Mirror it.

## Coding Standards

- Follow the coding standards defined in `metak-shared/coding-standards.md`.
- Sync API only — do not add a tokio runtime.
- No emojis in code, commits, or documentation.

## Custom Instructions

Read and follow `CUSTOM.md` in this directory for repo-specific custom instructions.
