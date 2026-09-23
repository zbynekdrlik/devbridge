# CI installer packaging gate (issue #80) -- runs on pz-snv in the
# e2e-deploy-client job, right after the silent NSIS install, as SYSTEM under
# Windows PowerShell 5.1 (exactly how install.ps1 / auto-update run it in
# production).
#
# post-install.ps1 dot-sources its sibling DevBridgeInstallerLib.ps1, so a
# packaging mistake (lib not bundled, wrong relocated dir, stale copy) would
# break the installer on EVERY store through auto-update. This gate proves the
# INSTALLED layout works:
#   1. both files exist in <InstallDir>\_up_\_up_\installer\ and match this
#      commit's checkout (line endings normalized -- the build checkout and this
#      runner's checkout may differ in autocrlf);
#   2. the INSTALLED post-install.ps1 -ValidateOnly -Mode client exits 0 with
#      "VALIDATE-OK <n> functions", <n> = the function count of this commit's lib;
#   3. the same post-install.ps1 WITHOUT the lib next to it exits 1 (fail loud);
#   4. the PRODUCTION config (C:\ProgramData\DevBridge\config.toml -- pz-snv is
#      the live pjsnvs store) is byte-identical before/after and no config
#      snapshot file appeared. -ValidateOnly never writes; this proves it.
# It NEVER runs post-install for real.

param(
    [string]$ProductionDataDir = "C:\ProgramData\DevBridge"
)

$ErrorActionPreference = "Stop"

Write-Host "=== Installer packaging gate (issue #80) ===" -ForegroundColor Cyan
Write-Host "PowerShell $($PSVersionTable.PSVersion) as $([Security.Principal.WindowsIdentity]::GetCurrent().Name)"

$repoRoot = Split-Path -Parent $PSScriptRoot

function Get-NormalizedText {
    param([Parameter(Mandatory)][string]$Path)
    return ([System.IO.File]::ReadAllText($Path)) -replace "`r`n", "`n"
}

function Get-ConfigState {
    param([Parameter(Mandatory)][string]$DataDir)
    $configPath = Join-Path $DataDir "config.toml"
    $hash = if (Test-Path -LiteralPath $configPath) { (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash } else { "absent" }
    $files = @(Get-ChildItem -LiteralPath $DataDir -Filter "config.toml*" -Force -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Name } | Sort-Object) -join ","
    return [pscustomobject]@{ Hash = $hash; Files = $files }
}

# Run a script under Windows PowerShell 5.1 exactly like install.ps1 does
# (powershell.exe -ExecutionPolicy Bypass -File), capturing stdout/stderr.
function Invoke-PostInstall {
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [Parameter(Mandatory)][string]$Arguments
    )
    $outDir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbgate-out-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    try {
        $outFile = Join-Path $outDir "stdout.txt"
        $errFile = Join-Path $outDir "stderr.txt"
        $argLine = "-NoProfile -ExecutionPolicy Bypass -File `"$ScriptPath`" $Arguments"
        $proc = Start-Process -FilePath "powershell.exe" -ArgumentList $argLine -Wait -PassThru -NoNewWindow `
            -RedirectStandardOutput $outFile -RedirectStandardError $errFile
        return [pscustomobject]@{
            ExitCode = $proc.ExitCode
            StdOut   = [System.IO.File]::ReadAllText($outFile)
            StdErr   = [System.IO.File]::ReadAllText($errFile)
        }
    } finally {
        Remove-Item -Recurse -Force $outDir -ErrorAction SilentlyContinue
    }
}

# -- 1. Installed layout ------------------------------------------------------
$installCandidates = @(
    "C:\Program Files\DevBridge",
    "$env:LOCALAPPDATA\DevBridge",
    "$env:LOCALAPPDATA\Programs\DevBridge"
)
$installDir = $installCandidates | Where-Object { Test-Path (Join-Path $_ "devbridge-service.exe") } | Select-Object -First 1
if (-not $installDir) {
    throw "PACKAGING-GATE FAIL: devbridge-service.exe not found in any install location"
}
$installedInstallerDir = Join-Path $installDir "_up_\_up_\installer"
$installedPostInstall = Join-Path $installedInstallerDir "post-install.ps1"
$installedLib = Join-Path $installedInstallerDir "DevBridgeInstallerLib.ps1"
foreach ($f in @($installedPostInstall, $installedLib)) {
    if (-not (Test-Path -LiteralPath $f -PathType Leaf)) {
        throw "PACKAGING-GATE FAIL: installed file missing: $f"
    }
    Write-Host "PACKAGING-GATE: installed $f ($((Get-Item -LiteralPath $f).Length) bytes)"
}

$repoPostInstall = Join-Path $repoRoot "installer\post-install.ps1"
$repoLib = Join-Path $repoRoot "installer\DevBridgeInstallerLib.ps1"
if ((Get-NormalizedText $installedPostInstall) -ne (Get-NormalizedText $repoPostInstall)) {
    throw "PACKAGING-GATE FAIL: installed post-install.ps1 differs from this commit's installer/post-install.ps1"
}
if ((Get-NormalizedText $installedLib) -ne (Get-NormalizedText $repoLib)) {
    throw "PACKAGING-GATE FAIL: installed DevBridgeInstallerLib.ps1 differs from this commit's installer/DevBridgeInstallerLib.ps1"
}
Write-Host "PACKAGING-GATE: installed post-install.ps1 + DevBridgeInstallerLib.ps1 match this commit"

