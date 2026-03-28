# DevBridge Post-Install Configuration
# Run after NSIS installer to configure service, certs, and tray app auto-start.
# Idempotent: safe to run on upgrades (stops service first, updates config, restarts).
#
# Usage:
#   .\post-install.ps1 -Mode server -IppPort 631 -GrpcPort 50051 -DashboardPort 9120
#   .\post-install.ps1 -Mode client -ServerHost print-server.lan -TargetPrinter "EPSON L3270"

param(
    [Parameter(Mandatory)][ValidateSet("server", "client")][string]$Mode,
    [string]$InstallDir = "C:\Program Files\DevBridge",
    [string]$DataDir = "C:\ProgramData\DevBridge",
    [string]$ServerHost = "print-server.lan",
    [string]$TargetPrinter = "Microsoft Print to PDF",
    [int]$IppPort = 631,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120,
    [string]$PrinterName = "DevBridge",
    [string]$CertsSource = ""
)

$ErrorActionPreference = "Stop"
$serviceExe = Join-Path $InstallDir "devbridge-service.exe"
$trayExe = Join-Path $InstallDir "devbridge-app.exe"
if (-not (Test-Path $trayExe)) {
    $trayExe = Join-Path $InstallDir "DevBridge.exe"
}

Write-Host "=== DevBridge Post-Install - $Mode mode ===" -ForegroundColor Cyan

# ── Stop existing instance if upgrading ──────────────────────────────────────
$taskName = "DevBridgeService"
$existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($existingTask -and $existingTask.State -eq "Running") {
    Write-Host "Stopping existing scheduled task for upgrade..."
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
}
Stop-Process -Name "devbridge-service" -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

# ── Create data directory structure ─────────────────────────────────────────
$subdirs = @("certs", "spool", "logs")
foreach ($sub in $subdirs) {
    $path = Join-Path $DataDir $sub
    if (-not (Test-Path $path)) {
        New-Item -ItemType Directory -Force -Path $path | Out-Null
        Write-Host "  Created $path"
    }
}

# ── Copy TLS certificates ──────────────────────────────────────────────────
$certsDir = Join-Path $DataDir "certs"
if ($CertsSource -and (Test-Path $CertsSource)) {
    Write-Host "Copying certificates from $CertsSource"
    Copy-Item "$CertsSource\*" $certsDir -Force
}

