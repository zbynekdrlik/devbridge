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
    [string]$ClientId = "",
    [string]$VirtualPrinterName = "",
    [string]$PrinterDisplayName = "",
    [string]$PrintBackend = "",
    [string]$PrinterAddress = "",
    [switch]$PrinterTls,
    [string]$GhostscriptDevice = "",
    [int]$GhostscriptResolution = 0
)

$ErrorActionPreference = "Stop"
$serviceExe = Join-Path $InstallDir "devbridge-service.exe"
$trayExe = Join-Path $InstallDir "devbridge-app.exe"
if (-not (Test-Path $trayExe)) {
    $trayExe = Join-Path $InstallDir "DevBridge.exe"
}

Write-Host "=== DevBridge Post-Install - $Mode mode ===" -ForegroundColor Cyan

# ── Validate configuration BEFORE any destructive action ─────────────────
# Runs before the Stop-Service block so a failed install is a no-op:
# if validation fails, the existing service is left running untouched.
# See docs/superpowers/specs/2026-04-10-installer-hardening-design.md

if ($Mode -eq "client") {
    $effectiveBackend = if ($PrintBackend) { $PrintBackend } else { "windows_spooler" }

    # 1. direct_ipp port auto-append (closes #16)
    # IPP default port is 631 per RFC 8011 §5.
    if ($effectiveBackend -eq "direct_ipp" -and $PrinterAddress -and
        ($PrinterAddress -notmatch ':') -and ($PrinterAddress -notmatch '/')) {
        $corrected = "${PrinterAddress}:631"
        Write-Warning "printer_address auto-corrected to $corrected (default IPP port per RFC 8011)"
        $PrinterAddress = $corrected
    }

    # 2. windows_spooler printer name validation (closes #17)
    if ($effectiveBackend -eq "windows_spooler" -or $effectiveBackend -eq "") {
        $installedPrinters = @(Get-Printer -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name)
        if ($installedPrinters.Count -eq 0) {
            Write-Host ""
            Write-Host "ERROR: No printers installed on this machine." -ForegroundColor Red
            Write-Host "  Install the printer driver before configuring DevBridge." -ForegroundColor Red
            Write-Host "  Suggestion: open Settings -> Bluetooth & devices -> Printers & scanners," -ForegroundColor Yellow
            Write-Host "              add the printer, then re-run the installer." -ForegroundColor Yellow
            [Console]::Error.WriteLine("ERROR: No printers installed on this machine (target_printer=`"$TargetPrinter`")")
            exit 1
        }
        $exactMatch = $installedPrinters | Where-Object { $_ -ieq $TargetPrinter }
        if (-not $exactMatch) {
            Write-Host ""
            Write-Host "ERROR: target_printer `"$TargetPrinter`" not found on this machine." -ForegroundColor Red
            Write-Host "  Available printers:" -ForegroundColor Red
            foreach ($p in $installedPrinters) {
                Write-Host "    - $p" -ForegroundColor Red
            }
            # Only suggest a printer if there's a real substring overlap in
            # either direction; otherwise we risk pointing at an arbitrary
            # printer and confusing the operator.
            $suggestion = $installedPrinters |
                Where-Object { $_ -like "*$TargetPrinter*" -or $TargetPrinter -like "*$_*" } |
                Select-Object -First 1
            if ($suggestion) {
                Write-Host "  Suggestion: re-run installer with " -NoNewline -ForegroundColor Yellow
                Write-Host "`$env:DEVBRIDGE_TARGET_PRINTER = `"$suggestion`"" -ForegroundColor Yellow
            } else {
                Write-Host "  Suggestion: pick one of the names above and re-run with " -NoNewline -ForegroundColor Yellow
                Write-Host "`$env:DEVBRIDGE_TARGET_PRINTER = `"<name>`"" -ForegroundColor Yellow
            }
            [Console]::Error.WriteLine("ERROR: target_printer `"$TargetPrinter`" not found on this machine")
            exit 1
        }
        Write-Host "  Validated target_printer: $TargetPrinter" -ForegroundColor Green
    }

    # 3. gRPC connectivity test (closes #21 main scope)
    Write-Host "  Probing gRPC server at ${ServerHost}:${GrpcPort}..."
    $tcp = Test-NetConnection -ComputerName $ServerHost -Port $GrpcPort `
        -InformationLevel Quiet -WarningAction SilentlyContinue
    if (-not $tcp) {
        Write-Host ""
        Write-Host "ERROR: gRPC server unreachable at ${ServerHost}:${GrpcPort}." -ForegroundColor Red
        Write-Host "  TCP connection timed out." -ForegroundColor Red
        Write-Host "  Suggestion: verify VPN is connected (e.g. wg show), and that the" -ForegroundColor Yellow
        Write-Host "              DevBridge service is running on the server." -ForegroundColor Yellow
        [Console]::Error.WriteLine("ERROR: gRPC server unreachable at ${ServerHost}:${GrpcPort}")
        exit 1
    }
    Write-Host "  gRPC server reachable" -ForegroundColor Green
}

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
# Ghostscript portable (for direct print backends)
$gsTarget = Join-Path $InstallDir "ghostscript"
if (-not (Test-Path (Join-Path $gsTarget "bin\gswin64c.exe"))) {
    # Search multiple locations: direct redist/ and Tauri _up_/_up_/ resource paths
    $gsCandidates = @(
        (Join-Path $InstallDir "redist\ghostscript"),
        (Join-Path $InstallDir "_up_\_up_\installer\redist\ghostscript")
    )
    $gsBundled = $gsCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($gsBundled) {
        Write-Host "Installing Ghostscript portable from $gsBundled..."
        Copy-Item -Recurse -Force $gsBundled $gsTarget
        Write-Host "  Ghostscript installed to $gsTarget" -ForegroundColor Green
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

[client]
server_address = "127.0.0.1:$GrpcPort"
target_printer = "unused"
dashboard_port = 9121
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

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

[client]
server_address = "${ServerHost}:${GrpcPort}"
target_printer = "$TargetPrinter"
dashboard_port = $DashboardPort
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60
$(if ($ClientId) { "client_id = `"$ClientId`"" })
$(if ($PrinterDisplayName) { "printer_display_name = `"$PrinterDisplayName`"" })
$(if ($PrintBackend) { "print_backend = `"$PrintBackend`"" })
$(if ($PrinterAddress) { "printer_address = `"$PrinterAddress`"" })
$(if ($PrinterTls) { "printer_tls = true" })
$(if ($GhostscriptDevice) { "ghostscript_device = `"$GhostscriptDevice`"" })
$(if ($GhostscriptResolution -gt 0) { "ghostscript_resolution = $GhostscriptResolution" })
$(if ($VirtualPrinterName) { "virtual_printer_name = `"$VirtualPrinterName`"" })

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
# Use a VBS wrapper to run the service hidden (no console window on desktop)
$vbsPath = Join-Path $installDir "start-hidden.vbs"
$vbsContent = @"
Set WshShell = CreateObject("WScript.Shell")
WshShell.Run """$serviceExe"" --config """"$configPath"""""", 0, False
"@
$vbsContent | Set-Content -Path $vbsPath -Encoding ASCII
# Register as SYSTEM scheduled task — runs devbridge-service.exe directly (no wscript/VBS
# wrapper). SYSTEM processes are sessionless so no window-hiding needed. This avoids the
# S4U logon error (0x80070520 / code 267009) that occurs when the domain controller is
# unreachable at boot time. See issue #36.
$action = New-ScheduledTaskAction -Execute $serviceExe -Argument "--config `"$configPath`"" -WorkingDirectory $dataDir
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
$settings.IdleSettings.StopOnIdleEnd = $false
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
try {
    Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -Principal $principal -Trigger $trigger | Out-Null
    Start-ScheduledTask -TaskName $taskName
    Start-Sleep -Seconds 5
    $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    if ($proc) {
        Write-Host "  Service started as SYSTEM (PID $($proc.Id))" -ForegroundColor Green
    } else {
        Write-Host "WARNING: Service process not found. Check logs at $dataDir\logs" -ForegroundColor Yellow
    }
} catch {
    Write-Host "  Scheduled task registration failed, starting process directly" -ForegroundColor Yellow
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

    # ── Step 3: Wait for dashboard API readiness ─────────────────────────
    Write-Host "  Waiting for DevBridge API readiness..."
    $apiReady = $false
    $dashUrl = "http://127.0.0.1:${DashboardPort}/api/virtual-printers"
    for ($i = 0; $i -lt 15; $i++) {
        try {
            $response = Invoke-WebRequest -Uri $dashUrl -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
            if ($response.StatusCode -eq 200) {
                $apiReady = $true
                Write-Host "  API ready (attempt $($i+1))"
                break
            }
        } catch {
            Start-Sleep -Seconds 1
        }
    }
    if (-not $apiReady) {
        Write-Host "  WARNING: DevBridge API not responding, using legacy single printer" -ForegroundColor Yellow
    }

    # ── Step 4: Get virtual printers from API ─────────────────────────────
    $virtualPrinters = @()
    if ($apiReady) {
        try {
            $vpResponse = Invoke-WebRequest -Uri $dashUrl -UseBasicParsing
            $virtualPrinters = $vpResponse.Content | ConvertFrom-Json
            Write-Host "  Found $($virtualPrinters.Count) virtual printer(s)"
        } catch {
            Write-Host "  WARNING: Failed to fetch virtual printers" -ForegroundColor Yellow
        }
    }
    # Fallback: register single printer with legacy path
    if ($virtualPrinters.Count -eq 0) {
        $virtualPrinters = @([PSCustomObject]@{
            display_name = $printerName
            ipp_name = "default"
        })
    }

    # ── Step 5: Clean existing DevBridge IPP printers ─────────────────────
    Get-Printer | Where-Object {
        $_.PortName -like "*127.0.0.1:${IppPort}*"
    } | ForEach-Object {
        Get-PrintJob -PrinterName $_.Name -ErrorAction SilentlyContinue | Remove-PrintJob -ErrorAction SilentlyContinue
        Remove-Printer -Name $_.Name -ErrorAction SilentlyContinue
        Write-Host "  Removed old printer '$($_.Name)'"
    }

    # ── Step 6: Register each virtual printer ─────────────────────────────
    # IMPORTANT: Must use rundll32 printui.dll, NOT Add-Printer cmdlet.
    # Add-Printer creates ports under "Standard TCP/IP Port" monitor (RAW TCP 9100)
    # which cannot do IPP. rundll32 printui.dll creates ports under "Internet Port"
    # monitor (inetpp.dll) which does proper IPP over HTTP.
    foreach ($vp in $virtualPrinters) {
        $vpName = $vp.display_name
        $vpIppName = $vp.ipp_name
        if ($vpIppName -eq "default") {
            $vpUrl = "http://127.0.0.1:${IppPort}/ipp/print"
        } else {
            $vpUrl = "http://127.0.0.1:${IppPort}/printers/${vpIppName}"
        }

        Write-Host "  Registering '$vpName' -> $vpUrl"
        $printUiArgs = "/if /b `"$vpName`" /r `"$vpUrl`" /m `"Microsoft IPP Class Driver`" /q"
        $proc = Start-Process -FilePath "rundll32.exe" `
            -ArgumentList "printui.dll,PrintUIEntry $printUiArgs" `
            -Wait -PassThru -NoNewWindow -ErrorAction SilentlyContinue
        if ($proc -and $proc.ExitCode -eq 0) {
            Write-Host "  Registered '$vpName'" -ForegroundColor Green
        } else {
            Write-Host "  printui.dll failed for '$vpName' (exit: $($proc.ExitCode))" -ForegroundColor Yellow
        }
    }

    # ── Step 7: Verify registration ───────────────────────────────────────
    foreach ($vp in $virtualPrinters) {
        $verifyPrinter = Get-Printer -Name $vp.display_name -ErrorAction SilentlyContinue
        if ($verifyPrinter) {
            Write-Host "  Verified: '$($vp.display_name)' port='$($verifyPrinter.PortName)'" -ForegroundColor Green
        } else {
            Write-Host "  WARNING: '$($vp.display_name)' not verified" -ForegroundColor Yellow
        }
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

    # Launch tray app in EVERY user session — Active AND Disconnected.
    # Each user gets their own tray instance which filters jobs by username.
    # Disconnected sessions are included so the tray icon is already running
    # when the user reconnects via RDP (HKLM:\Run only fires on fresh logon,
    # not on RDP reconnect to an existing disconnected session).
    # CI/SYSTEM sessions can't show tray icons directly, so we use temporary
    # scheduled tasks that run interactively as each user.
    #
    # `query user` output format (USERNAME is first column):
    #   >drlikzbynek           rdp-tcp#19         60  Active
    #    marketing                                22  Disc
    # Note: `query user` always returns exit code 1 on Windows even when it
    # succeeds, so we explicitly clear $LASTEXITCODE afterwards.
    $sessions = query user 2>$null | Select-Object -Skip 1 | ForEach-Object {
        $line = $_
        if ($line -match '^>?\s*(\S+)\s+.*?\s+(\d+)\s+(Active|Disc)') {
            [PSCustomObject]@{
                Username  = $matches[1]
                SessionId = [int]$matches[2]
                State     = $matches[3]
            }
        }
    } | Where-Object { $_ }
    $global:LASTEXITCODE = 0

    if ($sessions -and $sessions.Count -gt 0) {
        Write-Host "  Launching tray app for $($sessions.Count) active session(s)..."
        foreach ($s in $sessions) {
            $taskName = "DevBridgeTrayStart_$($s.Username)"
            try {
                $action = New-ScheduledTaskAction -Execute $trayExe
                $principal = New-ScheduledTaskPrincipal -UserId $s.Username -LogonType Interactive
                $task = New-ScheduledTask -Action $action -Principal $principal
                Register-ScheduledTask -TaskName $taskName -InputObject $task -Force | Out-Null
                Start-ScheduledTask -TaskName $taskName
                Start-Sleep -Milliseconds 500
                Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
                Write-Host "    [OK] $($s.Username) (session $($s.SessionId))"
            } catch {
                Write-Host "    [FAIL] $($s.Username): $_" -ForegroundColor Yellow
                Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
            }
        }
    } else {
        Write-Host "  No active sessions, tray will start on next login via HKLM:\Run"
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
