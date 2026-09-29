# DevBridge one-liner installer
# Usage:
#   irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
#   $env:DEVBRIDGE_VERSION="dev"; irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
#   $env:DEVBRIDGE_INSTALLER_PATH="C:\path\DevBridge_0.8.42_x64-setup.exe"; irm ... | iex   # a local installer file (#93)

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
            Write-Host ("  Service binary unlocked after {0}s" -f $i) -ForegroundColor Green
            return $true
        } catch {
            Start-Sleep -Milliseconds $SleepMilliseconds
        }
    }
    return $false
}

# Verify the installer actually swapped the binary by comparing pre/post SHA256.
# Returns @{ Ok; Reason } where Reason is fresh-install | updated | same-version | unchanged.
# The only failure (Ok=$false) is: a real binary existed (PreHash != "") AND the
# hash did not change AND the installed version does not already match the
# target version -- NSIS silently no-oped the overwrite of an in-use file.
#
# A hash-unchanged result is NOT automatically a failure: re-running the
# installer for the SAME version already on disk (e.g. a
# DEVBRIDGE_FORCE_CONFIG_REWRITE=true config-only rewrite) legitimately leaves
# the binary bytes untouched -- there is nothing for NSIS to swap. Distinguish
# that from a genuine failed swap by comparing the installed version (read
# from the registry AFTER the NSIS run) against the target version. Both are
# normalized (leading "v"/"V" stripped) since GitHub release tags carry a "v"
# prefix ("v0.8.32") while the registry DisplayVersion does not ("0.8.32").
# See issue #71 (pjzav: a same-version reinstall was wrongly reported as a
# failed install, left the service stopped, dashboard down).
function Test-DevBridgeBinarySwapOk {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$PreHash,
        [Parameter(Mandatory)][string]$PostHash,
        [AllowEmptyString()][string]$InstalledVersion,
        [AllowEmptyString()][string]$TargetVersion
    )
    if ($PreHash -eq "") {
        return [pscustomobject]@{ Ok = $true; Reason = "fresh-install" }
    }
    if ($PostHash -ne $PreHash) {
        return [pscustomobject]@{ Ok = $true; Reason = "updated" }
    }
    $installedNorm = if ($InstalledVersion) { $InstalledVersion.Trim() -replace '^[vV]', '' } else { "" }
    $targetNorm = if ($TargetVersion) { $TargetVersion.Trim() -replace '^[vV]', '' } else { "" }
    if ($installedNorm -ne "" -and $targetNorm -ne "" -and $installedNorm -eq $targetNorm) {
        return [pscustomobject]@{ Ok = $true; Reason = "same-version" }
    }
    return [pscustomobject]@{ Ok = $false; Reason = "unchanged" }
}

