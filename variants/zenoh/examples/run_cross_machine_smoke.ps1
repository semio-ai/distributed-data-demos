# T16.20 cross-machine smoke runner.
#
# Usage:
#   .\variants\zenoh\examples\run_cross_machine_smoke.ps1 <local-wifi-ipv4>
#
# Example:
#   .\variants\zenoh\examples\run_cross_machine_smoke.ps1 192.168.1.80
#
# This script does NOT start two processes for you -- it prints the four
# test commands you should run, paired with what to run on the OTHER
# machine. You stay in control of the two-machine orchestration.
#
# Read examples\CROSS_MACHINE_SMOKE.md for the full procedure and what
# each test bisects.

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$LocalWifiIp
)

$ErrorActionPreference = 'Stop'

# Validate the IP is a bare IPv4.
if ($LocalWifiIp -notmatch '^\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}$') {
    Write-Error "Expected a bare IPv4 like '192.168.1.80', got '$LocalWifiIp'"
}

$repoRoot = (Resolve-Path "$PSScriptRoot\..\..\..").Path
$exe = Join-Path $repoRoot 'target\release\examples\cross_machine_smoke.exe'

if (-not (Test-Path $exe)) {
    Write-Host "[wrapper] $exe not found; building..."
    Push-Location $repoRoot
    try {
        cargo build --release -p variant-zenoh --example cross_machine_smoke
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed (exit $LASTEXITCODE)"
        }
    } finally {
        Pop-Location
    }
}

Write-Host ""
Write-Host "============================================================"
Write-Host " T16.20 Zenoh cross-machine smoke -- runner cheatsheet"
Write-Host " Local WiFi IPv4: $LocalWifiIp"
Write-Host "============================================================"
Write-Host ""
Write-Host "Substitute <peer-wifi-ipv4> below with the OTHER machine's"
Write-Host "actual WiFi IPv4. Run each test in BOTH directions (swap"
Write-Host "alice and bob roles) to exercise the cross-WiFi path"
Write-Host "symmetrically."
Write-Host ""

function Print-Test {
    param(
        [int]$N,
        [string]$Description,
        [string]$SubCmd,
        [string]$PubCmd,
        [string]$Expected
    )
    Write-Host "------------------------------------------------------------"
    Write-Host ("TEST {0}: {1}" -f $N, $Description)
    Write-Host "------------------------------------------------------------"
    Write-Host ""
    Write-Host "  On the SUBSCRIBER machine (start first; wait ~5 s):"
    Write-Host ""
    Write-Host "    $SubCmd"
    Write-Host ""
    Write-Host "  On the PUBLISHER machine (start ~5 s after the subscriber):"
    Write-Host ""
    Write-Host "    $PubCmd"
    Write-Host ""
    Write-Host "  Expected: $Expected"
    Write-Host ""
}

$exeShort = '.\target\release\examples\cross_machine_smoke.exe'

Print-Test -N 1 `
    -Description "low-rate bare multicast at QoS 1 (BestEffort/Drop)" `
    -SubCmd "$exeShort --mode sub --qos 1 --key smoke/t1 --rate-hz 10 --values-per-tick 10 --duration-secs 30 --multicast-interface $LocalWifiIp" `
    -PubCmd "$exeShort --mode pub --qos 1 --key smoke/t1 --rate-hz 10 --values-per-tick 10 --duration-secs 15 --multicast-interface $LocalWifiIp --wait-peers 1 --wait-peers-timeout-secs 15" `
    -Expected "pub total = 150; sub total >= ~140 unique_keys = 10. If sub total = 0, multicast discovery is broken on this AP/firewall."

Print-Test -N 2 `
    -Description "low-rate bare multicast at QoS 4 (ReliableTcp/Block)" `
    -SubCmd "$exeShort --mode sub --qos 4 --key smoke/t2 --rate-hz 10 --values-per-tick 10 --duration-secs 30 --multicast-interface $LocalWifiIp" `
    -PubCmd "$exeShort --mode pub --qos 4 --key smoke/t2 --rate-hz 10 --values-per-tick 10 --duration-secs 15 --multicast-interface $LocalWifiIp --wait-peers 1 --wait-peers-timeout-secs 15" `
    -Expected "pub total = sub total = 150 exactly (CC=Block, no drops). If sub total = 0 while TEST 1 passed, T17.8 watchdog is implicated even at minimal load."

Print-Test -N 3 `
    -Description "matrix-rate (100 hz x 1000 vpt) at QoS 1 -- the failing bench shape" `
    -SubCmd "$exeShort --mode sub --qos 1 --key smoke/t3 --rate-hz 100 --values-per-tick 1000 --duration-secs 60 --multicast-interface $LocalWifiIp" `
    -PubCmd "$exeShort --mode pub --qos 1 --key smoke/t3 --rate-hz 100 --values-per-tick 1000 --duration-secs 30 --multicast-interface $LocalWifiIp --wait-peers 1 --wait-peers-timeout-secs 30" `
    -Expected "pub total = 3,000,000; sub total = some fraction. Record the per-5s sub: last5s=... lines (shape matters as much as total). If sub total = 0 while TEST 1 passed, we've reproduced the bench bug in this 200-line binary."

Print-Test -N 4 `
    -Description "explicit --connect (multicast bypass) at QoS 1, low rate" `
    -SubCmd "$exeShort --mode sub --qos 1 --key smoke/t4 --rate-hz 10 --values-per-tick 10 --duration-secs 30 --listen tcp/0.0.0.0:7447" `
    -PubCmd "$exeShort --mode pub --qos 1 --key smoke/t4 --rate-hz 10 --values-per-tick 10 --duration-secs 15 --connect tcp/<peer-wifi-ipv4>:7447 --wait-peers 1 --wait-peers-timeout-secs 15" `
    -Expected "sub total ~ 150 unique_keys = 10. If TEST 4 passes while TEST 1 failed: multicast HELLO is being suppressed. If TEST 4 also fails: the TCP data path between the two WiFi clients is broken (firewall / AP client isolation)."

Write-Host "------------------------------------------------------------"
Write-Host " End of test list. Run BOTH directions of each test (swap"
Write-Host " pub <-> sub between the two machines), then repeat the whole"
Write-Host " sequence on the LS105G wired switch setup if you have one."
Write-Host "============================================================"
Write-Host ""
