# E2E Cleanup: Remove all E2E artifacts from this machine.
# Called after E2E tests complete (on both server and client runners).
param(
    [string]$DataDir = "C:\ProgramData\DevBridge-E2E"
)

$ErrorActionPreference = "Continue"

Write-Host "=== E2E Cleanup ===" -ForegroundColor Cyan

# ── Stop and unregister E2E scheduled task ──
$task = Get-ScheduledTask -TaskName "DevBridgeE2E" -ErrorAction SilentlyContinue
if ($task) {
    if ($task.State -eq "Running") {
        Stop-ScheduledTask -TaskName "DevBridgeE2E" -ErrorAction SilentlyContinue
    }
    Unregister-ScheduledTask -TaskName "DevBridgeE2E" -Confirm:$false -ErrorAction SilentlyContinue
    Write-Host "  Removed DevBridgeE2E scheduled task"
}

# ── Kill E2E devbridge-service processes ──
Get-CimInstance Win32_Process -Filter "Name='devbridge-service.exe'" -ErrorAction SilentlyContinue | ForEach-Object {
    if ($_.CommandLine -like "*$DataDir*") {
        Write-Host "  Killing E2E process PID=$($_.ProcessId)"
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
}

# ── Remove E2E printers (server only) ──
@("DevBridge-E2E", "E2E Renamed Printer") | ForEach-Object {
    $p = Get-Printer -Name $_ -ErrorAction SilentlyContinue
    if ($p) {
        Remove-Printer -Name $_ -ErrorAction SilentlyContinue
        Write-Host "  Removed printer '$_'"
    }
}

# ── Restore PDF printer port if redirected to E2E path (client only) ──
$pdf = Get-Printer -Name "Microsoft Print to PDF" -ErrorAction SilentlyContinue
if ($pdf -and $pdf.PortName -like "*E2E*") {
    Set-Printer -Name "Microsoft Print to PDF" -PortName "PORTPROMPT:" -ErrorAction SilentlyContinue
    Restart-Service Spooler -ErrorAction SilentlyContinue
    Start-Sleep 2
    Write-Host "  Restored PDF printer port to default"
}

# ── Verify production service still running ──
try {
    $status = Invoke-RestMethod -Uri "http://127.0.0.1:9120/api/status" -TimeoutSec 5
    Write-Host "  Production service: $($status.status) v=$($status.version)" -ForegroundColor Green
} catch {
    Write-Warning "Production service not responding on port 9120"
}

Write-Host "E2E cleanup complete." -ForegroundColor Green
