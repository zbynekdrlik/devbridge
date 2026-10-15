# DevBridge installer function library (issue #80).
#
# Pure helpers used by installer/post-install.ps1: the config preserve/rewrite
# decision, config snapshots, the client/server serial-bridge TOML builders and
# merges, the DEVBRIDGE_SERIAL_BRIDGES spec parser, the com0com warnings, the
# VC++ runtime DLL check, the Odoo label source [client.odoo] builder/merge
# + API-key file ACL (issue #90), and the `query user` session parser for the
# tray relaunch (issue #93).
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

# ---------------------------------------------------------------------------
# Odoo label source, [client.odoo] (issue #90).
# Env: DEVBRIDGE_ODOO_URL, DEVBRIDGE_ODOO_API_KEY, DEVBRIDGE_ODOO_PRINTER_NAME,
# optional DEVBRIDGE_ODOO_LABEL_WIDTH_MM / _LABEL_HEIGHT_MM / _DPI and the label
# layout (issue #95) DEVBRIDGE_ODOO_ROTATE_180 (true|false, service default
# true) / _X_OFFSET_DOTS / _Y_OFFSET_DOTS (whole dots, -999..999, + = right /
# down as the label is read, service default 0). The API key
# is a secret: it is read by post-install.ps1 straight from the environment
# (never passed on a command line), never printed, and the config files that
# hold it are restricted to SYSTEM + Administrators.
# ---------------------------------------------------------------------------

# Quote any text as a TOML basic string. Backslash and double quote are
# escaped; control characters and EVERY non-ASCII character become \uXXXX
# (\UXXXXXXXX for a surrogate pair, U+FFFD for a lone surrogate), so the value
# survives post-install's ASCII config write ("Spisska" with diacritics would
# otherwise turn into '?').
function ConvertTo-DevBridgeTomlString {
    param([AllowEmptyString()][string]$Value)
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    $i = 0
    while ($i -lt $Value.Length) {
        $c = $Value[$i]
        $code = [int]$c
        if ($code -eq 0x22) {
            [void]$sb.Append('\"')
        } elseif ($code -eq 0x5C) {
            [void]$sb.Append('\\')
        } elseif ([char]::IsHighSurrogate($c) -and ($i + 1) -lt $Value.Length -and [char]::IsLowSurrogate($Value[$i + 1])) {
            [void]$sb.Append(('\U{0:X8}' -f [char]::ConvertToUtf32($c, $Value[$i + 1])))
            $i++
        } elseif ([char]::IsSurrogate($c)) {
            [void]$sb.Append('\uFFFD')
        } elseif ($code -lt 0x20 -or $code -ge 0x7F) {
            [void]$sb.Append(('\u{0:X4}' -f $code))
        } else {
            [void]$sb.Append($c)
        }
        $i++
    }
    [void]$sb.Append('"')
    return $sb.ToString()
}

# Problems with the requested Odoo settings (empty list = OK), checked BEFORE
# any change. -PrintBackend is the backend the client will run with (Odoo
# labels are TSPL, so it must be windows_spooler_raw). The API key value is
# NEVER part of a message.
function Get-DevBridgeOdooConfigProblems {
    param(
        [string]$Url = "",
        [string]$ApiKey = "",
        [string]$PrinterName = "",
        [string]$LabelWidthMm = "",
        [string]$LabelHeightMm = "",
        [string]$Dpi = "",
        [string]$PrintBackend = "",
        [string]$Rotate180 = "",
        [string]$XOffsetDots = "",
        [string]$YOffsetDots = ""
    )
    $problems = @()
    # Case-sensitive, exactly like the client's own startup check.
    if ($Url -cnotmatch '^https?://[^\s"\\]+\z') {
        $problems += "DEVBRIDGE_ODOO_URL '$Url' must be an http(s):// URL (lower-case scheme) without spaces, quotes or backslashes"
    }
    if (-not $ApiKey -or -not $ApiKey.Trim()) {
        $problems += "DEVBRIDGE_ODOO_API_KEY is not set"
    } elseif ($ApiKey -match '[\s\x00-\x1F\x7F]') {
        $problems += "DEVBRIDGE_ODOO_API_KEY contains whitespace or control characters (value not shown)"
    }
    if (-not $PrinterName -or -not $PrinterName.Trim()) {
        $problems += "DEVBRIDGE_ODOO_PRINTER_NAME (the Odoo printer name, e.g. 'TSC ML241P Spisska') is not set"
    }
    if ($PrintBackend -cne "windows_spooler_raw") {
        $problems += "Odoo labels are TSPL: print_backend must be windows_spooler_raw (is '$PrintBackend'); set DEVBRIDGE_PRINT_BACKEND=windows_spooler_raw (on an existing install also DEVBRIDGE_FORCE_CONFIG_REWRITE=true, the preserved config.toml keeps its backend)"
    }
    $mmChecks = @(
        @{ Name = "DEVBRIDGE_ODOO_LABEL_WIDTH_MM"; Value = $LabelWidthMm },
        @{ Name = "DEVBRIDGE_ODOO_LABEL_HEIGHT_MM"; Value = $LabelHeightMm }
    )
    foreach ($check in $mmChecks) {
        $v = $check.Value
        if (-not $v) { continue }
        $ok = $v -match '^[0-9]{1,4}(\.[0-9]{1,3})?\z'
        if ($ok) {
            $mm = [double]::Parse($v, [System.Globalization.CultureInfo]::InvariantCulture)
            $ok = ($mm -gt 0) -and ($mm -le 1000)
        }
        if (-not $ok) {
            $problems += "$($check.Name) '$v' must be a number of millimetres > 0 and <= 1000 (dot as decimal separator)"
        }
    }
    if ($Dpi -and (($Dpi -notmatch '^[0-9]{3,4}\z') -or ([int]$Dpi -lt 100) -or ([int]$Dpi -gt 1200))) {
        $problems += "DEVBRIDGE_ODOO_DPI '$Dpi' must be a whole number between 100 and 1200"
    }
    if ($Rotate180 -and ($Rotate180 -notmatch '^(true|false)\z')) {
        $problems += "DEVBRIDGE_ODOO_ROTATE_180 '$Rotate180' must be true or false"
    }
    $offsetChecks = @(
        @{ Name = "DEVBRIDGE_ODOO_X_OFFSET_DOTS"; Value = $XOffsetDots },
        @{ Name = "DEVBRIDGE_ODOO_Y_OFFSET_DOTS"; Value = $YOffsetDots }
    )
    foreach ($check in $offsetChecks) {
        # [0-9], not \d (.NET \d also takes other scripts' digits, which
        # [int]::Parse rejects); \z, not $ ($ also accepts a trailing newline).
        if ($check.Value -and ($check.Value -notmatch '^-?[0-9]{1,3}\z')) {
            $problems += "$($check.Name) '$($check.Value)' must be a whole number of dots between -999 and 999"
        }
    }
    return ,$problems
}

