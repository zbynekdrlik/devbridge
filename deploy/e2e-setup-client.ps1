# E2E Setup: Deploy and start DevBridge client on print-client.lan
param(
    [string]$BinaryPath = "artifacts\devbridge-service.exe",
    [string]$ClientHost = "print-client.lan",
    [string]$ServerHost = "print-server.lan",
    [string]$InstallDir = "C:\DevBridge",
    [string]$CertsDir = "$env:TEMP\devbridge-certs",
    [string]$TargetPrinter = $env:E2E_TARGET_PRINTER,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120
)

$ErrorActionPreference = "Stop"

if (-not $TargetPrinter) { $TargetPrinter = "Microsoft Print to PDF" }

Write-Host "=== E2E Client Setup ===" -ForegroundColor Cyan
Write-Host "Client: $ClientHost"
Write-Host "Target printer: $TargetPrinter"

# Create remote session
$session = New-PSSession -ComputerName $ClientHost

try {
    # Stop existing service on client
    Invoke-Command -Session $session -ScriptBlock {
        $existing = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
        if ($existing) {
            Write-Host "Stopping existing devbridge-service on client..."
            Stop-Process -Name "devbridge-service" -Force
            Start-Sleep -Seconds 2
        }

        # Create install directory
        New-Item -ItemType Directory -Force -Path $using:InstallDir | Out-Null
        New-Item -ItemType Directory -Force -Path "$using:InstallDir\spool" | Out-Null
        New-Item -ItemType Directory -Force -Path "$using:InstallDir\certs" | Out-Null
    }

    # Copy binary to client
    Copy-Item $BinaryPath "$InstallDir\devbridge-service.exe" -ToSession $session -Force

    # Copy TLS certificates
    if (Test-Path $CertsDir) {
        Copy-Item "$CertsDir\*" "$InstallDir\certs\" -ToSession $session -Force
    }

    # Write client config and start service
    Invoke-Command -Session $session -ScriptBlock {
        param($InstallDir, $ServerHost, $GrpcPort, $DashboardPort, $TargetPrinter)

        $config = @"
[general]
mode = "client"
log_level = "debug"
data_dir = "$($InstallDir -replace '\\', '\\\\')"

[server]
ipp_port = 631
grpc_port = $GrpcPort
dashboard_port = 9121
printer_name = "unused"
spool_dir = "$($InstallDir -replace '\\', '\\\\')\\\\spool"

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
cert_file = "$($InstallDir -replace '\\', '\\\\')\\\\certs\\\\client.crt"
key_file = "$($InstallDir -replace '\\', '\\\\')\\\\certs\\\\client.key"
ca_file = "$($InstallDir -replace '\\', '\\\\')\\\\certs\\\\ca.crt"

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
"@
        $config | Out-File -FilePath "$InstallDir\config.toml" -Encoding utf8

        # Start client in background
        Write-Host "Starting devbridge-service in client mode..."
        Start-Process -FilePath "$InstallDir\devbridge-service.exe" `
            -ArgumentList "--config", "$InstallDir\config.toml" `
            -WindowStyle Hidden
    } -ArgumentList $InstallDir, $ServerHost, $GrpcPort, $DashboardPort, $TargetPrinter

    Write-Host "Client setup complete." -ForegroundColor Green
}
finally {
    Remove-PSSession $session
}
