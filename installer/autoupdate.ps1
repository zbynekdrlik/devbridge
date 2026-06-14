# DevBridge Auto-Update Task (Windows)
#
# Registered by post-install.ps1 as the scheduled task "DevBridgeAutoUpdate"
# (runs once AtStartup + every 6 hours). It checks GitHub for a newer PATCH
# release on the SAME major.minor as the installed version and, when one exists
# and it is safe to do so, re-runs the hardened installer/install.ps1 to upgrade
# in place. Operators no longer have to run `irm | iex` by hand. See issue #54.
#
# SAFETY MODEL (every guard below is a hard refusal that leaves the running
# service untouched):
#   * PATCH-only: never crosses major or minor. Operators approve breaking
#     changes; auto-update only ships drop-in bug fixes.
#   * Kill-switch: env var DEVBRIDGE_AUTO_UPDATE=off OR a marker file
#     C:\ProgramData\DevBridge\autoupdate.disabled disables the whole task.
#   * Skip-if-printing: reads the dashboard /api/status active_jobs and refuses
#     to restart the service while a job is downloading/printing.
#   * Fail-safe: any error (version read, GitHub fetch, parse) makes the task
#     a NO-OP and exits 0 — the running service is never half-upgraded.
#
# The pure DECISION functions below are extracted by the Pester suite
# (installer/tests/autoupdate.Tests.ps1) via the PowerShell AST, so the tested
# code IS the production code — there is no second copy to drift. The orchestration
# at the bottom (network, registry, install.ps1 invocation) is only exercised on
# real machines; the decisions are unit-tested deterministically.

# ── Pure decision helpers (unit-tested via AST extraction) ──────────────────

# Parse a version string ("0.8.28", "v0.8.28", "0.8.28-dev.3") into a
# [pscustomobject]@{ Major; Minor; Patch } of [int], or $null if it is not a
# recognizable semantic version. The leading "v" and any pre-release/build
# suffix after the patch are tolerated and ignored. Returning $null (rather than
# throwing) lets every caller treat an unparseable version as "do nothing".
function Get-DevBridgeSemVer {
    param([string]$Version)
    if ([string]::IsNullOrWhiteSpace($Version)) { return $null }
    $v = $Version.Trim()
    if ($v.StartsWith("v") -or $v.StartsWith("V")) { $v = $v.Substring(1) }
    # major.minor.patch with optional -prerelease / +build suffix.
    $m = [regex]::Match($v, '^(\d+)\.(\d+)\.(\d+)')
    if (-not $m.Success) { return $null }
    return [pscustomobject]@{
        Major = [int]$m.Groups[1].Value
        Minor = [int]$m.Groups[2].Value
        Patch = [int]$m.Groups[3].Value
    }
}

# Decide whether to auto-update from $Installed to $Latest. Returns a
# [pscustomobject]@{ ShouldUpdate = [bool]; Reason = [string] } where Reason is
# one of: unparseable-installed | unparseable-latest | up-to-date |
# minor-or-major-change (operator-gated) | downgrade | patch-update.
#
# The ONLY case that yields ShouldUpdate=$true is a strictly-newer PATCH on the
# SAME major.minor. Equal versions, downgrades, and ANY minor/major delta are
# refused — operators approve those manually.
function Get-DevBridgeUpdateDecision {
    param(
        [string]$Installed,
        [string]$Latest
    )
    $i = Get-DevBridgeSemVer $Installed
    $l = Get-DevBridgeSemVer $Latest
    if (-not $i) { return [pscustomobject]@{ ShouldUpdate = $false; Reason = "unparseable-installed" } }
    if (-not $l) { return [pscustomobject]@{ ShouldUpdate = $false; Reason = "unparseable-latest" } }

    # Patch-only lock: refuse anything that changes major or minor, in EITHER
    # direction. A minor/major bump may carry config/schema/protocol changes the
    # operator must vet, so auto-update must never apply it.
    if ($i.Major -ne $l.Major -or $i.Minor -ne $l.Minor) {
        return [pscustomobject]@{ ShouldUpdate = $false; Reason = "minor-or-major-change" }
    }

    if ($l.Patch -gt $i.Patch) {
        return [pscustomobject]@{ ShouldUpdate = $true; Reason = "patch-update" }
    }
    if ($l.Patch -lt $i.Patch) {
        return [pscustomobject]@{ ShouldUpdate = $false; Reason = "downgrade" }
    }
    return [pscustomobject]@{ ShouldUpdate = $false; Reason = "up-to-date" }
}

