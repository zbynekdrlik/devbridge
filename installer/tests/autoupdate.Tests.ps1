# Pester v5 tests for the DevBridge auto-update DECISION logic (issue #54).
#
# The functions under test are defined INLINE inside the production script
# installer/autoupdate.ps1 (the scheduled task DevBridgeAutoUpdate runs that
# file). To test the REAL code with zero drift, BeforeAll parses the script with
# the PowerShell AST and extracts the named pure-decision function bodies WITHOUT
# executing the script (the orchestration that hits GitHub / the registry /
# install.ps1 is NOT run here — it is real-machine-only).
#
# Covered (every guard the issue mandates):
#   * version compare + semver parse (incl. v-prefix, pre-release suffix)
#   * patch-only lock: refuse minor/major bump in either direction, refuse
#     downgrade, refuse equal; allow ONLY a strictly-newer patch
#   * kill-switch: env var AND marker file both honored (marker wins)
#   * skip-when-active-jobs: idle vs jobs-active vs unknown
#   * no-op-on-fetch-failure: an unparseable/empty latest yields no update
#
# Every test runs in-process against the extracted functions; nothing touches a
# real machine. CI runs this via Invoke-Pester on windows-latest alongside the
# existing installer-lib suite.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot

    # Parse the real autoupdate.ps1 and return @{ name = function-source-text }
    # WITHOUT executing the script body (which would run the live update flow).
    function Get-FunctionSourceFromScript {
        param(
            [Parameter(Mandatory)][string]$ScriptPath,
            [Parameter(Mandatory)][string[]]$Names
        )
        $tokens = $null; $errors = $null
        $ast = [System.Management.Automation.Language.Parser]::ParseFile(
            $ScriptPath, [ref]$tokens, [ref]$errors)
        if ($errors -and $errors.Count -gt 0) {
            throw "Parse errors in ${ScriptPath}: $($errors -join '; ')"
        }
        $funcs = $ast.FindAll(
            { param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] },
            $true)
        $out = [ordered]@{}
        foreach ($name in $Names) {
            $def = $funcs | Where-Object { $_.Name -eq $name } | Select-Object -First 1
            if (-not $def) {
                throw "Function '$name' not found in $ScriptPath (was it renamed/removed?)"
            }
            $out[$name] = $def.Extent.Text
        }
        return $out
    }

    $sources = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "autoupdate.ps1") `
        -Names @(
            "Get-DevBridgeSemVer",
            "Get-DevBridgeUpdateDecision",
            "Test-DevBridgeAutoUpdateDisabled",
            "Test-DevBridgeSafeToUpdate"
        )
    foreach ($src in $sources.Values) {
        . ([scriptblock]::Create($src))
    }
}

Describe "Get-DevBridgeSemVer (semver parse)" {
    It "parses '<value>' to <maj>.<min>.<pat>" -ForEach @(
        @{ value = "0.8.28";        maj = 0; min = 8; pat = 28 }
        @{ value = "v0.8.28";       maj = 0; min = 8; pat = 28 }
        @{ value = "1.2.3";         maj = 1; min = 2; pat = 3 }
        @{ value = "0.8.28-dev.3";  maj = 0; min = 8; pat = 28 }  # pre-release suffix ignored
        @{ value = "v10.20.30";     maj = 10; min = 20; pat = 30 }
        @{ value = "  0.8.28  ";    maj = 0; min = 8; pat = 28 }  # whitespace tolerated
    ) {
        $r = Get-DevBridgeSemVer $value
        $r | Should -Not -BeNullOrEmpty
        $r.Major | Should -Be $maj
        $r.Minor | Should -Be $min
        $r.Patch | Should -Be $pat
    }

    It "returns `$null for unparseable '<value>'" -ForEach @(
        @{ value = "" }
        @{ value = $null }
        @{ value = "latest" }
        @{ value = "0.8" }       # missing patch
        @{ value = "abc" }
        @{ value = "v" }
    ) {
        Get-DevBridgeSemVer $value | Should -BeNullOrEmpty
    }
}

