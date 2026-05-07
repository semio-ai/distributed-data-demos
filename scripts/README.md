# Wrapper Scripts

`runner-resume.sh` and `runner-resume.ps1` re-launch the benchmark runner
with `--resume` appended ONLY when it exits with code **75** (`EX_TEMPFAIL`,
which the runner emits on a coordination barrier timeout). Any other exit
propagates immediately and stops the loop.

See `usage-guide.md` for end-user invocation examples and the
`metak-shared/api-contracts/runner-coordination.md` "Barrier Timeout"
subsection for the coordination contract.

## Manual smoke tests

Both smoke tests use a stub "runner" that exits 75 on the first call and 0
on the second. The wrapper must invoke the stub twice, append `--resume`
on the second invocation, and propagate the second invocation's exit code
(0) unchanged.

### bash

```bash
TMP=$(mktemp -d)
cat > "$TMP/fake-runner.sh" <<'EOF'
#!/usr/bin/env bash
N=$(cat "$TMP/n" 2>/dev/null || echo 0)
echo $((N + 1)) > "$TMP/n"
echo "[fake-runner] attempt $((N + 1)), args: $*" >&2
[[ $N -eq 0 ]] && exit 75 || exit 0
EOF
chmod +x "$TMP/fake-runner.sh"
TMP=$TMP scripts/runner-resume.sh "$TMP/fake-runner.sh" --name alice --config bench.toml
echo "exit: $?"   # expect 0
rm -rf "$TMP"
```

Expected output (abridged):

```
[runner-resume] attempt 1: ... fake-runner.sh --name alice --config bench.toml
[fake-runner] attempt 1, args: --name alice --config bench.toml
[runner-resume] runner exited 75 (barrier timeout); retrying with --resume
[runner-resume] attempt 2: ... fake-runner.sh --name alice --config bench.toml --resume
[fake-runner] attempt 2, args: --name alice --config bench.toml --resume
exit: 0
```

### PowerShell

```powershell
$tmp = Join-Path $env:TEMP ('rrs-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$stub = Join-Path $tmp 'fake-runner.ps1'
Set-Content -LiteralPath $stub -Encoding utf8 -Value @'
param([Parameter(ValueFromRemainingArguments=$true)][string[]] $RestArgs)
$counter = Join-Path $env:RUNNER_STATE 'n.txt'
$n = 0
if (Test-Path -LiteralPath $counter) { $n = [int](Get-Content -LiteralPath $counter -Raw) }
Set-Content -LiteralPath $counter -Value ($n + 1) -Encoding utf8
Write-Host "[fake-runner] attempt $($n + 1), args: $RestArgs"
if ($n -eq 0) { exit 75 } else { exit 0 }
'@
$env:RUNNER_STATE = $tmp
& .\scripts\runner-resume.ps1 -RunnerBinary 'powershell.exe' `
    -RunnerArgs @('-NoProfile','-File',$stub,'--name','alice','--config','bench.toml')
Write-Host "exit: $LASTEXITCODE"   # expect 0
Remove-Item -LiteralPath $tmp -Recurse -Force
```

## Non-75 propagation

The wrappers MUST propagate any other exit code unchanged (so panics, config
errors, and variant failures do not cause an infinite retry loop). Replace
`exit 75` with `exit 42` in the stub and the wrapper should exit 42 on the
first attempt with no retry.
