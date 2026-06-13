# DevBridge one-liner installer
# Usage:
#   irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
#   $env:DEVBRIDGE_VERSION="dev"; irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex

$ErrorActionPreference = "Stop"
$repo = "zbynekdrlik/devbridge"
$serviceName = "DevBridge"

# ── Upgrade-hardening helpers ───────────────────────────────────────────────
# install.ps1 runs via `irm | iex` (no script file on disk, $PSScriptRoot is
# empty, no sibling files), so these MUST be defined inline -- dot-sourcing a
# separate lib would break the production one-liner. The Pester suite
# (installer/tests/installer-lib.Tests.ps1) extracts these function bodies FROM
# this very file via the PowerShell AST, so the tested code IS the production
# code -- there is no second copy to drift. See issue #47.

# Wait up to -TimeoutSeconds for a binary to become writable (exclusive write
# handle) so NSIS can replace it on upgrade. A fresh install (file absent) is
# already "unlocked". FileAccess.Write is what NSIS needs (Read+ShareNone may
# pass while a writer handle is still pending).
function Wait-DevBridgeBinaryUnlocked {
    param(
        [Parameter(Mandatory)][string]$Path,
        [int]$TimeoutSeconds = 30,
        [int]$SleepMilliseconds = 1000
    )
    if (-not (Test-Path $Path)) {
        return $true   # fresh install -> already "unlocked"
    }
    for ($i = 1; $i -le $TimeoutSeconds; $i++) {
        try {
            $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            $fs.Close()
            return $true
        } catch {
            Start-Sleep -Milliseconds $SleepMilliseconds
        }
    }
    return $false
}

# Verify the installer actually swapped the binary by comparing pre/post SHA256.
# Returns @{ Ok; Reason } where Reason is fresh-install | updated | unchanged.
# The only failure (Ok=$false) is: a real binary existed (PreHash != "") AND the
# hash did not change -- NSIS silently no-oped the overwrite of an in-use file.
function Test-DevBridgeBinarySwap {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$PreHash,
        [Parameter(Mandatory)][string]$PostHash
    )
    if ($PreHash -eq "") {
        return [pscustomobject]@{ Ok = $true; Reason = "fresh-install" }
    }
    if ($PostHash -eq $PreHash) {
        return [pscustomobject]@{ Ok = $false; Reason = "unchanged" }
    }
    return [pscustomobject]@{ Ok = $true; Reason = "updated" }
}
$requestedVersion = if ($env:DEVBRIDGE_VERSION) { $env:DEVBRIDGE_VERSION } else { "latest" }

Write-Host "==> DevBridge Installer" -ForegroundColor Cyan

