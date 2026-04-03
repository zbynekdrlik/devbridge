# DevBridge one-liner installer
# Usage:
#   irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
#   $env:DEVBRIDGE_VERSION="dev"; irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex

$ErrorActionPreference = "Stop"
$repo = "zbynekdrlik/devbridge"
$serviceName = "DevBridge"
$requestedVersion = if ($env:DEVBRIDGE_VERSION) { $env:DEVBRIDGE_VERSION } else { "latest" }

Write-Host "==> DevBridge Installer" -ForegroundColor Cyan

# --- Detect release ---
$ghHeaders = @{ "User-Agent" = "DevBridge-Installer" }
if ($requestedVersion -eq "latest") {
    Write-Host "Fetching latest stable release..."
    $releaseUrl = "https://api.github.com/repos/$repo/releases/latest"
} elseif ($requestedVersion -eq "dev") {
    Write-Host "Fetching dev release..."
    $releaseUrl = "https://api.github.com/repos/$repo/releases/tags/dev-latest"
} else {
    Write-Host "Fetching release $requestedVersion..."
    $releaseUrl = "https://api.github.com/repos/$repo/releases/tags/$requestedVersion"
}

try {
    $release = Invoke-RestMethod -Uri $releaseUrl -Headers $ghHeaders
} catch {
    Write-Error "Failed to fetch release '$requestedVersion' from GitHub. Check your internet connection and version."
    exit 1
}

$version = $release.tag_name
Write-Host "Version: $version"

# --- Find installer asset (prefer NSIS setup .exe) ---
$installerAsset = $release.assets | Where-Object { $_.name -match "setup.*\.exe$" } | Select-Object -First 1
if (-not $installerAsset) {
    $installerAsset = $release.assets | Where-Object { $_.name -match "DevBridge.*\.exe$" } | Select-Object -First 1
}
if (-not $installerAsset) {
    Write-Error "No installer .exe found in release $version"
    exit 1
}

$checksumAsset = $release.assets | Where-Object { $_.name -match "SHA256SUMS" } | Select-Object -First 1

$downloadUrl = $installerAsset.browser_download_url
$fileName = $installerAsset.name
$tempDir = Join-Path $env:TEMP "devbridge-install"
$installerPath = Join-Path $tempDir $fileName

# --- Download ---
if (-not (Test-Path $tempDir)) {
    New-Item -ItemType Directory -Path $tempDir | Out-Null
}

Write-Host "Downloading $fileName..."
Invoke-WebRequest -Uri $downloadUrl -OutFile $installerPath -UseBasicParsing

# --- Verify checksum ---
if ($checksumAsset) {
    $checksumUrl = $checksumAsset.browser_download_url
    $checksumFile = Join-Path $tempDir "SHA256SUMS"
    Invoke-WebRequest -Uri $checksumUrl -OutFile $checksumFile -UseBasicParsing

    $expectedHash = (Get-Content $checksumFile | Where-Object { $_ -match $fileName }) -replace "\s+.*$", ""
    $actualHash = (Get-FileHash -Path $installerPath -Algorithm SHA256).Hash

    if ($expectedHash -and ($actualHash -ne $expectedHash)) {
        Write-Error "Checksum verification failed!"
        Write-Error "Expected: $expectedHash"
        Write-Error "Actual:   $actualHash"
        Remove-Item -Recurse -Force $tempDir
        exit 1
    }
    Write-Host "Checksum verified." -ForegroundColor Green
} else {
    Write-Warning "No SHA256SUMS file found in release; skipping checksum verification."
}

# --- Run installer ---
Write-Host "Running installer (silent mode)..."
$process = Start-Process -FilePath $installerPath -ArgumentList "/S" -Wait -PassThru
if ($process.ExitCode -ne 0) {
    Write-Error "Installer exited with code $($process.ExitCode)"
    exit 1
}

# --- Run post-install.ps1 ---
# Locate post-install.ps1 (bundled as Tauri resource or copied to install dir)
$installDir = "C:\Program Files\DevBridge"
$postInstallCandidates = @(
    (Join-Path $installDir "post-install.ps1"),
    (Join-Path $installDir "_up_\_up_\installer\post-install.ps1")
)
$postInstallScript = $postInstallCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if ($postInstallScript) {
    Write-Host "Running post-install script: $postInstallScript"

    # Build argument list from environment variables (irm|iex can't pass script params directly)
    $postArgs = @()
    $mode = if ($env:DEVBRIDGE_MODE) { $env:DEVBRIDGE_MODE } else { "server" }
    $postArgs += "-Mode"; $postArgs += $mode

    if ($env:DEVBRIDGE_SERVER_HOST)          { $postArgs += "-ServerHost";             $postArgs += $env:DEVBRIDGE_SERVER_HOST }
    if ($env:DEVBRIDGE_TARGET_PRINTER)       { $postArgs += "-TargetPrinter";          $postArgs += $env:DEVBRIDGE_TARGET_PRINTER }
    if ($env:DEVBRIDGE_CLIENT_ID)            { $postArgs += "-ClientId";               $postArgs += $env:DEVBRIDGE_CLIENT_ID }
    if ($env:DEVBRIDGE_VIRTUAL_PRINTER_NAME) { $postArgs += "-VirtualPrinterName";     $postArgs += $env:DEVBRIDGE_VIRTUAL_PRINTER_NAME }
    if ($env:DEVBRIDGE_PRINT_BACKEND)        { $postArgs += "-PrintBackend";           $postArgs += $env:DEVBRIDGE_PRINT_BACKEND }
    if ($env:DEVBRIDGE_PRINTER_ADDRESS)      { $postArgs += "-PrinterAddress";         $postArgs += $env:DEVBRIDGE_PRINTER_ADDRESS }
    if ($env:DEVBRIDGE_DASHBOARD_PORT)       { $postArgs += "-DashboardPort";          $postArgs += $env:DEVBRIDGE_DASHBOARD_PORT }
    if ($env:DEVBRIDGE_GHOSTSCRIPT_DEVICE)   { $postArgs += "-GhostscriptDevice";      $postArgs += $env:DEVBRIDGE_GHOSTSCRIPT_DEVICE }
    if ($env:DEVBRIDGE_GHOSTSCRIPT_RESOLUTION) { $postArgs += "-GhostscriptResolution"; $postArgs += $env:DEVBRIDGE_GHOSTSCRIPT_RESOLUTION }

    & powershell.exe -ExecutionPolicy Bypass -File $postInstallScript @postArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Error "post-install.ps1 exited with code $LASTEXITCODE"
        exit 1
    }
} else {
    Write-Warning "post-install.ps1 not found in install directory. Skipping post-install configuration."
}

# --- Verify service ---
Write-Host "Checking service status..."
Start-Sleep -Seconds 3

$proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($proc) {
    Write-Host "DevBridge service is running (PID: $($proc.Id))." -ForegroundColor Green
} else {
    Write-Host "Attempting to start via scheduled task..."
    Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3
    $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    if ($proc) {
        Write-Host "DevBridge service started (PID: $($proc.Id))." -ForegroundColor Green
    } else {
        Write-Warning "Could not start service. Please start it manually."
    }
}

# --- Cleanup ---
Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue

Write-Host "`n==> DevBridge $version installed successfully." -ForegroundColor Cyan