# ── Firewall rules ────────────────────────────────────────────────────────
Write-Host "Configuring firewall rules..."
$fwRules = @(
    @{ Name="DevBridge-Dashboard"; Port=$DashboardPort }
)
if ($Mode -eq "server") {
    $fwRules += @{ Name="DevBridge-gRPC"; Port=$GrpcPort }
    $fwRules += @{ Name="DevBridge-IPP"; Port=$IppPort }
}
foreach ($rule in $fwRules) {
    $existing = Get-NetFirewallRule -DisplayName $rule.Name -ErrorAction SilentlyContinue
    if (-not $existing) {
        New-NetFirewallRule -DisplayName $rule.Name -Direction Inbound `
            -Protocol TCP -LocalPort $rule.Port -Action Allow | Out-Null
        Write-Host "  Created firewall rule: $($rule.Name) (port $($rule.Port))"
    } else {
        Write-Host "  Firewall rule exists: $($rule.Name)"
    }
}
# Also allow the service binary itself (some firewalls block by executable)
$fwBinaryRule = "DevBridge-Service"
if (-not (Get-NetFirewallRule -DisplayName $fwBinaryRule -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName $fwBinaryRule -Direction Inbound `
        -Program $serviceExe -Action Allow | Out-Null
    Write-Host "  Created firewall rule: $fwBinaryRule (binary)"
}

# ── Check/install prerequisites ───────────────────────────────────────────
# VC++ Runtime is required for the Rust binary
$vcInstalled = Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64" -ErrorAction SilentlyContinue
if (-not $vcInstalled) {
    $vcPath = Join-Path $InstallDir "redist\vc_redist.x64.exe"
    if (Test-Path $vcPath) {
        Write-Host "Installing VC++ Runtime..."
        Start-Process -FilePath $vcPath -ArgumentList "/install /quiet /norestart" -Wait
        Write-Host "  VC++ Runtime installed" -ForegroundColor Green
    } else {
        Write-Warning "VC++ Runtime not found. Binary may fail with STATUS_DLL_NOT_FOUND."
    }
}
# SumatraPDF is used for headless PDF printing on client
$sumatraTarget = "C:\Program Files\SumatraPDF\SumatraPDF.exe"
if (-not (Test-Path $sumatraTarget)) {
    $sumatraBundled = Join-Path $InstallDir "redist\SumatraPDF.exe"
    if (Test-Path $sumatraBundled) {
        Write-Host "Installing SumatraPDF..."
        New-Item -ItemType Directory -Force -Path "C:\Program Files\SumatraPDF" | Out-Null
        Copy-Item $sumatraBundled $sumatraTarget
        Write-Host "  SumatraPDF installed" -ForegroundColor Green
    }
}

# ── Write configuration ────────────────────────────────────────────────────
$configPath = Join-Path $DataDir "config.toml"
# Use debug logging in CI for easier troubleshooting
if ($env:CI) { $logLevel = "debug" } else { $logLevel = "info" }
# Use forward slashes in TOML to avoid escaping issues
$tomlData = $DataDir -replace '\\', '/'

if ($Mode -eq "server") {
    $config = @"
[general]
mode = "server"
log_level = "$logLevel"
data_dir = "$tomlData"

[server]
ipp_port = $IppPort
grpc_port = $GrpcPort
dashboard_port = $DashboardPort
printer_name = "$PrinterName"
spool_dir = "$tomlData/spool"

[server.tls]
cert_file = "$tomlData/certs/server.crt"
key_file = "$tomlData/certs/server.key"
ca_file = "$tomlData/certs/ca.crt"

[client]
server_address = "127.0.0.1:$GrpcPort"
target_printer = "unused"
dashboard_port = 9121
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[client.tls]
cert_file = ""
key_file = ""
ca_file = ""

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
"@
} else {
    $config = @"
[general]
mode = "client"
log_level = "$logLevel"
data_dir = "$tomlData"

[server]
ipp_port = $IppPort
grpc_port = $GrpcPort
dashboard_port = 9121
printer_name = "unused"
spool_dir = "$tomlData/spool"

[server.tls]
cert_file = ""
key_file = ""
ca_file = ""

[client]
server_address = "${ServerHost}:${GrpcPort}"
target_printer = "$TargetPrinter"
dashboard_port = $DashboardPort
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[client.tls]
cert_file = "$tomlData/certs/client.crt"
key_file = "$tomlData/certs/client.key"
ca_file = "$tomlData/certs/ca.crt"

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
"@
}

$config | Set-Content -Path $configPath -Encoding ASCII
Write-Host "  Config written to $configPath"

# ── Start DevBridge via Scheduled Task ─────────────────────────────────────
# Scheduled tasks run in a separate process tree, surviving GitHub Actions
# runner cleanup which kills all child processes when jobs end.
Write-Host "Registering DevBridge scheduled task..."
$action = New-ScheduledTaskAction -Execute $serviceExe -Argument "--config `"$configPath`"" -WorkingDirectory $dataDir
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue

# Try SYSTEM first, then verify it actually runs. If SYSTEM fails at runtime
# (e.g., error 15 = invalid drive on some machines), fall back to current user with S4U.
$registered = $false
try {
    $principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
    Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -Principal $principal -Trigger $trigger | Out-Null
    Start-ScheduledTask -TaskName $taskName
    Start-Sleep -Seconds 5
    $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    if ($proc) {
        $registered = $true
    } else {
        $lastResult = (Get-ScheduledTask -TaskName $taskName | Get-ScheduledTaskInfo).LastTaskResult
        Write-Host "  SYSTEM task failed at runtime (code $lastResult), switching to current user..." -ForegroundColor Yellow
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    }
} catch {
    Write-Host "  SYSTEM registration failed, trying current user fallback..." -ForegroundColor Yellow
}

if (-not $registered) {
    $currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
    $taskRegistered = $false
    # Try S4U (runs at startup whether logged in or not)
    try {
        $principal = New-ScheduledTaskPrincipal -UserId $currentUser -LogonType S4U -RunLevel Limited
        Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -Principal $principal -Trigger $trigger | Out-Null
        Write-Host "  Registered as $currentUser with S4U logon" -ForegroundColor Cyan
        $taskRegistered = $true
    } catch {
        Write-Host "  S4U registration failed, trying simple registration..." -ForegroundColor Yellow
    }
    # Fall back to simple registration (no principal)
    if (-not $taskRegistered) {
        try {
            $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
            Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -Trigger $trigger | Out-Null
            $taskRegistered = $true
        } catch {
            Write-Host "  All scheduled task registrations failed, starting process directly" -ForegroundColor Yellow
        }
    }
    if ($taskRegistered) {
        Start-ScheduledTask -TaskName $taskName
    } else {
        # Last resort: start process directly (no auto-restart, no AtStartup, but at least it runs)
        Start-Process -FilePath $serviceExe -ArgumentList "--config `"$configPath`"" -WindowStyle Hidden
    }
    Start-Sleep -Seconds 3
}

$proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($proc) {
    Write-Host "  Service is running (PID: $($proc.Id))" -ForegroundColor Green
} else {
    Write-Warning "Service process not found. Check logs at ${DataDir}\logs"
}

# ── Register IPP printer in Windows (server mode only) ────────────────────
# Printer registration is non-fatal: CI runners lack admin access to the
# Print Monitors registry and DriverStore. E2E tests use raw IPP, not Windows printers.
if ($Mode -eq "server") {
  try {
    Write-Host "Registering IPP printer in Windows..."
    $printerName = $PrinterName
    $ippUrl = "http://127.0.0.1:${IppPort}/ipp/print"

    # ── Step 1: Ensure Internet Port monitor (inetpp.dll) is registered ────
    # Windows Server 2019 may not have it registered even though the DLL exists.
    # Without this monitor, printui.dll silently fails to create IPP printers.
    $monitorPath = "HKLM:\SYSTEM\CurrentControlSet\Control\Print\Monitors\Internet Port"
    if (-not (Test-Path $monitorPath)) {
        if (Test-Path "$env:SystemRoot\System32\inetpp.dll") {
            try {
                New-Item -Path $monitorPath -Force | Out-Null
                Set-ItemProperty -Path $monitorPath -Name "Driver" -Value "inetpp.dll"
                Restart-Service Spooler -Force
                Start-Sleep 2
                Write-Host "  Registered Internet Port monitor (inetpp.dll)" -ForegroundColor Cyan
            } catch {
                Write-Host "  Skipping Internet Port monitor registration (no admin access)" -ForegroundColor Yellow
            }
        } else {
            Write-Host "  WARNING: inetpp.dll not found, IPP printer registration may fail" -ForegroundColor Yellow
        }
    }

    # ── Step 2: Repair broken Microsoft printer driver packages ────────────
    # After Windows Update, driver packages in DriverStore may point to old
    # hash directories that no longer exist. This causes printui.dll to
    # silently fail (event 368 in PrintService/Operational log).
    $driversToRepair = @(
        @{ Name = "Microsoft IPP Class Driver"; Inf = "prnms012" },
        @{ Name = "Microsoft Software Printer Driver"; Inf = "prnms011" }
    )
    foreach ($drv in $driversToRepair) {
        try {
            $existing = Get-PrinterDriver -Name $drv.Name -ErrorAction SilentlyContinue
            if ($existing -and $existing.InfPath -and -not (Test-Path $existing.InfPath)) {
                Write-Host "  Repairing broken driver '$($drv.Name)' (INF path missing)..." -ForegroundColor Yellow
                $correctInf = Get-ChildItem "$env:SystemRoot\System32\DriverStore\FileRepository\$($drv.Inf)*\$($drv.Inf).inf" -ErrorAction SilentlyContinue |
                    Sort-Object LastWriteTime -Descending | Select-Object -First 1
                if ($correctInf) {
                    Remove-PrinterDriver -Name $drv.Name -ErrorAction SilentlyContinue
                    pnputil /add-driver $correctInf.FullName /install 2>&1 | Out-Null
                    Add-PrinterDriver -Name $drv.Name -InfPath $correctInf.FullName -ErrorAction SilentlyContinue
                    Write-Host "  Repaired '$($drv.Name)' from $($correctInf.Name)" -ForegroundColor Green
                } else {
                    Write-Host "  WARNING: No valid INF found for $($drv.Inf)" -ForegroundColor Yellow
                }
            }
        } catch {
            Write-Host "  Skipping driver repair for '$($drv.Name)' (no admin access)" -ForegroundColor Yellow
        }
    }

    # ── Step 3: Clean existing IPP printers and ports ──────────────────────
    Get-Printer | Where-Object {
        $_.Name -eq $printerName -or $_.PortName -like "*$IppPort/ipp*"
    } | ForEach-Object {
        Get-PrintJob -PrinterName $_.Name -ErrorAction SilentlyContinue | Remove-PrintJob -ErrorAction SilentlyContinue
        Remove-Printer -Name $_.Name -ErrorAction SilentlyContinue
        Write-Host "  Removed printer '$($_.Name)'"
    }
    Get-PrinterPort | Where-Object {
        $_.Name -like "*$IppPort/ipp*" -or $_.Name -like "*$IppPort*localhost*" -or $_.Name -like "*$IppPort*127.0.0.1*"
    } | ForEach-Object {
        Remove-PrinterPort -Name $_.Name -ErrorAction SilentlyContinue
    }

    # ── Step 4: Wait for IPP server readiness ──────────────────────────────
    Write-Host "  Waiting for IPP server HTTP readiness on port $IppPort..."
    $ippReady = $false
    for ($i = 0; $i -lt 15; $i++) {
        try {
            $response = Invoke-WebRequest -Uri $ippUrl -Method POST `
                -ContentType "application/ipp" -Body ([byte[]](1,1,0,0x0b,0,0,0,1,3)) `
                -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
            if ($response.StatusCode -eq 200) {
                $ippReady = $true
                Write-Host "  IPP server is HTTP-ready (attempt $($i+1))"
                break
            }
        } catch {
            Start-Sleep -Seconds 1
        }
    }
    if (-not $ippReady) {
        Write-Host "  WARNING: IPP server not responding to HTTP on port $IppPort" -ForegroundColor Yellow
    }

    # ── Step 5: Register the printer via printui.dll ───────────────────────
    $printUiArgs = "/if /b `"$printerName`" /r `"$ippUrl`" /m `"Microsoft IPP Class Driver`" /q"
    Write-Host "  Running: rundll32 printui.dll,PrintUIEntry $printUiArgs"
    $proc = Start-Process -FilePath "rundll32.exe" `
        -ArgumentList "printui.dll,PrintUIEntry $printUiArgs" `
        -Wait -PassThru -NoNewWindow -ErrorAction SilentlyContinue
    if ($proc -and $proc.ExitCode -eq 0) {
        Write-Host "  Registered printer via printui.dll" -ForegroundColor Green
    } else {
        Write-Host "  printui.dll failed (exit code: $($proc.ExitCode))" -ForegroundColor Yellow
    }

    # ── Step 6: Verify registration ───────────────────────────────────────
    $verifyPrinter = Get-Printer -Name $printerName -ErrorAction SilentlyContinue
    if ($verifyPrinter) {
        Write-Host "  Verified: name='$printerName' port='$($verifyPrinter.PortName)' driver='$($verifyPrinter.DriverName)'" -ForegroundColor Green
    } else {
        Write-Host "  WARNING: Printer registration could not be verified" -ForegroundColor Yellow
    }
  } catch {
    Write-Host "  Printer registration skipped (insufficient permissions: $_)" -ForegroundColor Yellow
  }
}

