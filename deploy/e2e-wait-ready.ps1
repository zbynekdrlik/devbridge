# E2E: Wait for both server and client to be ready
param(
    [string]$ServerHost = "localhost",
    [string]$ClientHost = "print-client.lan",
    [int]$DashboardPort = 9120,
    [int]$TimeoutSecs = 300,
    [int]$IntervalSecs = 10
)

$ErrorActionPreference = "Stop"

Write-Host "=== Waiting for services ===" -ForegroundColor Cyan

function Wait-ForEndpoint {
    param([string]$Url, [string]$Name, [int]$Timeout, [int]$Interval)

    $start = Get-Date
    while (((Get-Date) - $start).TotalSeconds -lt $Timeout) {
        try {
            $response = Invoke-RestMethod -Uri $Url -TimeoutSec 5
            if ($response.status -eq "running") {
                Write-Host "$Name is ready (mode: $($response.mode))" -ForegroundColor Green
                return
            }
        }
        catch {
            Write-Host "$Name not ready yet, retrying in ${Interval}s..."
        }
        Start-Sleep -Seconds $Interval
    }

    throw "$Name did not become ready within ${Timeout}s"
}

Wait-ForEndpoint -Url "http://${ServerHost}:${DashboardPort}/api/status" -Name "Server" -Timeout $TimeoutSecs -Interval $IntervalSecs
Wait-ForEndpoint -Url "http://${ClientHost}:${DashboardPort}/api/status" -Name "Client" -Timeout $TimeoutSecs -Interval $IntervalSecs

# Verify gRPC port is listening on server
$grpcReady = $false
$start = Get-Date
while (((Get-Date) - $start).TotalSeconds -lt 30) {
    $conn = Test-NetConnection -ComputerName $ServerHost -Port 50051 -InformationLevel Quiet -WarningAction SilentlyContinue
    if ($conn) { $grpcReady = $true; break }
    Write-Host "gRPC port 50051 not ready yet..."
    Start-Sleep -Seconds 2
}
if (-not $grpcReady) { throw "gRPC server port 50051 not listening" }
Write-Host "gRPC server port 50051 is ready" -ForegroundColor Green

Write-Host "Both services are ready." -ForegroundColor Green
