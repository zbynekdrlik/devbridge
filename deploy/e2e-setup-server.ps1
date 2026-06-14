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

# -- Stop ALL devbridge services (NSIS needs the binary unlocked) --
try {
    # Stop E2E task
    $taskName = "DevBridgeE2E"
    $existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($existingTask -and $existingTask.State -eq "Running") {
        Write-Host "Stopping existing E2E scheduled task..."
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    }
    # Stop production task (binary is shared -- NSIS can't overwrite if locked)
    $prodTask = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    if ($prodTask -and $prodTask.State -eq "Running") {
        Write-Host "Stopping production task for binary upgrade..."
        Stop-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    }
    # Kill ALL devbridge-service processes so the binary file is unlocked
    Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "Stopping devbridge-service (PID: $($_.Id))..."
        Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 3
} catch {
    Write-Host "  Cleanup warning (non-fatal): $_" -ForegroundColor Yellow
    Start-Sleep -Seconds 3
}

# -- Clean E2E data directory for fresh state --------------------------------
if (Test-Path $DataDir) {
    $dbPath = Join-Path $DataDir "devbridge.db"
    $spoolDir = Join-Path $DataDir "spool"
    # Delete DB and spool to ensure no stale jobs from previous runs
    if (Test-Path $dbPath) {
        Remove-Item $dbPath -Force -ErrorAction SilentlyContinue
        if (Test-Path $dbPath) {
            Write-Host "DB still locked, killing all devbridge processes..." -ForegroundColor Yellow
            Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue | Stop-Process -Force
            Start-Sleep 2
            Remove-Item $dbPath -Force -ErrorAction Stop
        }
        Write-Host "Cleaned previous E2E database"
    }
    if (Test-Path $spoolDir) {
        Remove-Item "$spoolDir\*" -Force -Recurse -ErrorAction SilentlyContinue
        Write-Host "Cleaned previous E2E spool files"
    }
} else {
    New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
    Write-Host "Created E2E data directory: $DataDir"
}

# -- Find NSIS installer --------------------------------------------
$installer = Get-ChildItem -Path $InstallerGlob -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $installer) {
    $installer = Get-ChildItem -Path "artifacts\*.exe" -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match "setup|DevBridge" -and $_.Name -notmatch "e2e" } |
        Select-Object -First 1
}
if (-not $installer) {
    throw "No NSIS installer found matching $InstallerGlob"
}

# -- Run NSIS installer silently -------------------------------------
Write-Host "Running installer: $($installer.Name)"

# Check if we're elevated (required for perMachine install to Program Files)
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "  Running as admin: $isAdmin"

# Run installer -- use cmd /c to ensure proper argument handling
$proc = Start-Process -FilePath $installer.FullName -ArgumentList "/S" -Wait -PassThru
if ($proc.ExitCode -ne 0) {
    throw "Installer exited with code $($proc.ExitCode)"
}

# Give the installer a moment to finish file operations
Start-Sleep -Seconds 3
Write-Host "  Installer completed successfully" -ForegroundColor Green

# -- Verify installation --------------------------------------------
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

# -- Write E2E config directly (don't use post-install to avoid production conflicts) --
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
print_timeout_secs = 1800
"@
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "spool") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "logs") | Out-Null
$config | Set-Content -Path $configPath -Encoding ASCII
Write-Host "  E2E config written to $configPath"

# -- Start E2E service directly (separate task name from production) --
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

