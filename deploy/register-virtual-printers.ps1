# DevBridge virtual-printer reconciler.
#
# Runs on boot via a Task Scheduler AtStartup trigger to guarantee that
# every virtual printer defined in DevBridge's storage has a matching
# Windows IPP printer entry. Without this the Windows spooler ends up in
# an inconsistent state after reboots — the IPP Class Driver-backed
# virtual printers are silently broken ("Settings to access printer not
# valid"), and users can't print until someone manually runs the installer.
#
# Flow:
#   1. Wait for DevBridge dashboard to come up (retry 60s).
#   2. Fetch /api/virtual-printers.
#   3. For each entry:
#        - If Windows printer "display_name" already exists with the
#          right port URL, skip (no-op; common post-boot case).
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
    [string]$LogPath = "C:\ProgramData\DevBridge\logs\register-virtual-printers.log"
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

Write-Log "=== register-virtual-printers start ==="

# Step 1: Wait for DevBridge dashboard (service may start slow after boot).
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

# Step 2: Pull virtual printer list.
try {
    $vps = Invoke-RestMethod -Uri "http://127.0.0.1:$DashboardPort/api/virtual-printers" -TimeoutSec 5 -ErrorAction Stop
} catch {
    Write-Log "ERROR: Failed to fetch virtual printers: $_"
    exit 2
}

if (-not $vps -or $vps.Count -eq 0) {
    Write-Log "No virtual printers configured — nothing to reconcile."
    exit 0
}

Write-Log "Found $($vps.Count) virtual printer(s) to reconcile."

# Step 3: Reconcile each.
foreach ($vp in $vps) {
    $name = $vp.display_name
    $ippName = $vp.ipp_name
    $url = "http://127.0.0.1:$IppPort/printers/$ippName"

    $existing = Get-Printer -Name $name -ErrorAction SilentlyContinue
    if ($existing -and $existing.PortName -eq $url) {
        Write-Log "  OK: '$name' -> $($existing.PortName)"
        continue
    }

    Write-Log "  RECONCILE: '$name' needs re-registration (existing port=$($existing.PortName))"

    # Clean stale printer + port to prevent rundll32's silent no-op on collision.
    Get-Printer -Name $name -ErrorAction SilentlyContinue | Remove-Printer -ErrorAction SilentlyContinue
    Get-PrinterPort -Name $url -ErrorAction SilentlyContinue | Remove-PrinterPort -ErrorAction SilentlyContinue

    # Probe the IPP endpoint — rundll32 /if silently fails if the endpoint
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
        continue
    }

    # Register the printer.
    $ifArgs = "/if /b `"$name`" /r `"$url`" /m `"Microsoft IPP Class Driver`" /q"
    try {
        Start-Process -FilePath rundll32.exe -ArgumentList "printui.dll,PrintUIEntry $ifArgs" `
            -Wait -NoNewWindow -ErrorAction Stop
    } catch {
        Write-Log "    ERR '$name': rundll32 failed: $_"
        continue
    }

    # rundll32 is async inside the spooler — poll for up to 15s.
    $registered = $false
    for ($i = 1; $i -le 15; $i++) {
        Start-Sleep 1
        $verify = Get-Printer -Name $name -ErrorAction SilentlyContinue
        if ($verify -and $verify.PortName -eq $url) {
            Write-Log "    OK '$name' registered after ${i}s -> $($verify.PortName)"
            $registered = $true
            break
        }
    }
    if (-not $registered) {
        Write-Log "    ERR '$name': did not appear after rundll32 (driver conflict or spooler issue)"
    }
}

Write-Log "=== register-virtual-printers done ==="
