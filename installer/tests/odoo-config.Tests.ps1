# Pester v5 tests for the Odoo label source installer path (issue #90):
# DEVBRIDGE_ODOO_* env -> post-install args (install.ps1), the [client.odoo]
# TOML builder / validation / preserved-config merge and the API-key file ACL
# (DevBridgeInstallerLib.ps1), and post-install.ps1 itself as a real child
# process (validation before any change, key never printed).
#
# Tests exercise the REAL functions, AST-extracted from the production scripts
# with the shared deploy/lib/Get-FunctionSourceFromScript.ps1. This file is
# pure ASCII (Windows PowerShell 5.1 runs it too); non-ASCII test text is
# built from [char] codes. Everything runs in throwaway temp dirs.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    $libPath = Join-Path $installerDir "DevBridgeInstallerLib.ps1"
    $postInstallPath = Join-Path $installerDir "post-install.ps1"
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    $sources = Get-FunctionSourceFromScript -ScriptPath $libPath -Names @(
        "ConvertTo-DevBridgeTomlString", "Get-DevBridgeOdooConfigProblems", "Get-DevBridgeOdooToml",
        "Get-DevBridgeClientConfigValue", "Merge-DevBridgeOdooIntoConfig", "Test-DevBridgeConfigHasOdoo",
        "Set-DevBridgeSecretFileAcl", "Protect-DevBridgeConfigFiles")
    foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }
    $installSources = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "install.ps1") `
        -Names @("Get-DevBridgePostInstallArgs")
    foreach ($src in $installSources.Values) { . ([scriptblock]::Create($src)) }

    # "Spisska" with its real diacritics (s-caron U+0161, a-acute U+00E1).
    $spisska = "Spi" + [char]0x0161 + "sk" + [char]0x00E1
    $secret = "k3y-VALUE-never-printed-9d2f"

    function New-TempDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbodoo-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        return $dir
    }

    function Invoke-ChildScript {
        param([Parameter(Mandatory)][string]$ScriptPath, [Parameter(Mandatory)][string]$Arguments)
        $exe = (Get-Process -Id $PID).Path
        $outDir = New-TempDir
        $outFile = Join-Path $outDir "stdout.txt"
        $errFile = Join-Path $outDir "stderr.txt"
        try {
            $proc = Start-Process -FilePath $exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$ScriptPath`" $Arguments" `
                -Wait -PassThru -NoNewWindow -RedirectStandardOutput $outFile -RedirectStandardError $errFile
            return @{
                ExitCode = $proc.ExitCode
                StdOut   = [System.IO.File]::ReadAllText($outFile)
                StdErr   = [System.IO.File]::ReadAllText($errFile)
            }
        } finally {
            Remove-Item -Recurse -Force $outDir -ErrorAction SilentlyContinue
        }
    }

    $sampleConfig = "[general]`nmode = `"client`"`n`n[client]`nserver_address = `"10.88.1.100:50051`"`ntarget_printer = `"TSC ML241P`"`nprint_backend = `"windows_spooler_raw`"`n`n[jobs]`nmax_retries = 3`n"
}

Describe "ConvertTo-DevBridgeTomlString (ASCII-safe TOML basic string)" {
    It "quotes plain ASCII unchanged" {
        ConvertTo-DevBridgeTomlString -Value "https://erp.example.sk" | Should -BeExactly '"https://erp.example.sk"'
        ConvertTo-DevBridgeTomlString -Value "" | Should -BeExactly '""'
    }

    It "escapes quote and backslash" {
        ConvertTo-DevBridgeTomlString -Value 'a"b\c' | Should -BeExactly '"a\"b\\c"'
    }

    It "writes non-ASCII as uppercase \uXXXX escapes (TSC ML241P Spisska)" {
        ConvertTo-DevBridgeTomlString -Value "TSC ML241P $spisska" | Should -BeExactly '"TSC ML241P Spi\u0161sk\u00E1"'
    }

    It "escapes control characters and DEL" {
        ConvertTo-DevBridgeTomlString -Value ("a" + [char]9 + "b" + [char]0x7F) | Should -BeExactly '"a\u0009b\u007F"'
    }

    It "writes a surrogate pair as one \UXXXXXXXX code point and a lone surrogate as U+FFFD" {
        $grin = [char]::ConvertFromUtf32(0x1F600)
        ConvertTo-DevBridgeTomlString -Value "x$grin" | Should -BeExactly '"x\U0001F600"'
        ConvertTo-DevBridgeTomlString -Value ("y" + [char]0xD800) | Should -BeExactly '"y\uFFFD"'
    }

    It "always returns pure ASCII" {
        $out = ConvertTo-DevBridgeTomlString -Value ("$spisska " + [char]0x20AC)
        @([System.Text.Encoding]::UTF8.GetBytes($out) | Where-Object { $_ -gt 127 }).Count | Should -Be 0
    }
}

Describe "Get-DevBridgeOdooConfigProblems (validated before any change)" {
    It "accepts the Spisska rollout values" {
        $p = Get-DevBridgeOdooConfigProblems -Url "https://erp.slovnormal.sk" -ApiKey $secret `
            -PrinterName "TSC ML241P $spisska" -LabelWidthMm "72.7" -LabelHeightMm "110.1" -Dpi "203" `
            -PrintBackend "windows_spooler_raw"
        $p.Count | Should -Be 0
    }

    It "accepts plain http (the E2E fake Odoo) and omitted size/dpi" {
        $p = Get-DevBridgeOdooConfigProblems -Url "http://10.88.1.100:9230" -ApiKey $secret -PrinterName "P" -PrintBackend "windows_spooler_raw"
        $p.Count | Should -Be 0
    }

    It "reports every problem and never the key value" {
        $badKey = "bad key $secret"
        $p = Get-DevBridgeOdooConfigProblems -Url "erp.slovnormal.sk" -ApiKey $badKey -PrinterName " " `
            -LabelWidthMm "0" -LabelHeightMm "72,7" -Dpi "50" -PrintBackend "windows_spooler"
        $text = $p -join "|"
        $p.Count | Should -Be 7
        $text | Should -Match "DEVBRIDGE_ODOO_URL"
        $text | Should -Match "whitespace or control characters \(value not shown\)"
        $text | Should -Match "DEVBRIDGE_ODOO_PRINTER_NAME"
        $text | Should -Match "must be windows_spooler_raw \(is 'windows_spooler'\)"
        $text | Should -Match "DEVBRIDGE_ODOO_LABEL_WIDTH_MM '0'"
        $text | Should -Match "DEVBRIDGE_ODOO_LABEL_HEIGHT_MM '72,7'"
        $text | Should -Match "DEVBRIDGE_ODOO_DPI '50'"
        $text | Should -Not -Match ([regex]::Escape($secret))
    }

    It "rejects an upper-case scheme like the client does (case-sensitive)" {
        $p = Get-DevBridgeOdooConfigProblems -Url "HTTPS://erp.x.sk" -ApiKey "k" -PrinterName "P" -PrintBackend "windows_spooler_raw"
        ($p -join "|") | Should -Match "DEVBRIDGE_ODOO_URL 'HTTPS://erp.x.sk'"
    }

    It "reports a missing key" {
        $p = Get-DevBridgeOdooConfigProblems -Url "https://x.sk" -ApiKey "" -PrinterName "P" -PrintBackend "windows_spooler_raw"
        ($p -join "|") | Should -Match "DEVBRIDGE_ODOO_API_KEY is not set"
    }

    It "enforces the size and dpi bounds" {
        (Get-DevBridgeOdooConfigProblems -Url "https://x.sk" -ApiKey "k" -PrinterName "P" -PrintBackend "windows_spooler_raw" -LabelWidthMm "1000" -Dpi "1200").Count | Should -Be 0
        (Get-DevBridgeOdooConfigProblems -Url "https://x.sk" -ApiKey "k" -PrinterName "P" -PrintBackend "windows_spooler_raw" -LabelWidthMm "1000.5").Count | Should -Be 1
        (Get-DevBridgeOdooConfigProblems -Url "https://x.sk" -ApiKey "k" -PrinterName "P" -PrintBackend "windows_spooler_raw" -Dpi "1201").Count | Should -Be 1
        (Get-DevBridgeOdooConfigProblems -Url "https://x.sk" -ApiKey "k" -PrinterName "P" -PrintBackend "windows_spooler_raw" -Dpi "100").Count | Should -Be 0
    }
}

