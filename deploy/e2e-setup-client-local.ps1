# E2E Setup: Install DevBridge client via NSIS installer on this machine
param(
    [string]$InstallerGlob = "artifacts\DevBridge_*_x64-setup.exe",
    [string]$ServerHost = "print-server.lan",
    [string]$TargetPrinter = $env:E2E_TARGET_PRINTER,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120
)

$ErrorActionPreference = "Stop"

if (-not $TargetPrinter) { $TargetPrinter = "Microsoft Print to PDF" }

Write-Host "=== E2E Client Setup (NSIS Installer) ===" -ForegroundColor Cyan
Write-Host "Target printer: $TargetPrinter"
Write-Host "Server: ${ServerHost}:${GrpcPort}"

# ── Stop existing service (keep task registered — runner lacks admin to re-create) ──
try {
    $taskName = "DevBridgeService"
    $existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($existingTask -and $existingTask.State -eq "Running") {
        Write-Host "Stopping existing DevBridge scheduled task..."
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    }
    $procs = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    if ($procs) {
        Write-Host "Stopping existing devbridge-service process..."
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 3
} catch {
    Write-Host "  Cleanup warning (non-fatal): $_" -ForegroundColor Yellow
    Start-Sleep -Seconds 3
}

# ── Clean database for fresh E2E state ────────────────────────────────
$dbPath = "C:\ProgramData\DevBridge\devbridge.db"
if (Test-Path $dbPath) {
    Remove-Item $dbPath -Force -ErrorAction SilentlyContinue
    Write-Host "Cleaned previous database for fresh E2E state"
}

# ── Find and run NSIS installer silently ────────────────────────────
$installer = Get-ChildItem -Path $InstallerGlob -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $installer) {
    $installer = Get-ChildItem -Path "artifacts\*.exe" -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match "setup|DevBridge" -and $_.Name -notmatch "e2e" } |
        Select-Object -First 1
}
if (-not $installer) {
    throw "No NSIS installer found matching $InstallerGlob"
}

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "Running installer: $($installer.Name) (admin: $isAdmin)"

$proc = Start-Process -FilePath $installer.FullName -ArgumentList "/S" -Wait -PassThru
if ($proc.ExitCode -ne 0) {
    throw "Installer exited with code $($proc.ExitCode)"
}

Start-Sleep -Seconds 3
Write-Host "  Installer completed successfully" -ForegroundColor Green

# ── Verify installation ────────────────────────────────────────────
$installCandidates = @(
    "C:\Program Files\DevBridge",
    "$env:LOCALAPPDATA\DevBridge",
    "$env:LOCALAPPDATA\Programs\DevBridge"
)

$installDir = $null
foreach ($candidate in $installCandidates) {
    if (Test-Path "$candidate\devbridge-service.exe") {
        $installDir = $candidate
        break
    }
}

if (-not $installDir) {
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

Write-Host "  Binaries installed to $installDir"

# ── Run post-install script ─────────────────────────────────────────
$postInstall = Join-Path $PSScriptRoot "..\installer\post-install.ps1"
if (-not (Test-Path $postInstall)) {
    $postInstall = "$installDir\post-install.ps1"
}

Write-Host "Running post-install configuration..."
& $postInstall -Mode client -InstallDir $installDir `
    -ServerHost $ServerHost -TargetPrinter $TargetPrinter `
    -GrpcPort $GrpcPort -DashboardPort $DashboardPort

# ── Configure headless PDF printing ─────────────────────────────────
if ($TargetPrinter -eq "Microsoft Print to PDF") {
    $outPath = "C:\ProgramData\DevBridge\e2e-output.pdf"
    Write-Host "Configuring PDF printer for headless output to $outPath"
    try {
        Add-PrinterPort -Name $outPath -ErrorAction SilentlyContinue
        Set-Printer -Name "Microsoft Print to PDF" -PortName $outPath -ErrorAction Stop
        Write-Host "  PDF printer port redirected" -ForegroundColor Green
    } catch {
        Write-Warning "Could not redirect PDF printer port (needs admin): $_"
        Write-Host "  Print jobs may prompt for filename in non-headless mode"
    }
}

Write-Host "Client setup complete." -ForegroundColor Green

# ── Keep job alive until E2E test completes ──────────────────────────
$signalFile = "C:\ProgramData\DevBridge\e2e-done"
$timeout = 600
$start = Get-Date
Write-Host "Keeping client job alive until E2E test completes (max ${timeout}s)..."
while (((Get-Date) - $start).TotalSeconds -lt $timeout) {
    if (Test-Path $signalFile) {
        Write-Host "E2E test completed signal received."
        Remove-Item $signalFile -ErrorAction SilentlyContinue
        break
    }
    $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    if (-not $proc) {
        Write-Warning "Process stopped unexpectedly, restarting via scheduled task..."
        Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 5
}
Write-Host "Client deploy job ending."
