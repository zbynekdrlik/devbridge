# E2E Setup: Deploy and start DevBridge client locally on this machine
param(
    [string]$BinaryPath = "artifacts\devbridge-service.exe",
    [string]$InstallDir = "C:\DevBridge",
    [string]$ServerHost = "print-server.lan",
    [string]$TargetPrinter = $env:E2E_TARGET_PRINTER,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120
)

$ErrorActionPreference = "Stop"

if (-not $TargetPrinter) { $TargetPrinter = "Microsoft Print to PDF" }

Write-Host "=== E2E Client Setup (local) ===" -ForegroundColor Cyan
Write-Host "Target printer: $TargetPrinter"
Write-Host "Server: ${ServerHost}:${GrpcPort}"

# Stop existing service if running
$existing = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "Stopping existing devbridge-service..."
    Stop-Process -Name "devbridge-service" -Force
    Start-Sleep -Seconds 2
}

# Create install directory
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path "$InstallDir\spool" | Out-Null
New-Item -ItemType Directory -Force -Path "$InstallDir\certs" | Out-Null

# Copy binary
Copy-Item $BinaryPath "$InstallDir\devbridge-service.exe" -Force

# Write client config
$escapedInstallDir = $InstallDir -replace '\\', '\\\\'
$config = @"
[general]
mode = "client"
log_level = "debug"
data_dir = "$escapedInstallDir"

[server]
ipp_port = 631
grpc_port = $GrpcPort
dashboard_port = 9121
printer_name = "unused"
spool_dir = "$escapedInstallDir\\\\spool"

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

# Register and start as Windows service so it survives GitHub Actions job cleanup
$svcName = "DevBridgeE2E"
$existing = Get-Service -Name $svcName -ErrorAction SilentlyContinue
if ($existing) {
    Stop-Service -Name $svcName -Force -ErrorAction SilentlyContinue
    sc.exe delete $svcName | Out-Null
    Start-Sleep -Seconds 2
}

Write-Host "Creating Windows service $svcName..."
$binPath = "`"$InstallDir\devbridge-service.exe`" --config `"$InstallDir\config.toml`""
sc.exe create $svcName binPath= $binPath start= demand | Out-Null
Start-Service -Name $svcName

Write-Host "Client setup complete." -ForegroundColor Green
