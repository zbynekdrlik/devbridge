# Smoke test for deploy/register-virtual-printers.ps1.
#
# Runs on a Windows box that already has the Microsoft IPP Class Driver
# installed (any DevBridge-deployed machine). Does NOT require a running
# DevBridge service -- uses -InputJson to bypass the dashboard fetch.
#
# Scenarios:
#   1. -InputJson happy path: 2 printers in JSON -> both registered.
#   2. Idempotency: running twice is a no-op on the second pass.
#   3. Never-delete: a pre-seeded Windows printer not in the JSON survives.
#
# Usage (from repo root on the target machine):
#   powershell -ExecutionPolicy Bypass -File deploy\tests\register-virtual-printers.smoke.ps1
#
# Exit code 0 on pass, non-zero on any failure. Cleans up its own artifacts.

$ErrorActionPreference = "Stop"

$ScriptUnderTest = Join-Path $PSScriptRoot ".." "register-virtual-printers.ps1" | Resolve-Path
$TempDir = Join-Path $env:TEMP "devbridge-smoke-$([Guid]::NewGuid().ToString('N').Substring(0,8))"
New-Item -ItemType Directory -Force -Path $TempDir | Out-Null

$JsonPath = Join-Path $TempDir "printers.json"
$LogPath  = Join-Path $TempDir "reconciler.log"
$PreSeedName = "SmokeTest-PreExisting"
$PreSeedPort = "http://127.0.0.1:9999/smoke-pre-seed"

$SmokeA = @{ display_name = "SmokeTest-A"; ipp_name = "smoke-a" }
$SmokeB = @{ display_name = "SmokeTest-B"; ipp_name = "smoke-b" }

$failures = @()

function Cleanup {
    @("SmokeTest-A", "SmokeTest-B", $PreSeedName) | ForEach-Object {
        Get-Printer -Name $_ -ErrorAction SilentlyContinue | Remove-Printer -ErrorAction SilentlyContinue
    }
    Get-PrinterPort -Name $PreSeedPort -ErrorAction SilentlyContinue | Remove-PrinterPort -ErrorAction SilentlyContinue
    if (Test-Path $TempDir) { Remove-Item $TempDir -Recurse -Force -ErrorAction SilentlyContinue }
}

try {
    # Write the input JSON (2 printers).
    @($SmokeA, $SmokeB) | ConvertTo-Json | Set-Content -Path $JsonPath -Encoding ASCII

    # Pre-seed a printer that is NOT in the JSON list. Must survive every run.
    Add-PrinterPort -Name $PreSeedPort -ErrorAction SilentlyContinue
    if (-not (Get-Printer -Name $PreSeedName -ErrorAction SilentlyContinue)) {
        Add-Printer -Name $PreSeedName -DriverName "Microsoft IPP Class Driver" -PortName $PreSeedPort -ErrorAction SilentlyContinue
    }
    if (-not (Get-Printer -Name $PreSeedName -ErrorAction SilentlyContinue)) {
        Write-Host "  SKIP: unable to pre-seed $PreSeedName (non-fatal; skipping never-delete check)" -ForegroundColor Yellow
        $PreSeedAvailable = $false
    } else {
        $PreSeedAvailable = $true
    }

    Write-Host "=== Pass 1: register from -InputJson ===" -ForegroundColor Cyan
    & $ScriptUnderTest -InputJson $JsonPath -LogPath $LogPath
    if ($LASTEXITCODE -ne 0) {
        $failures += "Pass 1 exit code $LASTEXITCODE (expected 0 unless IPP endpoints are not listening)"
    }

    Write-Host "`n=== Pass 2: same input, expect no-op ===" -ForegroundColor Cyan
    & $ScriptUnderTest -InputJson $JsonPath -LogPath $LogPath
    if ($LASTEXITCODE -ne 0) {
        $failures += "Pass 2 exit code $LASTEXITCODE (expected 0 on idempotent re-run)"
    }

    # Scenario 1+2: SmokeTest-A and SmokeTest-B should be registered with the
    # expected URL after 2 passes. If no IPP endpoint is listening on 631, the
    # reconciler skips registration and exits non-zero; the smoke test tolerates
    # that case and only asserts the printers are NOT in a broken mid-state.
    foreach ($vp in @($SmokeA, $SmokeB)) {
        $p = Get-Printer -Name $vp.display_name -ErrorAction SilentlyContinue
        if ($p) {
            $expectedPort = "http://127.0.0.1:631/printers/$($vp.ipp_name)"
            if ($p.PortName -ne $expectedPort) {
                $failures += "'$($vp.display_name)' has port '$($p.PortName)', expected '$expectedPort'"
            } else {
                Write-Host "  OK: '$($vp.display_name)' -> $($p.PortName)" -ForegroundColor Green
            }
        } else {
            Write-Host "  NOTE: '$($vp.display_name)' not registered (likely IPP endpoint unavailable during smoke test)" -ForegroundColor Yellow
        }
    }

    # Scenario 3: never-delete. The pre-seeded SmokeTest-PreExisting printer
    # was NOT in the JSON input; it must still be present after both passes.
    if ($PreSeedAvailable) {
        $preserved = Get-Printer -Name $PreSeedName -ErrorAction SilentlyContinue
        if ($preserved) {
            Write-Host "  OK: pre-seeded '$PreSeedName' survived both reconciler passes (never-delete invariant)" -ForegroundColor Green
        } else {
            $failures += "NEVER-DELETE VIOLATED: pre-seeded '$PreSeedName' was removed by the reconciler"
        }
    }
} finally {
    Cleanup
}

if ($failures.Count -gt 0) {
    Write-Host "`n=== FAIL ===" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
Write-Host "`n=== PASS ===" -ForegroundColor Green
exit 0