# Read the installed DisplayVersion from the Uninstall registry key (same
# source autoupdate.ps1's Get-InstalledVersion reads). install.ps1 can't
# dot-source that script (irm|iex has no sibling files on disk), so this is a
# small inline duplicate -- kept deliberately minimal (registry read only, no
# semver parsing) since Test-DevBridgeBinarySwapOk only needs string equality.
function Get-DevBridgeInstalledVersion {
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

# Derive the real target semver from the chosen installer asset's filename
# (review finding F2). $release.tag_name is "dev-latest" on the dev channel
# (a literal, non-semver string), so comparing it against the registry
# DisplayVersion in Test-DevBridgeBinarySwapOk never matched and the
# same-version-reinstall carve-out silently never applied for
# DEVBRIDGE_VERSION=dev. The NSIS installer filename always embeds the real
# workspace version regardless of channel -- "DevBridge_<ver>_x64-setup.exe"
# (see ci.yml's Windows Build / Version Drift jobs) -- so parse it from there.
# Returns $null (never throws) when the name doesn't match; the caller falls
# back to the release tag with a leading "v"/"V" stripped.
function Get-DevBridgeVersionFromAssetName {
    param([AllowEmptyString()][string]$Name)
    if ($Name -match '_(\d+\.\d+\.\d+)_') {
        return $Matches[1]
    }
    return $null
}

# Best-effort restart of the DevBridgeService scheduled task. MUST be called
# from every failure path that exits AFTER the service was stopped for the
# binary swap -- otherwise an aborted upgrade leaves the store without
# printing until someone notices and restarts it manually. See issue #71.
#
# Verifies the restart actually took (review finding F3): Start-ScheduledTask
# only queues the task -- it does not confirm the service process came back
# up, so the original unconditional "Service restarted" log was misleading
# on a genuine restart failure (e.g. the task was deleted/corrupted). Check
# both signals available without depending on each other: the scheduled
# task's own reported state, and whether the service process is actually
# running -- either one is enough to call it a success.
function Restore-DevBridgeService {
    Write-Host "Restarting DevBridgeService after failed install..." -ForegroundColor Yellow
    Start-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    $task = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
    $proc = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
    if (($task -and $task.State -eq "Running") -or $proc) {
        Write-Host "Service restarted after failed install." -ForegroundColor Yellow
    } else {
        Write-Warning "Could not restart DevBridgeService -- start it manually (Start-ScheduledTask DevBridgeService)"
    }
}

# Stop every DevBridge tray app -- devbridge-app.exe and the legacy
# DevBridge.exe -- in ALL user sessions BEFORE the NSIS installer runs (issue
# #93). The Tauri NSIS installer aborts with exit code 2 in silent mode while
# the app it installs is running; on 2026-09-29 that failed the auto-update on
# 6 stores whose user had just logged in (HKLM Run starts the tray at logon).
# The tray is cosmetic -- printing runs in the service -- and post-install.ps1
# relaunches it in every active/disconnected session (others get it at their
# next logon). Waits up to -TimeoutSeconds for the processes to be gone and
# returns how many it stopped. A survivor is only warned about: NSIS then
# fails and Install-DevBridgePackage restores the service.
function Stop-DevBridgeTrayApps {
    param([int]$TimeoutSeconds = 10)
    $trays = @(Get-Process -Name "devbridge-app", "DevBridge" -ErrorAction SilentlyContinue)
    foreach ($tray in $trays) {
        Write-Host ("  Stopping tray app {0} (PID {1}, session {2})" -f $tray.Name, $tray.Id, $tray.SessionId)
        Stop-Process -Id $tray.Id -Force -ErrorAction SilentlyContinue
    }
    $remaining = @()
    if ($trays.Count -gt 0) {
        $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
        do {
            $remaining = @(Get-Process -Name "devbridge-app", "DevBridge" -ErrorAction SilentlyContinue)
            if ($remaining.Count -eq 0) {
                break
            }
            Start-Sleep -Milliseconds 500
        } while ((Get-Date) -lt $deadline)
    }
    Write-Host "Stopped $($trays.Count) tray app process(es) before running the installer"
    if ($remaining.Count -gt 0) {
        $ids = ($remaining | ForEach-Object { "{0} PID {1}" -f $_.Name, $_.Id }) -join ", "
        Write-Warning "Tray app process(es) still running after ${TimeoutSeconds}s: $ids -- the installer may refuse to run"
    }
    return $trays.Count
}

# Run post-install.ps1 in Windows PowerShell and return its exit code. The
# child's output goes to the host (Out-Host), never into the return value, and
# its stderr passes straight through even under $ErrorActionPreference "Stop"
# (verified under PS 5.1, issue #93). A function of its own so the Pester
# suite can stand in for the child process.
function Invoke-DevBridgePostInstallScript {
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [AllowEmptyCollection()][object[]]$Arguments = @()
    )
    & powershell.exe -ExecutionPolicy Bypass -File $ScriptPath @Arguments | Out-Host
    return $LASTEXITCODE
}

