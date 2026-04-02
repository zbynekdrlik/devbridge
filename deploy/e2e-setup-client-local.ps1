# E2E Setup: Install DevBridge client via NSIS installer on this machine
param(
    [string]$InstallerGlob = "artifacts\DevBridge_*_x64-setup.exe",
    [string]$ServerHost = "10.88.1.100",
    [string]$TargetPrinter = $env:E2E_TARGET_PRINTER,
    [int]$GrpcPort = 50152,
    [int]$DashboardPort = 9220,
    [string]$DataDir = "C:\ProgramData\DevBridge-E2E"
)

$ErrorActionPreference = "Stop"

if (-not $TargetPrinter) { $TargetPrinter = "Microsoft Print to PDF" }

Write-Host "=== E2E Client Setup (NSIS Installer) ===" -ForegroundColor Cyan
Write-Host "Target printer: $TargetPrinter"
Write-Host "Server: ${ServerHost}:${GrpcPort}"

# ── Stop existing E2E service (don't touch production) ──
try {
    $taskName = "DevBridgeE2E"
    $existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($existingTask -and $existingTask.State -eq "Running") {
        Write-Host "Stopping existing E2E scheduled task..."
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    }
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

# ── Clean E2E database for fresh state ────────────────────────────────
if (-not (Test-Path $DataDir)) {
    New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
}
$dbPath = Join-Path $DataDir "devbridge.db"
if (Test-Path $dbPath) {
    Remove-Item $dbPath -Force -ErrorAction SilentlyContinue
    Write-Host "Cleaned previous E2E database"
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

# ── Write E2E config directly (don't use post-install to avoid production conflicts) ──
$configPath = Join-Path $DataDir "config.toml"
$tomlData = $DataDir -replace '\\', '/'
$config = @"
[general]
mode = "client"
log_level = "debug"
data_dir = "$tomlData"

[server]
ipp_port = 631
grpc_port = $GrpcPort
dashboard_port = 9221
printer_name = "unused"
spool_dir = "$tomlData/spool"

[client]
server_address = "${ServerHost}:${GrpcPort}"
target_printer = "$TargetPrinter"
dashboard_port = $DashboardPort
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60
client_id = "e2e-client"
virtual_printer_name = "E2E Printer"

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

# ── Start E2E service (separate task name from production) ──
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
Write-Host "  E2E client service started"

# ── Configure headless PDF printing ─────────────────────────────────
if ($TargetPrinter -eq "Microsoft Print to PDF") {
    $outPath = Join-Path $DataDir "e2e-output.pdf"
    Write-Host "Configuring PDF printer for headless output to $outPath"
    try {
        # Ensure the output file exists (printer port errors if file missing)
        New-Item -ItemType File -Force -Path $outPath -ErrorAction SilentlyContinue | Out-Null
        Add-PrinterPort -Name $outPath -ErrorAction SilentlyContinue
        Set-Printer -Name "Microsoft Print to PDF" -PortName $outPath -ErrorAction Stop
        # Restart spooler to clear any previous Error state
        Restart-Service Spooler -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
        $status = (Get-Printer -Name "Microsoft Print to PDF").PrinterStatus
        Write-Host "  PDF printer port redirected (status: $status)" -ForegroundColor Green
    } catch {
        Write-Warning "Could not redirect PDF printer port (needs admin): $_"
        Write-Host "  Print jobs may prompt for filename in non-headless mode"
    }
}

Write-Host "Client setup complete." -ForegroundColor Green

# ── Keep job alive until E2E test completes ──────────────────────────
$signalFile = Join-Path $DataDir "e2e-done"
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
        Start-ScheduledTask -TaskName "DevBridgeE2E" -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 5
}
Write-Host "Client deploy job ending."
