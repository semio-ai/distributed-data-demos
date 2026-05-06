# variants/webrtc Agent Guide

Repo-specific agent instructions for `variants/webrtc`.
Read the root `AGENTS.md` first for global rules, project structure, and coding standards.

## Repo Overview

Rust binary implementing the `Variant` trait from `variant-base` using
WebRTC DataChannels as the transport. The only variant that natively
covers all four QoS levels through DataChannel ordering and reliability
options without application-layer reliability code.

## Agent Rules

1. Follow all rules in the root `AGENTS.md`.
2. **Do not modify `metak-shared/`.** Propose changes via the orchestrator for user review.
3. Read your assignments from `metak-orchestrator/TASKS.md` and update `metak-orchestrator/STATUS.md` when done or blocked.
4. Consult `metak-shared/LEARNED.md` for useful methods, procedures, and tricks discovered during the project. Add new learnings as you discover them.
5. The QUIC variant's async-bridge pattern (`variants/quic/CUSTOM.md`)
   is the reference for the sync-trait → tokio-runtime → mpsc bridge
   you will need. Mirror it.

## Coding Standards

- Follow the coding standards defined in `metak-shared/coding-standards.md`.
- Async runtime is internal — the `Variant` trait surface stays sync.
- No emojis in code, commits, or documentation.

## Custom Instructions

Read and follow `CUSTOM.md` in this directory for repo-specific custom instructions.
