# Pester v5 tests for the post-install.ps1 / DevBridgeInstallerLib.ps1 split
# (issue #80).
#
# post-install.ps1 is shipped to every store through auto-update, so a packaging
# or loading mistake would break the installer fleet-wide. These tests pin:
#   - the lib is a pure, side-effect-free, ASCII function library holding every
#     installer helper (post-install.ps1 itself defines none any more);
#   - the Tauri bundle ships the lib next to post-install.ps1;
#   - post-install.ps1 fails loud (exit 1, nothing touched) when the lib is
#     missing, and -ValidateOnly loads the lib, runs the pre-change validation
#     and exits 0 with VALIDATE-OK without touching the data dir.
# The script runs are real child processes of the CURRENT PowerShell (pwsh 7 in
# one CI step, Windows PowerShell 5.1 in the other). Everything runs in
# throwaway temp dirs; nothing touches a real C:\ProgramData\DevBridge.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    $repoRoot = Split-Path -Parent $installerDir
    $libPath = Join-Path $installerDir "DevBridgeInstallerLib.ps1"
    $postInstallPath = Join-Path $installerDir "post-install.ps1"

    # The helpers the lib must provide (post-install.ps1 calls them; the Pester
    # suites and the E2E client setup extract them by name).
    $expectedLibFunctions = @(
        "Test-DevBridgeForceRewrite",
        "Get-DevBridgeConfigAction",
        "New-DevBridgeConfigSnapshot",
        "Get-DevBridgeSerialBridgeToml",
        "Get-DevBridgeClientConfigExtras",
        "Get-DevBridgeClientConfigProblems",
        "Merge-DevBridgeSerialBridgeIntoConfig",
        "ConvertFrom-DevBridgeSerialBridgesSpec",
        "Get-DevBridgeServerSerialBridgesToml",
        "Add-DevBridgeServerSerialBridgesToConfig",
        "Merge-DevBridgeServerSerialBridgesIntoConfig",
        "Get-DevBridgeCom0comMissingPortWarnings",
        "Get-DevBridgeMissingVcRuntimeDlls"
    )

    function Get-ScriptAst {
        param([Parameter(Mandatory)][string]$Path)
        $tokens = $null; $errors = $null
        $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
        if ($errors -and $errors.Count -gt 0) {
            throw "Parse errors in ${Path}: $($errors -join '; ')"
        }
        return $ast
    }

    function New-TempDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbpostlib-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        return $dir
    }

    # Run a script in a child process of the SAME PowerShell edition as this
    # test run (so the 5.1 CI step really exercises Windows PowerShell 5.1).
    # Returns @{ ExitCode; StdOut; StdErr }.
    function Invoke-ChildScript {
        param(
            [Parameter(Mandatory)][string]$ScriptPath,
            [Parameter(Mandatory)][string]$Arguments
        )
        $exe = (Get-Process -Id $PID).Path
        $outDir = New-TempDir
        $outFile = Join-Path $outDir "stdout.txt"
        $errFile = Join-Path $outDir "stderr.txt"
        try {
            $argLine = "-NoProfile -ExecutionPolicy Bypass -File `"$ScriptPath`" $Arguments"
            $proc = Start-Process -FilePath $exe -ArgumentList $argLine -Wait -PassThru -NoNewWindow `
                -RedirectStandardOutput $outFile -RedirectStandardError $errFile
            return @{
                ExitCode = $proc.ExitCode
                StdOut   = [System.IO.File]::ReadAllText($outFile)
                StdErr   = [System.IO.File]::ReadAllText($errFile)
            }
        } finally {
            Remove-Item -Recurse -Force $outDir -ErrorAction SilentlyContinue
        }
    }

    function Get-DirFileList {
        param([Parameter(Mandatory)][string]$Dir)
        return (@(Get-ChildItem -LiteralPath $Dir -Recurse -Force | ForEach-Object { $_.FullName }) -join "|")
    }
}

