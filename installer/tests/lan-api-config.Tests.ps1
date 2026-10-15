# Pester v5 tests for the LAN API installer path of the tills (issue #101):
# DEVBRIDGE_POKLADNA_KASY (the tills + their LAN keys, JSON) and
# DEVBRIDGE_LAN_API_* -> validation, the [[client.pokladna.kasy]] /
# [client.pokladna.lan_api] TOML builders, the preserved-config merge, the
# firewall rule, the install.ps1 argument mapping, and post-install.ps1 itself
# as a real child process (validation before any change, keys never printed).
#
# Tests exercise the REAL functions, AST-extracted from the production scripts
# with the shared deploy/lib/Get-FunctionSourceFromScript.ps1. This file is
# pure ASCII (Windows PowerShell 5.1 runs it too); non-ASCII test text is built
# from [char] codes. Everything runs in throwaway temp dirs.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    $libPath = Join-Path $installerDir "DevBridgeInstallerLib.ps1"
    $postInstallPath = Join-Path $installerDir "post-install.ps1"
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    $sources = Get-FunctionSourceFromScript -ScriptPath $libPath -Names @(
        "ConvertTo-DevBridgeTomlString", "ConvertFrom-DevBridgePokladnaKasySpec", "Get-DevBridgePokladnaKasyProblems",
        "Get-DevBridgePokladnaKasyToml", "Merge-DevBridgePokladnaKasyIntoConfig", "Merge-DevBridgeTomlTableIntoConfig",
        "Test-DevBridgeDnsName", "Get-DevBridgeLanApiConfigProblems", "Get-DevBridgeLanApiToml",
        "Test-DevBridgeConfigHasLanKey", "Set-DevBridgeLanApiFirewallRule", "Get-DevBridgePokladnaToml")
    foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }
    $installSources = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "install.ps1") `
        -Names @("Get-DevBridgePostInstallArgs")
    foreach ($src in $installSources.Values) { . ([scriptblock]::Create($src)) }

    $key3 = "till-3-lan-key-never-printed-0123456789"
    $key4 = "till-4-lan-key-never-printed-0123456789"
    $bridgeKey = "pokladna-bridge-key-never-printed-01234"
    $goodSpec = '[{"id": 3, "portos_url": "http://localhost:3010", "lan_key": "' + $key3 + '"}, {"id": 4, "lan_key": "' + $key4 + '"}]'

    function New-TempDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dblan-" + [guid]::NewGuid().ToString("N"))
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

    # The JSON of one till entry with extra / replaced fields.
    function New-KasySpec {
        param([Parameter(Mandatory)][string[]]$Entries)
        return "[" + ($Entries -join ", ") + "]"
    }

    $sampleConfig = "[general]`nmode = `"client`"`n`n[client]`nserver_address = `"10.88.1.100:50051`"`n`n[client.pokladna]`nenabled = true`n`n[jobs]`nmax_retries = 3`n"
}

