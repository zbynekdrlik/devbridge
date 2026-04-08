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

if (-not $TargetPrinter) { $TargetPrinter = "DevBridge-NullPrinter" }

# Ensure the NUL printer exists (prints to NUL port — no save dialog, works in CI)
$nullPrinter = Get-Printer -Name $TargetPrinter -ErrorAction SilentlyContinue
if (-not $nullPrinter) {
    Write-Host "Creating NUL printer '$TargetPrinter' for headless CI testing..."
    Add-PrinterPort -Name "NUL:" -ErrorAction SilentlyContinue
    Add-Printer -Name $TargetPrinter -DriverName "Microsoft Print To PDF" -PortName "NUL:" -ErrorAction Stop
}

Write-Host "=== E2E Client Setup (NSIS Installer) ===" -ForegroundColor Cyan
Write-Host "Target printer: $TargetPrinter"
Write-Host "Server: ${ServerHost}:${GrpcPort}"

# ── Stop ALL devbridge services (NSIS needs the binary unlocked) ──
try {
    $taskName = "DevBridgeE2E"
    $existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($existingTask -and $existingTask.State -eq "Running") {
        Write-Host "Stopping existing E2E scheduled task..."
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    }
    $prodTask = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    if ($prodTask -and $prodTask.State -eq "Running") {
        Write-Host "Stopping production task for binary upgrade..."
        Stop-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    }
    Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "Stopping devbridge-service (PID: $($_.Id))..."
        Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
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

# ── Configure headless PDF printing BEFORE starting service ─────────
if ($TargetPrinter -eq "Microsoft Print to PDF") {
    $outPath = Join-Path $DataDir "e2e-output.pdf"
    Write-Host "Configuring PDF printer for headless output to $outPath"
    try {
        # Force-clear stuck print jobs (Retained jobs survive Remove-PrintJob)
        # Safety: check for non-test print jobs before clearing
        $activeJobs = Get-PrintJob -PrinterName "Microsoft Print to PDF" -ErrorAction SilentlyContinue
        $nonTestJobs = $activeJobs | Where-Object { $_.DocumentName -notlike "*E2E*" -and $_.DocumentName -notlike "*Test*" }
        if ($nonTestJobs) {
            Write-Host "  WARNING: Non-test print jobs detected, skipping spooler clear"
        } else {
            Stop-Service Spooler -Force -ErrorAction SilentlyContinue
            Start-Sleep 1
            $spoolDir = "$env:SystemRoot\System32\spool\PRINTERS"
            Remove-Item "$spoolDir\*" -Force -ErrorAction SilentlyContinue
            Start-Service Spooler
            Start-Sleep 2
            Write-Host "  Cleared print spooler"
        }

        New-Item -ItemType File -Force -Path $outPath -ErrorAction SilentlyContinue | Out-Null
        Add-PrinterPort -Name $outPath -ErrorAction SilentlyContinue
        Set-Printer -Name "Microsoft Print to PDF" -PortName $outPath -ErrorAction Stop
        Restart-Service Spooler -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
        $status = (Get-Printer -Name "Microsoft Print to PDF").PrinterStatus
        Write-Host "  PDF printer port redirected (status: $status)" -ForegroundColor Green
    } catch {
        Write-Warning "Could not redirect PDF printer port (needs admin): $_"
        Write-Host "  Print jobs may prompt for filename in non-headless mode"
    }
}

# ── Start E2E service (separate task name from production) ──
$serviceExe = Join-Path $installDir "devbridge-service.exe"
$taskName = "DevBridgeE2E"
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
$action = New-ScheduledTaskAction -Execute $serviceExe -Argument "--config `"$configPath`"" -WorkingDirectory $DataDir
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
$settings.IdleSettings.StopOnIdleEnd = $false
$currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$principal = New-ScheduledTaskPrincipal -UserId $currentUser -LogonType Interactive -RunLevel Highest
Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -Principal $principal -Trigger $trigger | Out-Null
Start-ScheduledTask -TaskName $taskName
Start-Sleep 5
Write-Host "  E2E client service started"

# Production task stays stopped on client during E2E to avoid queue conflicts.
# It will be restarted when the keepalive loop ends or by the next production deploy.

# Restart production task (was stopped for binary upgrade)
$prodTask = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
if ($prodTask) {
    Write-Host "Restarting production task after binary upgrade..."
    Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    Start-Sleep 3
}

Write-Host "Client setup complete." -ForegroundColor Green
