# E2E: Stop services and clean up on both machines
param(
    [string]$ClientHost = "print-client.lan",
    [string]$InstallDir = "C:\DevBridge"
)

$ErrorActionPreference = "Continue"

Write-Host "=== E2E Cleanup ===" -ForegroundColor Cyan

# Stop local server
$serverProc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($serverProc) {
    Write-Host "Stopping server..."
    Stop-Process -Name "devbridge-service" -Force
}

# Clean up local install
if (Test-Path $InstallDir) {
    Write-Host "Removing server install directory..."
    Remove-Item -Recurse -Force $InstallDir
}

# Clean up client
try {
    $session = New-PSSession -ComputerName $ClientHost -ErrorAction Stop

    Invoke-Command -Session $session -ScriptBlock {
        param($InstallDir)

        # Stop service
        $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
        if ($proc) {
            Write-Host "Stopping client service..."
            Stop-Process -Name "devbridge-service" -Force
        }

        # Cancel pending print jobs
        Get-PrintJob -PrinterName * -ErrorAction SilentlyContinue | Remove-PrintJob -ErrorAction SilentlyContinue

        # Remove install directory
        if (Test-Path $InstallDir) {
            Write-Host "Removing client install directory..."
            Remove-Item -Recurse -Force $InstallDir
        }
    } -ArgumentList $InstallDir

    Remove-PSSession $session
}
catch {
    Write-Host "Warning: Could not clean up client ($ClientHost): $_" -ForegroundColor Yellow
}

# Remove IPP printer registration
try {
    Remove-Printer -Name "DevBridge" -ErrorAction SilentlyContinue
}
catch {
    Write-Host "Warning: Could not remove IPP printer: $_" -ForegroundColor Yellow
}

Write-Host "Cleanup complete." -ForegroundColor Green