# ── Tray app auto-start on login ────────────────────────────────────────────
if (Test-Path $trayExe) {
    # Try HKLM (all users, requires admin), fall back to HKCU (current user)
    try {
        $regPath = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
        Set-ItemProperty -Path $regPath -Name "DevBridge" -Value "`"$trayExe`""
        Write-Host "  Tray app registered for auto-start (all users)"
    } catch {
        $regPath = "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
        Set-ItemProperty -Path $regPath -Name "DevBridge" -Value "`"$trayExe`""
        Write-Host "  Tray app registered for auto-start (current user only)"
    }

    # Kill any existing tray app to avoid duplicate icons after upgrade
    Get-Process -Name "devbridge-app", "DevBridge" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep 1

    # Launch tray app in the logged-in user's desktop session.
    # CI/SYSTEM sessions can't show tray icons directly, so we use a
    # temporary scheduled task that runs interactively as the logged-in user.
    $loggedInUser = (Get-CimInstance -Class Win32_ComputerSystem).UserName
    if ($loggedInUser) {
        Write-Host "  Launching tray app for $loggedInUser..."
        $action = New-ScheduledTaskAction -Execute $trayExe
        $principal = New-ScheduledTaskPrincipal -UserId $loggedInUser -LogonType Interactive
        $task = New-ScheduledTask -Action $action -Principal $principal
        Register-ScheduledTask -TaskName "DevBridgeTrayStart" -InputObject $task -Force | Out-Null
        Start-ScheduledTask -TaskName "DevBridgeTrayStart"
        Start-Sleep 2
        Unregister-ScheduledTask -TaskName "DevBridgeTrayStart" -Confirm:$false -ErrorAction SilentlyContinue
    } else {
        Write-Host "  No logged-in user found, tray will start on next login"
    }
} else {
    Write-Host "  Tray app not found at $trayExe, skipping auto-start" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "=== Post-install complete ===" -ForegroundColor Green
Write-Host "  Mode:      $Mode"
Write-Host "  Dashboard: http://localhost:$DashboardPort"
Write-Host "  Data dir:  $DataDir"
$logsDir = Join-Path $DataDir "logs"
Write-Host "  Logs:      $logsDir"
