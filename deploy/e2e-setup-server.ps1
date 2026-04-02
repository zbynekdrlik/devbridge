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

# ── Write E2E config directly (don't use post-install to avoid production conflicts) ──
$configPath = Join-Path $DataDir "config.toml"
$tomlData = $DataDir -replace '\\', '/'
$config = @"
[general]
mode = "server"
log_level = "debug"
data_dir = "$tomlData"

[server]
ipp_port = $IppPort
grpc_port = $GrpcPort
dashboard_port = $DashboardPort
printer_name = "DevBridge-E2E"
spool_dir = "$tomlData/spool"

[client]
server_address = "127.0.0.1:$GrpcPort"
target_printer = "unused"
dashboard_port = 9221
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
"@
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "spool") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "logs") | Out-Null
$config | Set-Content -Path $configPath -Encoding ASCII
Write-Host "  E2E config written to $configPath"

# ── Start E2E service directly (separate task name from production) ──
$serviceExe = Join-Path $installDir "devbridge-service.exe"
$taskName = "DevBridgeE2E"
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
$action = New-ScheduledTaskAction -Execute $serviceExe -Argument "--config `"$configPath`"" -WorkingDirectory $DataDir
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
$settings.IdleSettings.StopOnIdleEnd = $false
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -Principal $principal -Trigger $trigger | Out-Null
Start-ScheduledTask -TaskName $taskName
Start-Sleep 5

$proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
Write-Host "  E2E service started (processes: $($proc.Count))"

# ── Verify E2E server responds ─────────────────────────────────────────
try {
    $status = Invoke-RestMethod -Uri "http://127.0.0.1:${DashboardPort}/api/status" -TimeoutSec 5
    Write-Host "  E2E server: mode=$($status.mode) version=$($status.version)" -ForegroundColor Green
} catch {
    Write-Warning "E2E server not responding on port $DashboardPort"
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
