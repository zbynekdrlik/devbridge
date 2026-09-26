# E2E Setup: Install DevBridge client via NSIS installer on this machine
param(
    [string]$InstallerGlob = "artifacts\DevBridge_*_x64-setup.exe",
    [string]$ServerHost = "10.88.1.100",
    [string]$TargetPrinter = $env:E2E_TARGET_PRINTER,
    [int]$GrpcPort = 50152,
    [int]$DashboardPort = 9220,
    [string]$DataDir = "C:\ProgramData\DevBridge-E2E",
    # Serial bridge written into the ISOLATED E2E config by the real installer
    # merge (issue #70). COM250 does not exist on pz-snv, so the client's reader
    # only warns + backs off; devbridge-e2e (src/serial_bridge.rs) asserts these
    # exact values on the client /api/status. Keep the two in sync.
    [string]$SerialPort = "COM250",
    [int]$SerialBaudRate = 9600,
    # Second isolated client for RAW passthrough (issue #88). Must match
    # crates/devbridge-e2e/src/raw_passthrough.rs (E2E_RAW_* constants) and
    # the approval skip in deploy/e2e-wait-ready.ps1.
    [string]$RawDataDir = "C:\ProgramData\DevBridge-E2E-Raw",
    [int]$RawDashboardPort = 9222,
    [string]$RawClientId = "e2e-raw-client",
    [string]$RawTargetPrinter = "DevBridge-E2E-Raw",
    [string]$RawVirtualPrinterName = "E2E Raw",
    [string]$RawVirtualPrinterDriver = "Generic / Text Only"
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

# RAW target printer (issue #88): the v4 "Microsoft Print To PDF" driver
# REJECTS RAW data (MS_XPS_PROC, 0x80070057), so the RAW E2E client prints to
# a v3 "Generic / Text Only" printer on NUL: (winprint, datatype RAW) -- the
# spooler logs EventID 307 with the exact byte count. The driver is an inbox
# driver; Add-PrinterDriver only stages it from the Windows driver store.
if (-not (Get-PrinterDriver -Name $RawVirtualPrinterDriver -ErrorAction SilentlyContinue)) {
    Write-Host "Installing inbox driver '$RawVirtualPrinterDriver' for the RAW E2E printer..."
    Add-PrinterDriver -Name $RawVirtualPrinterDriver -ErrorAction Stop
}
$rawPrinter = Get-Printer -Name $RawTargetPrinter -ErrorAction SilentlyContinue
if (-not $rawPrinter) {
    Write-Host "Creating RAW E2E printer '$RawTargetPrinter' ($RawVirtualPrinterDriver on NUL:)..."
    Add-PrinterPort -Name "NUL:" -ErrorAction SilentlyContinue
    Add-Printer -Name $RawTargetPrinter -DriverName $RawVirtualPrinterDriver -PortName "NUL:" -ErrorAction Stop
} elseif ($rawPrinter.DriverName -ne $RawVirtualPrinterDriver) {
    throw "RAW E2E printer '$RawTargetPrinter' exists with driver '$($rawPrinter.DriverName)', expected '$RawVirtualPrinterDriver'"
}

Write-Host "=== E2E Client Setup (NSIS Installer) ===" -ForegroundColor Cyan
Write-Host "Target printer: $TargetPrinter"
Write-Host "Server: ${ServerHost}:${GrpcPort}"

# ── Stop ALL devbridge services (NSIS needs the binary unlocked) ──
try {
    foreach ($taskName in @("DevBridgeE2E", "DevBridgeE2ERaw")) {
        $existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        if ($existingTask -and $existingTask.State -eq "Running") {
            Write-Host "Stopping existing $taskName scheduled task..."
            Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        }
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
$spoolDir = Join-Path $DataDir "spool"
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
print_timeout_secs = 1800
"@
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "spool") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "logs") | Out-Null
$config | Set-Content -Path $configPath -Encoding ASCII
Write-Host "  E2E config written to $configPath"

# ── Serial bridge via the REAL installer merge (issue #70) ──────────
# Exercise the exact function a DEVBRIDGE_SERIAL_PORT upgrade runs
# (installer/DevBridgeInstallerLib.ps1, which post-install.ps1 dot-sources --
# extracted via the AST like the Pester suite, so no script body is ever
# executed and the production data dir is never touched). The config above is rewritten fresh on every run, so anything but
# 'added' means the merge is broken.
$repoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $repoRoot "deploy\lib\Get-FunctionSourceFromScript.ps1")
$installerLibPath = Join-Path $repoRoot "installer\DevBridgeInstallerLib.ps1"
$serialSources = Get-FunctionSourceFromScript -ScriptPath $installerLibPath `
    -Names @("Get-DevBridgeSerialBridgeToml", "Merge-DevBridgeSerialBridgeIntoConfig")
foreach ($serialSrc in $serialSources.Values) {
    . ([scriptblock]::Create($serialSrc))
}
$serialMerge = Merge-DevBridgeSerialBridgeIntoConfig -Path $configPath `
    -SerialPort $SerialPort -SerialBaudRate $SerialBaudRate