Describe "Get-DevBridgeOdooToml ([client.odoo] block)" {
    It "writes exactly the enabled block with escaped strings" {
        $toml = Get-DevBridgeOdooToml -Url "https://erp.slovnormal.sk" -ApiKey $secret -PrinterName "TSC ML241P $spisska"
        $toml | Should -BeExactly ("[client.odoo]`nenabled = true`nurl = `"https://erp.slovnormal.sk`"`napi_key = `"$secret`"`nprinter_name = `"TSC ML241P Spi\u0161sk\u00E1`"")
    }

    It "adds size/dpi only when given, always as TOML floats for mm" {
        $toml = Get-DevBridgeOdooToml -Url "https://x.sk" -ApiKey "k" -PrinterName "P" -LabelWidthMm "72" -LabelHeightMm "110.1" -Dpi "300"
        $toml | Should -Match "(?m)^label_width_mm = 72\.0$"
        $toml | Should -Match "(?m)^label_height_mm = 110\.1$"
        $toml | Should -Match "(?m)^dpi = 300$"
        Get-DevBridgeOdooToml -Url "https://x.sk" -ApiKey "k" -PrinterName "P" | Should -Not -Match "label_|dpi"
    }
}

Describe "Get-DevBridgeClientConfigValue (preserved config print_backend)" {
    It "reads a [client] string key and falls back to the default" {
        Get-DevBridgeClientConfigValue -Config $sampleConfig -Key "print_backend" -Default "windows_spooler" | Should -BeExactly "windows_spooler_raw"
        Get-DevBridgeClientConfigValue -Config $sampleConfig -Key "client_id" -Default "none" | Should -BeExactly "none"
        Get-DevBridgeClientConfigValue -Config "[general]`nmode = `"client`"`n" -Key "print_backend" -Default "windows_spooler" | Should -BeExactly "windows_spooler"
    }

    It "ignores the same key in another table" {
        $cfg = "[client]`ntarget_printer = `"P`"`n`n[client.odoo]`nprint_backend = `"windows_spooler_raw`"`n"
        Get-DevBridgeClientConfigValue -Config $cfg -Key "print_backend" -Default "windows_spooler" | Should -BeExactly "windows_spooler"
    }

    It "handles CRLF files" {
        Get-DevBridgeClientConfigValue -Config ($sampleConfig -replace "`n", "`r`n") -Key "print_backend" | Should -BeExactly "windows_spooler_raw"
    }
}

Describe "Merge-DevBridgeOdooIntoConfig (preserved config.toml)" {
    BeforeEach { $script:dir = New-TempDir; $script:cfg = Join-Path $script:dir "config.toml" }
    AfterEach { Remove-Item -Recurse -Force $script:dir -ErrorAction SilentlyContinue }

    It "adds the block before [jobs] and leaves everything else byte-identical" {
        [System.IO.File]::WriteAllText($script:cfg, $sampleConfig)
        $block = Get-DevBridgeOdooToml -Url "https://x.sk" -ApiKey "k1" -PrinterName "P"
        Merge-DevBridgeOdooIntoConfig -Path $script:cfg -Block $block | Should -BeExactly "added"
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $jobsAt = $sampleConfig.IndexOf("[jobs]")
        $out | Should -BeExactly ($sampleConfig.Substring(0, $jobsAt) + $block + "`n`n" + $sampleConfig.Substring($jobsAt))
    }

    It "replaces an existing block (key rotation) and keeps the tables after it" {
        [System.IO.File]::WriteAllText($script:cfg, $sampleConfig)
        Merge-DevBridgeOdooIntoConfig -Path $script:cfg -Block (Get-DevBridgeOdooToml -Url "https://x.sk" -ApiKey "old-key" -PrinterName "P") | Out-Null
        $new = Get-DevBridgeOdooToml -Url "https://y.sk" -ApiKey "new-key" -PrinterName "Q" -Dpi "300"
        Merge-DevBridgeOdooIntoConfig -Path $script:cfg -Block $new | Should -BeExactly "replaced"
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $out | Should -Not -Match "old-key"
        ([regex]::Matches($out, '(?m)^\[client\.odoo\]')).Count | Should -Be 1
        $jobsAt = $sampleConfig.IndexOf("[jobs]")
        $out | Should -BeExactly ($sampleConfig.Substring(0, $jobsAt) + $new + "`n`n" + $sampleConfig.Substring($jobsAt))
    }

    It "replaces a block that is the last table in the file" {
        [System.IO.File]::WriteAllText($script:cfg, "[client]`nx = `"1`"`n`n[client.odoo]`nenabled = true`napi_key = `"old`"`n")
        Merge-DevBridgeOdooIntoConfig -Path $script:cfg -Block "[client.odoo]`nenabled = false" | Should -BeExactly "replaced"
        [System.IO.File]::ReadAllText($script:cfg) | Should -BeExactly "[client]`nx = `"1`"`n`n[client.odoo]`nenabled = false`n"
    }

    It "replaces a header on the last line with no trailing newline (no duplicate table)" {
        [System.IO.File]::WriteAllText($script:cfg, "[client]`nx = `"1`"`n`n[client.odoo]")
        Merge-DevBridgeOdooIntoConfig -Path $script:cfg -Block "[client.odoo]`nenabled = true" | Should -BeExactly "replaced"
        $out = [System.IO.File]::ReadAllText($script:cfg)
        ([regex]::Matches($out, '(?m)^\[client\.odoo\]')).Count | Should -Be 1
        $out | Should -BeExactly "[client]`nx = `"1`"`n`n[client.odoo]`nenabled = true`n"
    }

    It "appends when the file has no [jobs] table" {
        [System.IO.File]::WriteAllText($script:cfg, "[client]`nx = `"1`"")
        Merge-DevBridgeOdooIntoConfig -Path $script:cfg -Block "[client.odoo]`nenabled = true" | Should -BeExactly "added"
        [System.IO.File]::ReadAllText($script:cfg) | Should -BeExactly "[client]`nx = `"1`"`n`n[client.odoo]`nenabled = true`n"
    }

    It "keeps CRLF line endings and writes no BOM" {
        [System.IO.File]::WriteAllText($script:cfg, ($sampleConfig -replace "`n", "`r`n"))
        Merge-DevBridgeOdooIntoConfig -Path $script:cfg -Block "[client.odoo]`nenabled = true" | Out-Null
        $bytes = [System.IO.File]::ReadAllBytes($script:cfg)
        $bytes[0] | Should -Be ([byte][char]'[')
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $out | Should -Match "\[client\.odoo\]`r`nenabled = true`r`n`r`n\[jobs\]"
        ($out -replace "`r`n", "") | Should -Not -Match "`n"
    }
}

