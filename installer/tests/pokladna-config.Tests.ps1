# Pester v5 tests for the Odoo device-job source installer path (issue #99):
# DEVBRIDGE_POKLADNA_* env -> post-install args (install.ps1), the
# [client.pokladna] TOML builder / validation, the shared preserved-config
# table merge (DevBridgeInstallerLib.ps1), and post-install.ps1 itself as a
# real child process (validation before any change, key never printed).
#
# Tests exercise the REAL functions, AST-extracted from the production scripts
# with the shared deploy/lib/Get-FunctionSourceFromScript.ps1. This file is
# pure ASCII (Windows PowerShell 5.1 runs it too). Everything runs in
# throwaway temp dirs.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    $libPath = Join-Path $installerDir "DevBridgeInstallerLib.ps1"
    $postInstallPath = Join-Path $installerDir "post-install.ps1"
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    $sources = Get-FunctionSourceFromScript -ScriptPath $libPath -Names @(
        "ConvertTo-DevBridgeTomlString", "Get-DevBridgePokladnaConfigProblems", "Get-DevBridgePokladnaToml",
        "Merge-DevBridgeTomlTableIntoConfig", "Test-DevBridgeConfigHasPokladna", "Get-DevBridgeOdooToml")
    foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }
    $installSources = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "install.ps1") `
        -Names @("Get-DevBridgePostInstallArgs")
    foreach ($src in $installSources.Values) { . ([scriptblock]::Create($src)) }

    $secret = "pokladna-bridge-k3y-never-printed-5e1a"

    function New-TempDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbpokl-" + [guid]::NewGuid().ToString("N"))
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

    # A preserved store config: a PDF backend is fine, device jobs never print.
    $sampleConfig = "[general]`nmode = `"client`"`n`n[client]`nserver_address = `"10.88.1.100:50051`"`ntarget_printer = `"HP LaserJet`"`nprint_backend = `"windows_spooler`"`n`n[jobs]`nmax_retries = 3`n"
}

Describe "Get-DevBridgePokladnaConfigProblems (validated before any change)" {
    It "accepts a store rollout with and without a wait" {
        foreach ($wait in @("", "0", "25", "55", "07")) {
            $p = Get-DevBridgePokladnaConfigProblems -Url "https://erp.slovnormal.sk" -ApiKey $secret -WaitS $wait
            $p.Count | Should -Be 0 -Because "wait '$wait'"
        }
    }

    It "accepts plain http (the E2E fake Odoo)" {
        (Get-DevBridgePokladnaConfigProblems -Url "http://10.88.1.100:9231" -ApiKey $secret).Count | Should -Be 0
    }

    It "reports every problem and never the key value" {
        $badKey = "bad key $secret"
        $p = Get-DevBridgePokladnaConfigProblems -Url "erp.slovnormal.sk" -ApiKey $badKey -WaitS "56"
        $text = $p -join "|"
        $p.Count | Should -Be 3
        $text | Should -Match "DEVBRIDGE_POKLADNA_URL 'erp.slovnormal.sk'"
        $text | Should -Match "whitespace or control characters \(value not shown\)"
        $text | Should -Match "DEVBRIDGE_POKLADNA_WAIT_S '56' must be a whole number of seconds between 0 and 55"
        $text | Should -Not -Match ([regex]::Escape($secret))
    }

    It "reports a missing key and an upper-case scheme" {
        $p = Get-DevBridgePokladnaConfigProblems -Url "HTTPS://erp.x.sk" -ApiKey " "
        $text = $p -join "|"
        $p.Count | Should -Be 2
        $text | Should -Match "DEVBRIDGE_POKLADNA_API_KEY is not set"
        $text | Should -Match "DEVBRIDGE_POKLADNA_URL 'HTTPS://erp.x.sk'"
    }

    It "refuses a wait that is not a whole number 0..55" {
        foreach ($wait in @("-1", "1.5", "abc", "100", "5`n", " 5", ([string][char]0x0665))) {
            (Get-DevBridgePokladnaConfigProblems -Url "https://x.sk" -ApiKey "k" -WaitS $wait).Count | Should -Be 1 -Because "wait '$wait'"
        }
    }
}