if ($serialMerge -ne "added") {
    throw "Merge-DevBridgeSerialBridgeIntoConfig returned '$serialMerge' on the fresh E2E config (expected 'added')"
}
Write-Host "  Serial bridge merged into E2E config: $serialMerge (port=$SerialPort, baud=$SerialBaudRate)" -ForegroundColor Green

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
# Use SYSTEM/ServiceAccount like the production task. Interactive principal
# depends on an active user session which is fragile in CI.
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

# Verify service actually listens on the dashboard port (don't just sleep and hope)
$ready = $false
for ($i = 1; $i -le 30; $i++) {
    Start-Sleep -Seconds 1
    try {
        $s = Invoke-RestMethod -Uri "http://127.0.0.1:$DashboardPort/api/status" -TimeoutSec 2
        if ($s.status -eq "running") {
            Write-Host "  E2E client service ready (v=$($s.version), mode=$($s.mode), after ${i}s)" -ForegroundColor Green
            $ready = $true
            break
        }
    } catch {}
}
if (-not $ready) {
    $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    Write-Host "devbridge-service processes:" -ForegroundColor Yellow
    $proc | Select Id,StartTime,Path | Format-Table -AutoSize | Out-String | Write-Host
    throw "E2E client service did not become ready on port $DashboardPort within 30s"
}

# ── Second isolated client: RAW passthrough label printer (issue #88) ──
# Same binary, own data dir / dashboard port / task / client_id. The RAW-
# specific [client] keys are written by the REAL installer functions
# (Get-DevBridgeClientConfigProblems + Get-DevBridgeClientConfigExtras from
# installer/DevBridgeInstallerLib.ps1, AST-extracted -- nothing else of the
# installer runs), so this proves the installer writes a config the service
# accepts. e2e-wait-ready.ps1 leaves this client PENDING; devbridge-e2e step 34
# approves it, prints through it, then rejects it.
$rawSources = Get-FunctionSourceFromScript -ScriptPath $installerLibPath `
    -Names @("Get-DevBridgeSerialBridgeToml", "Get-DevBridgeClientConfigExtras", "Get-DevBridgeClientConfigProblems")
foreach ($rawSrc in $rawSources.Values) {
    . ([scriptblock]::Create($rawSrc))
}
$rawProblems = Get-DevBridgeClientConfigProblems -PrintBackend "windows_spooler_raw" -VirtualPrinterDriver $RawVirtualPrinterDriver
if ($rawProblems.Count -gt 0) {
    throw "Installer rejected the RAW E2E client config: $($rawProblems -join '; ')"
}
$rawExtras = Get-DevBridgeClientConfigExtras -ClientId $RawClientId -PrintBackend "windows_spooler_raw" `
    -VirtualPrinterName $RawVirtualPrinterName -VirtualPrinterDriver $RawVirtualPrinterDriver