Describe "Test-DevBridgeConfigHasOdoo" {
    It "detects the table and tolerates a missing file" {
        $dir = New-TempDir
        try {
            $cfg = Join-Path $dir "config.toml"
            Test-DevBridgeConfigHasOdoo -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, $sampleConfig)
            Test-DevBridgeConfigHasOdoo -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, $sampleConfig + "`n[client.odoo]`nenabled = true`n")
            Test-DevBridgeConfigHasOdoo -Path $cfg | Should -BeTrue
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "API-key file ACL (SYSTEM + Administrators only)" {
    BeforeEach { $script:dir = New-TempDir }
    AfterEach { Remove-Item -Recurse -Force $script:dir -ErrorAction SilentlyContinue }

    It "Set-DevBridgeSecretFileAcl leaves exactly SYSTEM + Administrators, no inheritance" {
        $f = Join-Path $script:dir "config.toml"
        [System.IO.File]::WriteAllText($f, "x")
        Set-DevBridgeSecretFileAcl -Path $f
        $acl = Get-Acl -LiteralPath $f
        $acl.AreAccessRulesProtected | Should -BeTrue
        $rules = @($acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier]))
        (@($rules | ForEach-Object { $_.IdentityReference.Value } | Sort-Object) -join ",") | Should -BeExactly "S-1-5-18,S-1-5-32-544"
        foreach ($r in $rules) {
            $r.AccessControlType | Should -Be ([System.Security.AccessControl.AccessControlType]::Allow)
            ($r.FileSystemRights -band [System.Security.AccessControl.FileSystemRights]::FullControl) |
                Should -Be ([System.Security.AccessControl.FileSystemRights]::FullControl)
        }
    }

    It "Protect-DevBridgeConfigFiles restricts config.toml and its snapshots only" {
        foreach ($n in @("config.toml", "config.toml.preupgrade-20260928-101010", "config.toml.replaced-20260928-101011", "devbridge.db", "notes.txt")) {
            [System.IO.File]::WriteAllText((Join-Path $script:dir $n), "x")
        }
        $done = Protect-DevBridgeConfigFiles -DataDir $script:dir
        $done.Count | Should -Be 3
        (@($done | ForEach-Object { Split-Path -Leaf $_ } | Sort-Object) -join ",") |
            Should -BeExactly "config.toml,config.toml.preupgrade-20260928-101010,config.toml.replaced-20260928-101011"
        (Get-Acl -LiteralPath (Join-Path $script:dir "config.toml.preupgrade-20260928-101010")).AreAccessRulesProtected | Should -BeTrue
        (Get-Acl -LiteralPath (Join-Path $script:dir "devbridge.db")).AreAccessRulesProtected | Should -BeFalse
    }
}