# The [client.odoo] TOML block (no trailing newline). Values validated by
# Get-DevBridgeOdooConfigProblems first; strings go through
# ConvertTo-DevBridgeTomlString. Size/dpi/layout lines only when given (the
# service defaults are the Spisska roll: 72.7 x 110.1 mm, 203 dpi, rotated
# 180 degrees, no offsets -- issue #95).
function Get-DevBridgeOdooToml {
    param(
        [Parameter(Mandatory)][string]$Url,
        [Parameter(Mandatory)][string]$ApiKey,
        [Parameter(Mandatory)][string]$PrinterName,
        [string]$LabelWidthMm = "",
        [string]$LabelHeightMm = "",
        [string]$Dpi = "",
        [string]$Rotate180 = "",
        [string]$XOffsetDots = "",
        [string]$YOffsetDots = ""
    )
    $lines = @(
        "[client.odoo]",
        "enabled = true",
        "url = $(ConvertTo-DevBridgeTomlString -Value $Url)",
        "api_key = $(ConvertTo-DevBridgeTomlString -Value $ApiKey)",
        "printer_name = $(ConvertTo-DevBridgeTomlString -Value $PrinterName)"
    )
    # TOML floats: "72" -> "72.0" so the key is always a float.
    if ($LabelWidthMm) {
        if ($LabelWidthMm -notmatch '\.') { $LabelWidthMm = "$LabelWidthMm.0" }
        $lines += "label_width_mm = $LabelWidthMm"
    }
    if ($LabelHeightMm) {
        if ($LabelHeightMm -notmatch '\.') { $LabelHeightMm = "$LabelHeightMm.0" }
        $lines += "label_height_mm = $LabelHeightMm"
    }
    if ($Dpi) { $lines += "dpi = $Dpi" }
    # TOML booleans are lower-case; integers without leading zeros ("007" is
    # invalid TOML), so the offsets are re-printed from their parsed value.
    if ($Rotate180) { $lines += "rotate_180 = $($Rotate180.ToLowerInvariant())" }
    if ($XOffsetDots) { $lines += "x_offset_dots = $([int]::Parse($XOffsetDots, [System.Globalization.CultureInfo]::InvariantCulture))" }
    if ($YOffsetDots) { $lines += "y_offset_dots = $([int]::Parse($YOffsetDots, [System.Globalization.CultureInfo]::InvariantCulture))" }
    return ($lines -join "`n")
}

# Value of a string key in the [client] table of config TEXT ($Default when
# the key or the table is absent). Used to learn the print_backend a
# PRESERVED config.toml will run with.
function Get-DevBridgeClientConfigValue {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Config,
        [Parameter(Mandatory)][string]$Key,
        [string]$Default = ""
    )
    $section = [regex]::Match($Config, '(?ms)^\[client\][ \t]*\r?$(.*?)(?=^\[|\z)')
    if (-not $section.Success) { return $Default }
    $m = [regex]::Match($section.Groups[1].Value, ('(?m)^[ \t]*' + [regex]::Escape($Key) + '[ \t]*=[ \t]*"([^"]*)"'))
    if ($m.Success) { return $m.Groups[1].Value }
    return $Default
}

