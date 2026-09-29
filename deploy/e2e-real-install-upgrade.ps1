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
#   1. makes sure a tray runs in the logged-on user's session (as on the
#      stores; started like post-install does if needed) and starts one more
#      in session 0 -- no user session = the step fails, never passes vacuously;
#   2. runs the checkout's install.ps1 as a child process with
#      DEVBRIDGE_INSTALLER_PATH = the CI-built NSIS installer (its dev-latest
#      release only exists after All Pass) -- the whole real flow: tray +
#      service stop, NSIS, hash check, post-install (config preserved);
#   3. asserts: exit 0, every tray from before stopped ("Stopped N of N"),
#      DevBridgeService running, registry DisplayVersion and the live
#      /api/status version == the installer's version, production config.toml
#      byte-identical (restored from a copy otherwise), and post-install
#      relaunched a NEW tray in a user session.
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
# The user-session parser post-install itself uses (AST-extracted, no script
# body runs), so "a user session exists" means the same thing here and there.
. (Join-Path $repoRoot "deploy\lib\Get-FunctionSourceFromScript.ps1")
$libFns = Get-FunctionSourceFromScript -ScriptPath (Join-Path $repoRoot "installer\DevBridgeInstallerLib.ps1") `
    -Names @("ConvertFrom-DevBridgeQueryUserOutput")
foreach ($libFn in $libFns.Values) {
    . ([scriptblock]::Create($libFn))
}
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
# Byte copy of the live config: restored if the upgrade ever changes it.
$configBackup = Join-Path $outDir "config.toml.before"
Copy-Item -LiteralPath $ProductionConfig -Destination $configBackup
$startedTray = $null
try {
    # -- 1. Running tray apps, as on a store at update time -------------------
    # The incident's trays ran in the logged-on USER's session, so one must
    # run there (started like post-install does, if the user has none); the
    # step also starts one in session 0 (SYSTEM, like the runner).
    $userSessions = ConvertFrom-DevBridgeQueryUserOutput -Lines @(cmd.exe /c "query user 2>nul")
    $global:LASTEXITCODE = 0
    if ($userSessions.Count -eq 0) {
        throw "No logged-on user session on this runner -- the incident (a tray in the user's session) cannot be reproduced; log the store user on"
    }
    $userTrays = @(Get-Process -Name $trayNames -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 })
    if ($userTrays.Count -eq 0) {
        $user = @($userSessions | Where-Object { $_.State -eq "Active" }) + @($userSessions) | Select-Object -First 1
        $launchTask = "DevBridgeE2ETrayStart_$($user.Username)"
        Write-Host "No tray app in a user session -- starting one for $($user.Username) (session $($user.SessionId))"
        $launchAction = New-ScheduledTaskAction -Execute $trayExe
        $launchPrincipal = New-ScheduledTaskPrincipal -UserId $user.Username -LogonType Interactive
        Register-ScheduledTask -TaskName $launchTask -InputObject (New-ScheduledTask -Action $launchAction -Principal $launchPrincipal) -Force | Out-Null
        try {
            Start-ScheduledTask -TaskName $launchTask
            Start-Sleep -Seconds 1
        } finally {
            Unregister-ScheduledTask -TaskName $launchTask -Confirm:$false -ErrorAction SilentlyContinue
        }
    }
    $startedTray = Start-Process -FilePath $trayExe -PassThru
    $null = $startedTray.Handle
    Start-Sleep -Seconds 5
    if ($startedTray.HasExited) {
        throw "The session-0 tray app started for this test (PID $($startedTray.Id)) exited on its own with code $($startedTray.ExitCode)"
    }
    $traysBefore = @(Get-Process -Name $trayNames -ErrorAction SilentlyContinue)
    Write-Host "Tray apps running before the upgrade: $(Format-TrayList $traysBefore)"
    if (@($traysBefore | Where-Object { $_.SessionId -ne 0 }).Count -eq 0) {
        throw "No tray app runs in a user session before the upgrade -- the test would not reproduce the incident"
    }
    $pidsBefore = @($traysBefore | ForEach-Object { $_.Id })

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

    if ($installOutput -notmatch 'Stopped (\d+) of (\d+) tray app process\(es\) before running the installer') {
        throw "install.ps1 output has no 'Stopped N of M tray app process(es)' line -- the pre-NSIS tray stop did not run"
    }
    $stoppedCount = [int]$Matches[1]
    $foundCount = [int]$Matches[2]
    if ($foundCount -lt $pidsBefore.Count -or $stoppedCount -ne $foundCount) {
        throw "install.ps1 stopped $stoppedCount of $foundCount tray app(s); $($pidsBefore.Count) were running before it"
    }
    $survivors = @(Get-Process -Id $pidsBefore -ErrorAction SilentlyContinue)
    if ($survivors.Count -gt 0) {
        throw "Tray app(s) from before the upgrade still running: $(Format-TrayList $survivors)"
    }
    Write-Host "  [OK] install.ps1 stopped all $stoppedCount tray app(s) before NSIS (PIDs $($pidsBefore -join ', ') gone)" -ForegroundColor Green

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

    # post-install relaunches the tray in every user session: a NEW tray
    # process (not one from before the upgrade) in a user session.
    $relaunched = @()
    for ($i = 1; $i -le 30; $i++) {
        $relaunched = @(Get-Process -Name $trayNames -ErrorAction SilentlyContinue |
            Where-Object { $_.SessionId -ne 0 -and $pidsBefore -notcontains $_.Id })
        if ($relaunched.Count -gt 0) {
            break
        }
        Start-Sleep -Seconds 1
    }
    if ($relaunched.Count -eq 0) {
        throw "post-install did not relaunch the tray app in any of the $($userSessions.Count) user session(s)"
    }
    Write-Host "  [OK] post-install relaunched the tray: $(Format-TrayList $relaunched)" -ForegroundColor Green

    Write-Host "=== Real install.ps1 upgrade with a running tray app: PASS ===" -ForegroundColor Green
} finally {
    if ($startedTray -and -not $startedTray.HasExited) {
        Stop-Process -Id $startedTray.Id -Force -ErrorAction SilentlyContinue
    }
    # The live store's config must come out byte-identical: undo any change.
    if ((Test-Path -LiteralPath $configBackup) -and
        (Get-FileHash -LiteralPath $ProductionConfig -Algorithm SHA256).Hash -ne $configHashBefore) {
        Write-Host "Production config.toml differs from before the step -- restoring the original bytes and restarting the service" -ForegroundColor Yellow
        Copy-Item -LiteralPath $configBackup -Destination $ProductionConfig -Force
        Stop-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
        Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 5
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