Describe "DevBridgeInstallerLib.ps1 (issue #80 -- pure installer function library)" {
    It "defines exactly the installer helpers post-install.ps1 needs" {
        $ast = Get-ScriptAst -Path $libPath
        $names = @($ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false) |
            ForEach-Object { $_.Name })
        (@($names | Sort-Object) -join ",") | Should -BeExactly (@($expectedLibFunctions | Sort-Object) -join ",")
    }

    It "contains ONLY function definitions at top level (dot-sourcing has no side effects)" {
        $ast = Get-ScriptAst -Path $libPath
        $ast.ParamBlock | Should -BeNullOrEmpty
        $ast.BeginBlock | Should -BeNullOrEmpty
        $ast.ProcessBlock | Should -BeNullOrEmpty
        $statements = @($ast.EndBlock.Statements)
        $statements.Count | Should -Be $expectedLibFunctions.Count
        foreach ($s in $statements) {
            $s | Should -BeOfType ([System.Management.Automation.Language.FunctionDefinitionAst])
        }
    }

    It "is pure ASCII (Windows PowerShell 5.1 reads a BOM-less UTF-8 script as ANSI)" {
        $bytes = [System.IO.File]::ReadAllBytes($libPath)
        @($bytes | Where-Object { $_ -gt 127 }).Count | Should -Be 0
    }

    It "post-install.ps1 no longer defines any function (all moved to the lib)" {
        $ast = Get-ScriptAst -Path $postInstallPath
        @($ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true)).Count |
            Should -Be 0
    }

    It "is bundled by Tauri next to post-install.ps1 (same _up_\_up_\installer\ dir)" {
        $conf = Get-Content -Raw (Join-Path $repoRoot "crates/devbridge-app/tauri.conf.json") | ConvertFrom-Json
        $resources = @($conf.bundle.resources)
        $resources | Should -Contain "../../installer/post-install.ps1"
        $resources | Should -Contain "../../installer/DevBridgeInstallerLib.ps1"
    }
}

Describe "post-install.ps1 loading the lib (issue #80 -- real child process)" {
    BeforeEach {
        $script:workDir = New-TempDir
        $script:dataDir = New-TempDir
    }

    AfterEach {
        Remove-Item -Recurse -Force $script:workDir -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "exits 1 before any change when DevBridgeInstallerLib.ps1 is missing" {
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "DevBridgeInstallerLib\.ps1 not found"
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        # Failed before the banner: nothing ran, the data dir is untouched.
        $r.StdOut | Should -Not -Match "=== DevBridge Post-Install"
        Get-DirFileList -Dir $script:dataDir | Should -BeExactly ""
    }

    It "-ValidateOnly loads the lib, prints VALIDATE-OK <n> functions and touches nothing" {
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        Copy-Item $libPath (Join-Path $script:workDir "DevBridgeInstallerLib.ps1")
        $before = Get-DirFileList -Dir $script:workDir
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
        $r.StdOut | Should -Match "VALIDATE-OK $($expectedLibFunctions.Count) functions"
        Get-DirFileList -Dir $script:dataDir | Should -BeExactly ""
        Get-DirFileList -Dir $script:workDir | Should -BeExactly $before
    }

    It "-ValidateOnly still validates: a malformed -SerialBridges exits 1 with no VALIDATE-OK" {
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        Copy-Item $libPath (Join-Path $script:workDir "DevBridgeInstallerLib.ps1")
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -SerialBridges `"not-a-mapping`" -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "ERROR:"
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        Get-DirFileList -Dir $script:dataDir | Should -BeExactly ""
    }

    It "-ValidateOnly accepts a valid -SerialBridges spec and reports the parsed mappings" {
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        Copy-Item $libPath (Join-Path $script:workDir "DevBridgeInstallerLib.ps1")
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -SerialBridges `"pjkeb-client=COM20,pjsln-client=COM22:19200`" -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
        $r.StdOut | Should -Match "pjkeb-client->COM20@9600, pjsln-client->COM22@19200"
        $r.StdOut | Should -Match "VALIDATE-OK $($expectedLibFunctions.Count) functions"
        Get-DirFileList -Dir $script:dataDir | Should -BeExactly ""
    }
}