# Put the [<Table>] block into an EXISTING (preserved) config.toml. Returns
# 'added' (spliced in before [jobs], else appended) or 'replaced' (the existing
# [<Table>] table up to the next table header is swapped for the new block --
# key rotation = re-run the installer with the new env; tables after it, e.g.
# [[client.pokladna.kasy]], are kept). Written UTF-8 without BOM, keeping the
# file's line-ending style. Shared by [client.odoo] (issue #90) and
# [client.pokladna] (issue #99).
function Merge-DevBridgeTomlTableIntoConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Table,
        [Parameter(Mandatory)][string]$Block
    )
    $raw = [System.IO.File]::ReadAllText($Path)
    if ($raw -match "`r`n") { $eol = "`r`n" } elseif ($raw -match "`n") { $eol = "`n" } else { $eol = "`r`n" }
    $blockText = $Block -replace "`r?`n", $eol

    $header = '^\[' + [regex]::Escape($Table) + '\]'
    $existing = [regex]::Match($raw, '(?ms)' + $header + '[^\r\n]*(?:\r?\n|\z).*?(?=^\[|\z)')
    if ($existing.Success) {
        $rest = $raw.Substring($existing.Index + $existing.Length)
        $separator = if ($rest.Length -gt 0) { $eol + $eol } else { $eol }
        $new = $raw.Substring(0, $existing.Index) + $blockText + $separator + $rest
        $result = "replaced"
    } else {
        $jobs = [regex]::Match($raw, '(?m)^\[jobs\]')
        if ($jobs.Success) {
            $new = $raw.Substring(0, $jobs.Index) + $blockText + $eol + $eol + $raw.Substring($jobs.Index)
        } else {
            $lead = if ($raw.Length -gt 0 -and -not $raw.EndsWith("`n")) { $eol } else { "" }
            $new = $raw + $lead + $eol + $blockText + $eol
        }
        $result = "added"
    }
    [System.IO.File]::WriteAllText($Path, $new, (New-Object System.Text.UTF8Encoding($false)))
    return $result
}

# Put the [client.odoo] block into an EXISTING (preserved) config.toml
# ('added' / 'replaced', see Merge-DevBridgeTomlTableIntoConfig).
function Merge-DevBridgeOdooIntoConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Block
    )
    return Merge-DevBridgeTomlTableIntoConfig -Path $Path -Table "client.odoo" -Block $Block
}

# True when the config file has a [client.odoo] table (i.e. may hold a key).
function Test-DevBridgeConfigHasOdoo {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    return [bool]([System.IO.File]::ReadAllText($Path) -match '(?m)^\[client\.odoo\]')
}

# ---------------------------------------------------------------------------
# Odoo device-job source of the store's pokladna, [client.pokladna] (issue #99):
# eKasa receipts and card payments, executed exactly once by the client.
# Env: DEVBRIDGE_POKLADNA_URL, DEVBRIDGE_POKLADNA_API_KEY (the key of the
# store's dedicated Odoo bridge user, scope pokladna_bridge -- NOT the label
# printer's key), optional DEVBRIDGE_POKLADNA_WAIT_S (long-poll seconds 0..55,
# service default 25; 0 = pure short polling). The API key is a secret: read by
# post-install.ps1 straight from the environment (never a command line), never
# printed, and the config files holding it are restricted to SYSTEM +
# Administrators. The device mapping ([[client.pokladna.kasy]]) and the
# E2E-only test executor are never written by the installer.
# ---------------------------------------------------------------------------

# Problems with the requested pokladna settings (empty list = OK), checked
# BEFORE any change. The API key value is NEVER part of a message.
function Get-DevBridgePokladnaConfigProblems {
    param(
        [string]$Url = "",
        [string]$ApiKey = "",
        [string]$WaitS = ""
    )
    $problems = @()
    # Case-sensitive, exactly like the client's own startup check.
    if ($Url -cnotmatch '^https?://[^\s"\\]+\z') {
        $problems += "DEVBRIDGE_POKLADNA_URL '$Url' must be an http(s):// URL (lower-case scheme) without spaces, quotes or backslashes"
    }
    if (-not $ApiKey -or -not $ApiKey.Trim()) {
        $problems += "DEVBRIDGE_POKLADNA_API_KEY is not set"
    } elseif ($ApiKey -match '[\s\x00-\x1F\x7F]') {
        $problems += "DEVBRIDGE_POKLADNA_API_KEY contains whitespace or control characters (value not shown)"
    }
    # [0-9], not \d (.NET \d also takes other scripts' digits); \z, not $.
    if ($WaitS -and (($WaitS -notmatch '^[0-9]{1,2}\z') -or ([int]$WaitS -gt 55))) {
        $problems += "DEVBRIDGE_POKLADNA_WAIT_S '$WaitS' must be a whole number of seconds between 0 and 55"
    }
    return ,$problems
}

# The [client.pokladna] TOML block (no trailing newline). Values validated by
# Get-DevBridgePokladnaConfigProblems first; strings go through
# ConvertTo-DevBridgeTomlString; wait_s only when given (re-printed from its
# parsed value: "07" is not a TOML integer).
function Get-DevBridgePokladnaToml {
    param(
        [Parameter(Mandatory)][string]$Url,
        [Parameter(Mandatory)][string]$ApiKey,
        [string]$WaitS = ""
    )
    $lines = @(
        "[client.pokladna]",
        "enabled = true",
        "url = $(ConvertTo-DevBridgeTomlString -Value $Url)",
        "api_key = $(ConvertTo-DevBridgeTomlString -Value $ApiKey)"
    )
    if ($WaitS) { $lines += "wait_s = $([int]::Parse($WaitS, [System.Globalization.CultureInfo]::InvariantCulture))" }
    return ($lines -join "`n")
}