Describe "Get-DevBridgePokladnaToml ([client.pokladna] block)" {
    It "writes exactly the enabled block with escaped strings" {
        $toml = Get-DevBridgePokladnaToml -Url "https://erp.slovnormal.sk" -ApiKey 'k"e\y'
        $toml | Should -BeExactly ("[client.pokladna]`nenabled = true`nurl = `"https://erp.slovnormal.sk`"`napi_key = `"k\`"e\\y`"")
    }

    It "adds wait_s only when given, as a plain TOML integer" {
        Get-DevBridgePokladnaToml -Url "https://x.sk" -ApiKey "k" | Should -Not -Match "wait_s"
        Get-DevBridgePokladnaToml -Url "https://x.sk" -ApiKey "k" -WaitS "07" | Should -Match "(?m)^wait_s = 7$"
        Get-DevBridgePokladnaToml -Url "https://x.sk" -ApiKey "k" -WaitS "0" | Should -Match "(?m)^wait_s = 0$"
        Get-DevBridgePokladnaToml -Url "https://x.sk" -ApiKey "k" -WaitS "55" | Should -Match "(?m)^wait_s = 55$"
    }
}

Describe "Merge-DevBridgeTomlTableIntoConfig with [client.pokladna] (preserved config.toml)" {
    BeforeEach { $script:dir = New-TempDir; $script:cfg = Join-Path $script:dir "config.toml" }
    AfterEach { Remove-Item -Recurse -Force $script:dir -ErrorAction SilentlyContinue }

    It "adds the block before [jobs] and leaves everything else byte-identical" {
        [System.IO.File]::WriteAllText($script:cfg, $sampleConfig)
        $block = Get-DevBridgePokladnaToml -Url "https://x.sk" -ApiKey "k1" -WaitS "25"
        Merge-DevBridgeTomlTableIntoConfig -Path $script:cfg -Table "client.pokladna" -Block $block | Should -BeExactly "added"
        $jobsAt = $sampleConfig.IndexOf("[jobs]")
        [System.IO.File]::ReadAllText($script:cfg) | Should -BeExactly ($sampleConfig.Substring(0, $jobsAt) + $block + "`n`n" + $sampleConfig.Substring($jobsAt))
    }

    It "replaces the block on key rotation and keeps the kasy device mapping after it" {
        $kasy = "[[client.pokladna.kasy]]`nid = 3`nportos_url = `"http://127.0.0.1:3010`"`n`n"
        $old = Get-DevBridgePokladnaToml -Url "https://x.sk" -ApiKey "old-key"
        $start = $sampleConfig.Replace("[jobs]", "$old`n`n$kasy[jobs]")
        [System.IO.File]::WriteAllText($script:cfg, $start)
        $new = Get-DevBridgePokladnaToml -Url "https://y.sk" -ApiKey "new-key" -WaitS "10"
        Merge-DevBridgeTomlTableIntoConfig -Path $script:cfg -Table "client.pokladna" -Block $new | Should -BeExactly "replaced"
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $out | Should -Not -Match "old-key"
        ([regex]::Matches($out, '(?m)^\[client\.pokladna\]')).Count | Should -Be 1
        $out | Should -BeExactly $sampleConfig.Replace("[jobs]", "$new`n`n$kasy[jobs]")
    }

    It "never touches a [client.odoo] block next to it" {
        $odoo = Get-DevBridgeOdooToml -Url "https://x.sk" -ApiKey "label-key" -PrinterName "P"
        $start = $sampleConfig.Replace("[jobs]", "$odoo`n`n[jobs]")
        [System.IO.File]::WriteAllText($script:cfg, $start)
        $block = Get-DevBridgePokladnaToml -Url "https://x.sk" -ApiKey "bridge-key"
        Merge-DevBridgeTomlTableIntoConfig -Path $script:cfg -Table "client.pokladna" -Block $block | Should -BeExactly "added"
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $out | Should -BeExactly $sampleConfig.Replace("[jobs]", "$odoo`n`n$block`n`n[jobs]")
    }

    It "keeps CRLF line endings and writes no BOM" {
        [System.IO.File]::WriteAllText($script:cfg, ($sampleConfig -replace "`n", "`r`n"))
        Merge-DevBridgeTomlTableIntoConfig -Path $script:cfg -Table "client.pokladna" -Block "[client.pokladna]`nenabled = true" | Out-Null
        $bytes = [System.IO.File]::ReadAllBytes($script:cfg)
        $bytes[0] | Should -Be ([byte][char]'[')
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $out | Should -Match "\[client\.pokladna\]`r`nenabled = true`r`n`r`n\[jobs\]"
        ($out -replace "`r`n", "") | Should -Not -Match "`n"
    }
}

Describe "Test-DevBridgeConfigHasPokladna" {
    It "detects the table only, tolerates a missing file" {
        $dir = New-TempDir
        try {
            $cfg = Join-Path $dir "config.toml"
            Test-DevBridgeConfigHasPokladna -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, $sampleConfig)
            Test-DevBridgeConfigHasPokladna -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, $sampleConfig + "`n[[client.pokladna.kasy]]`nid = 3`n")
            Test-DevBridgeConfigHasPokladna -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, $sampleConfig + "`n[client.pokladna]`nenabled = true`n")
            Test-DevBridgeConfigHasPokladna -Path $cfg | Should -BeTrue
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Get-DevBridgePostInstallArgs (install.ps1 DEVBRIDGE_POKLADNA_* mapping)" {
    It "forwards the URL and the wait as one -Name:value argument" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{
            DEVBRIDGE_POKLADNA_URL    = "https://erp.slovnormal.sk"
            DEVBRIDGE_POKLADNA_WAIT_S = "25"
        }
        $a[[array]::IndexOf($a, "-PokladnaUrl") + 1] | Should -BeExactly "https://erp.slovnormal.sk"
        (@($a) -ccontains "-PokladnaWaitS:25") | Should -BeTrue
    }

    It "NEVER puts the bridge key on the post-install command line" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{ DEVBRIDGE_POKLADNA_URL = "https://x.sk"; DEVBRIDGE_POKLADNA_API_KEY = $secret }
        ($a -join " ") | Should -Not -Match ([regex]::Escape($secret))
        ($a -join " ") | Should -Not -Match "ApiKey"
    }

    It "adds no pokladna argument when no DEVBRIDGE_POKLADNA_* is set" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{ DEVBRIDGE_TARGET_PRINTER = "P" }
        ($a -join " ") | Should -Not -Match "Pokladna"
    }
}

