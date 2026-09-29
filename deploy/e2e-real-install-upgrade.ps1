# E2E (issue #93): the REAL installer/install.ps1 upgrades the PRODUCTION
# install on pz-snv (the live pjsnvs store) while a DevBridge tray app runs.
#
# That is the situation that broke the 0.8.40 -> 0.8.41 auto-update on 6
# stores: the user's tray app (devbridge-app.exe, started at logon) made the
# Tauri NSIS installer exit 2, and install.ps1 then left DevBridgeService
# stopped. Before #93 the E2E harness stopped the trays itself, so CI stayed
# green while the stores broke. This step runs FIRST in the E2E Deploy Client
# job, as SYSTEM under Windows PowerShell 5.1 (like the DevBridgeAutoUpdate
# task), and:
#   1. starts devbridge-app.exe in session 0 (NSIS finds the app by process
#      name in any session; the store user's own tray usually runs too);
#   2. runs the checkout's install.ps1 as a child process with
#      DEVBRIDGE_INSTALLER_PATH = the CI-built NSIS installer (its dev-latest
#      release only exists after All Pass) -- the whole real flow: tray +
#      service stop, NSIS, hash check, post-install (config preserved);
#   3. asserts: exit 0, at least one tray app stopped (ours is gone),
#      DevBridgeService running, registry DisplayVersion and the live
#      /api/status version == the installer's version, production config.toml
#      byte-identical, and post-install relaunched the tray in the user's
#      session.
# Whatever happens, the production service is running again when this step
# ends (a failed step must never leave the store without printing).

param(
    [string]$InstallerGlob = "artifacts\DevBridge_*_x64-setup.exe",
    [string]$InstallDir = "C:\Program Files\DevBridge",
    [string]$ProductionConfig = "C:\ProgramData\DevBridge\config.toml",
    [int]$ProductionDashboardPort = 9120,
    [int]$InstallTimeoutSeconds = 600
)

$ErrorActionPreference = "Stop"

Write-Host "=== E2E: real install.ps1 upgrade with a running tray app (issue #93) ===" -ForegroundColor Cyan
Write-Host "PowerShell $($PSVersionTable.PSVersion) as $([Security.Principal.WindowsIdentity]::GetCurrent().Name)"

$repoRoot = Split-Path -Parent $PSScriptRoot
$installScript = Join-Path $repoRoot "installer\install.ps1"
$trayExe = Join-Path $InstallDir "devbridge-app.exe"
$trayNames = @("devbridge-app", "DevBridge")

function Get-InstalledDisplayVersion {
    foreach ($k in @("HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge",
            "HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge")) {
        $prop = Get-ItemProperty -Path $k -Name "DisplayVersion" -ErrorAction SilentlyContinue
        if ($prop -and $prop.DisplayVersion) {
            return [string]$prop.DisplayVersion
        }
    }
    return ""
}

function Test-ProductionServiceRunning {
    $task = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    $procs = @(Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue)
    return [bool]($task -and $task.State -eq "Running" -and $procs.Count -gt 0)
}

function Format-TrayList {
    param([object[]]$Trays)
    if (-not $Trays -or $Trays.Count -eq 0) {
        return "none"
    }
    return (($Trays | ForEach-Object { "{0} PID {1} session {2}" -f $_.Name, $_.Id, $_.SessionId }) -join ", ")
}

# -- Installer under test and its version ------------------------------------
$installer = Get-ChildItem -Path $InstallerGlob -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $installer) {
    throw "No NSIS installer found matching $InstallerGlob"
}
if ($installer.Name -notmatch '_(\d+\.\d+\.\d+)_') {
    throw "Installer name '$($installer.Name)' carries no version"
}
$targetVersion = $Matches[1]

# -- Preconditions: an existing production install (this is an UPGRADE) -----
if (-not (Test-Path -LiteralPath $ProductionConfig -PathType Leaf)) {
    throw "No production config at $ProductionConfig -- this step upgrades an EXISTING install"
}
if (-not (Test-Path -LiteralPath $trayExe -PathType Leaf)) {
    throw "Tray app not found at $trayExe"
}
$configHashBefore = (Get-FileHash -LiteralPath $ProductionConfig -Algorithm SHA256).Hash
Write-Host "Before: installed $(Get-InstalledDisplayVersion), target $targetVersion ($($installer.Name))"
Write-Host "Before: production config SHA256 $configHashBefore"