# True when the config file has a [client.pokladna] table (i.e. may hold a key).
function Test-DevBridgeConfigHasPokladna {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    return [bool]([System.IO.File]::ReadAllText($Path) -match '(?m)^\[client\.pokladna\]')
}

# Restrict a file holding an Odoo API key to SYSTEM + BUILTIN\Administrators
# (FullControl, inheritance removed). Well-known SIDs, so it works on a Slovak
# Windows where the group names are localized. Throws on failure.
function Set-DevBridgeSecretFileAcl {
    param([Parameter(Mandatory)][string]$Path)
    $acl = New-Object System.Security.AccessControl.FileSecurity
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @("S-1-5-18", "S-1-5-32-544")) {
        $identity = New-Object System.Security.Principal.SecurityIdentifier($sid)
        $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($identity,
            [System.Security.AccessControl.FileSystemRights]::FullControl,
            [System.Security.AccessControl.AccessControlType]::Allow)
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}

# Apply Set-DevBridgeSecretFileAcl to config.toml AND every config.toml.*
# snapshot in -DataDir (the pre-upgrade / replaced snapshots are copies that
# hold the same key). Returns the paths it restricted.
function Protect-DevBridgeConfigFiles {
    param([Parameter(Mandatory)][string]$DataDir)
    $protected = @()
    $files = @(Get-ChildItem -LiteralPath $DataDir -Force -File -ErrorAction Stop |
        Where-Object { $_.Name -eq "config.toml" -or $_.Name -like "config.toml.*" })
    foreach ($f in $files) {
        Set-DevBridgeSecretFileAcl -Path $f.FullName
        $protected += $f.FullName
    }
    return ,$protected
}

# Parse `query user` output lines into one [pscustomobject]@{ Username;
# SessionId; State } per Active or disconnected (Disc) session -- the header
# and any other line are skipped, a leading ">" marks the caller's own session.
# Pure. Returns an object[] (unary comma): ASSIGN it before using .Count. Under
# Windows PowerShell 5.1 a lone [pscustomobject] has no .Count, which is how a
# single-user store PC got "No active sessions" and no tray app after an
# upgrade (issue #93) -- and never a List[object]: 5.1 throws "Argument types
# do not match" on @() over one. Output format (USERNAME is the first column):
#    USERNAME              SESSIONNAME        ID  STATE   IDLE TIME  LOGON TIME
#   >drlikzbynek           rdp-tcp#19         60  Active          .  13. 7. 2026 22:11
#    marketing                                22  Disc      5+15:03  14. 7. 2026 11:24
function ConvertFrom-DevBridgeQueryUserOutput {
    param([AllowNull()][AllowEmptyCollection()][string[]]$Lines)
    $sessions = @()
    foreach ($line in @($Lines)) {
        if ($line -match '^>?\s*(\S+)\s+.*?\s+(\d+)\s+(Active|Disc)') {
            $sessions += [pscustomobject]@{
                Username  = $Matches[1]
                SessionId = [int]$Matches[2]
                State     = $Matches[3]
            }
        }
    }
    return ,$sessions
}

# ---------------------------------------------------------------------------
# LAN API of the tills (issue #101): the Android pokladna sends its fiscal.*
# jobs (eKasa receipts) to this PC directly, also while the store's internet
# is down. Env:
#   DEVBRIDGE_POKLADNA_KASY -- a JSON array of the tills, written as WHOLE
#     [[client.pokladna.kasy]] blocks (every existing one is replaced):
#       [{"id": 3, "portos_url": "http://localhost:3010", "lan_key": "<key>"}]
#     id = the till's Odoo pos.config id; portos_url optional (default
#     http://localhost:3010); lan_key = the till's LAN API key -- a secret,
#     read from the environment only, never printed; 32-256 printable
#     characters without spaces, quotes or backslashes, one key per till,
#     never the Odoo bridge key. Today the parser takes exactly these keys;
#     the card terminal (issue #102) adds terminal_ip / terminal_tid here.
#   DEVBRIDGE_LAN_API_ENABLED (true / false, default true),
#   DEVBRIDGE_LAN_API_PORT (default 9443), DEVBRIDGE_LAN_API_HOSTNAME
#     (<client_id>.<zone>, the certificate's name) -> [client.pokladna.lan_api].
# ---------------------------------------------------------------------------