# The guarded upgrade (issue #93): stop the service and the tray apps, wait
# for the binary to unlock, run NSIS, verify the swap, run post-install -- all
# inside ONE try/catch. Every failure after the service stop THROWS, and the
# catch restarts DevBridgeService BEFORE the error surfaces, whatever the
# caller's $ErrorActionPreference. (install.ps1 runs with "Stop": the old
# `Write-Error ...; Restore-DevBridgeService` order aborted on the Write-Error,
# the restore never ran, and the 0.8.41 auto-update left 6 stores without
# printing for ~5 h.) -TargetVersion is the real semver being installed; see
# Test-DevBridgeBinarySwapOk for why it matters.
function Install-DevBridgePackage {
    param(
        [Parameter(Mandatory)][string]$InstallerPath,
        [AllowEmptyString()][string]$TargetVersion,
        [Parameter(Mandatory)][string]$InstallDir,
        [AllowEmptyCollection()][object[]]$PostInstallArgs = @(),
        [int]$UnlockTimeoutSeconds = 30
    )
    $svcExe = Join-Path $InstallDir "devbridge-service.exe"
    try {
        # --- Stop running service so NSIS can replace the binary --
        # NSIS silently skips overwriting a file it can't open exclusively. If
        # devbridge-service.exe is running, the upgrade leaves the OLD binary
        # in place and `==> installed successfully` is a lie. Stopping the
        # scheduled task + waiting for the process to actually exit prevents
        # that.
        $existingTask = Get-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
        if ($existingTask) {
            Write-Host "Stopping DevBridgeService for binary swap..."
            Stop-ScheduledTask -TaskName "DevBridgeService" -ErrorAction SilentlyContinue
        }
        $existingProcs = Get-Process -Name "devbridge-service" -ErrorAction SilentlyContinue
        if ($existingProcs) {
            $existingProcs | Stop-Process -Force -ErrorAction SilentlyContinue
        }

        # --- Stop the tray apps: NSIS exits 2 while one runs (issue #93) ---
        $null = Stop-DevBridgeTrayApps

        # Wait for the file to become writable. WaitForExit alone isn't
        # sufficient -- Windows / Defender / antivirus can hold the file
        # briefly after the process dies. See Wait-DevBridgeBinaryUnlocked.
        $unlocked = Wait-DevBridgeBinaryUnlocked -Path $svcExe -TimeoutSeconds $UnlockTimeoutSeconds
        if (-not $unlocked) {
            throw "Service binary still locked after ${UnlockTimeoutSeconds}s. Aborting to avoid silent no-op install. Manual recovery: Task Manager -> kill devbridge-service.exe -> re-run installer."
        }

        # Capture the pre-install SHA256 so we can verify the swap actually
        # happened. Hash beats mtime here: Tauri/NSIS may preserve the
        # source-binary mtime (especially with reproducible Rust builds via
        # SOURCE_DATE_EPOCH), making an mtime check report a false negative
        # on a real upgrade.
        $preInstallHash = if (Test-Path $svcExe) { (Get-FileHash $svcExe -Algorithm SHA256).Hash } else { "" }

        # Capture the installed version BEFORE running NSIS (review finding
        # F1). Tauri's NSIS writes DisplayVersion=<target> to the Uninstall
        # registry key even when it silently no-oped the binary swap because
        # the file was locked -- reading the registry AFTER Start-Process
        # would read the TARGET version regardless of whether the swap
        # happened, making the "hash unchanged" failure branch below
        # unreachable for a genuinely failed swap.
        $installedVersion = Get-DevBridgeInstalledVersion

        # --- Run installer ---
        Write-Host "Running installer (silent mode)..."
        $process = Start-Process -FilePath $installerPath -ArgumentList "/S" -Wait -PassThru
        if ($process.ExitCode -ne 0) {
            $stillRunning = @(Get-Process -Name "devbridge-*", "DevBridge" -ErrorAction SilentlyContinue |
                ForEach-Object { "{0} PID {1} session {2}" -f $_.Name, $_.Id, $_.SessionId })
            $stillRunningText = if ($stillRunning.Count -gt 0) { $stillRunning -join ", " } else { "none" }
            throw "Installer exited with code $($process.ExitCode) (DevBridge processes still running: $stillRunningText)"
        }

        # --- Verify binary was actually replaced --
        # NSIS exits 0 even when its file-overwrite step silently no-ops (file
        # in use). Compare hashes; if unchanged AND the installed version
        # doesn't already match the target, fail loudly so the operator
        # doesn't think they upgraded when they didn't. A same-version
        # reinstall (hash unchanged, installed version == target -- e.g.
        # DEVBRIDGE_FORCE_CONFIG_REWRITE=true) is NOT a failure; see
        # Test-DevBridgeBinarySwapOk above and issue #71.
        if (Test-Path $svcExe) {
            $postInstallHash = (Get-FileHash $svcExe -Algorithm SHA256).Hash
            $swap = Test-DevBridgeBinarySwapOk -PreHash $preInstallHash -PostHash $postInstallHash `
                -InstalledVersion $installedVersion -TargetVersion $TargetVersion
            if (-not $swap.Ok) {
                throw "Binary SHA256 unchanged after install ($postInstallHash). NSIS likely could not replace the file. Manual recovery: stop the service, delete '$svcExe', re-run the installer."
            }
            if ($swap.Reason -eq "fresh-install") {
                Write-Host "  Binary installed (hash $($postInstallHash.Substring(0,12)))"
            } elseif ($swap.Reason -eq "same-version") {
                Write-Host "  Same version already installed ($TargetVersion) -- binary unchanged, continuing with post-install" -ForegroundColor Yellow
            } else {
                Write-Host "  Binary updated (hash $($preInstallHash.Substring(0,12)) -> $($postInstallHash.Substring(0,12)))"
            }
        }

        # --- Run post-install.ps1 ---
        # Bundled as a Tauri resource (or copied to the install dir). It
        # restarts the service and relaunches the tray app in every session.
        $postInstallCandidates = @(
            (Join-Path $InstallDir "post-install.ps1"),
            (Join-Path $InstallDir "_up_\_up_\installer\post-install.ps1")
        )
        $postInstallScript = $postInstallCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
        if ($postInstallScript) {
            Write-Host "Running post-install script: $postInstallScript"
            $postInstallExitCode = Invoke-DevBridgePostInstallScript -ScriptPath $postInstallScript -Arguments $PostInstallArgs
            if ($postInstallExitCode -ne 0) {
                throw "post-install.ps1 exited with code $postInstallExitCode"
            }
        } else {
            Write-Warning "post-install.ps1 not found in install directory. Skipping post-install configuration."
        }
    } catch {
        $failure = $_
        Write-Host "Upgrade failed: $($failure.Exception.Message)" -ForegroundColor Red
        try {
            Restore-DevBridgeService
        } catch {
            Write-Warning "Restoring DevBridgeService failed too: $($_.Exception.Message) -- start it manually (Start-ScheduledTask DevBridgeService)"
        }
        throw $failure
    }
}

# Build the argument list passed to post-install.ps1 from a snapshot of
# DEVBRIDGE_* env vars (irm|iex can't pass script params directly). Takes the
# resolved Mode plus a hashtable (rather than reading $env: directly) so the
# Pester suite can test the mapping without mutating the process environment.
# See issue #35 (original mapping) and #68 (DEVBRIDGE_SERIAL_PORT/BAUD).
#
# -SerialBaudRate is forwarded ONLY alongside -SerialPort (review finding
# F4): a bare DEVBRIDGE_SERIAL_BAUD with no DEVBRIDGE_SERIAL_PORT has no
# serial_bridge block to apply to, and post-install.ps1's own default (9600)
# already covers the "port set, baud unset" case.
function Get-DevBridgePostInstallArgs {
    param(
        [Parameter(Mandatory)][string]$Mode,
        [Parameter(Mandatory)][hashtable]$Env
    )
    $postArgs = @()
    $postArgs += "-Mode"; $postArgs += $Mode

    if ($Env.DEVBRIDGE_SERVER_HOST)            { $postArgs += "-ServerHost";             $postArgs += $Env.DEVBRIDGE_SERVER_HOST }
    if ($Env.DEVBRIDGE_TARGET_PRINTER)         { $postArgs += "-TargetPrinter";          $postArgs += $Env.DEVBRIDGE_TARGET_PRINTER }
    if ($Env.DEVBRIDGE_CLIENT_ID)              { $postArgs += "-ClientId";               $postArgs += $Env.DEVBRIDGE_CLIENT_ID }
    if ($Env.DEVBRIDGE_VIRTUAL_PRINTER_NAME)   { $postArgs += "-VirtualPrinterName";     $postArgs += $Env.DEVBRIDGE_VIRTUAL_PRINTER_NAME }
    if ($Env.DEVBRIDGE_VIRTUAL_PRINTER_DRIVER) { $postArgs += "-VirtualPrinterDriver";   $postArgs += $Env.DEVBRIDGE_VIRTUAL_PRINTER_DRIVER }
    if ($Env.DEVBRIDGE_PRINTER_DISPLAY_NAME)   { $postArgs += "-PrinterDisplayName";     $postArgs += $Env.DEVBRIDGE_PRINTER_DISPLAY_NAME }
    if ($Env.DEVBRIDGE_PRINT_BACKEND)          { $postArgs += "-PrintBackend";           $postArgs += $Env.DEVBRIDGE_PRINT_BACKEND }
    if ($Env.DEVBRIDGE_PRINTER_ADDRESS)        { $postArgs += "-PrinterAddress";         $postArgs += $Env.DEVBRIDGE_PRINTER_ADDRESS }
    if ($Env.DEVBRIDGE_PRINTER_TLS -eq "true") { $postArgs += "-PrinterTls" }
    if ($Env.DEVBRIDGE_DASHBOARD_PORT)         { $postArgs += "-DashboardPort";          $postArgs += $Env.DEVBRIDGE_DASHBOARD_PORT }
    if ($Env.DEVBRIDGE_GHOSTSCRIPT_DEVICE)     { $postArgs += "-GhostscriptDevice";      $postArgs += $Env.DEVBRIDGE_GHOSTSCRIPT_DEVICE }
    if ($Env.DEVBRIDGE_GHOSTSCRIPT_RESOLUTION) { $postArgs += "-GhostscriptResolution"; $postArgs += $Env.DEVBRIDGE_GHOSTSCRIPT_RESOLUTION }
    if ($Env.DEVBRIDGE_SERIAL_PORT) {
        $postArgs += "-SerialPort"; $postArgs += $Env.DEVBRIDGE_SERIAL_PORT
        if ($Env.DEVBRIDGE_SERIAL_BAUD) { $postArgs += "-SerialBaudRate"; $postArgs += $Env.DEVBRIDGE_SERIAL_BAUD }
    }
    # issue #90: Odoo label source. DEVBRIDGE_ODOO_API_KEY is deliberately NOT
    # forwarded -- post-install.ps1 reads the secret from the inherited
    # environment, so it never appears on a command line / in a process list.
    if ($Env.DEVBRIDGE_ODOO_URL)             { $postArgs += "-OdooUrl";               $postArgs += $Env.DEVBRIDGE_ODOO_URL }
    if ($Env.DEVBRIDGE_ODOO_PRINTER_NAME)    { $postArgs += "-OdooPrinterName";       $postArgs += $Env.DEVBRIDGE_ODOO_PRINTER_NAME }
    if ($Env.DEVBRIDGE_ODOO_LABEL_WIDTH_MM)  { $postArgs += "-OdooLabelWidthMm";      $postArgs += $Env.DEVBRIDGE_ODOO_LABEL_WIDTH_MM }
    if ($Env.DEVBRIDGE_ODOO_LABEL_HEIGHT_MM) { $postArgs += "-OdooLabelHeightMm";     $postArgs += $Env.DEVBRIDGE_ODOO_LABEL_HEIGHT_MM }
    if ($Env.DEVBRIDGE_ODOO_DPI)             { $postArgs += "-OdooDpi";               $postArgs += $Env.DEVBRIDGE_ODOO_DPI }
    # issue #69: server-side serial bridge mappings, forwarded ONLY in server
    # mode (a client has no [[server.serial_bridges]] to write).
    if ($Mode -eq "server" -and $Env.DEVBRIDGE_SERIAL_BRIDGES) {
        $postArgs += "-SerialBridges"; $postArgs += $Env.DEVBRIDGE_SERIAL_BRIDGES
    }

    return $postArgs
}

# Validate DEVBRIDGE_SERIAL_BAUD (if set) is a positive integer BEFORE any
# destructive action -- review finding F4. Called near the top of the script
# (see below), not from Get-DevBridgePostInstallArgs, because that function
# is only invoked after the service binary has already been stopped and
# swapped; throwing there would mean the installer already did irreversible
# work before discovering a typo'd env var.
function Assert-DevBridgeSerialBaud {
    param([string]$Value)
    if ($Value -and ($Value -notmatch '^\d+$')) {
        throw "DEVBRIDGE_SERIAL_BAUD must be a positive integer, got '$Value'"
    }
}

# Parse DEVBRIDGE_SERIAL_BRIDGES (issue #69): comma list of `client_id=COMn[:baud]`
# (baud default 9600), e.g. "pjkeb-client=COM20,pjsln-client=COM22:19200". Pure;
# emits one [pscustomobject]@{ClientId; VirtualPort; BaudRate} per entry (callers
# wrap in @()). Throws on a malformed entry, zero baud, or a duplicate client_id
# (case-sensitive, like the Rust HashMap) / virtual port, so a typo fails BEFORE
# any change. DEFINED IDENTICALLY in install.ps1 and DevBridgeInstallerLib.ps1
# (install.ps1 runs via irm|iex and cannot dot-source the lib); Pester asserts
# the two copies are byte-identical.
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

$requestedVersion = if ($env:DEVBRIDGE_VERSION) { $env:DEVBRIDGE_VERSION } else { "latest" }

Write-Host "==> DevBridge Installer" -ForegroundColor Cyan

# Fail fast on a bad DEVBRIDGE_SERIAL_BAUD, before touching VC++, the
# service binary, or anything else irreversible (review finding F4).
Assert-DevBridgeSerialBaud -Value $env:DEVBRIDGE_SERIAL_BAUD
# Same for DEVBRIDGE_SERIAL_BRIDGES (issue #69): a malformed mapping list
# throws here, before anything irreversible happens -- in ANY mode (the mode is
# only resolved later; a client install merely ignores a valid value).
$requestedSerialBridges = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec $env:DEVBRIDGE_SERIAL_BRIDGES)
if ($requestedSerialBridges.Count -gt 0) {
    Write-Host "DEVBRIDGE_SERIAL_BRIDGES: $($requestedSerialBridges.Count) mapping(s) parsed (applied in server mode only)"
}

# --- Ensure Visual C++ Redistributable (required by bundled Ghostscript) ---
# gsdll64.dll links against msvcp140.dll / vcruntime140.dll. On a fresh
# Windows box without VC++ 2015-2022 Redistributable, Ghostscript fails
# with LoadLibrary error 126 and every print job fails with exit code
# -1073741515 (STATUS_DLL_NOT_FOUND). Check for the runtime DLLs in
# System32 and install silently if missing (same helper post-install.ps1 uses,
# issue #85).
$vcMissing = @(Get-DevBridgeMissingVcRuntimeDlls -System32 ([System.Environment]::SystemDirectory))
if ($vcMissing.Count -gt 0) {
    Write-Host "Installing Visual C++ Runtime (required by Ghostscript; missing: $($vcMissing -join ', '))..."
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

# --- Installer source ---
# DEVBRIDGE_INSTALLER_PATH (issue #93): install from a LOCAL NSIS .exe instead
# of a GitHub release -- an offline install, and how the CI E2E runs this very
# script on the build under test (its dev-latest release is only published
# after the E2E passes). The target version comes from the file name
# (DevBridge_<ver>_x64-setup.exe); a local file has no SHA256SUMS to check.
# Validated here, before anything is stopped.
$localInstaller = $env:DEVBRIDGE_INSTALLER_PATH
$tempDir = $null
if ($localInstaller) {
    if (-not (Test-Path -LiteralPath $localInstaller -PathType Leaf)) {
        throw "DEVBRIDGE_INSTALLER_PATH '$localInstaller' is not a file -- nothing was changed"
    }
    $installerPath = (Resolve-Path -LiteralPath $localInstaller).Path
    $fileName = Split-Path -Leaf $installerPath
    $targetVersion = Get-DevBridgeVersionFromAssetName -Name $fileName
    if (-not $targetVersion) {
        throw "DEVBRIDGE_INSTALLER_PATH file name '$fileName' carries no version (expected DevBridge_<major.minor.patch>_x64-setup.exe) -- nothing was changed"
    }
    $version = "v$targetVersion"
    if ($env:DEVBRIDGE_VERSION) {
        Write-Warning "DEVBRIDGE_VERSION='$($env:DEVBRIDGE_VERSION)' is ignored: DEVBRIDGE_INSTALLER_PATH selects the installer"
    }
    Write-Host "Using local installer $installerPath (version $version; no release download, no checksum file)"
} else {
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

    # Real target semver for the same-version-reinstall check (review finding
    # F2) -- $version above is $release.tag_name ("dev-latest" on the dev
    # channel), not usable for the comparison in Test-DevBridgeBinarySwapOk.
    # Fall back to the tag (leading v/V stripped) if the asset name ever
    # doesn't match the expected DevBridge_<ver>_x64-setup.exe shape.
    $targetVersion = Get-DevBridgeVersionFromAssetName -Name $fileName
    if (-not $targetVersion) { $targetVersion = $version.Trim() -replace '^[vV]', '' }

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
}

# --- Post-install arguments (read-only, resolved BEFORE anything is stopped) ---
# irm|iex can't pass script params directly, so post-install.ps1 gets them from
# the environment. Mode resolution: env var wins; otherwise inherit from the
# preserved config (so a bare upgrade doesn't silently flip a client back to
# server defaults); otherwise fall back to "server" for greenfield installs.
$installDir = "C:\Program Files\DevBridge"
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

# Generic snapshot of every DEVBRIDGE_* env var (review finding F6) --
# Get-DevBridgePostInstallArgs picks out only the ones it maps, so a new
# DEVBRIDGE_* var no longer needs a matching line added here by hand.
$envSnapshot = @{}
Get-ChildItem Env: | Where-Object { $_.Name -like 'DEVBRIDGE_*' } | ForEach-Object {
    $envSnapshot[$_.Name] = $_.Value
}
$postArgs = Get-DevBridgePostInstallArgs -Mode $mode -Env $envSnapshot
if ($env:DEVBRIDGE_SERIAL_BRIDGES -and $mode -ne "server") {
    Write-Warning "DEVBRIDGE_SERIAL_BRIDGES is a server-mode setting; ignored in $mode mode"
}

# --- Upgrade: stop -> NSIS -> verify -> post-install, guarded (issue #93) ---
# Any failure in there restarts DevBridgeService before the error surfaces.
Install-DevBridgePackage -InstallerPath $installerPath -TargetVersion $targetVersion `
    -InstallDir $installDir -PostInstallArgs $postArgs

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

# --- Cleanup (the download dir only -- never a DEVBRIDGE_INSTALLER_PATH file) ---
if ($tempDir) {
    Remove-Item -Recurse -Force $tempDir -ErrorAction SilentlyContinue
}

Write-Host "`n==> DevBridge $version installed successfully." -ForegroundColor Cyan
