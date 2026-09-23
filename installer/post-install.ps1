# DevBridge Post-Install Configuration
# Run after NSIS installer to configure service, certs, and tray app auto-start.
# Idempotent: safe to run on upgrades (stops service first, updates config, restarts).
#
# Usage:
#   .\post-install.ps1 -Mode server -IppPort 631 -GrpcPort 50051 -DashboardPort 9120
#   .\post-install.ps1 -Mode client -ServerHost print-server.lan -TargetPrinter "EPSON L3270"
#   .\post-install.ps1 -Mode server -SerialBridges "pjkeb-client=COM20,pjsln-client=COM22"

param(
    [Parameter(Mandatory)][ValidateSet("server", "client")][string]$Mode,
    [string]$InstallDir = "C:\Program Files\DevBridge",
    [string]$DataDir = "C:\ProgramData\DevBridge",
    [string]$ServerHost = "print-server.lan",
    [string]$TargetPrinter = "Microsoft Print to PDF",
    [int]$IppPort = 631,
    [int]$GrpcPort = 50051,
    [int]$DashboardPort = 9120,
    [string]$PrinterName = "DevBridge",
    [string]$ClientId = "",
    [string]$VirtualPrinterName = "",
    [string]$PrinterDisplayName = "",
    [string]$PrintBackend = "",
    [string]$PrinterAddress = "",
    [switch]$PrinterTls,
    [string]$GhostscriptDevice = "",
    [int]$GhostscriptResolution = 0,
    [string]$SerialPort = "",
    [int]$SerialBaudRate = 9600,
    [string]$SerialBridges = ""
)

$ErrorActionPreference = "Stop"

# ── Upgrade-hardening helpers ───────────────────────────────────────────────
# Defined inline (NOT dot-sourced): post-install.ps1 is bundled as a lone Tauri
# resource and relocated under _up_\_up_\ at install time, so a sibling lib path
# is not reliable. The Pester suite (installer/tests/installer-lib.Tests.ps1)
# extracts these function bodies FROM this very file via the PowerShell AST --
# the tested code IS the production code, with no second copy to drift. See
# issue #47.

# Permissive parse of DEVBRIDGE_FORCE_CONFIG_REWRITE: "true"/"1"/"yes"/"on"
# (case-insensitive, surrounding whitespace tolerated) opt in; anything else is
# NOT a force-rewrite.
function Test-DevBridgeForceRewrite {
    param([string]$Value)
    return [bool]($Value -match '^\s*(true|1|yes|on)\s*$')
}

# Decide what post-install does with config.toml given existence + force flag:
# "preserve" | "rewrite-existing" | "write-fresh".
function Get-DevBridgeConfigAction {
    param(
        [bool]$ExistingConfig,
        [bool]$ForceRewrite
    )
    if ($ExistingConfig -and -not $ForceRewrite) { return "preserve" }
    if ($ExistingConfig -and $ForceRewrite) { return "rewrite-existing" }
    return "write-fresh"
}

# Snapshot the current config (Prefix + timestamp) and, when KeepCount > 0,
# prune to the N most recent matching snapshots. Returns the snapshot path, or
# $null if the copy failed (caller treats a snapshot failure as non-fatal:
# config still preserved, just no backup).
function New-DevBridgeConfigSnapshot {
    param(
        [Parameter(Mandatory)][string]$ConfigPath,
        [Parameter(Mandatory)][string]$DataDir,
        [Parameter(Mandatory)][string]$Prefix,
        [int]$KeepCount = 0
    )
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $backup = Join-Path $DataDir ("{0}{1}" -f $Prefix, $stamp)
    $created = $null
    try {
        Copy-Item -Path $ConfigPath -Destination $backup -Force -ErrorAction Stop
        $created = $backup
    } catch {
        # Non-fatal: config is left in place. Log and still prune below so a
        # copy failure does not skip pruning (matches pre-refactor behavior).
        Write-Warning ("  Config snapshot copy failed ({0}): {1}" -f $backup, $_)
    }
    if ($KeepCount -gt 0) {
        Get-ChildItem -Path $DataDir -Filter ("{0}*" -f $Prefix) -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending |
            Select-Object -Skip $KeepCount |
            Remove-Item -Force -ErrorAction SilentlyContinue
    }
    return $created
}