# Parse DEVBRIDGE_POKLADNA_KASY into its entries (PSCustomObjects). Throws on
# anything but a JSON array of objects; the message never shows the value.
# Returns an object[] (unary comma): ASSIGN it before using .Count.
function ConvertFrom-DevBridgePokladnaKasySpec {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Spec)
    if ($Spec.Trim() -notmatch '^\[') {
        throw "DEVBRIDGE_POKLADNA_KASY must be a JSON array of tills, e.g. [{`"id`": 3, `"lan_key`": `"...`"}] (value not shown)"
    }
    # pwsh 7's ConvertFrom-Json (Newtonsoft) accepts a TRUNCATED document
    # ('[{"id": 3' -> one till), which would silently drop the tills after it,
    # so on pwsh 7 the text must first parse with System.Text.Json (strict: no
    # truncation, trailing commas or trailing text). Windows PowerShell 5.1's
    # ConvertFrom-Json (JavaScriptSerializer) is already strict.
    $strictJson = $null
    if ($PSVersionTable.PSVersion.Major -ge 6) {
        Add-Type -AssemblyName System.Text.Json -ErrorAction Stop
        $strictJson = "System.Text.Json.JsonDocument" -as [type]
        if ($null -eq $strictJson) {
            throw "System.Text.Json.JsonDocument is not available to check DEVBRIDGE_POKLADNA_KASY"
        }
    }
    try {
        if ($null -ne $strictJson) {
            $strictJson::Parse($Spec).Dispose()
        }
        $parsed = ConvertFrom-Json -InputObject $Spec -ErrorAction Stop
    } catch {
        throw "DEVBRIDGE_POKLADNA_KASY is not valid JSON (value not shown)"
    }
    # pwsh 7 returns nothing for "[]"; 5.1 returns the array as ONE object.
    if ($null -eq $parsed) { $parsed = @() }
    $entries = @()
    foreach ($item in @($parsed)) {
        if ($item -isnot [System.Management.Automation.PSCustomObject]) {
            throw "DEVBRIDGE_POKLADNA_KASY: every till must be a JSON object (value not shown)"
        }
        $entries += , $item
    }
    return , $entries
}

# Problems with DEVBRIDGE_POKLADNA_KASY (empty list = OK), checked BEFORE any
# change. No key value is ever part of a message. -ApiKey is the Odoo bridge
# key (DEVBRIDGE_POKLADNA_API_KEY), which a till must never share.
function Get-DevBridgePokladnaKasyProblems {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Spec,
        [string]$ApiKey = ""
    )
    $problems = @()
    try {
        $entries = ConvertFrom-DevBridgePokladnaKasySpec -Spec $Spec
    } catch {
        $problems += $_.Exception.Message
        return , $problems
    }
    if ($entries.Count -eq 0) {
        $problems += "DEVBRIDGE_POKLADNA_KASY lists no till"
    }
    $allowed = @("id", "portos_url", "lan_key")
    $seenIds = @{}
    $seenKeys = [System.Collections.Hashtable]::new([System.StringComparer]::Ordinal)
    for ($i = 0; $i -lt $entries.Count; $i++) {
        $entry = $entries[$i]
        $where = "DEVBRIDGE_POKLADNA_KASY entry $($i + 1)"
        foreach ($name in @($entry.PSObject.Properties.Name)) {
            if ($allowed -cnotcontains $name) {
                $problems += "${where}: unknown key '$name' (supported: $($allowed -join ', '))"
            }
        }
        $id = $entry.id
        if ((($id -isnot [int]) -and ($id -isnot [long])) -or ($id -le 0)) {
            $problems += "${where}: id must be the till's positive pos.config id (a whole number)"
        } elseif ($seenIds.ContainsKey([long]$id)) {
            $problems += "${where}: id $id is listed twice"
        } else {
            $seenIds[[long]$id] = $true
        }
        if ($null -ne $entry.PSObject.Properties["portos_url"]) {
            $url = $entry.portos_url
            if (($url -isnot [string]) -or ($url -cnotmatch '^https?://[^\s"\\]+\z')) {
                $problems += "${where}: portos_url must be an http(s):// URL (lower-case scheme) without spaces, quotes or backslashes"
            }
        }
        if ($null -ne $entry.PSObject.Properties["lan_key"]) {
            $key = $entry.lan_key
            if (($key -isnot [string]) -or ($key.Length -lt 32) -or ($key.Length -gt 256) -or
                ($key -cnotmatch '^[\x21\x23-\x5B\x5D-\x7E]+\z')) {
                $problems += "${where}: lan_key must be 32-256 printable characters without spaces, quotes or backslashes (value not shown)"
            } elseif ($ApiKey -and ($key -ceq $ApiKey)) {
                $problems += "${where}: lan_key must not be the Odoo bridge key (DEVBRIDGE_POKLADNA_API_KEY)"
            } elseif ($seenKeys.ContainsKey($key)) {
                $problems += "${where}: lan_key is the same as another till's (a key belongs to one till)"
            } else {
                $seenKeys[$key] = $true
            }
        }
    }
    return , $problems
}

# The [[client.pokladna.kasy]] blocks of DEVBRIDGE_POKLADNA_KASY (no trailing
# newline). Validated by Get-DevBridgePokladnaKasyProblems first; strings go
# through ConvertTo-DevBridgeTomlString.
function Get-DevBridgePokladnaKasyToml {
    param([Parameter(Mandatory)][string]$Spec)
    $entries = ConvertFrom-DevBridgePokladnaKasySpec -Spec $Spec
    $blocks = @()
    foreach ($entry in $entries) {
        $lines = @("[[client.pokladna.kasy]]", "id = $([long]$entry.id)")
        if ($null -ne $entry.PSObject.Properties["portos_url"]) {
            $lines += "portos_url = $(ConvertTo-DevBridgeTomlString -Value $entry.portos_url)"
        }
        if ($null -ne $entry.PSObject.Properties["lan_key"]) {
            $lines += "lan_key = $(ConvertTo-DevBridgeTomlString -Value $entry.lan_key)"
        }
        $blocks += ($lines -join "`n")
    }
    return ($blocks -join "`n`n")
}

