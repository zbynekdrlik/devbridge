# E2E Setup: Deploy and start DevBridge server on print-server.lan
param(
    [string]$BinaryPath = "artifacts\devbridge-service.exe",
    [string]$InstallDir = "C:\DevBridge",
    [string]$CertsDir = "$env:TEMP\devbridge-certs",
    [int]$IppPort = 631,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120
)

$ErrorActionPreference = "Stop"

Write-Host "=== E2E Server Setup ===" -ForegroundColor Cyan

# Stop existing service if running and release ports
$existing = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "Stopping existing devbridge-service..."
    Stop-Process -Name "devbridge-service" -Force
    Start-Sleep -Seconds 3
}
# Ensure ports are released
$portsInUse = netstat -an | Select-String ":631 |:50051 |:9120 " | Select-String "LISTENING"
if ($portsInUse) {
    Write-Host "Warning: ports still in use, waiting..."
    Start-Sleep -Seconds 5
}

# Create install directory
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path "$InstallDir\spool" | Out-Null
New-Item -ItemType Directory -Force -Path "$InstallDir\certs" | Out-Null

# Copy binary
Copy-Item $BinaryPath "$InstallDir\devbridge-service.exe" -Force

# Copy TLS certificates
if (Test-Path $CertsDir) {
    Copy-Item "$CertsDir\*" "$InstallDir\certs\" -Force
}

# Write server config (use forward slashes to avoid TOML escaping issues)
$tomlDir = $InstallDir -replace '\\', '/'
$config = @"
[general]
mode = "server"
log_level = "debug"
data_dir = "$tomlDir"

[server]
ipp_port = $IppPort
grpc_port = $GrpcPort
dashboard_port = $DashboardPort
printer_name = "DevBridge"
spool_dir = "$tomlDir/spool"

[server.tls]
cert_file = "$tomlDir/certs/server.crt"
key_file = "$tomlDir/certs/server.key"
ca_file = "$tomlDir/certs/ca.crt"

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
$config | Out-File -FilePath "$InstallDir\config.toml" -Encoding utf8

# Skip Windows printer registration for E2E — the DevBridge IPP server
# runs its own HTTP-based IPP endpoint. Jobs are submitted directly via HTTP POST.
Write-Host "Skipping Windows printer registration (IPP server handles it)"

# Start server in background
Write-Host "Starting devbridge-service in server mode..."
Start-Process -FilePath "$InstallDir\devbridge-service.exe" `
    -ArgumentList "--config", "$InstallDir\config.toml" `
    -WindowStyle Hidden

Write-Host "Server setup complete." -ForegroundColor Green