# --- Ensure Visual C++ Redistributable (required by bundled Ghostscript) ---
# gsdll64.dll links against msvcp140.dll / vcruntime140.dll. On a fresh
# Windows box without VC++ 2015-2022 Redistributable, Ghostscript fails
# with LoadLibrary error 126 and every print job fails with exit code
# -1073741515 (STATUS_DLL_NOT_FOUND). Check for the runtime DLLs in
# System32 and install silently if missing.
$vcRuntimeDlls = @(
    "C:\Windows\System32\vcruntime140.dll",
    "C:\Windows\System32\msvcp140.dll"
)
$vcMissing = $vcRuntimeDlls | Where-Object { -not (Test-Path $_) }
if ($vcMissing) {
    Write-Host "Installing Visual C++ Runtime (required by Ghostscript)..."
    $vcUrl = "https://aka.ms/vs/17/release/vc_redist.x64.exe"
    $vcExe = Join-Path $env:TEMP "vc_redist.x64.exe"
    try {
        Invoke-WebRequest -Uri $vcUrl -OutFile $vcExe -UseBasicParsing
        $vcProc = Start-Process -FilePath $vcExe -ArgumentList "/install", "/quiet", "/norestart" -Wait -PassThru
        if ($vcProc.ExitCode -ne 0 -and $vcProc.ExitCode -ne 3010) {
            # 3010 = success, reboot required; treat as OK
            Write-Warning "VC++ Redist installer exited with code $($vcProc.ExitCode) -- continuing anyway"
        } else {
            Write-Host "VC++ Runtime installed." -ForegroundColor Green
        }
    } catch {
        Write-Warning "Failed to install VC++ Runtime: $_. Print jobs may fail until it's installed manually."
    }
} else {
    Write-Host "VC++ Runtime present."
}

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
# Sort by updated_at descending so the newest upload wins. The dev-latest
# pre-release accumulates assets for many versions; GitHub returns them in
# a stable order that puts older versions first alphabetically. Picking the
# most recently uploaded one guarantees we install the current dev build.
# See issue #35.
$installerAsset = $release.assets | Where-Object { $_.name -match "setup.*\.exe$" } | Sort-Object { [datetime]$_.updated_at } -Descending | Select-Object -First 1
if (-not $installerAsset) {
    $installerAsset = $release.assets | Where-Object { $_.name -match "DevBridge.*\.exe$" } | Sort-Object { [datetime]$_.updated_at } -Descending | Select-Object -First 1
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

# --- Stop running service so NSIS can replace the binary --
# NSIS silently skips overwriting a file it can't open exclusively. If
# devbridge-service.exe is running, the upgrade leaves the OLD binary in
# place and `==> installed successfully` is a lie. Stopping the scheduled
# task + waiting for the process to actually exit prevents that.
$existingTask = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
if ($existingTask) {
    Write-Host "Stopping DevBridgeService for binary swap..."
    Stop-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
}
$existingProcs = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($existingProcs) {
    $existingProcs | Stop-Process -Force -ErrorAction SilentlyContinue
}
# Wait up to 30 s for the file to become writable. WaitForExit alone
# isn't sufficient -- Windows / Defender / antivirus can hold the file
# briefly after the process dies. Test with FileAccess.Write since that
# is what NSIS actually needs (Read+ShareNone may pass while a writer
# handle is still pending). See Wait-DevBridgeBinaryUnlocked above.
$svcExe = "C:\Program Files\DevBridge\devbridge-service.exe"
$unlocked = Wait-DevBridgeBinaryUnlocked -Path $svcExe -TimeoutSeconds 30
if (-not $unlocked) {
    Write-Error "Service binary still locked after 30s. Aborting to avoid silent no-op install."
    Write-Error "Manual recovery: Task Manager -> kill devbridge-service.exe -> re-run installer."
    exit 1
}

# Capture the pre-install SHA256 so we can verify the swap actually happened.
# Hash beats mtime here: Tauri/NSIS may preserve the source-binary mtime
# (especially with reproducible Rust builds via SOURCE_DATE_EPOCH), making
# an mtime check report a false negative on a real upgrade.
$preInstallHash = if (Test-Path $svcExe) { (Get-FileHash $svcExe -Algorithm SHA256).Hash } else { "" }

# --- Run installer ---
Write-Host "Running installer (silent mode)..."
$process = Start-Process -FilePath $installerPath -ArgumentList "/S" -Wait -PassThru
if ($process.ExitCode -ne 0) {
    Write-Error "Installer exited with code $($process.ExitCode)"
    exit 1
}

# --- Verify binary was actually replaced --
# NSIS exits 0 even when its file-overwrite step silently no-ops (file
# in use). Compare hashes; if unchanged, fail loudly so the operator
# doesn't think they upgraded when they didn't. (Identical-version
# reinstalls are caught by `$preInstallHash -ne ""` -- we only complain
# when a real binary already existed and didn't change.)
if (Test-Path $svcExe) {
    $postInstallHash = (Get-FileHash $svcExe -Algorithm SHA256).Hash
    # See Test-DevBridgeBinarySwap above for the decision logic.
    $swap = Test-DevBridgeBinarySwap -PreHash $preInstallHash -PostHash $postInstallHash
    if (-not $swap.Ok) {
        Write-Error "Binary SHA256 unchanged after install ($postInstallHash). NSIS likely could not replace the file."
        Write-Error "Manual recovery: stop the service, delete '$svcExe', re-run the installer."
        exit 1
    }
    if ($swap.Reason -eq "fresh-install") {
        Write-Host "  Binary installed (hash $($postInstallHash.Substring(0,12)))"
    } else {
        Write-Host "  Binary updated (hash $($preInstallHash.Substring(0,12)) -> $($postInstallHash.Substring(0,12)))"
    }
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
    # Mode resolution: env var wins; otherwise inherit from preserved config
    # (so a bare upgrade doesn't silently flip a client back to server defaults);
    # otherwise fall back to "server" for greenfield installs.
    $mode = $env:DEVBRIDGE_MODE
    if (-not $mode) {
        $existingConfig = "C:\ProgramData\DevBridge\config.toml"
        if (Test-Path $existingConfig) {
            $modeMatch = (Get-Content $existingConfig -ErrorAction SilentlyContinue |
                Select-String -Pattern '^\s*mode\s*=\s*"(client|server)"' |
                Select-Object -First 1)
            if ($modeMatch) {
                $mode = $modeMatch.Matches[0].Groups[1].Value
                Write-Host "Detected mode='$mode' from preserved config; carrying over." -ForegroundColor Cyan
            }
        }
    }
    if (-not $mode) { $mode = "server" }
    $postArgs += "-Mode"; $postArgs += $mode

    if ($env:DEVBRIDGE_SERVER_HOST)          { $postArgs += "-ServerHost";             $postArgs += $env:DEVBRIDGE_SERVER_HOST }
    if ($env:DEVBRIDGE_TARGET_PRINTER)       { $postArgs += "-TargetPrinter";          $postArgs += $env:DEVBRIDGE_TARGET_PRINTER }
    if ($env:DEVBRIDGE_CLIENT_ID)            { $postArgs += "-ClientId";               $postArgs += $env:DEVBRIDGE_CLIENT_ID }
    if ($env:DEVBRIDGE_VIRTUAL_PRINTER_NAME) { $postArgs += "-VirtualPrinterName";     $postArgs += $env:DEVBRIDGE_VIRTUAL_PRINTER_NAME }
    if ($env:DEVBRIDGE_PRINTER_DISPLAY_NAME) { $postArgs += "-PrinterDisplayName";     $postArgs += $env:DEVBRIDGE_PRINTER_DISPLAY_NAME }
    if ($env:DEVBRIDGE_PRINT_BACKEND)        { $postArgs += "-PrintBackend";           $postArgs += $env:DEVBRIDGE_PRINT_BACKEND }
    if ($env:DEVBRIDGE_PRINTER_ADDRESS)      { $postArgs += "-PrinterAddress";         $postArgs += $env:DEVBRIDGE_PRINTER_ADDRESS }
    if ($env:DEVBRIDGE_PRINTER_TLS -eq "true") { $postArgs += "-PrinterTls" }
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