Describe "Get-DevBridgePokladnaKasyProblems (DEVBRIDGE_POKLADNA_KASY, validated before any change)" {
    It "accepts the tills with their keys" {
        $p = Get-DevBridgePokladnaKasyProblems -Spec $goodSpec -ApiKey $bridgeKey
        $p.Count | Should -Be 0 -Because ($p -join "; ")
        $one = Get-DevBridgePokladnaKasyProblems -Spec '[{"id": 7}]'
        $one.Count | Should -Be 0 -Because "a till without a LAN key or Portos URL is fine"
    }

    It "names every problem and never the key values" {
        $short = "short-key"
        $spec = New-KasySpec @(
            '{"id": 0, "lan_key": "' + $short + '"}',
            '{"id": "4", "lan_key": "' + $bridgeKey + '"}',
            '{"id": 5, "lan_key": "' + $key3 + '", "terminal_ip": "10.0.0.5"}',
            '{"id": 5, "lan_key": "' + $key3 + '", "portos_url": "localhost:3010"}',
            '{"id": 3.5}'
        )
        $p = Get-DevBridgePokladnaKasyProblems -Spec $spec -ApiKey $bridgeKey
        $text = $p -join "; "
        $text | Should -Match "entry 1: id must be the till's positive pos.config id"
        $text | Should -Match "entry 1: lan_key must be 32-256 printable characters"
        $text | Should -Match "entry 2: id must be the till's positive pos.config id"
        $text | Should -Match "entry 2: lan_key must not be the Odoo bridge key"
        $text | Should -Match "entry 3: unknown key 'terminal_ip' \(supported: id, portos_url, lan_key\)"
        $text | Should -Match "entry 4: id 5 is listed twice"
        $text | Should -Match "entry 4: portos_url must be an http\(s\)://"
        $text | Should -Match "entry 4: lan_key is the same as another till's"
        $text | Should -Match "entry 5: id must be"
        $text | Should -Not -Match ([regex]::Escape($short))
        $text | Should -Not -Match ([regex]::Escape($bridgeKey))
        $text | Should -Not -Match ([regex]::Escape($key3))
    }

    It "refuses keys of the wrong length or characters" {
        $base = "a" * 31
        foreach ($bad in @(" ", '\"', '\\', ("a" * 226))) {
            $spec = '[{"id": 3, "lan_key": "' + $base + $bad + '"}]'
            $p = Get-DevBridgePokladnaKasyProblems -Spec $spec
            ($p -join "; ") | Should -Match "lan_key must be 32-256" -Because "suffix '$bad'"
        }
        $nonAscii = '[{"id": 3, "lan_key": "' + $base + [char]0x0161 + '"}]'
        ((Get-DevBridgePokladnaKasyProblems -Spec $nonAscii) -join "; ") | Should -Match "lan_key must be 32-256"
        foreach ($ok in @("!", "~", "Z", "-")) {
            $p = Get-DevBridgePokladnaKasyProblems -Spec ('[{"id": 3, "lan_key": "' + $base + $ok + '"}]')
            $p.Count | Should -Be 0 -Because "suffix '$ok'"
        }
        $p = Get-DevBridgePokladnaKasyProblems -Spec ('[{"id": 3, "lan_key": "' + ("a" * 256) + '"}]')
        $p.Count | Should -Be 0 -Because "256 characters is the maximum"
    }

    It "refuses a spec that is not a JSON array of tills" {
        foreach ($case in @(
                @{ Spec = '{"id": 3}'; Want = "must be a JSON array" },
                @{ Spec = "not json"; Want = "must be a JSON array" },
                @{ Spec = '[{"id": 3'; Want = "not valid JSON" },
                @{ Spec = '[1, 2]'; Want = "every till must be a JSON object" },
                @{ Spec = '[]'; Want = "lists no till" }
            )) {
            $p = Get-DevBridgePokladnaKasyProblems -Spec $case.Spec
            ($p -join "; ") | Should -Match $case.Want -Because $case.Spec
        }
    }
}

