# Pester v5 tests for the shared AST function extractor
# deploy/lib/Get-FunctionSourceFromScript.ps1 (issue #70).
#
# The helper is shared by both installer Pester suites AND by the CI E2E client
# setup (deploy/e2e-setup-client-local.ps1), which uses it to call the REAL
# Merge-DevBridgeSerialBridgeIntoConfig on the isolated E2E config. These tests
# pin its contract (exact extent text, script body never executed, loud failure
# on a missing function / parse error) and replay the exact E2E setup usage on
# a temp config so a break surfaces here instead of on the self-hosted runner.
#
# Every test runs against a throwaway temp dir; nothing touches a real
# C:\ProgramData\DevBridge or DevBridge-E2E.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    $repoRoot = Split-Path -Parent $installerDir
    . (Join-Path (Join-Path $repoRoot "deploy") "lib\Get-FunctionSourceFromScript.ps1")

    function New-TempDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbimport-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        return $dir
    }
}

Describe "Get-FunctionSourceFromScript (shared AST extractor, issue #70)" {
    BeforeEach {
        $script:dir = New-TempDir
        $script:marker = Join-Path $script:dir "body-ran.txt"
        $script:scriptPath = Join-Path $script:dir "sample.ps1"
        $body = @(
            'function Get-Alpha {',
            '    param([int]$X)',
            '    return $X * 2',
            '}',
            'function Get-Beta { return "beta" }',
            ('Set-Content -Path "' + $script:marker + '" -Value "executed"')
        ) -join "`r`n"
        [System.IO.File]::WriteAllText($script:scriptPath, $body)
    }

    AfterEach {
        Remove-Item -Recurse -Force $script:dir -ErrorAction SilentlyContinue
    }

    It "returns the exact function source text, in the requested order" {
        $sources = Get-FunctionSourceFromScript -ScriptPath $script:scriptPath -Names @("Get-Beta", "Get-Alpha")
        (@($sources.Keys) -join ",") | Should -BeExactly "Get-Beta,Get-Alpha"
        $sources["Get-Beta"] | Should -BeExactly 'function Get-Beta { return "beta" }'
        $sources["Get-Alpha"] | Should -BeExactly (@(
            'function Get-Alpha {',
            '    param([int]$X)',
            '    return $X * 2',
            '}'
        ) -join "`r`n")
    }

    It "never executes the script body" {
        Get-FunctionSourceFromScript -ScriptPath $script:scriptPath -Names @("Get-Alpha") | Out-Null
        Test-Path $script:marker | Should -BeFalse
    }

    It "yields callable functions once dot-sourced" {
        $sources = Get-FunctionSourceFromScript -ScriptPath $script:scriptPath -Names @("Get-Alpha")
        foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }
        Get-Alpha -X 21 | Should -Be 42
    }

    It "throws when a requested function is missing" {
        { Get-FunctionSourceFromScript -ScriptPath $script:scriptPath -Names @("Get-Gamma") } |
            Should -Throw "*Function 'Get-Gamma' not found*"
    }

    It "throws on a parse error" {
        [System.IO.File]::WriteAllText($script:scriptPath, "function Broken {`r`n")
        { Get-FunctionSourceFromScript -ScriptPath $script:scriptPath -Names @("Broken") } |
            Should -Throw "*Parse errors in*"
    }
}

Describe "E2E client setup serial-bridge merge (issue #70 -- replays deploy/e2e-setup-client-local.ps1)" {
    BeforeAll {
        # Exactly what the E2E setup does: extract the REAL merge + its TOML
        # builder from post-install.ps1 and dot-source them here.
        $sources = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "post-install.ps1") `
            -Names @("Get-DevBridgeSerialBridgeToml", "Merge-DevBridgeSerialBridgeIntoConfig")
        foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }
    }

    BeforeEach {
        $script:dir = New-TempDir
        $script:configPath = Join-Path $script:dir "config.toml"
        # Same shape + encoding as the E2E setup's config (Set-Content ASCII, CRLF).
        $e2eConfig = @(
            '[general]',
            'mode = "client"',
            '',
            '[client]',
            'server_address = "10.88.1.100:50152"',
            'client_id = "e2e-client"',
            '',
            '[jobs]',
            'max_retries = 3'
        ) -join "`r`n"
        $e2eConfig | Set-Content -Path $script:configPath -Encoding ASCII
    }

    AfterEach {
        Remove-Item -Recurse -Force $script:dir -ErrorAction SilentlyContinue
    }

    It "returns 'added' and splices [client.serial_bridge] (COM250/9600) before [jobs]" {
        Merge-DevBridgeSerialBridgeIntoConfig -Path $script:configPath -SerialPort "COM250" -SerialBaudRate 9600 |
            Should -BeExactly "added"
        $text = [System.IO.File]::ReadAllText($script:configPath)
        $text | Should -Match '(?m)^\[client\.serial_bridge\]\r?$'
        $text | Should -Match '(?m)^port = "COM250"\r?$'
        $text | Should -Match '(?m)^baud_rate = 9600\r?$'
        $text.IndexOf("[client.serial_bridge]") | Should -BeLessThan $text.IndexOf("[jobs]")
    }

    It "reports 'kept' on a second call (why the setup rewrites the config fresh before merging)" {
        Merge-DevBridgeSerialBridgeIntoConfig -Path $script:configPath -SerialPort "COM250" | Out-Null
        Merge-DevBridgeSerialBridgeIntoConfig -Path $script:configPath -SerialPort "COM250" |
            Should -BeExactly "kept"
    }
}