# install.ps1 tries a ROOT <InstallDir>\post-install.ps1 BEFORE the _up_ copy.
# NSIS never updates such a copy and no lib sits next to it, so install.ps1
# would run stale logic (or, for a lib-based copy, exit 1) on every upgrade.
$rootPostInstall = Join-Path $installDir "post-install.ps1"
if (Test-Path -LiteralPath $rootPostInstall) {
    throw "PACKAGING-GATE FAIL: stale $rootPostInstall would be picked by install.ps1 before the bundled _up_ copy"
}
Write-Host "PACKAGING-GATE: no root post-install.ps1 shadows the _up_ copy for install.ps1"

$libTokens = $null; $libErrors = $null
$libAst = [System.Management.Automation.Language.Parser]::ParseFile($repoLib, [ref]$libTokens, [ref]$libErrors)
$expectedCount = @($libAst.FindAll({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)).Count
if ($expectedCount -lt 1) {
    throw "PACKAGING-GATE FAIL: no function definitions found in $repoLib"
}

# -- Production config: state BEFORE ------------------------------------------
$before = Get-ConfigState -DataDir $ProductionDataDir
Write-Host "PACKAGING-GATE: production config.toml SHA256 before = $($before.Hash) (files: $($before.Files))"
# Precondition: -ValidateOnly -Mode client below relies on the PRESERVED-config
# path (no printer / gRPC probe with default params). Without a production
# config it would fail for a reason unrelated to packaging.
if ($before.Hash -eq "absent") {
    throw "PACKAGING-GATE FAIL: precondition - $ProductionDataDir\config.toml is missing on this runner (the gate expects the pjsnvs production client)"
}

$negDir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbgate-nolib-" + [guid]::NewGuid().ToString("N"))
$negDataDir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbgate-data-" + [guid]::NewGuid().ToString("N"))
try {
    # -- 2. Installed post-install.ps1 -ValidateOnly under real 5.1 -----------
    $r = Invoke-PostInstall -ScriptPath $installedPostInstall -Arguments "-ValidateOnly -Mode client"
    Write-Host "--- post-install.ps1 -ValidateOnly -Mode client (exit $($r.ExitCode)) ---"
    Write-Host $r.StdOut
    if ($r.StdErr.Trim()) { Write-Host "stderr: $($r.StdErr)" }
    if ($r.ExitCode -ne 0) {
        throw "PACKAGING-GATE FAIL: installed post-install.ps1 -ValidateOnly exited $($r.ExitCode)"
    }
    $ok = [regex]::Match($r.StdOut, "VALIDATE-OK (\d+) functions")
    if (-not $ok.Success) {
        throw "PACKAGING-GATE FAIL: no VALIDATE-OK line from the installed post-install.ps1 -ValidateOnly"
    }
    $loaded = [int]$ok.Groups[1].Value
    if ($loaded -ne $expectedCount) {
        throw "PACKAGING-GATE FAIL: installed lib loaded $loaded functions, this commit's lib defines $expectedCount"
    }
    Write-Host "PACKAGING-GATE: installed post-install.ps1 -ValidateOnly -> VALIDATE-OK $loaded functions (expected $expectedCount), exit 0" -ForegroundColor Green

    # -- 3. Missing lib -> exit 1 ---------------------------------------------
    New-Item -ItemType Directory -Force -Path $negDir | Out-Null
    New-Item -ItemType Directory -Force -Path $negDataDir | Out-Null
    Copy-Item -LiteralPath $installedPostInstall -Destination (Join-Path $negDir "post-install.ps1")
    $neg = Invoke-PostInstall -ScriptPath (Join-Path $negDir "post-install.ps1") -Arguments "-ValidateOnly -Mode client -DataDir `"$negDataDir`""
    if ($neg.ExitCode -ne 1) {
        throw "PACKAGING-GATE FAIL: post-install.ps1 without its lib exited $($neg.ExitCode), expected 1"
    }
    if ($neg.StdOut -match "VALIDATE-OK") {
        throw "PACKAGING-GATE FAIL: post-install.ps1 without its lib printed VALIDATE-OK"
    }
    if (($neg.StdOut + $neg.StdErr) -notmatch "DevBridgeInstallerLib\.ps1 not found") {
        throw "PACKAGING-GATE FAIL: post-install.ps1 without its lib did not report the missing lib"
    }
    if (@(Get-ChildItem -LiteralPath $negDataDir -Force).Count -ne 0) {
        throw "PACKAGING-GATE FAIL: post-install.ps1 without its lib wrote into its data dir"
    }
    Write-Host "PACKAGING-GATE: post-install.ps1 without DevBridgeInstallerLib.ps1 -> exit 1, nothing written" -ForegroundColor Green
} finally {
    Remove-Item -Recurse -Force $negDir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $negDataDir -ErrorAction SilentlyContinue
}

# -- 4. Production config: state AFTER ----------------------------------------
$after = Get-ConfigState -DataDir $ProductionDataDir
Write-Host "PACKAGING-GATE: production config.toml SHA256 after  = $($after.Hash) (files: $($after.Files))"
if ($after.Hash -ne $before.Hash) {
    throw "PACKAGING-GATE FAIL: production config.toml changed ($($before.Hash) -> $($after.Hash))"
}
if ($after.Files -ne $before.Files) {
    throw "PACKAGING-GATE FAIL: production config files changed ($($before.Files) -> $($after.Files))"
}
Write-Host "PACKAGING-GATE PASS: installed layout loads under Windows PowerShell $($PSVersionTable.PSVersion); production config unchanged" -ForegroundColor Green
