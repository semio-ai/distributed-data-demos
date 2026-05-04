# Coding Standards

- All code must pass linting and tests before committing.
- Never import directly from another repo's source code. Use shared contracts in `metak-shared/api-contracts/`.
- When in doubt about system boundaries, consult `metak-shared/architecture.md`.
- **No emojis in code, commits, or documentation.** Use text alternatives like `[OK]`, `[FAIL]`, `[WARN]` when status indicators are needed.
- **`log_dir` is always `"./logs"`** — for every config in `configs/` and every test/validation fixture under `*/tests/fixtures/`. Per-run isolation is the auto-created `logs/<run-name>-<launch-ts>/` session subfolder produced by the runner. Anything a task wants to break out (analysis output, plots, scratch artifacts) goes INSIDE the session subfolder. Never introduce sibling roots like `logs-<tag>/` at the repo level. See `metak-shared/api-contracts/toml-config-schema.md` `log_dir` row for the contract.

## Commit Messages

Follow Conventional Commits: `type(scope): description`

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`

## Code Review

- All changes go through PRs.
- At least one human approval required before merge.
- CI must pass.

## Language: Rust (runner, variants)

- Use `cargo fmt` and `cargo clippy` before committing. No warnings allowed.
- Use `arora_types::Value` for all replicated data — do not define custom
  value enums.
- Timestamps must use RFC 3339 with nanosecond precision.
- Log output uses JSONL format per `metak-shared/api-contracts/jsonl-log-schema.md`.
- CLI argument parsing: use `clap` (derive style preferred).
- Error handling: use `anyhow` for applications (runner, variants). Use
  `thiserror` for library crates if any are extracted.
- Serialization: use `serde` + `serde_json` for JSONL output.
- TOML parsing: use the `toml` crate.

## Language: Python (analysis tool)

- Target Python 3.10+.
- Use type hints throughout.
- Format with `ruff format`, lint with `ruff check`.
- Use standard library where possible; `matplotlib` for diagrams.
- No `pandas` unless justified — the data model is simple enough for
  plain dicts/dataclasses.

## Testing

- All code must have tests that pass before committing.
- Integration test conventions are project-specific — define them in the `tests/CUSTOM.md` file.
- Rust: `cargo test` must pass. Use `#[test]` for unit tests, `tests/` directory for integration tests.
- Python: use `pytest`. Tests live in `tests/` alongside the source.

## Documentation

- Do not document every change you made — docs should reflect the current state of the project, not its history
- Before writing documentation, check if the information already exists elsewhere
- Check STRUCT.md to understand where documentation files are located