# Put the [[client.pokladna.kasy]] blocks into an EXISTING (preserved)
# config.toml: every existing [[client.pokladna.kasy]] block is removed (the
# list is always given WHOLE -- a new till, a key rotation) and the new blocks
# go before [jobs] (else at the end). Returns 'added' or 'replaced'. Written
# UTF-8 without BOM, keeping the file's line-ending style.
function Merge-DevBridgePokladnaKasyIntoConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Block
    )
    $raw = [System.IO.File]::ReadAllText($Path)
    if ($raw -match "`r`n") { $eol = "`r`n" } elseif ($raw -match "`n") { $eol = "`n" } else { $eol = "`r`n" }
    $blockText = $Block -replace "`r?`n", $eol
    $pattern = '(?ms)^\[\[client\.pokladna\.kasy\]\][^\r\n]*(?:\r?\n|\z).*?(?=^\[|\z)'
    $result = if ([regex]::IsMatch($raw, $pattern)) { "replaced" } else { "added" }
    $rest = [regex]::Replace($raw, $pattern, "")
    $jobs = [regex]::Match($rest, '(?m)^\[jobs\]')
    if ($jobs.Success) {
        $new = $rest.Substring(0, $jobs.Index) + $blockText + $eol + $eol + $rest.Substring($jobs.Index)
    } else {
        $lead = if ($rest.Length -gt 0 -and -not $rest.EndsWith("`n")) { $eol } else { "" }
        $new = $rest + $lead + $eol + $blockText + $eol
    }
    [System.IO.File]::WriteAllText($Path, $new, (New-Object System.Text.UTF8Encoding($false)))
    return $result
}

# A lower-case DNS name of at least two labels (pjsnvs.pos.example.sk), at
# most 253 characters, the last label with a letter (no IP address) -- the
# service's own rule for the LAN API hostname. Pure.
function Test-DevBridgeDnsName {
    param([AllowEmptyString()][string]$Name = "")
    if (-not $Name -or $Name.Length -gt 253) { return $false }
    $labels = $Name.Split('.')
    if ($labels.Count -lt 2) { return $false }
    foreach ($label in $labels) {
        if ($label -cnotmatch '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\z') { return $false }
    }
    return [bool]($labels[$labels.Count - 1] -cmatch '[a-z]')
}

# Problems with the requested [client.pokladna.lan_api] settings (empty list =
# OK), checked BEFORE any change. The hostname is needed while it is enabled.
function Get-DevBridgeLanApiConfigProblems {
    param(
        [string]$Enabled = "",
        [string]$Port = "",
        [string]$Hostname = "",
        [int]$DashboardPort = 9120
    )
    $problems = @()
    $on = $true
    if ($Enabled) {
        $value = $Enabled.ToLowerInvariant()
        if (@("true", "false") -cnotcontains $value) {
            $problems += "DEVBRIDGE_LAN_API_ENABLED '$Enabled' must be true or false"
        }
        $on = $value -ne "false"
    }
    if ($Port) {
        if (($Port -notmatch '^[0-9]{1,5}\z') -or ([int]$Port -lt 1) -or ([int]$Port -gt 65535)) {
            $problems += "DEVBRIDGE_LAN_API_PORT '$Port' must be a port number between 1 and 65535"
        } elseif ([int]$Port -eq $DashboardPort) {
            $problems += "DEVBRIDGE_LAN_API_PORT $Port is the dashboard's port"
        }
    }
    if ($on -and -not (Test-DevBridgeDnsName -Name $Hostname)) {
        $problems += "DEVBRIDGE_LAN_API_HOSTNAME '$Hostname' must be the PC's lower-case DNS name <client_id>.<zone> (e.g. pjsnvs.pos.example.sk)"
    }
    return , $problems
}

# The [client.pokladna.lan_api] block (no trailing newline): enabled (default
# true), port and hostname when given. Validated by
# Get-DevBridgeLanApiConfigProblems first.
function Get-DevBridgeLanApiToml {
    param(
        [string]$Enabled = "",
        [string]$Port = "",
        [string]$Hostname = ""
    )
    $on = if ($Enabled) { $Enabled.ToLowerInvariant() } else { "true" }
    $lines = @("[client.pokladna.lan_api]", "enabled = $on")
    if ($Port) { $lines += "port = $([int]::Parse($Port, [System.Globalization.CultureInfo]::InvariantCulture))" }
    if ($Hostname) { $lines += "hostname = $(ConvertTo-DevBridgeTomlString -Value $Hostname)" }
    return ($lines -join "`n")
}

# True when the config file holds a till's LAN key (it must be restricted).
function Test-DevBridgeConfigHasLanKey {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    return [bool]([System.IO.File]::ReadAllText($Path) -match '(?m)^[ \t]*lan_key[ \t]*=')
}