# Build the exact 4-line [client.serial_bridge] TOML block (issue #68). Pure
# function -- no file I/O -- shared by Get-DevBridgeClientConfigExtras (fresh
# config) and Merge-DevBridgeSerialBridgeIntoConfig (preserve-branch splice,
# review finding F1) so the block text is defined exactly once. See
# devbridge-core::config::SerialBridgeClientConfig for the TOML shape this
# must match exactly (enabled/port/baud_rate).
function Get-DevBridgeSerialBridgeToml {
    param(
        [Parameter(Mandatory)][string]$SerialPort,
        [int]$SerialBaudRate = 9600
    )
    return @(
        "[client.serial_bridge]",
        "enabled = true",
        "port = `"$SerialPort`"",
        "baud_rate = $SerialBaudRate"
    ) -join "`n"
}

# Build the optional [client]-section field lines emitted into config.toml,
# plus (issue #68) the [client.serial_bridge] block when a serial port is
# configured. Only fields the caller actually passed produce output -- this
# keeps a bare upgrade / no-env-var install byte-identical to before.
#
# INVARIANT (review finding F7): scalar [client] keys MUST stay first, the
# [client.serial_bridge] sub-table MUST stay LAST -- the caller (the
# here-string near [jobs] below) splices this return value directly ahead of
# `[jobs]` with nothing appended after it. Adding a new scalar field after
# the serial_bridge block here, or appending text after this function's
# result at the call site, would land it INSIDE the [client.serial_bridge]
# TOML table instead of the [client] table.
function Get-DevBridgeClientConfigExtras {
    param(
        [string]$ClientId = "",
        [string]$PrinterDisplayName = "",
        [string]$PrintBackend = "",
        [string]$PrinterAddress = "",
        [switch]$PrinterTls,
        [string]$GhostscriptDevice = "",
        [int]$GhostscriptResolution = 0,
        [string]$VirtualPrinterName = "",
        [string]$SerialPort = "",
        [int]$SerialBaudRate = 9600
    )
    $lines = @()
    if ($ClientId) { $lines += "client_id = `"$ClientId`"" }
    if ($PrinterDisplayName) { $lines += "printer_display_name = `"$PrinterDisplayName`"" }
    if ($PrintBackend) { $lines += "print_backend = `"$PrintBackend`"" }
    if ($PrinterAddress) { $lines += "printer_address = `"$PrinterAddress`"" }
    if ($PrinterTls) { $lines += "printer_tls = true" }
    if ($GhostscriptDevice) { $lines += "ghostscript_device = `"$GhostscriptDevice`"" }
    if ($GhostscriptResolution -gt 0) { $lines += "ghostscript_resolution = $GhostscriptResolution" }
    if ($VirtualPrinterName) { $lines += "virtual_printer_name = `"$VirtualPrinterName`"" }
    if ($SerialPort) {
        $lines += ""
        $lines += (Get-DevBridgeSerialBridgeToml -SerialPort $SerialPort -SerialBaudRate $SerialBaudRate)
    }
    return ($lines -join "`n")
}

# Merge [client.serial_bridge] into an EXISTING (preserved) config.toml on
# upgrade (issue #68 review F1). Before this, the preserve branch returned
# before the client here-string ever ran, so -SerialPort was silently
# dropped on every existing install -- the exact target of this feature.
#
# Returns 'added' | 'kept' | 'skipped':
#   'skipped' - no -SerialPort given; file untouched.
#   'kept'    - -SerialPort given but the preserved config already has a
#               [client.serial_bridge] section; file untouched (values are
#               NEVER overwritten here -- use DEVBRIDGE_FORCE_CONFIG_REWRITE
#               to regenerate the whole file instead).
#   'added'   - -SerialPort given, no existing section; the block is spliced
#               in before `[jobs]` if present, else appended at the end.
#               Written UTF-8 without BOM, preserving the file's existing
#               line-ending style (CRLF vs LF).
function Merge-DevBridgeSerialBridgeIntoConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [string]$SerialPort = "",
        [int]$SerialBaudRate = 9600
    )
    if (-not $SerialPort) {
        return "skipped"
    }
    $raw = [System.IO.File]::ReadAllText($Path)
    if ($raw -match '(?m)^\[client\.serial_bridge\]') {
        return "kept"
    }

    # Preserve the file's existing line-ending style rather than imposing one.
    if ($raw -match "`r`n") { $eol = "`r`n" } elseif ($raw -match "`n") { $eol = "`n" } else { $eol = "`r`n" }
    $block = (Get-DevBridgeSerialBridgeToml -SerialPort $SerialPort -SerialBaudRate $SerialBaudRate) -replace "`n", $eol

    $jobsMatch = [regex]::Match($raw, '(?m)^\[jobs\]')
    if ($jobsMatch.Success) {
        $before = $raw.Substring(0, $jobsMatch.Index).TrimEnd()
        $after = $raw.Substring($jobsMatch.Index)
        $newContent = $before + $eol + $eol + $block + $eol + $eol + $after
    } else {
        $newContent = $raw.TrimEnd() + $eol + $eol + $block + $eol
    }

    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $newContent, $utf8NoBom)
    return "added"
}

# Parse DEVBRIDGE_SERIAL_BRIDGES (issue #69): comma list of `client_id=COMn[:baud]`
# (baud default 9600), e.g. "pjkeb-client=COM20,pjsln-client=COM22:19200". Pure;
# emits one [pscustomobject]@{ClientId; VirtualPort; BaudRate} per entry (callers
# wrap in @()). Throws on a malformed entry, zero baud, or a duplicate client_id
# (case-sensitive, like the Rust HashMap) / virtual port, so a typo fails BEFORE
# any change. DEFINED IDENTICALLY in install.ps1 and post-install.ps1 (neither
# can dot-source the other); Pester asserts the two copies are byte-identical.
function ConvertFrom-DevBridgeSerialBridgesSpec {
    param([AllowNull()][AllowEmptyString()][string]$Spec)
    $entries = @()
    if (-not $Spec -or -not $Spec.Trim()) {
        return $entries
    }
    $seenClients = [System.Collections.Hashtable]::new([System.StringComparer]::Ordinal)
    $seenPorts = @{}
    foreach ($part in ($Spec -split ',')) {
        $item = $part.Trim()
        if (-not $item) {
            continue
        }
        if ($item -notmatch '^([A-Za-z0-9][A-Za-z0-9._-]*)\s*=\s*(COM[1-9][0-9]{0,2})(?:\s*:\s*([0-9]{1,7}))?$') {
            throw "DEVBRIDGE_SERIAL_BRIDGES entry '$item' is malformed (expected client_id=COMn or client_id=COMn:baud)"
        }
        $clientId = $Matches[1]
        $port = $Matches[2].ToUpperInvariant()
        $baud = 9600
        if ($Matches[3]) {
            $baud = [int]$Matches[3]
        }
        if ($baud -le 0) {
            throw "DEVBRIDGE_SERIAL_BRIDGES entry '$item' has an invalid baud rate (must be a positive integer)"
        }
        if ($seenClients.ContainsKey($clientId)) {
            throw "DEVBRIDGE_SERIAL_BRIDGES lists client_id '$clientId' more than once"
        }
        if ($seenPorts.ContainsKey($port)) {
            throw "DEVBRIDGE_SERIAL_BRIDGES maps virtual port $port more than once"
        }
        $seenClients[$clientId] = $true
        $seenPorts[$port] = $true
        $entries += [pscustomobject]@{ ClientId = $clientId; VirtualPort = $port; BaudRate = $baud }
    }
    return $entries
}

# [[server.serial_bridges]] TOML blocks (issue #69), one per entry, blank line
# between, "`n" endings. Pure. Shape = devbridge-core SerialBridgeServerEntry.
function Get-DevBridgeServerSerialBridgesToml {
    param([AllowNull()][AllowEmptyCollection()][object[]]$Entries = @())
    $blocks = @()
    foreach ($e in @($Entries | Where-Object { $_ })) {
        $blocks += (@(
            "[[server.serial_bridges]]",
            "client_id = `"$($e.ClientId)`"",
            "virtual_port = `"$($e.VirtualPort)`"",
            "baud_rate = $($e.BaudRate)"
        ) -join "`n")
    }
    return ($blocks -join "`n`n")
}

# Append the blocks to a FRESH server config (issue #69) at the very END: an
# array-of-tables after [jobs] is valid TOML (core test
# test_serial_bridge_server_entries parses this layout) and matches pz-server.
# No entries -> text unchanged (a no-env install stays byte-identical).
function Add-DevBridgeServerSerialBridgesToConfig {
    param(
        [Parameter(Mandatory)][string]$Config,
        [AllowNull()][AllowEmptyCollection()][object[]]$Entries = @()
    )
    $requested = @($Entries | Where-Object { $_ })
    if ($requested.Count -eq 0) {
        return $Config
    }
    return $Config.TrimEnd() + "`n`n" + (Get-DevBridgeServerSerialBridgesToml -Entries $requested) + "`n"
}

# Merge mappings into a PRESERVED server config.toml (issue #69), the server
# sibling of Merge-DevBridgeSerialBridgeIntoConfig. An existing mapping is NEVER
# modified. Returns [pscustomobject]:
#   Added     - client_ids appended
#   Kept      - client_ids already mapped (left as-is even if port/baud differ)
#   Conflicts - @{ClientId; VirtualPort; ExistingClientId}: NOT added because the
#               port is already mapped to another client (bridges would fight)
#   Refused   - $true when the file declares `serial_bridges = [...]` inline:
#               appending [[server.serial_bridges]] would be a duplicate TOML key
#               (server would not start), so nothing is written
# New blocks go right after the LAST existing [[server.serial_bridges]] table
# (array stays contiguous), else at the end. Read/written as UTF-8 (no BOM) --
# NOT Get-Content, which PS 5.1 reads as ANSI; the detected EOL is used for
# every line (a mixed-EOL file is normalized to it). Untouched unless Added.
function Merge-DevBridgeServerSerialBridgesIntoConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [AllowNull()][AllowEmptyCollection()][object[]]$Entries = @()
    )
    $result = [pscustomobject]@{ Added = @(); Kept = @(); Conflicts = @(); Refused = $false }
    $requested = @($Entries | Where-Object { $_ })
    if ($requested.Count -eq 0) {
        return $result
    }

    $raw = [System.IO.File]::ReadAllText($Path)
    if ($raw -match "`r`n") { $eol = "`r`n" } elseif ($raw -match "`n") { $eol = "`n" } else { $eol = "`r`n" }
    $lines = $raw -split "`r?`n"

    # Scan existing [[server.serial_bridges]] tables: client_id -> port, and the
    # line the last table ends on. Values may be basic ("") or literal ('') strings.
    $existingByClient = [System.Collections.Hashtable]::new([System.StringComparer]::Ordinal)
    $existingByPort = @{}
    $inBridgeTable = $false
    $lastBridgeLine = -1
    $tableClient = $null
    $tablePort = $null
    for ($i = 0; $i -le $lines.Count; $i++) {
        $line = if ($i -lt $lines.Count) { $lines[$i] } else { "[end-of-file]" }
        if ($line -match '^\s*serial_bridges\s*=') {
            $result.Refused = $true
            return $result
        }
        if ($line -match '^\s*\[') {
            if ($inBridgeTable -and $tableClient) {
                $existingByClient[$tableClient] = $tablePort
                if ($tablePort) { $existingByPort[$tablePort] = $tableClient }
            }
            $inBridgeTable = ($line -match '^\s*\[\[\s*server\.serial_bridges\s*\]\]')
            $tableClient = $null
            $tablePort = $null
            if ($inBridgeTable) { $lastBridgeLine = $i }
            continue
        }
        if (-not $inBridgeTable) {
            continue
        }
        if ($line.Trim()) { $lastBridgeLine = $i }
        if ($line -match '^\s*client_id\s*=\s*["'']([^"'']*)["'']') { $tableClient = $Matches[1] }
        if ($line -match '^\s*virtual_port\s*=\s*["'']([^"'']*)["'']') { $tablePort = $Matches[1].ToUpperInvariant() }
    }

    $toAdd = @()
    foreach ($e in $requested) {
        if ($existingByClient.ContainsKey($e.ClientId)) {
            $result.Kept += $e.ClientId
        } elseif ($existingByPort.ContainsKey($e.VirtualPort)) {
            $result.Conflicts += [pscustomobject]@{ ClientId = $e.ClientId; VirtualPort = $e.VirtualPort; ExistingClientId = $existingByPort[$e.VirtualPort] }
        } else {
            $toAdd += $e
        }
    }
    if ($toAdd.Count -eq 0) {
        return $result
    }

    $block = (Get-DevBridgeServerSerialBridgesToml -Entries $toAdd) -replace "`n", $eol
    if ($lastBridgeLine -ge 0) {
        $newContent = (($lines[0..$lastBridgeLine]) -join $eol) + $eol + $eol + $block
        if ($lastBridgeLine -lt ($lines.Count - 1)) {
            $newContent += $eol + (($lines[($lastBridgeLine + 1)..($lines.Count - 1)]) -join $eol)
        } else {
            $newContent += $eol
        }
    } else {
        $newContent = $raw.TrimEnd() + $eol + $eol + $block + $eol
    }

    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $newContent, $utf8NoBom)
    $result.Added = @($toAdd | ForEach-Object { $_.ClientId })
    return $result
}