Describe "post-install.ps1 pokladna validation (real child process, -ValidateOnly)" {
    BeforeEach {
        $script:workDir = New-TempDir
        $script:dataDir = New-TempDir
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        Copy-Item $libPath (Join-Path $script:workDir "DevBridgeInstallerLib.ps1")
        Copy-Item (Join-Path $installerDir "DevBridgeLanApiLib.ps1") (Join-Path $script:workDir "DevBridgeLanApiLib.ps1")
        $env:DEVBRIDGE_POKLADNA_API_KEY = $secret
    }
    AfterEach {
        Remove-Item Env:\DEVBRIDGE_POKLADNA_API_KEY -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:workDir -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "refuses a bad wait before any change and never prints the key" {
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -PokladnaUrl https://erp.example.sk -PokladnaWaitS:60 -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "DEVBRIDGE_POKLADNA_WAIT_S '60'"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($secret))
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        @(Get-ChildItem -LiteralPath $script:dataDir -Force).Count | Should -Be 0
    }

    It "accepts it on a preserved config with any print backend, reads nothing else, prints no key" {
        $cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($cfg, $sampleConfig)
        $hashBefore = (Get-FileHash -LiteralPath $cfg).Hash
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -PokladnaUrl https://erp.example.sk -PokladnaWaitS:25 -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
        $r.StdOut | Should -Match "Odoo device-job source \(pokladna\) requested: https://erp.example.sk, wait_s='25'"
        $r.StdOut | Should -Match "VALIDATE-OK"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($secret))
        (Get-FileHash -LiteralPath $cfg).Hash | Should -BeExactly $hashBefore
    }

    It "refuses a missing key when only the URL is given" {
        Remove-Item Env:\DEVBRIDGE_POKLADNA_API_KEY -ErrorAction SilentlyContinue
        $cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($cfg, $sampleConfig)
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -PokladnaUrl https://erp.example.sk -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "DEVBRIDGE_POKLADNA_API_KEY is not set"
    }

    It "warns that a wait without URL and key is ignored" {
        Remove-Item Env:\DEVBRIDGE_POKLADNA_API_KEY -ErrorAction SilentlyContinue
        $cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($cfg, $sampleConfig)
        $hashBefore = (Get-FileHash -LiteralPath $cfg).Hash
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -PokladnaWaitS:10 -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
        # two short matches: the host may word-wrap a long WARNING line
        ($r.StdOut + $r.StdErr) | Should -Match "POKLADNA_WAIT_S"
        ($r.StdOut + $r.StdErr) | Should -Match "IGNORED"
        $r.StdOut | Should -Not -Match "device-job source \(pokladna\) requested"
        (Get-FileHash -LiteralPath $cfg).Hash | Should -BeExactly $hashBefore
    }
}