# Create the inbound TCP rule -DisplayName for -Port, or move an existing one
# (Set-NetFirewallRule) -- same action and profiles either way. Returns
# 'created' or 'updated'.
function Set-DevBridgeFirewallPortRule {
    param(
        [Parameter(Mandatory)][string]$DisplayName,
        [Parameter(Mandatory)][int]$Port,
        [Parameter(Mandatory)][ValidateSet("Allow", "Block")][string]$Action,
        [Parameter(Mandatory)][string[]]$Profiles
    )
    if (Get-NetFirewallRule -DisplayName $DisplayName -ErrorAction SilentlyContinue) {
        Set-NetFirewallRule -DisplayName $DisplayName -Direction Inbound -Protocol TCP -LocalPort $Port `
            -Action $Action -Profile $Profiles -ErrorAction Stop
        return "updated"
    }
    New-NetFirewallRule -DisplayName $DisplayName -Direction Inbound -Protocol TCP -LocalPort $Port `
        -Action $Action -Profile $Profiles -ErrorAction Stop | Out-Null
    return "created"
}

# The LAN API port on a store LAN only: ALLOW on the Private and Domain
# profiles, and BLOCK on Public (rule "<DisplayName>-Public") -- the service
# binary's own allow rule (DevBridge-Service, every profile, every port) would
# otherwise open it on a public network too; a block rule beats an allow
# rule. Returns 'created' or 'updated' (the allow rule).
function Set-DevBridgeLanApiFirewallRule {
    param(
        [Parameter(Mandatory)][int]$Port,
        [string]$DisplayName = "DevBridge-LanApi"
    )
    $allow = Set-DevBridgeFirewallPortRule -DisplayName $DisplayName -Port $Port -Action Allow -Profiles @("Domain", "Private")
    Set-DevBridgeFirewallPortRule -DisplayName "$DisplayName-Public" -Port $Port -Action Block -Profiles @("Public") | Out-Null
    return $allow
}

# Remove both LAN API rules (the API was switched off). Returns how many
# existed.
function Remove-DevBridgeLanApiFirewallRule {
    param([string]$DisplayName = "DevBridge-LanApi")
    $removed = 0
    foreach ($name in @($DisplayName, "$DisplayName-Public")) {
        if (Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue) {
            Remove-NetFirewallRule -DisplayName $name -ErrorAction Stop
            $removed++
        }
    }
    return $removed
}

# The body of the [<Table>] table of config TEXT (up to the next table
# header), "" when absent. [client.pokladna] never matches
# [client.pokladna.lan_api].
function Get-DevBridgeTomlTableText {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Config,
        [Parameter(Mandatory)][string]$Table
    )
    $m = [regex]::Match($Config, '(?ms)^\[' + [regex]::Escape($Table) + '\][ \t]*\r?$(.*?)(?=^\[|\z)')
    if ($m.Success) { return $m.Groups[1].Value }
    return ""
}

# A quoted string value of TOML table TEXT ("" when absent).
function Get-DevBridgeTomlStringValue {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Text,
        [Parameter(Mandatory)][string]$Key
    )
    $m = [regex]::Match($Text, ('(?m)^[ \t]*' + [regex]::Escape($Key) + '[ \t]*=[ \t]*"([^"]*)"'))
    if ($m.Success) { return $m.Groups[1].Value }
    return ""
}

# A boolean value of TOML table TEXT ($Default when absent).
function Get-DevBridgeTomlBoolValue {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Text,
        [Parameter(Mandatory)][string]$Key,
        [bool]$Default = $false
    )
    $m = [regex]::Match($Text, ('(?m)^[ \t]*' + [regex]::Escape($Key) + '[ \t]*=[ \t]*(true|false)[ \t]*(?:#.*)?\r?$'))
    if ($m.Success) { return ($m.Groups[1].Value -ceq "true") }
    return $Default
}

# Problems of the LAN API as the SERVICE will see it after this install: the
# requested values over the preserved config (issue #101). Mirrors the
# client's start-up check (startup_validation.rs lan_api_problems), which
# would refuse the WHOLE client service -- printing included. -Existing is the
# preserved config text ("" = a fresh config, -ClientId then applies); a
# -LanApiToml / -KasySpec / -PokladnaToml that is given replaces the preserved
# part, -ApiKey is DEVBRIDGE_POKLADNA_API_KEY. No key value is ever part of a
# message.
function Get-DevBridgeLanApiEffectiveProblems {
    param(
        [AllowEmptyString()][string]$Existing = "",
        [string]$ClientId = "",
        [string]$LanApiToml = "",
        [string]$KasySpec = "",
        [string]$PokladnaToml = "",
        [string]$ApiKey = ""
    )
    $problems = @()
    $lanText = if ($LanApiToml) { $LanApiToml } else { Get-DevBridgeTomlTableText -Config $Existing -Table "client.pokladna.lan_api" }
    $pokladnaText = if ($PokladnaToml) { $PokladnaToml } else { Get-DevBridgeTomlTableText -Config $Existing -Table "client.pokladna" }
    # the LAN API runs only with an enabled [client.pokladna]
    if (-not (Get-DevBridgeTomlBoolValue -Text $lanText -Key "enabled") -or
        -not (Get-DevBridgeTomlBoolValue -Text $pokladnaText -Key "enabled")) {
        return , $problems
    }
    $clientText = Get-DevBridgeTomlTableText -Config $Existing -Table "client"
    $clientId = if ($Existing) { Get-DevBridgeTomlStringValue -Text $clientText -Key "client_id" } else { $ClientId }
    $hostname = Get-DevBridgeTomlStringValue -Text $lanText -Key "hostname"
    if (Get-DevBridgeTomlBoolValue -Text $lanText -Key "acme" -Default $true) {
        if (-not $clientId) {
            $problems += "the LAN API needs [client] client_id (DEVBRIDGE_CLIENT_ID): pz-server issues its certificate only for <client_id>.<zone>"
        } elseif (-not $hostname.StartsWith($clientId.ToLowerInvariant() + ".", [System.StringComparison]::Ordinal)) {
            $problems += "DEVBRIDGE_LAN_API_HOSTNAME '$hostname' must be <client_id>.<zone> for client_id '$clientId' -- the only name pz-server issues a certificate for"
        }
    }
    $keys = @()
    if ($KasySpec) {
        foreach ($entry in (ConvertFrom-DevBridgePokladnaKasySpec -Spec $KasySpec)) {
            if ($null -ne $entry.PSObject.Properties["lan_key"]) { $keys += [string]$entry.lan_key }
        }
    } else {
        foreach ($m in [regex]::Matches($Existing, '(?m)^[ \t]*lan_key[ \t]*=[ \t]*"([^"]*)"')) {
            $keys += $m.Groups[1].Value
        }
    }
    if ($keys.Count -eq 0) {
        $problems += "no till has a lan_key (DEVBRIDGE_POKLADNA_KASY) -- no till could call the LAN API"
    }
    $bridgeKey = if ($PokladnaToml) { $ApiKey } else { Get-DevBridgeTomlStringValue -Text $pokladnaText -Key "api_key" }
    if ($bridgeKey -and ($keys -ccontains $bridgeKey)) {
        $problems += "a till's lan_key must not be the Odoo bridge key (DEVBRIDGE_POKLADNA_API_KEY, value not shown)"
    }
    return , $problems
}

