# DevBridge installer function library (issue #80).
#
# Pure helpers used by installer/post-install.ps1: the config preserve/rewrite
# decision, config snapshots, the client/server serial-bridge TOML builders and
# merges, the DEVBRIDGE_SERIAL_BRIDGES spec parser, the com0com warnings and the
# VC++ runtime DLL check.
#
# SHIPPING: bundled as a Tauri resource NEXT TO post-install.ps1
# (crates/devbridge-app/tauri.conf.json bundle.resources), so both land in
# <InstallDir>\_up_\_up_\installer\. post-install.ps1 dot-sources it from
# $PSScriptRoot and exits 1 BEFORE any change when it is missing. The CI
# packaging gate (deploy/e2e-installer-packaging-gate.ps1, e2e-deploy-client job
# on pz-snv) runs the INSTALLED post-install.ps1 -ValidateOnly under Windows
# PowerShell 5.1 to prove the installed layout loads.
#
# RULES for this file (each asserted by installer/tests/post-install-lib.Tests.ps1):
# - Function definitions ONLY: dot-sourcing it must have no side effects.
# - Pure ASCII: Windows PowerShell 5.1 reads a BOM-less UTF-8 script as ANSI.
# - Windows PowerShell 5.1 compatible (production runs 5.1 as SYSTEM).
# - install.ps1 runs via irm|iex (no files on disk) and can NOT dot-source this
#   file: ConvertFrom-DevBridgeSerialBridgesSpec and
#   Get-DevBridgeMissingVcRuntimeDlls are also defined inline there, and Pester
#   asserts the two copies are byte-identical. Edit one -> copy it verbatim to
#   the other.
# - Tests exercise the REAL code: the Pester suites and the E2E client setup
#   extract these functions via the shared AST extractor
#   (deploy/lib/Get-FunctionSourceFromScript.ps1).

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

# Client [client] values the installer writes verbatim into config.toml
# strings (issue #88). Returns a list of human-readable problems (empty = OK)
# so post-install.ps1 can refuse BEFORE any change:
#   - print_backend must be one the service knows (an unknown one only fails
#     later, at the first print job);
#   - virtual_printer_driver must not contain a quote, backslash or control
#     character (it lands in a TOML string here and in the server's quoted
#     printui.dll /m "<driver>" argument).
function Get-DevBridgeClientConfigProblems {
    param(
        [string]$PrintBackend = "",
        [string]$VirtualPrinterDriver = ""
    )
    $problems = @()
    $knownBackends = @("windows_spooler", "windows_spooler_raw", "direct_ipp", "direct_raw", "print_proxy", "cups")
    if ($PrintBackend -and -not ($knownBackends -ccontains $PrintBackend)) {
        $problems += "unknown print_backend '$PrintBackend' (expected one of: $($knownBackends -join ', '))"
    }
    if ($VirtualPrinterDriver -and ($VirtualPrinterDriver -match '["\\]' -or $VirtualPrinterDriver -match '[\x00-\x1F\x7F]')) {
        $problems += "virtual_printer_driver '$VirtualPrinterDriver' contains a forbidden character (quote, backslash or control character)"
    }
    return ,$problems
}

# Build the optional [client]-section field lines emitted into config.toml,
# plus (issue #68) the [client.serial_bridge] block when a serial port is
# configured. Only fields the caller actually passed produce output -- this
# keeps a bare upgrade / no-env-var install byte-identical to before.
#
# INVARIANT (review finding F7): scalar [client] keys MUST stay first, the
# [client.serial_bridge] sub-table MUST stay LAST -- the caller (the
# here-string near [jobs] in post-install.ps1) splices this return value directly ahead of
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
        [string]$VirtualPrinterDriver = "",
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
    if ($VirtualPrinterDriver) { $lines += "virtual_printer_driver = `"$VirtualPrinterDriver`"" }
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
# any change. DEFINED IDENTICALLY in install.ps1 and this lib (install.ps1 runs
# via irm|iex and cannot dot-source it); Pester asserts the two are byte-identical.
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

# VC++ 2015-2022 runtime check (issue #85): returns the full paths of the runtime
# DLLs missing from -System32 (empty = present). The Rust service binary and the
# bundled Ghostscript (gsdll64.dll) load vcruntime140.dll + msvcp140.dll, so the
# FILES are the signal -- not the VisualStudio\14.0\VC\Runtimes registry key,
# which is absent when the runtime came from another installer (pjkes: DLLs
# present, key absent -> false "VC++ Runtime not found"). Pure. DEFINED
# IDENTICALLY in install.ps1 and DevBridgeInstallerLib.ps1 (install.ps1 runs via
# irm|iex and cannot dot-source the lib); Pester asserts the two are byte-identical.
function Get-DevBridgeMissingVcRuntimeDlls {
    param([Parameter(Mandatory)][string]$System32)
    $missing = @()
    foreach ($dll in @("vcruntime140.dll", "msvcp140.dll")) {
        $path = Join-Path $System32 $dll
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            $missing += $path
        }
    }
    return $missing
}
