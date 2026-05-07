#!/usr/bin/env bash
#
# Auto-resume wrapper for the benchmark runner.
#
# Re-launches the runner with `--resume` appended ONLY when the runner exits
# with code 75 (EX_TEMPFAIL — a coordination barrier hit its timeout). Any
# other exit (including 0 / success) propagates immediately and stops the
# loop. Panics, config errors, and variant failures must NOT be retried; only
# transient peer-side hangs are.
#
# Usage:
#   scripts/runner-resume.sh target/release/runner --name alice --config bench.toml
#
# The first argument is the runner binary (path or `runner` on PATH); the
# remaining arguments are passed verbatim. The wrapper appends `--resume` to
# every iteration after the first, even if the original command line already
# contains `--resume`. Duplicate flags are harmless — clap takes the last
# value.

set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <runner-binary> [runner args...]" >&2
    exit 2
fi

RUNNER_BIN="$1"; shift
ORIGINAL_ARGS=("$@")
EXTRA_ARGS=()
ATTEMPT=1
EX_TEMPFAIL=75
MAX_ATTEMPTS="${RUNNER_RESUME_MAX_ATTEMPTS:-50}"

while :; do
    echo "[runner-resume] attempt $ATTEMPT: $RUNNER_BIN ${ORIGINAL_ARGS[*]} ${EXTRA_ARGS[*]:-}" >&2
    set +e
    "$RUNNER_BIN" "${ORIGINAL_ARGS[@]}" ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}
    RC=$?
    set -e

    if [[ $RC -eq $EX_TEMPFAIL ]]; then
        if [[ $ATTEMPT -ge $MAX_ATTEMPTS ]]; then
            echo "[runner-resume] hit max attempts ($MAX_ATTEMPTS); giving up with exit $RC" >&2
            exit $RC
        fi
        echo "[runner-resume] runner exited 75 (barrier timeout); retrying with --resume" >&2
        EXTRA_ARGS=(--resume)
        ATTEMPT=$((ATTEMPT + 1))
        continue
    fi

    exit $RC
done
