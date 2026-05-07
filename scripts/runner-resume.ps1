# Auto-resume wrapper for the benchmark runner (Windows PowerShell 5.1+).
#
# Re-launches the runner with `--resume` appended ONLY when it exits with
# code 75 (EX_TEMPFAIL — a coordination barrier hit its timeout). Any other
# exit (including 0 / success) propagates immediately and stops the loop.
# Panics, config errors, and variant failures must NOT be retried; only
# transient peer-side hangs are.
#
# Usage:
#   .\scripts\runner-resume.ps1 -RunnerBinary .\target\release\runner.exe `
#       -RunnerArgs '--name','alice','--config','bench.toml'
#
# Compatibility: Windows PowerShell 5.1. No `??`, no ternary, no `?.` — all
# PS 7-only constructs are avoided.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $RunnerBinary,

    [Parameter(Mandatory = $true)]
    [string[]] $RunnerArgs,

    [int] $MaxAttempts = 50
)

$ErrorActionPreference = 'Stop'
$EX_TEMPFAIL = 75
$attempt = 1
$extraArgs = @()

while ($true) {
    $allArgs = @()
    $allArgs += $RunnerArgs
    if ($extraArgs.Count -gt 0) {
        $allArgs += $extraArgs
    }

    Write-Host "[runner-resume] attempt $attempt`: $RunnerBinary $($allArgs -join ' ')"

    & $RunnerBinary @allArgs
    $rc = $LASTEXITCODE

    if ($rc -eq $EX_TEMPFAIL) {
        if ($attempt -ge $MaxAttempts) {
            Write-Host "[runner-resume] hit max attempts ($MaxAttempts); giving up with exit $rc"
            exit $rc
        }
        Write-Host "[runner-resume] runner exited 75 (barrier timeout); retrying with --resume"
        $extraArgs = @('--resume')
        $attempt = $attempt + 1
        continue
    }

    exit $rc
}