New-Item -ItemType Directory -Force -Path $RawDataDir | Out-Null
$rawDb = Join-Path $RawDataDir "devbridge.db"
if (Test-Path $rawDb) {
    Remove-Item $rawDb -Force -ErrorAction Stop
    Write-Host "Cleaned previous RAW E2E database"
}
$rawSpool = Join-Path $RawDataDir "spool"
if (Test-Path $rawSpool) { Remove-Item "$rawSpool\*" -Force -Recurse -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force -Path $rawSpool | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $RawDataDir "logs") | Out-Null
$rawToml = $RawDataDir -replace '\\', '/'
$rawConfigPath = Join-Path $RawDataDir "config.toml"
$rawConfig = @"
[general]
mode = "client"
log_level = "debug"
data_dir = "$rawToml"

[server]
ipp_port = 631
grpc_port = $GrpcPort
dashboard_port = 9223
printer_name = "unused"
spool_dir = "$rawToml/spool"

[client]
server_address = "${ServerHost}:${GrpcPort}"
target_printer = "$RawTargetPrinter"
dashboard_port = $RawDashboardPort
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60
$rawExtras

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
print_timeout_secs = 1800
"@
$rawConfig | Set-Content -Path $rawConfigPath -Encoding ASCII
Write-Host "  RAW E2E config written to $rawConfigPath (print_backend=windows_spooler_raw, virtual_printer_driver=$RawVirtualPrinterDriver)"

$rawTaskName = "DevBridgeE2ERaw"
Unregister-ScheduledTask -TaskName $rawTaskName -Confirm:$false -ErrorAction SilentlyContinue
$rawAction = New-ScheduledTaskAction -Execute $serviceExe -Argument "--config `"$rawConfigPath`"" -WorkingDirectory $RawDataDir
$rawTrigger = New-ScheduledTaskTrigger -AtStartup
$rawSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
$rawSettings.IdleSettings.StopOnIdleEnd = $false
$rawPrincipal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
Register-ScheduledTask -TaskName $rawTaskName -Action $rawAction -Settings $rawSettings -Principal $rawPrincipal -Trigger $rawTrigger | Out-Null
Start-ScheduledTask -TaskName $rawTaskName

$rawReady = $false
for ($i = 1; $i -le 30; $i++) {
    Start-Sleep -Seconds 1
    try {
        $s = Invoke-RestMethod -Uri "http://127.0.0.1:$RawDashboardPort/api/status" -TimeoutSec 2
        if ($s.status -eq "running" -and $s.print_backend -eq "windows_spooler_raw") {
            Write-Host "  RAW E2E client ready (v=$($s.version), backend=$($s.print_backend), after ${i}s)" -ForegroundColor Green
            $rawReady = $true
            break
        }
    } catch {}
}
if (-not $rawReady) {
    $rawLog = Get-ChildItem (Join-Path $RawDataDir "logs") -Filter "*.log" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($rawLog) { Get-Content $rawLog.FullName -Tail 40 | ForEach-Object { Write-Host "    $_" } }
    throw "RAW E2E client did not become ready (backend windows_spooler_raw) on port $RawDashboardPort within 30s"
}

# Production task stays stopped on client during E2E to avoid queue conflicts.
# It will be restarted when the keepalive loop ends or by the next production deploy.

# Restart production task (was stopped for binary upgrade)
$prodTask = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
if ($prodTask) {
    Write-Host "Restarting production task after binary upgrade..."
    Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    Start-Sleep 3
}

# ── Restart tray app for all active sessions ─────────────────────────
# Binary upgrade kills existing tray instances. Restart one per active
# session so each user gets their per-user tray with notifications.
$trayExe = "C:\Program Files\DevBridge\devbridge-app.exe"
if (Test-Path $trayExe) {
    Get-Process devbridge-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep 1

    # Include Active AND Disconnected sessions — disconnected users may
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
    Write-Host "Restarting tray app for $count active session(s)..."
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
            Write-Host "  [OK] $($s.Username)"
        } catch {
            Write-Host "  [FAIL] $($s.Username): $_" -ForegroundColor Yellow
            Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
        }
    }
}

Write-Host "Client setup complete." -ForegroundColor Green
