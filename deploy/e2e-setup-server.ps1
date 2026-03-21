# E2E Setup: Install DevBridge server via NSIS installer on print-server.lan
param(
    [string]$InstallerGlob = "artifacts\DevBridge_*_x64-setup.exe",
    [int]$IppPort = 631,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120,
    [string]$CertsDir = "$env:TEMP\devbridge-certs"
)

$ErrorActionPreference = "Stop"

Write-Host "=== E2E Server Setup (NSIS Installer) ===" -ForegroundColor Cyan

# ── Stop existing service if running (upgrade path) ─────────────────
$svc = Get-Service -Name "DevBridge" -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -eq "Running") {
    Write-Host "Stopping existing DevBridge service..."
    Stop-Service -Name "DevBridge" -Force
    Start-Sleep -Seconds 3
}

# ── Find and run NSIS installer silently ────────────────────────────
$installer = Get-ChildItem -Path $InstallerGlob -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $installer) {
    # Fallback: try any .exe in artifacts that looks like an installer
    $installer = Get-ChildItem -Path "artifacts\*.exe" -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match "setup|DevBridge" -and $_.Name -notmatch "e2e" } |
        Select-Object -First 1
}
if (-not $installer) {
    throw "No NSIS installer found matching $InstallerGlob"
}

Write-Host "Running installer: $($installer.Name)"
$proc = Start-Process -FilePath $installer.FullName -ArgumentList "/S" -Wait -PassThru
if ($proc.ExitCode -ne 0) {
    throw "Installer exited with code $($proc.ExitCode)"
}
Write-Host "  Installer completed successfully" -ForegroundColor Green

# ── Verify installation ────────────────────────────────────────────
$installDir = "C:\Program Files\DevBridge"
if (-not (Test-Path "$installDir\devbridge-service.exe")) {
    throw "Service binary not found at $installDir\devbridge-service.exe after install"
}
Write-Host "  Binaries installed to $installDir"

# ── Run post-install script ─────────────────────────────────────────
$postInstall = Join-Path $PSScriptRoot "..\installer\post-install.ps1"
if (-not (Test-Path $postInstall)) {
    # Fallback: might be bundled in install dir
    $postInstall = "$installDir\post-install.ps1"
}

$postInstallArgs = @{
    Mode = "server"
    InstallDir = $installDir
    IppPort = $IppPort
    GrpcPort = $GrpcPort
    DashboardPort = $DashboardPort
}
if ($CertsDir -and (Test-Path $CertsDir)) {
    $postInstallArgs.CertsSource = $CertsDir
}

Write-Host "Running post-install configuration..."
& $postInstall @postInstallArgs

Write-Host "Server setup complete." -ForegroundColor Green
