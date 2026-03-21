# DevBridge one-liner installer
# Usage: irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex

$ErrorActionPreference = "Stop"
$repo = "zbynekdrlik/devbridge"
$serviceName = "DevBridge"

Write-Host "==> DevBridge Installer" -ForegroundColor Cyan

# --- Detect latest release ---
Write-Host "Fetching latest release..."
$releaseUrl = "https://api.github.com/repos/$repo/releases/latest"
try {
    $release = Invoke-RestMethod -Uri $releaseUrl -Headers @{ "User-Agent" = "DevBridge-Installer" }
} catch {
    Write-Error "Failed to fetch latest release from GitHub. Check your internet connection."
    exit 1
}

$version = $release.tag_name
Write-Host "Latest version: $version"

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

# --- Verify service ---
Write-Host "Checking service status..."
Start-Sleep -Seconds 3

$service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($service -and $service.Status -eq "Running") {
    Write-Host "DevBridge service is running." -ForegroundColor Green
} elseif ($service) {
    Write-Warning "DevBridge service exists but is not running (Status: $($service.Status))."
    Write-Host "Attempting to start service..."
    Start-Service -Name $serviceName -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
    $service = Get-Service -Name $serviceName
    if ($service.Status -eq "Running") {
        Write-Host "DevBridge service started successfully." -ForegroundColor Green
    } else {
        Write-Warning "Could not start service. Please start it manually."
    }
} else {
    Write-Warning "DevBridge service not found. The installer may not have registered it."
}

# --- Cleanup ---
Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue

Write-Host "`n==> DevBridge $version installed successfully." -ForegroundColor Cyan