$outDir = Join-Path ([System.IO.Path]::GetTempPath()) ("devbridge-e2e-93-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$startedTray = $null
try {
    # -- 1. A running tray app --------------------------------------------------
    $startedTray = Start-Process -FilePath $trayExe -PassThru
    $null = $startedTray.Handle
    Start-Sleep -Seconds 5
    if ($startedTray.HasExited) {
        throw "The tray app started for this test (PID $($startedTray.Id)) exited on its own with code $($startedTray.ExitCode) -- the upgrade would not run against a live tray"
    }
    $traysBefore = @(Get-Process -Name $trayNames -ErrorAction SilentlyContinue)
    Write-Host "Tray apps running before the upgrade: $(Format-TrayList $traysBefore)"

    # -- 2. The real install.ps1 on the CI-built installer ---------------------
    $outFile = Join-Path $outDir "install.stdout.txt"
    $errFile = Join-Path $outDir "install.stderr.txt"
    $powershellExe = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
    $env:DEVBRIDGE_INSTALLER_PATH = $installer.FullName
    try {
        $proc = Start-Process -FilePath $powershellExe `
            -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$installScript`"" `
            -PassThru -NoNewWindow -RedirectStandardOutput $outFile -RedirectStandardError $errFile
        $null = $proc.Handle
        if (-not $proc.WaitForExit($InstallTimeoutSeconds * 1000)) {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
            throw "install.ps1 did not finish within ${InstallTimeoutSeconds}s"
        }
        $installExitCode = $proc.ExitCode
    } finally {
        Remove-Item -Path Env:DEVBRIDGE_INSTALLER_PATH -ErrorAction SilentlyContinue
        Write-Host "--- install.ps1 stdout ---"
        if (Test-Path -LiteralPath $outFile) { Get-Content -LiteralPath $outFile | ForEach-Object { Write-Host "  $_" } }
        Write-Host "--- install.ps1 stderr ---"
        if (Test-Path -LiteralPath $errFile) { Get-Content -LiteralPath $errFile | ForEach-Object { Write-Host "  $_" } }
        Write-Host "--- end of install.ps1 output ---"
    }
    $installOutput = if (Test-Path -LiteralPath $outFile) { [System.IO.File]::ReadAllText($outFile) } else { "" }

    # -- 3. Assertions ------------------------------------------------------------
    $serviceRunning = Test-ProductionServiceRunning
    if ($installExitCode -ne 0) {
        throw "install.ps1 exited with code $installExitCode (production service running afterwards: $serviceRunning)"
    }
    Write-Host "  [OK] install.ps1 exited 0" -ForegroundColor Green

    if ($installOutput -notmatch 'Stopped (\d+) tray app process\(es\) before running the installer') {
        throw "install.ps1 output has no 'Stopped N tray app process(es)' line -- the pre-NSIS tray stop did not run"
    }
    $stoppedCount = [int]$Matches[1]
    if ($stoppedCount -lt 1) {
        throw "install.ps1 stopped $stoppedCount tray app(s) although $($traysBefore.Count) were running"
    }
    $startedTray.Refresh()
    if (-not $startedTray.HasExited) {
        throw "The tray app started for this test (PID $($startedTray.Id)) is still running after the upgrade"
    }
    Write-Host "  [OK] install.ps1 stopped $stoppedCount tray app(s) before NSIS (test tray PID $($startedTray.Id) gone)" -ForegroundColor Green

    if (-not $serviceRunning) {
        throw "DevBridgeService is not running after a successful install.ps1"
    }
    Write-Host "  [OK] DevBridgeService running" -ForegroundColor Green

    $installedVersion = Get-InstalledDisplayVersion
    if ($installedVersion -ne $targetVersion) {
        throw "Registry DisplayVersion is '$installedVersion', expected $targetVersion"
    }
    Write-Host "  [OK] registry DisplayVersion $installedVersion" -ForegroundColor Green

    $liveVersion = $null
    for ($i = 1; $i -le 60; $i++) {
        try {
            $status = Invoke-RestMethod -Uri "http://127.0.0.1:$ProductionDashboardPort/api/status" -TimeoutSec 3
            if ($status.status -eq "running") {
                $liveVersion = [string]$status.version
                break
            }
        } catch {
            Write-Host "  waiting for the production dashboard ($i/60): $($_.Exception.Message)"
        }
        Start-Sleep -Seconds 1
    }
    if ($liveVersion -ne $targetVersion) {
        throw "Production /api/status version is '$liveVersion', expected $targetVersion"
    }
    Write-Host "  [OK] live production /api/status version $liveVersion" -ForegroundColor Green

    $configHashAfter = (Get-FileHash -LiteralPath $ProductionConfig -Algorithm SHA256).Hash
    if ($configHashAfter -ne $configHashBefore) {
        throw "Production config.toml changed during the upgrade: $configHashBefore -> $configHashAfter"
    }
    Write-Host "  [OK] production config.toml unchanged ($configHashAfter)" -ForegroundColor Green

    # post-install relaunches the tray in every user session `query user`
    # reports (Active or Disc). Exit code 1 of `query user` is normal.
    $userSessions = @(query user 2>$null | Select-Object -Skip 1 | Where-Object { $_ -match '\s(\d+)\s+(Active|Disc)' })
    $global:LASTEXITCODE = 0
    if ($userSessions.Count -gt 0) {
        $userTrays = @()
        for ($i = 1; $i -le 30; $i++) {
            $userTrays = @(Get-Process -Name $trayNames -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 })
            if ($userTrays.Count -gt 0) {
                break
            }
            Start-Sleep -Seconds 1
        }
        if ($userTrays.Count -eq 0) {
            throw "post-install did not relaunch the tray app in any of the $($userSessions.Count) user session(s)"
        }
        Write-Host "  [OK] post-install relaunched the tray: $(Format-TrayList $userTrays)" -ForegroundColor Green
    } else {
        Write-Host "  No user session on this machine -- tray relaunch not checkable (it starts at the next logon)"
    }

    Write-Host "=== Real install.ps1 upgrade with a running tray app: PASS ===" -ForegroundColor Green
} finally {
    if ($startedTray -and -not $startedTray.HasExited) {
        Stop-Process -Id $startedTray.Id -Force -ErrorAction SilentlyContinue
    }
    # Never leave the pjsnvs store without printing, whatever failed above.
    if (-not (Test-ProductionServiceRunning)) {
        Write-Host "Production DevBridgeService is NOT running -- starting it (E2E safety net)" -ForegroundColor Yellow
        Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 5
        Write-Host "  DevBridgeService running now: $(Test-ProductionServiceRunning)"
    }
    Remove-Item -Recurse -Force $outDir -ErrorAction SilentlyContinue
}