Describe "Get-DevBridgePokladnaKasyToml ([[client.pokladna.kasy]] blocks)" {
    It "writes one block per till, strings TOML-escaped" {
        $toml = Get-DevBridgePokladnaKasyToml -Spec $goodSpec
        $toml | Should -BeExactly ("[[client.pokladna.kasy]]`nid = 3`nportos_url = `"http://localhost:3010`"`nlan_key = `"$key3`"`n`n" +
            "[[client.pokladna.kasy]]`nid = 4`nlan_key = `"$key4`"")
        Get-DevBridgePokladnaKasyToml -Spec '[{"id": 9}]' | Should -BeExactly "[[client.pokladna.kasy]]`nid = 9"
    }
}

Describe "Merge-DevBridgePokladnaKasyIntoConfig (preserved config.toml)" {
    BeforeEach { $script:dir = New-TempDir; $script:cfg = Join-Path $script:dir "config.toml" }
    AfterEach { Remove-Item -Recurse -Force $script:dir -ErrorAction SilentlyContinue }

    It "adds the blocks before [jobs] and leaves everything else byte-identical" {
        [System.IO.File]::WriteAllText($script:cfg, $sampleConfig)
        $block = Get-DevBridgePokladnaKasyToml -Spec $goodSpec
        Merge-DevBridgePokladnaKasyIntoConfig -Path $script:cfg -Block $block | Should -BeExactly "added"
        $jobsAt = $sampleConfig.IndexOf("[jobs]")
        [System.IO.File]::ReadAllText($script:cfg) |
            Should -BeExactly ($sampleConfig.Substring(0, $jobsAt) + $block + "`n`n" + $sampleConfig.Substring($jobsAt))
    }

    It "replaces EVERY existing till block and keeps the tables around them" {
        $lan = Get-DevBridgeLanApiToml -Enabled "true" -Port "9443" -Hostname "pjsnvs.pos.example.sk"
        $old = "[[client.pokladna.kasy]]`nid = 1`nlan_key = `"old-key-1`"`n`n[[client.pokladna.kasy]]`nid = 2`n`n"
        $start = $sampleConfig.Replace("[jobs]", "$old$lan`n`n[jobs]")
        [System.IO.File]::WriteAllText($script:cfg, $start)
        $new = Get-DevBridgePokladnaKasyToml -Spec $goodSpec
        Merge-DevBridgePokladnaKasyIntoConfig -Path $script:cfg -Block $new | Should -BeExactly "replaced"
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $out | Should -Not -Match "old-key-1"
        $out | Should -Not -Match "(?m)^id = [12]$"
        ([regex]::Matches($out, '(?m)^\[\[client\.pokladna\.kasy\]\]')).Count | Should -Be 2
        $out | Should -BeExactly $sampleConfig.Replace("[jobs]", "$lan`n`n$new`n`n[jobs]")
    }

    It "appends when there is no [jobs] table, keeps CRLF and writes no BOM" {
        [System.IO.File]::WriteAllText($script:cfg, "[client]`r`nserver_address = `"x:1`"`r`n")
        Merge-DevBridgePokladnaKasyIntoConfig -Path $script:cfg -Block "[[client.pokladna.kasy]]`nid = 3" | Should -BeExactly "added"
        $bytes = [System.IO.File]::ReadAllBytes($script:cfg)
        $bytes[0] | Should -Be ([byte][char]'[')
        $out = [System.IO.File]::ReadAllText($script:cfg)
        $out | Should -BeExactly "[client]`r`nserver_address = `"x:1`"`r`n`r`n[[client.pokladna.kasy]]`r`nid = 3`r`n"
    }
}

Describe "Test-DevBridgeDnsName (the LAN API hostname rule of the service)" {
    It "accepts lower-case names of two or more labels" {
        foreach ($ok in @("pjsnvs.pos.example.sk", "a.b", "e2e-raw-client.lan-api.test", "x1.y2z")) {
            Test-DevBridgeDnsName -Name $ok | Should -BeTrue -Because $ok
        }
        Test-DevBridgeDnsName -Name (("a" * 63) + ".b") | Should -BeTrue
    }

    It "refuses everything else" {
        foreach ($bad in @("", "pjsnvs", "Pjsnvs.pos.sk", "a..b", ".a.b", "a.b.", "-a.b", "a-.b", "*.pos.sk",
                "10.78.2.10", "a.42", "a_b.sk", "a b.sk", (("a" * 64) + ".b"), (("a." * 126) + "bcd"))) {
            Test-DevBridgeDnsName -Name $bad | Should -BeFalse -Because "'$bad'"
        }
    }
}

Describe "Get-DevBridgeLanApiConfigProblems and Get-DevBridgeLanApiToml ([client.pokladna.lan_api])" {
    It "accepts a complete request and builds the block" {
        $p = Get-DevBridgeLanApiConfigProblems -Enabled "TRUE" -Port "9443" -Hostname "pjsnvs.pos.example.sk"
        $p.Count | Should -Be 0 -Because ($p -join "; ")
        Get-DevBridgeLanApiToml -Enabled "TRUE" -Port "09443" -Hostname "pjsnvs.pos.example.sk" |
            Should -BeExactly "[client.pokladna.lan_api]`nenabled = true`nport = 9443`nhostname = `"pjsnvs.pos.example.sk`""
        Get-DevBridgeLanApiToml -Hostname "a.b" | Should -BeExactly "[client.pokladna.lan_api]`nenabled = true`nhostname = `"a.b`""
        Get-DevBridgeLanApiToml -Enabled "False" | Should -BeExactly "[client.pokladna.lan_api]`nenabled = false"
    }

    It "names every problem" {
        $text = (Get-DevBridgeLanApiConfigProblems -Enabled "yes" -Port "70000" -Hostname "PJSNVS") -join "; "
        $text | Should -Match "DEVBRIDGE_LAN_API_ENABLED 'yes' must be true or false"
        $text | Should -Match "DEVBRIDGE_LAN_API_PORT '70000' must be a port number between 1 and 65535"
        $text | Should -Match "DEVBRIDGE_LAN_API_HOSTNAME 'PJSNVS' must be the PC's lower-case DNS name"
        foreach ($port in @("0", "-1", "9x", "123456")) {
            ((Get-DevBridgeLanApiConfigProblems -Port $port -Hostname "a.b") -join "; ") |
                Should -Match "DEVBRIDGE_LAN_API_PORT" -Because "port '$port'"
        }
        ((Get-DevBridgeLanApiConfigProblems -Port "9120" -Hostname "a.b") -join "; ") | Should -Match "is the dashboard's port"
        ((Get-DevBridgeLanApiConfigProblems -Port "9222" -Hostname "a.b" -DashboardPort 9222) -join "; ") |
            Should -Match "is the dashboard's port"
        (Get-DevBridgeLanApiConfigProblems -Port "65535" -Hostname "a.b").Count | Should -Be 0
        (Get-DevBridgeLanApiConfigProblems -Port "1" -Hostname "a.b").Count | Should -Be 0
    }

    It "needs no hostname to switch it off" {
        (Get-DevBridgeLanApiConfigProblems -Enabled "false").Count | Should -Be 0
        ((Get-DevBridgeLanApiConfigProblems -Port "9443") -join "; ") | Should -Match "DEVBRIDGE_LAN_API_HOSTNAME"
    }
}

Describe "Test-DevBridgeConfigHasLanKey" {
    It "detects a till's key only, tolerates a missing file" {
        $dir = New-TempDir
        try {
            $cfg = Join-Path $dir "config.toml"
            Test-DevBridgeConfigHasLanKey -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, $sampleConfig)
            Test-DevBridgeConfigHasLanKey -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, $sampleConfig + "`n[[client.pokladna.kasy]]`nid = 3`n  lan_key = `"k`"`n")
            Test-DevBridgeConfigHasLanKey -Path $cfg | Should -BeTrue
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Set-DevBridgeLanApiFirewallRule (Private / Domain only)" {
    It "creates the rule when there is none" {
        Mock Get-NetFirewallRule { $null }
        Mock New-NetFirewallRule { [pscustomobject]@{ DisplayName = "x" } }
        Mock Set-NetFirewallRule {}
        Set-DevBridgeLanApiFirewallRule -Port 9443 | Should -BeExactly "created"
        Should -Invoke New-NetFirewallRule -Times 1 -Exactly -ParameterFilter {
            $DisplayName -eq "DevBridge-LanApi" -and "$LocalPort" -eq "9443" -and
            "$($PesterBoundParameters['Profile'])" -match "Domain" -and "$($PesterBoundParameters['Profile'])" -match "Private" -and
            "$($PesterBoundParameters['Profile'])" -notmatch "Public"
        }
        Should -Invoke Set-NetFirewallRule -Times 0 -Exactly
    }

    It "moves an existing rule to the new port" {
        Mock Get-NetFirewallRule { [pscustomobject]@{ DisplayName = "DevBridge-E2E-LanApi" } }
        Mock New-NetFirewallRule {}
        Mock Set-NetFirewallRule {}
        Set-DevBridgeLanApiFirewallRule -Port 9243 -DisplayName "DevBridge-E2E-LanApi" | Should -BeExactly "updated"
        Should -Invoke Set-NetFirewallRule -Times 1 -Exactly -ParameterFilter {
            $DisplayName -eq "DevBridge-E2E-LanApi" -and "$LocalPort" -eq "9243"
        }
        Should -Invoke New-NetFirewallRule -Times 0 -Exactly
    }
}

Describe "Get-DevBridgePostInstallArgs (install.ps1 DEVBRIDGE_LAN_API_* mapping)" {
    It "forwards the LAN API settings, each value bound unambiguously" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{
            DEVBRIDGE_LAN_API_ENABLED  = "true"
            DEVBRIDGE_LAN_API_PORT     = "9443"
            DEVBRIDGE_LAN_API_HOSTNAME = "pjsnvs.pos.example.sk"
        }
        (@($a) -ccontains "-LanApiEnabled:true") | Should -BeTrue
        (@($a) -ccontains "-LanApiPort:9443") | Should -BeTrue
        $a[[array]::IndexOf($a, "-LanApiHostname") + 1] | Should -BeExactly "pjsnvs.pos.example.sk"
    }

    It "NEVER puts the tills' LAN keys on the post-install command line" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{ DEVBRIDGE_POKLADNA_KASY = $goodSpec; DEVBRIDGE_LAN_API_PORT = "9443" }
        ($a -join " ") | Should -Not -Match ([regex]::Escape($key3))
        ($a -join " ") | Should -Not -Match "Kasy"
    }

    It "adds no LAN argument when nothing is set" {
        $a = Get-DevBridgePostInstallArgs -Mode "client" -Env @{ DEVBRIDGE_TARGET_PRINTER = "P" }
        ($a -join " ") | Should -Not -Match "LanApi"
    }
}

Describe "post-install.ps1 LAN API validation (real child process, -ValidateOnly)" {
    BeforeEach {
        $script:workDir = New-TempDir
        $script:dataDir = New-TempDir
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        Copy-Item $libPath (Join-Path $script:workDir "DevBridgeInstallerLib.ps1")
        $script:cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($script:cfg, $sampleConfig)
    }
    AfterEach {
        Remove-Item Env:\DEVBRIDGE_POKLADNA_KASY -ErrorAction SilentlyContinue
        Remove-Item Env:\DEVBRIDGE_POKLADNA_API_KEY -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:workDir -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "refuses a bad till list before any change and never prints a key" {
        $env:DEVBRIDGE_POKLADNA_KASY = '[{"id": 3, "lan_key": "' + $key3 + '"}, {"id": 4, "lan_key": "' + $key3 + '"}]'
        $hashBefore = (Get-FileHash -LiteralPath $script:cfg).Hash
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "lan_key is the same as another till's"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($key3))
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        (Get-FileHash -LiteralPath $script:cfg).Hash | Should -BeExactly $hashBefore
    }

    It "refuses a till key equal to the Odoo bridge key" {
        $env:DEVBRIDGE_POKLADNA_API_KEY = $bridgeKey
        $env:DEVBRIDGE_POKLADNA_KASY = '[{"id": 3, "lan_key": "' + $bridgeKey + '"}]'
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -PokladnaUrl https://erp.example.sk -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "must not be the Odoo bridge key"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($bridgeKey))
    }

    It "refuses a bad LAN API hostname" {
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -LanApiEnabled:true -LanApiPort:9443 -LanApiHostname 10.78.2.10 -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "DEVBRIDGE_LAN_API_HOSTNAME '10.78.2.10'"
    }

    It "accepts a valid request, changes nothing and prints no key" {
        $env:DEVBRIDGE_POKLADNA_KASY = $goodSpec
        $hashBefore = (Get-FileHash -LiteralPath $script:cfg).Hash
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -LanApiEnabled:true -LanApiPort:9443 -LanApiHostname pjsnvs.pos.example.sk -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
        $r.StdOut | Should -Match "Pokladna tills requested: 2 \(LAN keys set, not shown\)"
        $r.StdOut | Should -Match "LAN API requested: enabled='true' port='9443' hostname='pjsnvs.pos.example.sk'"
        $r.StdOut | Should -Match "VALIDATE-OK"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($key3))
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($key4))
        (Get-FileHash -LiteralPath $script:cfg).Hash | Should -BeExactly $hashBefore
    }
}
