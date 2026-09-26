# DevBridge virtual-printer reconciler.
#
# Runs on boot via a Task Scheduler AtStartup trigger to guarantee that
# every virtual printer defined in DevBridge's storage has a matching
# Windows IPP printer entry. Without this the Windows spooler ends up in
# an inconsistent state after reboots -- the IPP Class Driver-backed
# virtual printers are silently broken ("Settings to access printer not
# valid"), and users can't print until someone manually runs the installer.
#
# Flow:
#   1. Wait for DevBridge dashboard to come up (retry 60s).
#   2. Fetch /api/virtual-printers.
#   3. For each entry:
#        - Resolve its Windows driver: the entry's optional "driver"
#          override (issue #88 -- e.g. "TSC ML241P" for a RAW label
#          printer), else "Microsoft IPP Class Driver".
#        - If Windows printer "display_name" already exists with the
#          right port URL AND that driver, skip (no-op; common post-boot case).
#        - If the driver is NOT installed on this machine: log an ERROR and
#          leave the printer alone. This script only USES installed drivers,
#          it never installs or removes one (except the IPP Class Driver
#          InfPath repair in Step 0).
#        - Otherwise: remove any stale printer/port with that name,
#          probe the IPP endpoint until it responds, then register
#          via rundll32 printui.dll,PrintUIEntry /if.
#   4. Log outcomes so a reboot that doesn't recover a printer is visible
#      in C:\ProgramData\DevBridge\logs\register-virtual-printers.log.

param(
    [int]$DashboardPort = 9120,
    [int]$IppPort = 631,
    [int]$DashboardWaitSecs = 60,
    [int]$IppWaitSecs = 15,
    [string]$LogPath = "C:\ProgramData\DevBridge\logs\register-virtual-printers.log",
    # When set, read the virtual-printer list from this JSON file instead of
    # querying the dashboard API. The DevBridge service passes this when it
    # spawns the script, eliminating the dashboard-startup race.
    [string]$InputJson = ""
)

$ErrorActionPreference = "Continue"

function Write-Log($msg) {
    $line = "[$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')] $msg"
    Write-Host $line
    try {
        $dir = Split-Path $LogPath -Parent
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
        Add-Content -Path $LogPath -Value $line -Encoding ASCII -ErrorAction SilentlyContinue
    } catch {}
}

# Driver every virtual printer uses unless its entry carries an override.
$DefaultVpDriver = "Microsoft IPP Class Driver"

# Resolve the Windows driver for one virtual-printer entry (issue #88).
# No/blank "driver" -> $DefaultDriver. Throws on a name that could break out
# of the quoted printui.dll /m "<driver>" argument (quote, backslash, control
# character) -- the server API rejects those too; this is defence in depth.
function Resolve-DevBridgeVpDriver {
    param($Vp, [string]$DefaultDriver = "Microsoft IPP Class Driver")
    $raw = $Vp.driver
    if ($null -eq $raw) { return $DefaultDriver }
    $name = ([string]$raw).Trim()
    if ($name -eq "") { return $DefaultDriver }
    if ($name -match '["\\]' -or $name -match '[\x00-\x1F\x7F]') {
        throw "driver name '$name' contains a forbidden character (quote, backslash or control character)"
    }
    return $name
}

# True when the existing Windows printer already points at $Url with $Driver
# (nothing to do). A driver mismatch means re-registration.
function Test-DevBridgePrinterUpToDate {
    param($Existing, [string]$Url, [string]$Driver)
    if (-not $Existing) { return $false }
    return (($Existing.PortName -eq $Url) -and ($Existing.DriverName -eq $Driver))
}

# True when $Driver is one of the installed printer-driver names
# (Get-PrinterDriver). Exact name match (case-insensitive, no wildcards).
function Test-DevBridgePrinterDriverInstalled {
    param([string]$Driver, [string[]]$InstalledDrivers)
    foreach ($d in @($InstalledDrivers)) {
        if ($d -eq $Driver) { return $true }
    }
    return $false
}

# (installer/tests/register-virtual-printers.Tests.ps1 extracts the three
# functions above via the AST -- the script body below never runs in tests.)

Write-Log "=== register-virtual-printers start ==="