# Warnings (issue #69) for requested ports missing from this machine, each with
# the exact com0com command (B side = A + 1, what Codex reads). Pure: the caller
# passes the SERIALCOMM port names. Pairs are deliberately NOT auto-created -- a
# driver change on the prod server could renumber/steal the live scanner ports.
function Get-DevBridgeCom0comMissingPortWarnings {
    param(
        [AllowNull()][AllowEmptyCollection()][object[]]$Entries = @(),
        [AllowNull()][AllowEmptyCollection()][string[]]$ExistingPorts = @()
    )
    $warnings = @()
    foreach ($e in @($Entries | Where-Object { $_ })) {
        if (@($ExistingPorts) -contains $e.VirtualPort) {
            continue
        }
        $pairB = "COM{0}" -f ([int]($e.VirtualPort -replace '^COM', '') + 1)
        $warnings += ("Serial bridge port {0} (client_id '{1}') does not exist on this machine -- create the com0com pair from C:\ProgramData\DevBridge\tools\com0com\ : setupc.exe --silent install PortName={0},EmuBR=yes PortName={2},EmuBR=yes" -f $e.VirtualPort, $e.ClientId, $pairB)
    }
    return $warnings
}

$serviceExe = Join-Path $InstallDir "devbridge-service.exe"
$trayExe = Join-Path $InstallDir "devbridge-app.exe"
if (-not (Test-Path $trayExe)) {
    $trayExe = Join-Path $InstallDir "DevBridge.exe"
}

Write-Host "=== DevBridge Post-Install - $Mode mode ===" -ForegroundColor Cyan

