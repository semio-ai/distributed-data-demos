# variant-aeron File Structure

```
variants/aeron/
  .claude/
    CLAUDE.md              -- Worker agent instructions (read-only scope)
  src/
    main.rs                -- Binary entry point: parse CLI + extra args, construct AeronVariant, run protocol
    aeron.rs               -- AeronVariant struct implementing Variant trait, message serialization
  Cargo.toml               -- Crate manifest: binary target depending on variant-base, rusteron-client, anyhow
  AGENTS.md                -- Agent guide for this repo
  CUSTOM.md                -- Repo-specific custom instructions (tech stack, design guidance)
  STRUCT.md                -- This file: describes the file layout
```