Describe "Get-DevBridgePostInstallArgs (install.ps1 DEVBRIDGE_ODOO_* mapping)" {
    It "forwards URL, printer name, size and dpi" {
        $envSnapshot = @{
            DEVBRIDGE_ODOO_URL             = "https://erp.slovnormal.sk"
            DEVBRIDGE_ODOO_PRINTER_NAME    = "TSC ML241P $spisska"
            DEVBRIDGE_ODOO_LABEL_WIDTH_MM  = "72.7"
            DEVBRIDGE_ODOO_LABEL_HEIGHT_MM = "110.1"
            DEVBRIDGE_ODOO_DPI             = "203"
        }
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env $envSnapshot
        $a[[array]::IndexOf($a, "-OdooUrl") + 1] | Should -BeExactly "https://erp.slovnormal.sk"
        $a[[array]::IndexOf($a, "-OdooPrinterName") + 1] | Should -BeExactly "TSC ML241P $spisska"
        $a[[array]::IndexOf($a, "-OdooLabelWidthMm") + 1] | Should -BeExactly "72.7"
        $a[[array]::IndexOf($a, "-OdooLabelHeightMm") + 1] | Should -BeExactly "110.1"
        $a[[array]::IndexOf($a, "-OdooDpi") + 1] | Should -BeExactly "203"
    }

    It "NEVER puts the API key on the post-install command line" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{ DEVBRIDGE_ODOO_URL = "https://x.sk"; DEVBRIDGE_ODOO_API_KEY = $secret }
        ($a -join " ") | Should -Not -Match ([regex]::Escape($secret))
        ($a -join " ") | Should -Not -Match "ApiKey"
    }

    It "adds no Odoo argument when no DEVBRIDGE_ODOO_* is set" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{ DEVBRIDGE_TARGET_PRINTER = "P" }
        ($a -join " ") | Should -Not -Match "-Odoo"
    }
}

