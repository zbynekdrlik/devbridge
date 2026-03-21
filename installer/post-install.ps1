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
    [string]$CertsSource = ""
)

$ErrorActionPreference = "Stop"
$serviceName = "DevBridge"
# Tauri installs sidecar binaries with target-triple suffix
$serviceExe = Join-Path $InstallDir "devbridge-service-x86_64-pc-windows-msvc.exe"
if (-not (Test-Path $serviceExe)) {
    # Fallback to plain name (manual install or future change)
    $serviceExe = Join-Path $InstallDir "devbridge-service.exe"
}
$trayExe = Join-Path $InstallDir "DevBridge.exe"

Write-Host "=== DevBridge Post-Install ($Mode mode) ===" -ForegroundColor Cyan

# ── Stop existing service if upgrading ──────────────────────────────────────
$existingService = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($existingService -and $existingService.Status -eq "Running") {
    Write-Host "Stopping existing service for upgrade..."
    Stop-Service -Name $serviceName -Force
    Start-Sleep -Seconds 3
}

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

# ── Write configuration ────────────────────────────────────────────────────
$configPath = Join-Path $DataDir "config.toml"
# Use debug logging in CI for easier troubleshooting
$logLevel = if ($env:CI) { "debug" } else { "info" }
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
printer_name = "DevBridge"
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

# ── Register Windows service ───────────────────────────────────────────────
$binPath = "`"$serviceExe`" --config `"$configPath`" --service"

if ($existingService) {
    Write-Host "Updating existing service registration..."
    & sc.exe config $serviceName binPath= $binPath start= auto | Out-Null
} else {
    Write-Host "Registering Windows service..."
    & sc.exe create $serviceName binPath= $binPath start= auto DisplayName= "DevBridge Print Bridge" | Out-Null
}

# Set service description
& sc.exe description $serviceName "DevBridge print bridge service - forwards print jobs between server and client" | Out-Null

# ── Start the service ───────────────────────────────────────────────────────
Write-Host "Starting service..."
Start-Service -Name $serviceName
Start-Sleep -Seconds 2

$svc = Get-Service -Name $serviceName
if ($svc.Status -eq "Running") {
    Write-Host "  Service is running" -ForegroundColor Green
} else {
    Write-Warning "Service status: $($svc.Status). Check logs at $DataDir\logs\"
}

# ── Tray app auto-start on login ────────────────────────────────────────────
$regPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$regName = "DevBridge"

if (Test-Path $trayExe) {
    Set-ItemProperty -Path $regPath -Name $regName -Value "`"$trayExe`""
    Write-Host "  Tray app registered for auto-start"

    # Launch tray app if not already running
    $trayProc = Get-Process -Name "DevBridge" -ErrorAction SilentlyContinue
    if (-not $trayProc) {
        Write-Host "  Launching tray app..."
        Start-Process -FilePath $trayExe -WindowStyle Normal
    }
} else {
    Write-Host "  Tray app not found at $trayExe, skipping auto-start" -ForegroundColor Yellow
}

Write-Host "`n=== Post-install complete ($Mode mode) ===" -ForegroundColor Green
Write-Host "  Dashboard: http://localhost:$DashboardPort"
Write-Host "  Data dir:  $DataDir"
Write-Host "  Logs:      $DataDir\logs\"
