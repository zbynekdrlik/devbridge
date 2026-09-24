# Pester v5 tests for the VC++ runtime check (issue #85).
#
# install.ps1 and post-install.ps1 both decide "is the VC++ 2015-2022 runtime
# there?" through Get-DevBridgeMissingVcRuntimeDlls -- by the DLL FILES the Rust
# binary and Ghostscript load, not the VisualStudio 14.0 registry key (absent on
# pjkes although the DLLs are present -> false "VC++ Runtime not found"). The
# helper is defined in DevBridgeInstallerLib.ps1 AND inline in install.ps1
# (irm|iex cannot dot-source), so these tests pin both copies byte-identical and
# exercise the REAL lib copy, extracted via the shared AST extractor.
#
# Every test runs against a throwaway temp dir; nothing touches a real System32.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    function New-TempDataDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbvc-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        return $dir
    }
}

Describe "Get-DevBridgeMissingVcRuntimeDlls (issue #85 -- VC++ runtime check by DLL files, not registry)" {
    BeforeAll {
        # Loaded from the LIB copy (what post-install.ps1 calls); the byte-identical
        # test below pins the inline install.ps1 copy to it.
        $vcSources = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "DevBridgeInstallerLib.ps1") `
            -Names @("Get-DevBridgeMissingVcRuntimeDlls")
        . ([scriptblock]::Create($vcSources["Get-DevBridgeMissingVcRuntimeDlls"]))
    }

    BeforeEach {
        $script:sys32 = New-TempDataDir
    }

    AfterEach {
        Remove-Item -Recurse -Force $script:sys32 -ErrorAction SilentlyContinue
    }

    It "returns nothing when both vcruntime140.dll and msvcp140.dll exist" {
        Set-Content -Path (Join-Path $script:sys32 "vcruntime140.dll") -Value "x"
        Set-Content -Path (Join-Path $script:sys32 "msvcp140.dll") -Value "x"
        @(Get-DevBridgeMissingVcRuntimeDlls -System32 $script:sys32).Count | Should -Be 0
    }

    It "returns only the missing DLL path when one of the two is absent" {
        Set-Content -Path (Join-Path $script:sys32 "vcruntime140.dll") -Value "x"
        $missing = @(Get-DevBridgeMissingVcRuntimeDlls -System32 $script:sys32)
        ($missing -join ",") | Should -BeExactly (Join-Path $script:sys32 "msvcp140.dll")
    }

    It "returns both DLL paths (vcruntime140 first) when neither exists" {
        $missing = @(Get-DevBridgeMissingVcRuntimeDlls -System32 $script:sys32)
        ($missing -join ",") | Should -BeExactly ((Join-Path $script:sys32 "vcruntime140.dll") + "," + (Join-Path $script:sys32 "msvcp140.dll"))
    }

    It "does not count a DIRECTORY named like the DLL as present" {
        New-Item -ItemType Directory -Force -Path (Join-Path $script:sys32 "vcruntime140.dll") | Out-Null
        Set-Content -Path (Join-Path $script:sys32 "msvcp140.dll") -Value "x"
        $missing = @(Get-DevBridgeMissingVcRuntimeDlls -System32 $script:sys32)
        ($missing -join ",") | Should -BeExactly (Join-Path $script:sys32 "vcruntime140.dll")
    }

    It "is byte-identical in install.ps1 and DevBridgeInstallerLib.ps1 (no drift between the two copies)" {
        $fromInstall = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "install.ps1") `
            -Names @("Get-DevBridgeMissingVcRuntimeDlls")
        $fromLib = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "DevBridgeInstallerLib.ps1") `
            -Names @("Get-DevBridgeMissingVcRuntimeDlls")
        $fromInstall["Get-DevBridgeMissingVcRuntimeDlls"] | Should -BeExactly $fromLib["Get-DevBridgeMissingVcRuntimeDlls"]
    }
}

Describe "VC++ runtime check wiring in install.ps1 and post-install.ps1 (issue #85)" {
    BeforeAll {
        $script:postInstallText = [System.IO.File]::ReadAllText((Join-Path $installerDir "post-install.ps1"))
        $script:installText = [System.IO.File]::ReadAllText((Join-Path $installerDir "install.ps1"))
    }

    It "post-install.ps1 no longer keys the check on the VisualStudio 14.0 registry key (false 'not found' on pjkes)" {
        # Asserts on CODE (a registry read of the VisualStudio key), not on comment wording.
        $script:postInstallText | Should -Not -Match 'Get-ItemProperty\s+["'']?HKLM:\\SOFTWARE\\Microsoft\\VisualStudio'
    }

    It "post-install.ps1 calls the shared DLL helper AFTER the -ValidateOnly exit (validation stays read-only)" {
        $validateIdx = $script:postInstallText.IndexOf('if ($ValidateOnly) {')
        $callIdx = $script:postInstallText.IndexOf('Get-DevBridgeMissingVcRuntimeDlls -System32')
        $validateIdx | Should -BeGreaterThan -1
        $callIdx | Should -BeGreaterThan $validateIdx
    }

    It "post-install.ps1 logs that the runtime DLLs are present on the happy path" {
        $script:postInstallText | Should -Match ([regex]::Escape('VC++ runtime DLLs present'))
    }

    It "install.ps1 uses the shared DLL helper instead of its own ad-hoc DLL list" {
        $script:installText | Should -Match ([regex]::Escape('Get-DevBridgeMissingVcRuntimeDlls -System32'))
        $script:installText | Should -Not -Match ([regex]::Escape('$vcRuntimeDlls'))
    }
}