# -- Server serial-bridge mappings (issue #69) -----------------------------
# Parsed BEFORE any destructive action: a malformed -SerialBridges exits here
# and leaves the running service untouched.
$serialBridgeEntries = @()
if ($SerialBridges) {
    if ($Mode -eq "server") {
        try {
            $serialBridgeEntries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec $SerialBridges)
        } catch {
            Write-Host "ERROR: $($_.Exception.Message)" -ForegroundColor Red
            [Console]::Error.WriteLine("ERROR: $($_.Exception.Message)")
            exit 1
        }
        $mappingText = ($serialBridgeEntries | ForEach-Object { "{0}->{1}@{2}" -f $_.ClientId, $_.VirtualPort, $_.BaudRate }) -join ", "
        Write-Host "  Serial bridge mappings requested ($($serialBridgeEntries.Count)): $mappingText"
    } else {
        Write-Warning "-SerialBridges (DEVBRIDGE_SERIAL_BRIDGES) is a server-mode setting; ignored in client mode"
    }
}

# -- Validate configuration BEFORE any destructive action -----------------
# Runs before the Stop-Service block so a failed install is a no-op:
# if validation fails, the existing service is left running untouched.
# See docs/superpowers/specs/2026-04-10-installer-hardening-design.md
#
# Skip when an existing config.toml will be preserved (bare upgrade flow).
# Validation here uses param defaults (TargetPrinter, ServerHost, etc.)
# which on a no-env-var upgrade are "Microsoft Print to PDF" /
# "print-server.lan" -- testing the wrong values would always fail and
# would leave the service stopped after the binary swap. The preserved
# config has already been validated by the running service; nothing to
# re-prove. Validation still runs on fresh install or forced rewrite.
$preservedExistingConfig = (Test-Path (Join-Path $DataDir "config.toml")) -and
    (-not (Test-DevBridgeForceRewrite $env:DEVBRIDGE_FORCE_CONFIG_REWRITE))
if ($preservedExistingConfig) {
    Write-Host "  Skipping validation: config will be preserved from previous install." -ForegroundColor Cyan
}