Describe "post-install.ps1 Odoo validation (real child process, -ValidateOnly)" {
    BeforeEach {
        $script:workDir = New-TempDir
        $script:dataDir = New-TempDir
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        Copy-Item $libPath (Join-Path $script:workDir "DevBridgeInstallerLib.ps1")
        $env:DEVBRIDGE_ODOO_API_KEY = $secret
    }
    AfterEach {
        Remove-Item Env:\DEVBRIDGE_ODOO_API_KEY -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:workDir -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "refuses a non-RAW backend before any change and never prints the key" {
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -PrintBackend windows_spooler -OdooUrl https://erp.example.sk -OdooPrinterName P -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "must be windows_spooler_raw"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($secret))
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        @(Get-ChildItem -LiteralPath $script:dataDir -Force).Count | Should -Be 0
    }

    It "validates against the PRESERVED config's backend, reads nothing else, prints no key" {
        $cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($cfg, $sampleConfig)
        $hashBefore = (Get-FileHash -LiteralPath $cfg).Hash
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -OdooUrl https://erp.example.sk -OdooPrinterName `"TSC ML241P`" -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
        $r.StdOut | Should -Match "Odoo label source requested: https://erp.example.sk, printer 'TSC ML241P' \(API key set, not shown\)"
        $r.StdOut | Should -Match "VALIDATE-OK"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($secret))
        (Get-FileHash -LiteralPath $cfg).Hash | Should -BeExactly $hashBefore
    }

    It "a preserved PDF-backend config refuses the Odoo source" {
        $cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($cfg, ($sampleConfig -replace 'windows_spooler_raw', 'windows_spooler'))
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -OdooUrl https://erp.example.sk -OdooPrinterName P -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "is 'windows_spooler'"
    }
}
