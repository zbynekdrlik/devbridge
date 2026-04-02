# E2E Setup: Install DevBridge server via NSIS installer on 10.88.1.100
# Uses separate ports and data dir to avoid interfering with production.
param(
    [string]$InstallerGlob = "artifacts\DevBridge_*_x64-setup.exe",
    [int]$IppPort = 1631,
    [int]$GrpcPort = 50152,
    [int]$DashboardPort = 9220,
    [string]$DataDir = "C:\ProgramData\DevBridge-E2E",
    [string]$CertsDir = ""
)

$ErrorActionPreference = "Stop"

Write-Host "=== E2E Server Setup (NSIS Installer) ===" -ForegroundColor Cyan

# ── Stop existing E2E service (don't touch production) ──
try {
    $taskName = "DevBridgeE2E"
    $existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($existingTask -and $existingTask.State -eq "Running") {
        Write-Host "Stopping existing E2E scheduled task..."
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    }
    # Kill any E2E devbridge-service processes (on E2E ports) — identify by command line
    Get-CimInstance Win32_Process -Filter "Name='devbridge-service.exe'" | ForEach-Object {
        if ($_.CommandLine -like "*$DataDir*") {
            Write-Host "Stopping E2E devbridge-service (PID: $($_.ProcessId))..."
            Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    Start-Sleep -Seconds 3
} catch {
    Write-Host "  Cleanup warning (non-fatal): $_" -ForegroundColor Yellow
    Start-Sleep -Seconds 3
}

# ── Clean E2E data directory for fresh state ────────────────────────────────
if (Test-Path $DataDir) {
    $dbPath = Join-Path $DataDir "devbridge.db"
    if (Test-Path $dbPath) {
        Remove-Item $dbPath -Force -ErrorAction SilentlyContinue
        Write-Host "Cleaned previous E2E database"
    }
} else {
    New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
    Write-Host "Created E2E data directory: $DataDir"
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
    DataDir = $DataDir
    IppPort = $IppPort
    GrpcPort = $GrpcPort
    DashboardPort = $DashboardPort
    PrinterName = "DevBridge-E2E"
}

Write-Host "Running post-install configuration..."
& $postInstall @postInstallArgs

# ── Verify printer registered ─────────────────────────────────────────
$printer = Get-Printer -Name "DevBridge" -ErrorAction SilentlyContinue
if ($printer) {
    Write-Host "  DevBridge printer registered" -ForegroundColor Green
} else {
    Write-Host "  WARNING: DevBridge printer not found" -ForegroundColor Yellow
}

# ── Verify tray app installed ─────────────────────────────────────────
$trayPath = "C:\Program Files\DevBridge\DevBridge.exe"
$trayAlt = "C:\Program Files\DevBridge\devbridge-app.exe"
if ((Test-Path $trayPath) -or (Test-Path $trayAlt)) {
    Write-Host "  Tray app binary found" -ForegroundColor Green
} else {
    Write-Host "  WARNING: Tray app binary not found" -ForegroundColor Yellow
}

Write-Host "Server setup complete." -ForegroundColor Green