# -- Register E2E auto-update task (issue #54) --------------------------------
# The E2E setup deliberately does NOT run post-install.ps1 (it writes its own
# config + task to avoid production conflicts), so it must mirror the auto-update
# task registration here for test 32 to verify. We stage the REAL autoupdate.ps1
# from the NSIS payload and register it under an E2E-specific task name
# (DevBridgeAutoUpdateE2E) with the SAME triggers production uses (AtStartup +
# every 6h). We do NOT start it: the task is trigger-driven, those triggers will
# not fire within the short E2E window, and e2e-cleanup.ps1 unregisters it so it
# can NEVER auto-upgrade this runner later. This proves registration works
# without ever firing a real auto-upgrade on CI.
$autoUpdateTaskName = "DevBridgeAutoUpdateE2E"
$autoUpdateSrcCandidates = @(
    (Join-Path $installDir "_up_\_up_\installer\autoupdate.ps1"),
    (Join-Path $installDir "autoupdate.ps1")
)
$autoUpdateSrc = $autoUpdateSrcCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($autoUpdateSrc) {
    $autoUpdateDst = Join-Path $DataDir "autoupdate.ps1"
    Copy-Item $autoUpdateSrc $autoUpdateDst -Force
    try {
        $auAction = New-ScheduledTaskAction -Execute "powershell.exe" `
            -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$autoUpdateDst`"" `
            -WorkingDirectory $DataDir
        $auStartupTrigger = New-ScheduledTaskTrigger -AtStartup
        $auRepeatTrigger = New-ScheduledTaskTrigger -Once -At (Get-Date) `
            -RepetitionInterval (New-TimeSpan -Hours 6)
        $auSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries `
            -DontStopIfGoingOnBatteries -StartWhenAvailable `
            -ExecutionTimeLimit (New-TimeSpan -Hours 1)
        $auPrincipal = New-ScheduledTaskPrincipal -UserId "SYSTEM" `
            -LogonType ServiceAccount -RunLevel Highest
        Unregister-ScheduledTask -TaskName $autoUpdateTaskName -Confirm:$false -ErrorAction SilentlyContinue
        Register-ScheduledTask -TaskName $autoUpdateTaskName -Action $auAction `
            -Settings $auSettings -Principal $auPrincipal `
            -Trigger @($auStartupTrigger, $auRepeatTrigger) | Out-Null
        Write-Host "  Registered '$autoUpdateTaskName' (AtStartup + every 6h, not started)" -ForegroundColor Green
    } catch {
        Write-Host "  WARNING: E2E auto-update task registration failed: $_" -ForegroundColor Yellow
    }
} else {
    Write-Host "  WARNING: autoupdate.ps1 not found in NSIS payload; E2E auto-update task not registered" -ForegroundColor Yellow
}

# -- Restart production task (was stopped for binary upgrade) --
$prodTask = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
if ($prodTask) {
    Write-Host "Restarting production task after binary upgrade..."
    Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    Start-Sleep 3
}

# -- Verify E2E server responds -----------------------------------------
try {
    $status = Invoke-RestMethod -Uri "http://127.0.0.1:${DashboardPort}/api/status" -TimeoutSec 5
    Write-Host "  E2E server: mode=$($status.mode) version=$($status.version)" -ForegroundColor Green
} catch {
    Write-Warning "E2E server not responding on port $DashboardPort"
}

# -- Register E2E Windows IPP printer ---------------------------------
# $ErrorActionPreference=Stop above makes the caller eat the whole
# block silently if any cmdlet throws -- which is how a prior version
# of this section "succeeded" without ever registering the printer.
# Run the whole registration inside its own try/catch so every step
# logs, and surface any failure as a hard error at the end.
Write-Host ""
Write-Host "=== Verify E2E Windows IPP printer ==="
# In 0.8.20+ the devbridge-service process owns Windows-printer
# registration. It spawns register-virtual-printers.ps1 on startup (with
# -IppPort matching the service's IPP listener) AND on every virtual-
# printer DB change. Previous versions of this script shelled out to
# rundll32 printui.dll to register DevBridge-E2E directly -- redundant
# now, and used the wrong IPP port (hardcoded 631 vs the E2E instance's
# 1631). This block just probes the endpoint and polls until the service
# has reconciled the Windows printer into place.
$printerName = "DevBridge-E2E"
$ippUrl = "http://127.0.0.1:${IppPort}/ipp/print"
try {
    Write-Host "  Probing IPP endpoint at $ippUrl ..."
    $ippReady = $false
    for ($i = 1; $i -le 30; $i++) {
        $conn = Test-NetConnection -ComputerName 127.0.0.1 -Port $IppPort -InformationLevel Quiet -WarningAction SilentlyContinue -ErrorAction SilentlyContinue
        if ($conn) { $ippReady = $true; Write-Host "  IPP port $IppPort listening after ${i}s"; break }
        Start-Sleep -Seconds 1
    }
    if (-not $ippReady) {
        throw "IPP port $IppPort not listening after 30s; service failed to start"
    }

    # The service's reconciler runs asynchronously after startup. Poll for
    # up to 30s for the Windows printer to appear with the expected URL.
    $expectedPort = "http://127.0.0.1:${IppPort}/printers/devbridge-e2e"
    $verify = $null
    for ($i = 1; $i -le 30; $i++) {
        $verify = Get-Printer -Name $printerName -ErrorAction SilentlyContinue
        if ($verify -and $verify.PortName -eq $expectedPort) {
            Write-Host "  Service-owned reconciler registered '$printerName' -> $($verify.PortName) (after ${i}s)" -ForegroundColor Green
            break
        }
        $verify = $null
        Start-Sleep -Seconds 1
    }
    if (-not $verify) {
        # Dump the reconciler log so CI failure output explains what the
        # service tried. The service writes its own logs under DataDir.
        $rLog = Join-Path $DataDir "logs\register-virtual-printers.log"
        if (Test-Path $rLog) {
            Write-Host "  Reconciler log tail:" -ForegroundColor Yellow
            Get-Content $rLog -Tail 40 | ForEach-Object { Write-Host "    $_" }
        }
        $sLog = Get-ChildItem (Join-Path $DataDir "logs") -Filter "service*.log" -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($sLog) {
            Write-Host "  Service log tail:" -ForegroundColor Yellow
            Get-Content $sLog.FullName -Tail 20 | Select-String "reconciler" | ForEach-Object { Write-Host "    $_" }
        }
        throw "Service-owned reconciler did not register '$printerName' within 30s"
    }
} catch {
    Write-Host "  ERROR: $_" -ForegroundColor Red
    throw
}

# -- Restart tray app for all active sessions -------------------------
# Binary upgrade kills existing tray instances. Restart one per active
# RDP session so each user gets their per-user tray with notifications.
$trayExe = "C:\Program Files\DevBridge\devbridge-app.exe"
if (Test-Path $trayExe) {
    Get-Process devbridge-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep 1

    # Include Active AND Disconnected sessions -- disconnected users may
    # reconnect later and the tray app needs to already be running in their
    # session. `query user` puts USERNAME in the first column.
    # Note: `query user` always returns exit code 1 on Windows even when it
    # succeeds, so we explicitly clear $LASTEXITCODE afterwards.
    $sessions = query user 2>$null | Select-Object -Skip 1 | ForEach-Object {
        if ($_ -match '^>?\s*(\S+)\s+.*?\s+(\d+)\s+(Active|Disc)') {
            [PSCustomObject]@{
                Username  = $matches[1]
                SessionId = [int]$matches[2]
                State     = $matches[3]
            }
        }
    } | Where-Object { $_ }
    $global:LASTEXITCODE = 0

    $count = if ($sessions) { @($sessions).Count } else { 0 }
    Write-Host "  Restarting tray app for $count active session(s)..."
    foreach ($s in @($sessions)) {
        $taskName = "DevBridgeTrayStart_$($s.Username)"
        try {
            $action = New-ScheduledTaskAction -Execute $trayExe
            $principal = New-ScheduledTaskPrincipal -UserId $s.Username -LogonType Interactive
            $task = New-ScheduledTask -Action $action -Principal $principal
            Register-ScheduledTask -TaskName $taskName -InputObject $task -Force | Out-Null
            Start-ScheduledTask -TaskName $taskName
            Start-Sleep -Milliseconds 500
            Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
            Write-Host "    [OK] $($s.Username)"
        } catch {
            Write-Host "    [FAIL] $($s.Username): $_" -ForegroundColor Yellow
            Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
        }
    }
} else {
    Write-Host "  WARNING: Tray app binary not found at $trayExe" -ForegroundColor Yellow
}

Write-Host "Server setup complete." -ForegroundColor Green