# ---------------------------------------------------------------------------
# LAN API certificates of the stores on pz-server, [server.acme] (issue #101):
# Let's Encrypt DNS-01 through Cloudflare. Env (server mode):
# DEVBRIDGE_ACME_ZONE (certificates are <client_id>.<zone>),
# DEVBRIDGE_ACME_CONTACT_EMAIL (optional), DEVBRIDGE_CLOUDFLARE_ZONE_ID and
# DEVBRIDGE_CLOUDFLARE_API_TOKEN (Zone.DNS: Edit on that zone). The token is a
# secret: read from the environment only, never printed, and the config files
# holding it are restricted to SYSTEM + Administrators (a machine-wide
# environment variable would be readable by every local user).
# ---------------------------------------------------------------------------

# Problems with the requested [server.acme] settings (empty list = OK),
# checked BEFORE any change. The token value is NEVER part of a message.
function Get-DevBridgeAcmeConfigProblems {
    param(
        [string]$Zone = "",
        [string]$ContactEmail = "",
        [string]$CloudflareZoneId = "",
        [string]$ApiToken = ""
    )
    $problems = @()
    if (-not (Test-DevBridgeDnsName -Name $Zone)) {
        $problems += "DEVBRIDGE_ACME_ZONE '$Zone' must be a lower-case DNS name such as pos.example.sk"
    }
    if ($ContactEmail -and ($ContactEmail -cnotmatch '^[^\s"\\@]+@[^\s"\\@]+\z')) {
        $problems += "DEVBRIDGE_ACME_CONTACT_EMAIL '$ContactEmail' must be an e-mail address without spaces or quotes"
    }
    if ($CloudflareZoneId -cnotmatch '^[A-Za-z0-9]{1,64}\z') {
        $problems += "DEVBRIDGE_CLOUDFLARE_ZONE_ID '$CloudflareZoneId' must be the Cloudflare zone id (letters and digits)"
    }
    if (-not $ApiToken -or -not $ApiToken.Trim()) {
        $problems += "DEVBRIDGE_CLOUDFLARE_API_TOKEN is not set"
    } elseif ($ApiToken -match '[\s\x00-\x1F\x7F"\\]') {
        $problems += "DEVBRIDGE_CLOUDFLARE_API_TOKEN contains whitespace, quotes, backslashes or control characters (value not shown)"
    }
    return , $problems
}

# The [server.acme] and [server.acme.cloudflare] blocks (no trailing
# newline), as TWO strings: each table is merged on its own into a preserved
# config. Validated by Get-DevBridgeAcmeConfigProblems first.
function Get-DevBridgeAcmeToml {
    param(
        [Parameter(Mandatory)][string]$Zone,
        [string]$ContactEmail = "",
        [Parameter(Mandatory)][string]$CloudflareZoneId,
        [Parameter(Mandatory)][string]$ApiToken
    )
    $acme = @("[server.acme]", "enabled = true", "zone = $(ConvertTo-DevBridgeTomlString -Value $Zone)")
    if ($ContactEmail) { $acme += "contact_email = $(ConvertTo-DevBridgeTomlString -Value $ContactEmail)" }
    $cloudflare = @(
        "[server.acme.cloudflare]",
        "zone_id = $(ConvertTo-DevBridgeTomlString -Value $CloudflareZoneId)",
        "api_token = $(ConvertTo-DevBridgeTomlString -Value $ApiToken)"
    )
    return , @(($acme -join "`n"), ($cloudflare -join "`n"))
}

# True when the config file holds the Cloudflare API token (it must be
# restricted).
function Test-DevBridgeConfigHasAcmeToken {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    return [bool]([System.IO.File]::ReadAllText($Path) -match '(?m)^[ \t]*api_token[ \t]*=')
}