# Kill-switch evaluation. Auto-update is DISABLED when EITHER the env var
# DEVBRIDGE_AUTO_UPDATE is set to a falsey value ("off"/"false"/"0"/"no"/
# "disabled") OR the marker file exists on disk. Returns
# [pscustomobject]@{ Disabled = [bool]; Reason = [string] } where Reason is
# env-var | marker-file | none.
#
# The env-var check is permissive about case + whitespace. ANY value other than
# the recognized "off" set leaves the env-var path NOT disabling (so a stray
# DEVBRIDGE_AUTO_UPDATE="on" does not accidentally turn it off), but the marker
# file ALWAYS wins if present.
function Test-DevBridgeAutoUpdateDisabled {
    param(
        [string]$EnvValue,
        [bool]$MarkerFileExists
    )
    if ($MarkerFileExists) {
        return [pscustomobject]@{ Disabled = $true; Reason = "marker-file" }
    }
    if ($EnvValue -match '^\s*(off|false|0|no|disabled)\s*$') {
        return [pscustomobject]@{ Disabled = $true; Reason = "env-var" }
    }
    return [pscustomobject]@{ Disabled = $false; Reason = "none" }
}

# Decide whether it is safe to restart the service for an upgrade given the
# dashboard's reported active_jobs count. Returns
# [pscustomobject]@{ Safe = [bool]; Reason = [string] } where Reason is
# idle | jobs-active | status-unknown.
#
# A $null ActiveJobs (status unreachable / field missing) is treated as NOT safe
# — we would rather skip this cycle and retry in 6h than risk interrupting a job
# we could not account for. Only an explicit 0 is "idle".
function Test-DevBridgeSafeToUpdate {
    param([Nullable[int]]$ActiveJobs)
    if ($null -eq $ActiveJobs) {
        return [pscustomobject]@{ Safe = $false; Reason = "status-unknown" }
    }
    if ($ActiveJobs -gt 0) {
        return [pscustomobject]@{ Safe = $false; Reason = "jobs-active" }
    }
    return [pscustomobject]@{ Safe = $true; Reason = "idle" }
}

# ── Orchestration (real-machine only; not unit-tested) ──────────────────────
# Guarded so dot-sourcing this file in the Pester suite loads ONLY the functions
# above and never runs the live update flow. $MyInvocation.InvocationName is '.'
# when dot-sourced; the AST extractor never executes the body at all, but this is
# a belt-and-suspenders guard for anyone who dot-sources the whole file.
if ($MyInvocation.InvocationName -eq '.') { return }

$ErrorActionPreference = "Stop"
$repo = "zbynekdrlik/devbridge"
$dataDir = "C:\ProgramData\DevBridge"
$logFile = Join-Path $dataDir "logs\autoupdate.log"
$markerFile = Join-Path $dataDir "autoupdate.disabled"
$dashboardUrl = "http://127.0.0.1:9120/api/status"

# Comprehensive, append-only logging. Every decision branch logs WHY it took the
# path it did, so a 3am "why did/didn't store X upgrade?" is answerable from this
# file alone (comprehensive-logging.md).
function Write-Log {
    param([string]$Message, [string]$Level = "INFO")
    $line = "{0} [{1}] {2}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $Level, $Message
    try {
        $logDir = Split-Path -Parent $logFile
        if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Force -Path $logDir | Out-Null }
        Add-Content -Path $logFile -Value $line -Encoding UTF8
    } catch {
        # Never let a logging failure abort the task.
    }
    Write-Host $line
}

# Read the installed DisplayVersion from the Uninstall registry key. Returns the
# version string, or $null if the key/value is absent.
function Get-InstalledVersion {
    $keys = @(
        "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge",
        "HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge"
    )
    foreach ($k in $keys) {
        try {
            $prop = Get-ItemProperty -Path $k -Name "DisplayVersion" -ErrorAction Stop
            if ($prop.DisplayVersion) { return [string]$prop.DisplayVersion }
        } catch {
            # Try next key.
        }
    }
    return $null
}

Write-Log "=== DevBridge auto-update check starting (pid $PID) ==="