if ($Mode -eq "client" -and -not $preservedExistingConfig) {
    $effectiveBackend = if ($PrintBackend) { $PrintBackend } else { "windows_spooler" }

    # 1. direct_ipp port auto-append (closes #16)
    # IPP default port is 631 per RFC 8011 section 5.
    if ($effectiveBackend -eq "direct_ipp" -and $PrinterAddress -and
        ($PrinterAddress -notmatch ':') -and ($PrinterAddress -notmatch '/')) {
        $corrected = "${PrinterAddress}:631"
        Write-Warning "printer_address auto-corrected to $corrected (default IPP port per RFC 8011)"
        $PrinterAddress = $corrected
    }

    # 2. windows_spooler printer name validation (closes #17)
    if ($effectiveBackend -eq "windows_spooler" -or $effectiveBackend -eq "") {
        $installedPrinters = @(Get-Printer -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name)
        if ($installedPrinters.Count -eq 0) {
            Write-Host ""
            Write-Host "ERROR: No printers installed on this machine." -ForegroundColor Red
            Write-Host "  Install the printer driver before configuring DevBridge." -ForegroundColor Red
            Write-Host "  Suggestion: open Settings -> Bluetooth & devices -> Printers & scanners," -ForegroundColor Yellow
            Write-Host "              add the printer, then re-run the installer." -ForegroundColor Yellow
            [Console]::Error.WriteLine("ERROR: No printers installed on this machine (target_printer=`"$TargetPrinter`")")
            exit 1
        }
        $exactMatch = $installedPrinters | Where-Object { $_ -ieq $TargetPrinter }
        if (-not $exactMatch) {
            Write-Host ""
            Write-Host "ERROR: target_printer `"$TargetPrinter`" not found on this machine." -ForegroundColor Red
            Write-Host "  Available printers:" -ForegroundColor Red
            foreach ($p in $installedPrinters) {
                Write-Host "    - $p" -ForegroundColor Red
            }
            # Only suggest a printer if there's a real substring overlap in
            # either direction; otherwise we risk pointing at an arbitrary
            # printer and confusing the operator.
            $suggestion = $installedPrinters |
                Where-Object { $_ -like "*$TargetPrinter*" -or $TargetPrinter -like "*$_*" } |
                Select-Object -First 1
            if ($suggestion) {
                Write-Host "  Suggestion: re-run installer with " -NoNewline -ForegroundColor Yellow
                Write-Host "`$env:DEVBRIDGE_TARGET_PRINTER = `"$suggestion`"" -ForegroundColor Yellow
            } else {
                Write-Host "  Suggestion: pick one of the names above and re-run with " -NoNewline -ForegroundColor Yellow
                Write-Host "`$env:DEVBRIDGE_TARGET_PRINTER = `"<name>`"" -ForegroundColor Yellow
            }
            [Console]::Error.WriteLine("ERROR: target_printer `"$TargetPrinter`" not found on this machine")
            exit 1
        }
        Write-Host "  Validated target_printer: $TargetPrinter" -ForegroundColor Green
    }

    # 3. gRPC connectivity test (closes #21 main scope)
    Write-Host "  Probing gRPC server at ${ServerHost}:${GrpcPort}..."
    $tcp = Test-NetConnection -ComputerName $ServerHost -Port $GrpcPort `
        -InformationLevel Quiet -WarningAction SilentlyContinue
    if (-not $tcp) {
        Write-Host ""
        Write-Host "ERROR: gRPC server unreachable at ${ServerHost}:${GrpcPort}." -ForegroundColor Red
        Write-Host "  TCP connection timed out." -ForegroundColor Red
        Write-Host "  Suggestion: verify VPN is connected (e.g. wg show), and that the" -ForegroundColor Yellow
        Write-Host "              DevBridge service is running on the server." -ForegroundColor Yellow
        [Console]::Error.WriteLine("ERROR: gRPC server unreachable at ${ServerHost}:${GrpcPort}")
        exit 1
    }
    Write-Host "  gRPC server reachable" -ForegroundColor Green
}

# -- Stop existing instance if upgrading --------------------------------------
$taskName = "DevBridgeService"
$existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($existingTask -and $existingTask.State -eq "Running") {
    Write-Host "Stopping existing scheduled task for upgrade..."
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
}
Stop-Process -Name "devbridge-service" -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

# -- Create data directory structure -----------------------------------------
$subdirs = @("certs", "spool", "logs")
foreach ($sub in $subdirs) {
    $path = Join-Path $DataDir $sub
    if (-not (Test-Path $path)) {
        New-Item -ItemType Directory -Force -Path $path | Out-Null
        Write-Host "  Created $path"
    }
}

# -- Firewall rules --------------------------------------------------------
Write-Host "Configuring firewall rules..."
$fwRules = @(
    @{ Name="DevBridge-Dashboard"; Port=$DashboardPort }
)
if ($Mode -eq "server") {
    $fwRules += @{ Name="DevBridge-gRPC"; Port=$GrpcPort }
    $fwRules += @{ Name="DevBridge-IPP"; Port=$IppPort }
}
foreach ($rule in $fwRules) {
    $existing = Get-NetFirewallRule -DisplayName $rule.Name -ErrorAction SilentlyContinue
    if (-not $existing) {
        New-NetFirewallRule -DisplayName $rule.Name -Direction Inbound `
            -Protocol TCP -LocalPort $rule.Port -Action Allow | Out-Null
        Write-Host "  Created firewall rule: $($rule.Name) (port $($rule.Port))"
    } else {
        Write-Host "  Firewall rule exists: $($rule.Name)"
    }
}
# Also allow the service binary itself (some firewalls block by executable)
$fwBinaryRule = "DevBridge-Service"
if (-not (Get-NetFirewallRule -DisplayName $fwBinaryRule -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName $fwBinaryRule -Direction Inbound `
        -Program $serviceExe -Action Allow | Out-Null
    Write-Host "  Created firewall rule: $fwBinaryRule (binary)"
}

# -- Check/install prerequisites -------------------------------------------
# VC++ Runtime is required for the Rust binary
$vcInstalled = Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64" -ErrorAction SilentlyContinue
if (-not $vcInstalled) {
    $vcPath = Join-Path $InstallDir "redist\vc_redist.x64.exe"
    if (Test-Path $vcPath) {
        Write-Host "Installing VC++ Runtime..."
        Start-Process -FilePath $vcPath -ArgumentList "/install /quiet /norestart" -Wait
        Write-Host "  VC++ Runtime installed" -ForegroundColor Green
    } else {
        Write-Warning "VC++ Runtime not found. Binary may fail with STATUS_DLL_NOT_FOUND."
    }
}
# SumatraPDF is used for headless PDF printing on client
$sumatraTarget = "C:\Program Files\SumatraPDF\SumatraPDF.exe"
if (-not (Test-Path $sumatraTarget)) {
    $sumatraBundled = Join-Path $InstallDir "redist\SumatraPDF.exe"
    if (Test-Path $sumatraBundled) {
        Write-Host "Installing SumatraPDF..."
        New-Item -ItemType Directory -Force -Path "C:\Program Files\SumatraPDF" | Out-Null
        Copy-Item $sumatraBundled $sumatraTarget
        Write-Host "  SumatraPDF installed" -ForegroundColor Green
    }
}
# Ghostscript portable (for direct print backends)
$gsTarget = Join-Path $InstallDir "ghostscript"
if (-not (Test-Path (Join-Path $gsTarget "bin\gswin64c.exe"))) {
    # Search multiple locations: direct redist/ and Tauri _up_/_up_/ resource paths
    $gsCandidates = @(
        (Join-Path $InstallDir "redist\ghostscript"),
        (Join-Path $InstallDir "_up_\_up_\installer\redist\ghostscript")
    )
    $gsBundled = $gsCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($gsBundled) {
        Write-Host "Installing Ghostscript portable from $gsBundled..."
        Copy-Item -Recurse -Force $gsBundled $gsTarget
        Write-Host "  Ghostscript installed to $gsTarget" -ForegroundColor Green
    }
}

# -- Write configuration ----------------------------------------------------
# CRITICAL: Existing config.toml is PRESERVED on upgrade. The bare
# `irm install.ps1 | iex` upgrade flow does NOT pass any DEVBRIDGE_*
# env vars (operators just run the installer to bump the binary), so
# overwriting the config with defaults wipes per-store target_printer,
# print_backend, printer_address, virtual_printer_name, serial_bridge
# config, etc. -- and a server-default `mode = "server"` would convert
# every retail-store client into a broken local server. Lost a half-day
# of production on 2026-04-24 to this exact mistake.
#
# To rewrite the config (rare: actual reconfigure, not a binary upgrade)
# the operator either deletes config.toml first OR sets
# DEVBRIDGE_FORCE_CONFIG_REWRITE=true.
$configPath = Join-Path $DataDir "config.toml"
$existingConfig = Test-Path $configPath
# Permissive parse: "true"/"True"/"TRUE"/"1"/"yes"/"on" all opt in (see
# Test-DevBridgeForceRewrite above). A typo or unknown value is logged so the
# operator gets immediate feedback instead of a silent no-op.
$rewriteEnv = $env:DEVBRIDGE_FORCE_CONFIG_REWRITE
$forceRewrite = Test-DevBridgeForceRewrite $rewriteEnv
if ($rewriteEnv -and -not $forceRewrite) {
    Write-Host "  DEVBRIDGE_FORCE_CONFIG_REWRITE='$rewriteEnv' was ignored (expected true/1/yes/on)." -ForegroundColor Yellow
}

$configAction = Get-DevBridgeConfigAction -ExistingConfig $existingConfig -ForceRewrite $forceRewrite
if ($configAction -eq "preserve") {
    Write-Host "  Existing config preserved at $configPath" -ForegroundColor Cyan
    Write-Host "  (set `$env:DEVBRIDGE_FORCE_CONFIG_REWRITE = 'true' to overwrite)" -ForegroundColor DarkGray
    # Stamp a backup of the current config alongside, so the operator has a
    # recoverable snapshot if a future installer ever does something
    # destructive. Then prune to the 5 most recent so years of weekly
    # upgrades don't accumulate hundreds of identical snapshots.
    $backup = New-DevBridgeConfigSnapshot -ConfigPath $configPath -DataDir $DataDir `
        -Prefix "config.toml.preupgrade-" -KeepCount 5
    if ($backup) {
        Write-Host "  Snapshot: $backup" -ForegroundColor DarkGray
    } else {
        Write-Warning "  Snapshot failed (config preserved but no backup written)"
    }

    # issue #68 review F1: -SerialPort must not be silently dropped just
    # because the config is being preserved on an upgrade.
    $serialMergeResult = Merge-DevBridgeSerialBridgeIntoConfig -Path $configPath `
        -SerialPort $SerialPort -SerialBaudRate $SerialBaudRate
    if ($serialMergeResult -eq "added") {
        Write-Host "Added [client.serial_bridge] (port=$SerialPort, baud=$SerialBaudRate) to preserved config" -ForegroundColor Green
    } elseif ($serialMergeResult -eq "kept") {
        Write-Warning "Existing [client.serial_bridge] section kept in preserved config (values not overwritten); set `$env:DEVBRIDGE_FORCE_CONFIG_REWRITE = 'true' to regenerate"
    }

    # issue #69: server mappings merged the same way (existing ones never touched).
    if ($serialBridgeEntries.Count -gt 0) {
        $bridgeMerge = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $configPath -Entries $serialBridgeEntries
        if ($bridgeMerge.Refused) {
            Write-Warning "$configPath declares serial_bridges inline (serial_bridges = [...]); requested mappings NOT merged -- convert it to [[server.serial_bridges]] tables"
        }
        foreach ($id in $bridgeMerge.Added) { Write-Host "Added [[server.serial_bridges]] mapping for client_id '$id' to preserved config" -ForegroundColor Green }
        foreach ($id in $bridgeMerge.Kept) { Write-Host "  Serial bridge mapping for client_id '$id' already present -- kept unchanged" -ForegroundColor Cyan }
        foreach ($c in $bridgeMerge.Conflicts) {
            Write-Warning "Serial bridge mapping $($c.ClientId) -> $($c.VirtualPort) NOT added: $($c.VirtualPort) is already mapped to client_id '$($c.ExistingClientId)'"
        }
    }
} else {
    if ($configAction -eq "rewrite-existing") {
        $backup = New-DevBridgeConfigSnapshot -ConfigPath $configPath -DataDir $DataDir `
            -Prefix "config.toml.replaced-"
        if ($backup) {
            Write-Host "  DEVBRIDGE_FORCE_CONFIG_REWRITE=true; previous config saved to $backup" -ForegroundColor Yellow
        } else {
            Write-Warning "  Pre-rewrite snapshot failed (proceeding anyway)"
        }
    }

    # Use debug logging in CI for easier troubleshooting
    if ($env:CI) { $logLevel = "debug" } else { $logLevel = "info" }
    # Use forward slashes in TOML to avoid escaping issues
    $tomlData = $DataDir -replace '\\', '/'

    if ($Mode -eq "server") {
        $config = @"
[general]
mode = "server"
log_level = "$logLevel"
data_dir = "$tomlData"

[server]
ipp_port = $IppPort
grpc_port = $GrpcPort
dashboard_port = $DashboardPort
printer_name = "$PrinterName"
spool_dir = "$tomlData/spool"

[client]
server_address = "127.0.0.1:$GrpcPort"
target_printer = "unused"
dashboard_port = 9121
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
print_timeout_secs = 1800
"@
        # issue #69: [[server.serial_bridges]] blocks go at the END (after [jobs]).
        $config = Add-DevBridgeServerSerialBridgesToConfig -Config $config -Entries $serialBridgeEntries
        if ($serialBridgeEntries.Count -gt 0) {
            Write-Host "  Wrote $($serialBridgeEntries.Count) [[server.serial_bridges]] mapping(s) into the new config" -ForegroundColor Green
        }
    } else {
        $config = @"
[general]
mode = "client"
log_level = "$logLevel"
data_dir = "$tomlData"

[server]
ipp_port = $IppPort
grpc_port = $GrpcPort
dashboard_port = 9121
printer_name = "unused"
spool_dir = "$tomlData/spool"

[client]
server_address = "${ServerHost}:${GrpcPort}"
target_printer = "$TargetPrinter"
dashboard_port = $DashboardPort
reconnect_interval_secs = 5
max_reconnect_interval_secs = 60
$(Get-DevBridgeClientConfigExtras -ClientId $ClientId -PrinterDisplayName $PrinterDisplayName -PrintBackend $PrintBackend -PrinterAddress $PrinterAddress -PrinterTls:$PrinterTls -GhostscriptDevice $GhostscriptDevice -GhostscriptResolution $GhostscriptResolution -VirtualPrinterName $VirtualPrinterName -SerialPort $SerialPort -SerialBaudRate $SerialBaudRate)

[jobs]
max_retries = 3
retry_delay_secs = 30
job_expiry_hours = 24
max_payload_size_mb = 100
print_timeout_secs = 1800
"@
    }

    $config | Set-Content -Path $configPath -Encoding ASCII
    Write-Host "  Config written to $configPath"
}

# -- com0com pair check for requested serial bridge ports (issue #69) --------
# Warn-only: pair creation stays a deliberate manual step (see
# Get-DevBridgeCom0comMissingPortWarnings for why).
if ($serialBridgeEntries.Count -gt 0) {
    $serialComm = Get-ItemProperty -Path "HKLM:\HARDWARE\DEVICEMAP\SERIALCOMM" -ErrorAction SilentlyContinue
    $existingSerialPorts = @()
    if ($serialComm) {
        $existingSerialPorts = @($serialComm.PSObject.Properties |
            Where-Object { $_.Name -notlike 'PS*' } |
            ForEach-Object { [string]$_.Value })
    }
    Write-Host "  Serial ports present on this machine: $($existingSerialPorts -join ', ')"
    foreach ($warning in @(Get-DevBridgeCom0comMissingPortWarnings -Entries $serialBridgeEntries -ExistingPorts $existingSerialPorts)) {
        Write-Warning $warning
    }
}

# -- Start DevBridge via Scheduled Task -------------------------------------
# Scheduled tasks run in a separate process tree, surviving GitHub Actions
# runner cleanup which kills all child processes when jobs end.
Write-Host "Registering DevBridge scheduled task..."
# Use a VBS wrapper to run the service hidden (no console window on desktop)
$vbsPath = Join-Path $installDir "start-hidden.vbs"
$vbsContent = @"
Set WshShell = CreateObject("WScript.Shell")
WshShell.Run """$serviceExe"" --config """"$configPath"""""", 0, False
"@
$vbsContent | Set-Content -Path $vbsPath -Encoding ASCII
# Register as SYSTEM scheduled task -- runs devbridge-service.exe directly (no wscript/VBS
# wrapper). SYSTEM processes are sessionless so no window-hiding needed. This avoids the
# S4U logon error (0x80070520 / code 267009) that occurs when the domain controller is
# unreachable at boot time. See issue #36.
$action = New-ScheduledTaskAction -Execute $serviceExe -Argument "--config `"$configPath`"" -WorkingDirectory $dataDir
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
$settings.IdleSettings.StopOnIdleEnd = $false
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
try {
    Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -Principal $principal -Trigger $trigger | Out-Null
    Start-ScheduledTask -TaskName $taskName
    Start-Sleep -Seconds 5
    $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    if ($proc) {
        Write-Host "  Service started as SYSTEM (PID $($proc.Id))" -ForegroundColor Green
    } else {
        Write-Host "WARNING: Service process not found. Check logs at $dataDir\logs" -ForegroundColor Yellow
    }
} catch {
    Write-Host "  Scheduled task registration failed, starting process directly" -ForegroundColor Yellow
    Start-Process -FilePath $serviceExe -ArgumentList "--config `"$configPath`"" -WindowStyle Hidden
}

$proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
if ($proc) {
    Write-Host "  Service is running (PID: $($proc.Id))" -ForegroundColor Green
} else {
    Write-Warning "Service process not found. Check logs at ${DataDir}\logs"
}

# -- Register DevBridge auto-update scheduled task (issue #54) ----------------
# A SECOND scheduled task, "DevBridgeAutoUpdate", that self-upgrades the client
# on a PATCH-only basis so stores no longer sit on an old version waiting for an
# operator to run `irm | iex`. Runs once AtStartup (POS machines are off outside
# business hours, so boot is the main opportunity) plus every 6 hours while on.
# The script (autoupdate.ps1) owns all the safety guards: kill-switch, patch-only
# lock, skip-if-printing, and fail-safe no-op. It re-runs the hardened install.ps1
# to actually upgrade -- no install logic is duplicated here.
Write-Host "Registering DevBridge auto-update scheduled task..."
# Stage autoupdate.ps1 into ProgramData (like the reconciler) so the task has a
# stable path that survives Program Files upgrades.
$autoUpdateSrcCandidates = @(
    (Join-Path $InstallDir "_up_\_up_\installer\autoupdate.ps1"),
    (Join-Path $InstallDir "autoupdate.ps1")
)
$autoUpdateSrc = $autoUpdateSrcCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
$autoUpdateDst = Join-Path $DataDir "autoupdate.ps1"
if ($autoUpdateSrc) {
    Copy-Item $autoUpdateSrc $autoUpdateDst -Force
    Write-Host "  Staged auto-update script at $autoUpdateDst" -ForegroundColor Cyan

    $autoUpdateTaskName = "DevBridgeAutoUpdate"
    try {
        $auAction = New-ScheduledTaskAction -Execute "powershell.exe" `
            -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$autoUpdateDst`"" `
            -WorkingDirectory $DataDir
        # Two triggers: once at machine startup AND every 6 hours indefinitely.
        $auStartupTrigger = New-ScheduledTaskTrigger -AtStartup
        # Start the 6h repeat 5 min out (not "now"): with -StartWhenAvailable a
        # start time of Get-Date is seen as a missed run and fires once
        # immediately on every re-registration (i.e. after each upgrade). That
        # extra run is a clean no-op, but avoid the needless GitHub API hit
        # (issue #54). Boot is already covered by the AtStartup trigger.
        $auRepeatTrigger = New-ScheduledTaskTrigger -Once -At ((Get-Date).AddMinutes(5)) `
            -RepetitionInterval (New-TimeSpan -Hours 6)
        $auSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries `
            -DontStopIfGoingOnBatteries -StartWhenAvailable `
            -ExecutionTimeLimit (New-TimeSpan -Hours 1)
        $auPrincipal = New-ScheduledTaskPrincipal -UserId "SYSTEM" `
            -LogonType ServiceAccount -RunLevel Highest
        Unregister-ScheduledTask -TaskName $autoUpdateTaskName -Confirm:$false -ErrorAction SilentlyContinue
        Register-ScheduledTask -TaskName $autoUpdateTaskName -Action $auAction `
            -Settings $auSettings -Principal $auPrincipal `
            -Trigger @($auStartupTrigger, $auRepeatTrigger) | Out-Null
        Write-Host "  Registered '$autoUpdateTaskName' (AtStartup + every 6h)" -ForegroundColor Green
    } catch {
        Write-Host "  WARNING: auto-update task registration failed: $_" -ForegroundColor Yellow
    }
} else {
    Write-Host "  WARNING: autoupdate.ps1 not found in installer payload; auto-update task not registered" -ForegroundColor Yellow
}

# -- Register IPP printer in Windows (server mode only) --------------------
# Printer registration is non-fatal: CI runners lack admin access to the
# Print Monitors registry and DriverStore. E2E tests use raw IPP, not Windows printers.
if ($Mode -eq "server") {
  try {
    Write-Host "Registering IPP printer in Windows..."
    $printerName = $PrinterName
    $ippUrl = "http://127.0.0.1:${IppPort}/ipp/print"

    # -- Step 1: Ensure Internet Port monitor (inetpp.dll) is registered ----
    # Windows Server 2019 may not have it registered even though the DLL exists.
    # Without this monitor, printui.dll silently fails to create IPP printers.
    $monitorPath = "HKLM:\SYSTEM\CurrentControlSet\Control\Print\Monitors\Internet Port"
    if (-not (Test-Path $monitorPath)) {
        if (Test-Path "$env:SystemRoot\System32\inetpp.dll") {
            try {
                New-Item -Path $monitorPath -Force | Out-Null
                Set-ItemProperty -Path $monitorPath -Name "Driver" -Value "inetpp.dll"
                Restart-Service Spooler -Force
                Start-Sleep 2
                Write-Host "  Registered Internet Port monitor (inetpp.dll)" -ForegroundColor Cyan
            } catch {
                Write-Host "  Skipping Internet Port monitor registration (no admin access)" -ForegroundColor Yellow
            }
        } else {
            Write-Host "  WARNING: inetpp.dll not found, IPP printer registration may fail" -ForegroundColor Yellow
        }
    }

    # -- Step 2: Repair broken Microsoft printer driver packages ------------
    # After Windows Update, driver packages in DriverStore may point to old
    # hash directories that no longer exist. This causes printui.dll to
    # silently fail (event 368 in PrintService/Operational log).
    $driversToRepair = @(
        @{ Name = "Microsoft IPP Class Driver"; Inf = "prnms012" },
        @{ Name = "Microsoft Software Printer Driver"; Inf = "prnms011" }
    )
    foreach ($drv in $driversToRepair) {
        try {
            $existing = Get-PrinterDriver -Name $drv.Name -ErrorAction SilentlyContinue
            if ($existing -and $existing.InfPath -and -not (Test-Path $existing.InfPath)) {
                Write-Host "  Repairing broken driver '$($drv.Name)' (INF path missing)..." -ForegroundColor Yellow
                $correctInf = Get-ChildItem "$env:SystemRoot\System32\DriverStore\FileRepository\$($drv.Inf)*\$($drv.Inf).inf" -ErrorAction SilentlyContinue |
                    Sort-Object LastWriteTime -Descending | Select-Object -First 1
                if ($correctInf) {
                    Remove-PrinterDriver -Name $drv.Name -ErrorAction SilentlyContinue
                    pnputil /add-driver $correctInf.FullName /install 2>&1 | Out-Null
                    Add-PrinterDriver -Name $drv.Name -InfPath $correctInf.FullName -ErrorAction SilentlyContinue
                    Write-Host "  Repaired '$($drv.Name)' from $($correctInf.Name)" -ForegroundColor Green
                } else {
                    Write-Host "  WARNING: No valid INF found for $($drv.Inf)" -ForegroundColor Yellow
                }
            }
        } catch {
            Write-Host "  Skipping driver repair for '$($drv.Name)' (no admin access)" -ForegroundColor Yellow
        }
    }

    # -- Step 3: Service owns printer registration ------------------------
    # Previous installer versions (<= 0.8.19) registered Windows printers
    # here (querying /api/virtual-printers and shelling out to printui.dll)
    # AND set up a DevBridgeReconcilePrinters scheduled task that re-ran
    # the same logic at boot. That created a startup race: if the service
    # wasn't ready when this block fired, post-install fell back to a
    # legacy "single DevBridge printer" mode and silently broke the
    # multi-store setup.
    #
    # In 0.8.20 the devbridge-service process is the sole owner of
    # Windows-printer registration: it spawns register-virtual-printers.ps1
    # once at startup AND on every virtual-printer DB insert/update.
    # post-install just stages the script; the service runs it.

    # Stage the reconciler script into ProgramData so the service can find it.
    $reconcilerSrc = Join-Path $InstallDir "_up_\_up_\deploy\register-virtual-printers.ps1"
    if (-not (Test-Path $reconcilerSrc)) {
        $reconcilerSrc = Join-Path $InstallDir "register-virtual-printers.ps1"
    }
    $reconcilerDst = Join-Path $DataDir "register-virtual-printers.ps1"
    if (Test-Path $reconcilerSrc) {
        Copy-Item $reconcilerSrc $reconcilerDst -Force
        Write-Host "  Staged reconciler at $reconcilerDst" -ForegroundColor Cyan
    } else {
        Write-Host "  WARNING: register-virtual-printers.ps1 not found in installer payload" -ForegroundColor Yellow
    }

    # -- Step 4: Upgrade cleanup -- unregister stale scheduled task ------
    # 0.8.19 and earlier registered DevBridgeReconcilePrinters AtStartup.
    # 0.8.20+ no longer needs it (service does the same work). Idempotent
    # on fresh installs (no-op if the task doesn't exist).
    Unregister-ScheduledTask -TaskName "DevBridgeReconcilePrinters" `
        -Confirm:$false -ErrorAction SilentlyContinue
  } catch {
    Write-Host "  Printer registration skipped (insufficient permissions: $_)" -ForegroundColor Yellow
  }
}

# -- Tray app auto-start on login --------------------------------------------
if (Test-Path $trayExe) {
    # Try HKLM (all users, requires admin), fall back to HKCU (current user)
    try {
        $regPath = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
        Set-ItemProperty -Path $regPath -Name "DevBridge" -Value "`"$trayExe`""
        Write-Host "  Tray app registered for auto-start (all users)"
    } catch {
        $regPath = "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
        Set-ItemProperty -Path $regPath -Name "DevBridge" -Value "`"$trayExe`""
        Write-Host "  Tray app registered for auto-start (current user only)"
    }

    # Kill any existing tray app to avoid duplicate icons after upgrade
    Get-Process -Name "devbridge-app", "DevBridge" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep 1

    # Launch tray app in EVERY user session -- Active AND Disconnected.
    # Each user gets their own tray instance which filters jobs by username.
    # Disconnected sessions are included so the tray icon is already running
    # when the user reconnects via RDP (HKLM:\Run only fires on fresh logon,
    # not on RDP reconnect to an existing disconnected session).
    # CI/SYSTEM sessions can't show tray icons directly, so we use temporary
    # scheduled tasks that run interactively as each user.
    #
    # `query user` output format (USERNAME is first column):
    #   >drlikzbynek           rdp-tcp#19         60  Active
    #    marketing                                22  Disc
    # Note: `query user` always returns exit code 1 on Windows even when it
    # succeeds, so we explicitly clear $LASTEXITCODE afterwards.
    $sessions = query user 2>$null | Select-Object -Skip 1 | ForEach-Object {
        $line = $_
        if ($line -match '^>?\s*(\S+)\s+.*?\s+(\d+)\s+(Active|Disc)') {
            [PSCustomObject]@{
                Username  = $matches[1]
                SessionId = [int]$matches[2]
                State     = $matches[3]
            }
        }
    } | Where-Object { $_ }
    $global:LASTEXITCODE = 0

    if ($sessions -and $sessions.Count -gt 0) {
        Write-Host "  Launching tray app for $($sessions.Count) active session(s)..."
        foreach ($s in $sessions) {
            $taskName = "DevBridgeTrayStart_$($s.Username)"
            try {
                $action = New-ScheduledTaskAction -Execute $trayExe
                $principal = New-ScheduledTaskPrincipal -UserId $s.Username -LogonType Interactive
                $task = New-ScheduledTask -Action $action -Principal $principal
                Register-ScheduledTask -TaskName $taskName -InputObject $task -Force | Out-Null
                Start-ScheduledTask -TaskName $taskName
                Start-Sleep -Milliseconds 500
                Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
                Write-Host "    [OK] $($s.Username) (session $($s.SessionId))"
            } catch {
                Write-Host "    [FAIL] $($s.Username): $_" -ForegroundColor Yellow
                Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
            }
        }
    } else {
        Write-Host "  No active sessions, tray will start on next login via HKLM:\Run"
    }
} else {
    Write-Host "  Tray app not found at $trayExe, skipping auto-start" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "=== Post-install complete ===" -ForegroundColor Green
Write-Host "  Mode:      $Mode"
Write-Host "  Dashboard: http://localhost:$DashboardPort"
Write-Host "  Data dir:  $DataDir"
$logsDir = Join-Path $DataDir "logs"
Write-Host "  Logs:      $logsDir"
