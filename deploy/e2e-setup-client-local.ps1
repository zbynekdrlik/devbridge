# E2E Setup: Install DevBridge client via NSIS installer on this machine
param(
    [string]$InstallerGlob = "artifacts\DevBridge_*_x64-setup.exe",
    [string]$ServerHost = "print-server.lan",
    [string]$TargetPrinter = $env:E2E_TARGET_PRINTER,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120
)

$ErrorActionPreference = "Stop"

if (-not $TargetPrinter) { $TargetPrinter = "Microsoft Print to PDF" }

Write-Host "=== E2E Client Setup (NSIS Installer) ===" -ForegroundColor Cyan
Write-Host "Target printer: $TargetPrinter"
Write-Host "Server: ${ServerHost}:${GrpcPort}"

# ── Stop existing service if running (upgrade path) ─────────────────
$svc = Get-Service -Name "DevBridge" -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -eq "Running") {
    Write-Host "Stopping existing DevBridge service..."
    Stop-Service -Name "DevBridge" -Force
    Start-Sleep -Seconds 3
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

Write-Host "Running installer: $($installer.Name)"
$proc = Start-Process -FilePath $installer.FullName -ArgumentList "/S" -Wait -PassThru
if ($proc.ExitCode -ne 0) {
    throw "Installer exited with code $($proc.ExitCode)"
}
Write-Host "  Installer completed successfully" -ForegroundColor Green

# ── Verify installation ────────────────────────────────────────────
$installDir = "C:\Program Files\DevBridge"
# Tauri installs sidecar with target-triple suffix
$svcBin = "$installDir\devbridge-service-x86_64-pc-windows-msvc.exe"
if (-not (Test-Path $svcBin)) { $svcBin = "$installDir\devbridge-service.exe" }
if (-not (Test-Path $svcBin)) {
    Write-Host "Install dir contents:" -ForegroundColor Yellow
    Get-ChildItem $installDir -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $($_.Name)" }
    throw "Service binary not found in $installDir after install"
}

# ── Run post-install script ─────────────────────────────────────────
$postInstall = Join-Path $PSScriptRoot "..\installer\post-install.ps1"
if (-not (Test-Path $postInstall)) {
    $postInstall = "$installDir\post-install.ps1"
}

Write-Host "Running post-install configuration..."
& $postInstall -Mode client -InstallDir $installDir `
    -ServerHost $ServerHost -TargetPrinter $TargetPrinter `
    -GrpcPort $GrpcPort -DashboardPort $DashboardPort

# ── Configure headless PDF printing ─────────────────────────────────
if ($TargetPrinter -eq "Microsoft Print to PDF") {
    $outPath = "C:\ProgramData\DevBridge\e2e-output.pdf"
    Write-Host "Configuring PDF printer for headless output to $outPath"
    Add-PrinterPort -Name $outPath -ErrorAction SilentlyContinue
    Set-Printer -Name "Microsoft Print to PDF" -PortName $outPath
}

Write-Host "Client setup complete." -ForegroundColor Green

# ── Keep job alive until E2E test completes ──────────────────────────
# The service runs as a Windows service now, but we keep the CI job alive
# so the runner stays available for the duration of the E2E test.
$signalFile = "C:\ProgramData\DevBridge\e2e-done"
$timeout = 600
$start = Get-Date
Write-Host "Keeping client job alive until E2E test completes (max ${timeout}s)..."
while (((Get-Date) - $start).TotalSeconds -lt $timeout) {
    if (Test-Path $signalFile) {
        Write-Host "E2E test completed signal received."
        Remove-Item $signalFile -ErrorAction SilentlyContinue
        break
    }
    # Verify service is still running
    $svc = Get-Service -Name "DevBridge" -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -ne "Running") {
        Write-Warning "Service stopped unexpectedly, restarting..."
        Start-Service -Name "DevBridge" -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 5
}
Write-Host "Client deploy job ending."