Describe "Get-DevBridgeUpdateDecision (patch-only lock)" {
    It "ALLOWS a strictly-newer patch on the same major.minor" {
        $d = Get-DevBridgeUpdateDecision -Installed "0.8.27" -Latest "0.8.28"
        $d.ShouldUpdate | Should -BeTrue
        $d.Reason | Should -Be "patch-update"
    }

    It "REFUSES an equal version (already up to date)" {
        $d = Get-DevBridgeUpdateDecision -Installed "0.8.28" -Latest "0.8.28"
        $d.ShouldUpdate | Should -BeFalse
        $d.Reason | Should -Be "up-to-date"
    }

    It "REFUSES a downgrade (latest patch lower than installed)" {
        $d = Get-DevBridgeUpdateDecision -Installed "0.8.28" -Latest "0.8.27"
        $d.ShouldUpdate | Should -BeFalse
        $d.Reason | Should -Be "downgrade"
    }

    It "REFUSES a MINOR bump even when newer (operator-gated)" {
        $d = Get-DevBridgeUpdateDecision -Installed "0.8.28" -Latest "0.9.0"
        $d.ShouldUpdate | Should -BeFalse
        $d.Reason | Should -Be "minor-or-major-change"
    }

    It "REFUSES a MAJOR bump even when newer (operator-gated)" {
        $d = Get-DevBridgeUpdateDecision -Installed "0.8.28" -Latest "1.0.0"
        $d.ShouldUpdate | Should -BeFalse
        $d.Reason | Should -Be "minor-or-major-change"
    }

    It "REFUSES a minor DOWNGRADE too (cross-minor in either direction)" {
        $d = Get-DevBridgeUpdateDecision -Installed "0.9.0" -Latest "0.8.99"
        $d.ShouldUpdate | Should -BeFalse
        $d.Reason | Should -Be "minor-or-major-change"
    }

    It "tolerates a leading 'v' on the latest tag and still patch-updates" {
        $d = Get-DevBridgeUpdateDecision -Installed "0.8.27" -Latest "v0.8.28"
        $d.ShouldUpdate | Should -BeTrue
        $d.Reason | Should -Be "patch-update"
    }

    It "NO-OPS on unparseable installed version (fail-safe)" {
        $d = Get-DevBridgeUpdateDecision -Installed "garbage" -Latest "0.8.28"
        $d.ShouldUpdate | Should -BeFalse
        $d.Reason | Should -Be "unparseable-installed"
    }

    It "NO-OPS on unparseable / empty latest version (fetch-failure fail-safe)" -ForEach @(
        @{ latest = "latest" }
        @{ latest = "" }
        @{ latest = "not-a-version" }
    ) {
        $d = Get-DevBridgeUpdateDecision -Installed "0.8.27" -Latest $latest
        $d.ShouldUpdate | Should -BeFalse
        $d.Reason | Should -Be "unparseable-latest"
    }
}

Describe "Test-DevBridgeAutoUpdateDisabled (kill-switch: env var + marker file)" {
    It "is DISABLED when the marker file exists (regardless of env)" {
        $r = Test-DevBridgeAutoUpdateDisabled -EnvValue "" -MarkerFileExists $true
        $r.Disabled | Should -BeTrue
        $r.Reason | Should -Be "marker-file"
    }

    It "marker file WINS even when env says 'on'" {
        $r = Test-DevBridgeAutoUpdateDisabled -EnvValue "on" -MarkerFileExists $true
        $r.Disabled | Should -BeTrue
        $r.Reason | Should -Be "marker-file"
    }

    It "is DISABLED via env var '<value>' (no marker file)" -ForEach @(
        @{ value = "off" }
        @{ value = "OFF" }
        @{ value = "false" }
        @{ value = "0" }
        @{ value = "no" }
        @{ value = "disabled" }
        @{ value = "  off  " }   # whitespace tolerated
    ) {
        $r = Test-DevBridgeAutoUpdateDisabled -EnvValue $value -MarkerFileExists $false
        $r.Disabled | Should -BeTrue
        $r.Reason | Should -Be "env-var"
    }

    It "is ENABLED (not disabled) for env '<value>' with no marker file" -ForEach @(
        @{ value = "" }
        @{ value = "on" }
        @{ value = "true" }
        @{ value = "1" }
        @{ value = "yes" }
        @{ value = "anything-else" }
    ) {
        $r = Test-DevBridgeAutoUpdateDisabled -EnvValue $value -MarkerFileExists $false
        $r.Disabled | Should -BeFalse
        $r.Reason | Should -Be "none"
    }
}

Describe "Test-DevBridgeSafeToUpdate (skip-if-printing)" {
    It "is SAFE only when exactly 0 jobs are active" {
        $r = Test-DevBridgeSafeToUpdate -ActiveJobs 0
        $r.Safe | Should -BeTrue
        $r.Reason | Should -Be "idle"
    }

    It "is NOT safe when <n> job(s) are active" -ForEach @(
        @{ n = 1 }
        @{ n = 3 }
        @{ n = 99 }
    ) {
        $r = Test-DevBridgeSafeToUpdate -ActiveJobs $n
        $r.Safe | Should -BeFalse
        $r.Reason | Should -Be "jobs-active"
    }

    It "is NOT safe when the active-job count is unknown (`$null status)" {
        $r = Test-DevBridgeSafeToUpdate -ActiveJobs $null
        $r.Safe | Should -BeFalse
        $r.Reason | Should -Be "status-unknown"
    }
}
