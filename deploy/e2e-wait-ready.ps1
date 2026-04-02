# E2E: Wait for both server and client to be ready
param(
    [string]$ServerHost = "localhost",
    [string]$ClientHost = "10.78.2.10",
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

# Verify devbridge-service process is running
$proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($proc) {
    Write-Host "devbridge-service process is running (PID: $($proc.Id))" -ForegroundColor Green
} else {
    Write-Warning "devbridge-service process not detected on server"
}

# Auto-approve all pending clients (pairing gate blocks job delivery)
# E2E Deploy Client runs in parallel, so e2e-client may not have connected yet.
# Loop until e2e-client is either approved or we approve it ourselves.
Write-Host "Waiting for E2E client and approving all pending clients..."
$approveStart = Get-Date
$e2eApproved = $false
while (((Get-Date) - $approveStart).TotalSeconds -lt 120) {
    try {
        $clients = Invoke-RestMethod -Uri "http://${ServerHost}:${DashboardPort}/api/clients" -TimeoutSec 5
        # Approve every pending client
        foreach ($client in $clients) {
            if ($client.pairing_state -eq "pending") {
                $cid = $client.machine_id
                Write-Host "  Approving client: $cid"
                Invoke-RestMethod -Uri "http://${ServerHost}:${DashboardPort}/api/clients/$cid/approve" -Method Post -TimeoutSec 5 | Out-Null
            }
        }
        # Check if e2e-client exists and is approved
        $e2e = $clients | Where-Object { $_.machine_id -eq "e2e-client" }
        if ($e2e -and $e2e.pairing_state -ne "pending") {
            $e2eApproved = $true
            break
        }
        Write-Host "  e2e-client not connected yet, waiting 5s..."
        Start-Sleep -Seconds 5
    } catch {
        Write-Host "  API error, retrying in 5s..."
        Start-Sleep -Seconds 5
    }
}
if ($e2eApproved) {
    Write-Host "  e2e-client approved" -ForegroundColor Green
} else {
    Write-Warning "  e2e-client not found after 120s - E2E tests may fail"
}

Write-Host "Both services are ready." -ForegroundColor Green
