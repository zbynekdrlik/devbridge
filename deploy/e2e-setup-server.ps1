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

# ── Find NSIS installer ────────────────────────────────────────────
$installer = Get-ChildItem -Path $InstallerGlob -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $installer) {
    $installer = Get-ChildItem -Path "artifacts\*.exe" -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match "setup|DevBridge" -and $_.Name -notmatch "e2e" } |
        Select-Object -First 1
}
if (-not $installer) {
    throw "No NSIS installer found matching $InstallerGlob"
}

# ── Run NSIS installer silently ─────────────────────────────────────
Write-Host "Running installer: $($installer.Name)"

# Check if we're elevated (required for perMachine install to Program Files)
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "  Running as admin: $isAdmin"

# Run installer — use cmd /c to ensure proper argument handling
$proc = Start-Process -FilePath $installer.FullName -ArgumentList "/S" -Wait -PassThru
if ($proc.ExitCode -ne 0) {
    throw "Installer exited with code $($proc.ExitCode)"
}

# Give the installer a moment to finish file operations
Start-Sleep -Seconds 3
Write-Host "  Installer completed successfully" -ForegroundColor Green

# ── Verify installation ────────────────────────────────────────────
$installDir = "C:\Program Files\DevBridge"

# Check multiple possible install locations
$installCandidates = @(
    "C:\Program Files\DevBridge",
    "$env:LOCALAPPDATA\DevBridge",
    "$env:LOCALAPPDATA\Programs\DevBridge"
)

$foundDir = $null
foreach ($candidate in $installCandidates) {
    if (Test-Path "$candidate\devbridge-service.exe") {
        $foundDir = $candidate
        break
    }
}

if (-not $foundDir) {
    Write-Host "Searching for installed files..." -ForegroundColor Yellow
    foreach ($candidate in $installCandidates) {
        Write-Host "  Checking $candidate :"
        if (Test-Path $candidate) {
            Get-ChildItem $candidate -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $($_.Name)" }
        } else {
            Write-Host "    (does not exist)"
        }
    }
    throw "Service binary not found in any expected install location after install"
}

$installDir = $foundDir
Write-Host "  Binaries installed to $installDir"

# ── Run post-install script ─────────────────────────────────────────
$postInstall = Join-Path $PSScriptRoot "..\installer\post-install.ps1"
if (-not (Test-Path $postInstall)) {
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