# 1. Kill-switch (env var OR marker file).
$killSwitch = Test-DevBridgeAutoUpdateDisabled -EnvValue $env:DEVBRIDGE_AUTO_UPDATE `
    -MarkerFileExists (Test-Path $markerFile)
if ($killSwitch.Disabled) {
    Write-Log "Auto-update DISABLED via $($killSwitch.Reason) (env DEVBRIDGE_AUTO_UPDATE='$($env:DEVBRIDGE_AUTO_UPDATE)', marker='$markerFile'). Exiting no-op."
    exit 0
}
Write-Log "Kill-switch check: enabled (env DEVBRIDGE_AUTO_UPDATE='$($env:DEVBRIDGE_AUTO_UPDATE)', no marker file)."

# 2. Read installed version.
$installed = Get-InstalledVersion
if (-not $installed) {
    Write-Log "Could not read installed DisplayVersion from registry. Exiting no-op (service left untouched)." "WARN"
    exit 0
}
Write-Log "Installed version: $installed"

# 3. Determine the latest available version. DEVBRIDGE_VERSION (if set on the
#    machine) pins a target; otherwise query GitHub's latest stable release.
$latest = $null
if ($env:DEVBRIDGE_VERSION) {
    $latest = $env:DEVBRIDGE_VERSION
    Write-Log "DEVBRIDGE_VERSION pin present: target version '$latest'."
} else {
    try {
        $headers = @{ "User-Agent" = "DevBridge-AutoUpdate" }
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers $headers -TimeoutSec 30
        $latest = $release.tag_name
        Write-Log "GitHub latest release tag: '$latest'."
    } catch {
        Write-Log "Failed to fetch latest release from GitHub: $_. Exiting no-op." "WARN"
        exit 0
    }
}

# 4. Patch-only update decision.
$decision = Get-DevBridgeUpdateDecision -Installed $installed -Latest $latest
Write-Log "Update decision: ShouldUpdate=$($decision.ShouldUpdate) Reason=$($decision.Reason) (installed=$installed, latest=$latest)."
if (-not $decision.ShouldUpdate) {
    Write-Log "No patch-update to apply ($($decision.Reason)). Exiting no-op."
    exit 0
}

# 5. Skip-if-printing: do not interrupt a job mid-page. Read active_jobs from the
#    local dashboard. A failure to read => treat as unknown => skip this cycle.
$activeJobs = $null
try {
    $status = Invoke-RestMethod -Uri $dashboardUrl -TimeoutSec 10
    if ($null -ne $status.active_jobs) {
        $activeJobs = [int]$status.active_jobs
        Write-Log "Dashboard active_jobs = $activeJobs."
    } else {
        Write-Log "Dashboard /api/status has no 'active_jobs' field (older service?). Treating as unknown." "WARN"
    }
} catch {
    Write-Log "Failed to read dashboard /api/status: $_. Treating active jobs as unknown." "WARN"
}
$safe = Test-DevBridgeSafeToUpdate -ActiveJobs $activeJobs
Write-Log "Safe-to-update check: Safe=$($safe.Safe) Reason=$($safe.Reason)."
if (-not $safe.Safe) {
    Write-Log "Not safe to update right now ($($safe.Reason)). Will retry next cycle. Exiting no-op."
    exit 0
}

# 6. Apply the upgrade by re-running the hardened install.ps1 (it owns the
#    Stop-Service + 30s unlock poll + SHA256 binary-swap guard + config
#    preserve/snapshot). We do NOT reimplement any install logic here.
Write-Log "Applying patch-update $installed -> $latest via install.ps1 (DEVBRIDGE_VERSION='$latest')."
try {
    $env:DEVBRIDGE_VERSION = $latest
    $installScript = "https://raw.githubusercontent.com/$repo/main/installer/install.ps1"
    $installer = (New-Object System.Net.WebClient).DownloadString($installScript)
    & ([scriptblock]::Create($installer))
    Write-Log "install.ps1 completed. Verifying installed version..."
    $newInstalled = Get-InstalledVersion
    Write-Log "Post-update installed version: $newInstalled (was $installed, target $latest)."
    if ($newInstalled -and ($newInstalled -ne $installed)) {
        Write-Log "Auto-update SUCCESS: $installed -> $newInstalled."
    } else {
        Write-Log "Auto-update ran but installed version did not change (still $newInstalled). Investigate install.ps1 output above." "WARN"
    }
} catch {
    Write-Log "Auto-update FAILED while running install.ps1: $_. The previous version remains installed." "ERROR"
    exit 1
}

Write-Log "=== DevBridge auto-update check finished ==="
exit 0