# Step 0: Repair Microsoft IPP Class Driver if its InfPath is stale.
# Windows Update replaces the DriverStore package (prnms012.inf_amd64_*)
# during patching, but the spooler's printer-driver registration keeps
# pointing at the OLD hash directory which no longer exists. rundll32 /if
# then silently fails for every IPP printer registration attempt. This
# was the root cause of pz-server's overnight outage 2026-04-22.
try {
    $drv = Get-PrinterDriver -Name "Microsoft IPP Class Driver" -ErrorAction SilentlyContinue
    if ($drv -and $drv.InfPath -and -not (Test-Path $drv.InfPath)) {
        Write-Log "IPP driver InfPath is PHANTOM: $($drv.InfPath)"
        $newest = Get-ChildItem "$env:SystemRoot\System32\DriverStore\FileRepository\prnms012.inf_amd64_*\prnms012.inf" `
            -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($newest) {
            Write-Log "  Repairing driver to $($newest.FullName)"
            # Remove every IPP printer first -- Remove-PrinterDriver fails
            # while any printer references it. The reconciler loop below
            # recreates them all from /api/virtual-printers.
            $affected = Get-Printer | Where-Object { $_.DriverName -eq "Microsoft IPP Class Driver" }
            foreach ($p in $affected) {
                Remove-Printer -Name $p.Name -ErrorAction SilentlyContinue
                Write-Log "  Removed stale printer '$($p.Name)'"
            }
            Remove-PrinterDriver -Name "Microsoft IPP Class Driver" -ErrorAction SilentlyContinue
            Start-Sleep 2
            Add-PrinterDriver -Name "Microsoft IPP Class Driver" -InfPath $newest.FullName -ErrorAction SilentlyContinue
            Start-Sleep 2
            $drvAfter = Get-PrinterDriver -Name "Microsoft IPP Class Driver" -ErrorAction SilentlyContinue
            if ($drvAfter -and (Test-Path $drvAfter.InfPath)) {
                Write-Log "  Driver repaired -> $($drvAfter.InfPath)"
            } else {
                Write-Log "  ERROR: driver repair failed, IPP printers will not work"
            }
        } else {
            Write-Log "  ERROR: no valid prnms012.inf found in DriverStore"
        }
    } else {
        Write-Log "IPP driver InfPath OK: $($drv.InfPath)"
    }
} catch {
    Write-Log "WARN: driver repair check failed: $_"
}

# Step 1+2: Source the virtual-printer list -- either from -InputJson (called
# by the service) or by polling the dashboard API (legacy path for any
# manual / scheduled-task invocation).
$vps = $null
if ($InputJson -ne "") {
    if (-not (Test-Path $InputJson)) {
        Write-Log "ERROR: -InputJson path '$InputJson' does not exist"
        exit 3
    }
    try {
        $vps = Get-Content -Raw -Path $InputJson | ConvertFrom-Json
        Write-Log "Loaded $($vps.Count) virtual printer(s) from -InputJson"
    } catch {
        Write-Log "ERROR: Failed to parse -InputJson '$InputJson': $_"
        exit 3
    }
} else {
    # Legacy: wait for dashboard, then fetch /api/virtual-printers.
    $dashReady = $false
    for ($i = 1; $i -le $DashboardWaitSecs; $i++) {
        try {
            $status = Invoke-RestMethod -Uri "http://127.0.0.1:$DashboardPort/api/status" -TimeoutSec 3 -ErrorAction Stop
            if ($status.status -eq "running" -and $status.mode -eq "server") {
                $dashReady = $true
                Write-Log "Dashboard ready after ${i}s (version=$($status.version))"
                break
            }
        } catch {}
        Start-Sleep 1
    }
    if (-not $dashReady) {
        Write-Log "ERROR: Dashboard not ready after ${DashboardWaitSecs}s, aborting."
        exit 1
    }
    try {
        $vps = Invoke-RestMethod -Uri "http://127.0.0.1:$DashboardPort/api/virtual-printers" -TimeoutSec 5 -ErrorAction Stop
    } catch {
        Write-Log "ERROR: Failed to fetch virtual printers: $_"
        exit 2
    }
}

if (-not $vps -or $vps.Count -eq 0) {
    Write-Log "No virtual printers configured -- nothing to reconcile."
    exit 0
}

Write-Log "Found $($vps.Count) virtual printer(s) to reconcile."

# Installed drivers, read once (an override is only ever USED, never installed).
$installedDrivers = @(Get-PrinterDriver -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })

# Step 3: Reconcile each.
$failureCount = 0
foreach ($vp in $vps) {
    $name = $vp.display_name
    $ippName = $vp.ipp_name
    $url = "http://127.0.0.1:$IppPort/printers/$ippName"

    try {
        $driver = Resolve-DevBridgeVpDriver -Vp $vp -DefaultDriver $DefaultVpDriver
    } catch {
        Write-Log "    ERROR '$name': $_ -- printer NOT registered"
        $failureCount++
        continue
    }

    $existing = Get-Printer -Name $name -ErrorAction SilentlyContinue
    if (Test-DevBridgePrinterUpToDate -Existing $existing -Url $url -Driver $driver) {
        Write-Log "  OK: '$name' -> $($existing.PortName) [$driver]"
        continue
    }

    if (-not (Test-DevBridgePrinterDriverInstalled -Driver $driver -InstalledDrivers $installedDrivers)) {
        # Loud, and the existing printer (if any) is left untouched: never
        # fall back to another driver, never install one (issue #88).
        Write-Log "    ERROR '$name': Windows driver '$driver' is NOT installed on this machine -- printer NOT created/changed. Install the vendor driver first; this script never installs drivers."
        $failureCount++
        continue
    }

    Write-Log "  RECONCILE: '$name' needs re-registration (existing port=$($existing.PortName) driver=$($existing.DriverName); want driver=$driver)"

    # Clean stale printer + port to prevent rundll32's silent no-op on collision.
    Get-Printer -Name $name -ErrorAction SilentlyContinue | Remove-Printer -ErrorAction SilentlyContinue
    Get-PrinterPort -Name $url -ErrorAction SilentlyContinue | Remove-PrinterPort -ErrorAction SilentlyContinue

    # Probe the IPP endpoint -- rundll32 /if silently fails if the endpoint
    # isn't responding to IPP Get-Printer-Attributes during install.
    $ippReady = $false
    $minimalGetPrinterAttrs = [byte[]](0x01, 0x01, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x01, 0x03)
    for ($i = 1; $i -le $IppWaitSecs; $i++) {
        try {
            $r = Invoke-WebRequest -Uri $url -Method POST -ContentType 'application/ipp' `
                -Body $minimalGetPrinterAttrs -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
            if ($r.StatusCode -eq 200) { $ippReady = $true; break }
        } catch {}
        Start-Sleep 1
    }
    if (-not $ippReady) {
        Write-Log "    SKIP '$name': IPP endpoint $url did not respond within ${IppWaitSecs}s"
        $failureCount++
        continue
    }

    # Register the printer.
    $ifArgs = "/if /b `"$name`" /r `"$url`" /m `"$driver`" /q"
    try {
        Start-Process -FilePath rundll32.exe -ArgumentList "printui.dll,PrintUIEntry $ifArgs" `
            -Wait -NoNewWindow -ErrorAction Stop
    } catch {
        Write-Log "    ERR '$name': rundll32 failed: $_"
        $failureCount++
        continue
    }

    # rundll32 is async inside the spooler -- poll for up to 15s.
    $registered = $false
    for ($i = 1; $i -le 15; $i++) {
        Start-Sleep 1
        $verify = Get-Printer -Name $name -ErrorAction SilentlyContinue
        if (Test-DevBridgePrinterUpToDate -Existing $verify -Url $url -Driver $driver) {
            Write-Log "    OK '$name' registered after ${i}s -> $($verify.PortName) [$driver]"
            $registered = $true
            break
        }
    }
    if (-not $registered) {
        Write-Log "    ERR '$name': did not appear after rundll32 (driver conflict or spooler issue)"
        $failureCount++
    }
}

if ($failureCount -gt 0) {
    Write-Log "=== register-virtual-printers done with $failureCount failure(s) ==="
    # Non-zero exit so Task Scheduler's 'Last Run Result' surfaces the partial
    # failure to ops monitoring; history otherwise always shows 0x0 green.
    exit $failureCount
}
Write-Log "=== register-virtual-printers done (all OK) ==="
