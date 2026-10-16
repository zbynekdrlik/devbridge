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
    $lanLibPath = Join-Path $installerDir "DevBridgeLanApiLib.ps1"
    $postInstallPath = Join-Path $installerDir "post-install.ps1"
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    # the LAN API helpers (issue #101) and what they call from the main lib
    $sources = Get-FunctionSourceFromScript -ScriptPath $libPath -Names @(
        "ConvertTo-DevBridgeTomlString", "Merge-DevBridgeTomlTableIntoConfig", "Get-DevBridgeTomlTableText",
        "Get-DevBridgeTomlStringValue", "Get-DevBridgeTomlBoolValue", "Get-DevBridgePokladnaToml")
    foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }
    $lanSources = Get-FunctionSourceFromScript -ScriptPath $lanLibPath -Names @(
        "ConvertFrom-DevBridgePokladnaKasySpec", "Get-DevBridgePokladnaKasyProblems",
        "Get-DevBridgePokladnaKasyToml", "Merge-DevBridgePokladnaKasyIntoConfig",
        "Test-DevBridgeDnsName", "Get-DevBridgeLanApiConfigProblems", "Get-DevBridgeLanApiToml",
        "Test-DevBridgeConfigHasLanKey", "Set-DevBridgeFirewallPortRule", "Set-DevBridgeLanApiFirewallRule",
        "Remove-DevBridgeLanApiFirewallRule", "Get-DevBridgeLanApiEffectiveProblems", "Get-DevBridgeAcmeConfigProblems",
        "Get-DevBridgeAcmeToml", "Test-DevBridgeConfigHasAcmeToken", "Get-DevBridgeLanApiRequest",
        "Get-DevBridgeAcmeRequest", "Get-DevBridgePublicNetworkWarnings")
    foreach ($src in $lanSources.Values) { . ([scriptblock]::Create($src)) }
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

    $sampleConfig = "[general]`nmode = `"client`"`n`n[client]`nserver_address = `"10.88.1.100:50051`"`nclient_id = `"pjsnvs`"`n`n[client.pokladna]`nenabled = true`n`n[jobs]`nmax_retries = 3`n"
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
        # Every entry in parentheses: PowerShell's "," binds tighter than "+".
        $spec = New-KasySpec @(
            ('{"id": 0, "lan_key": "' + $short + '"}'),
            ('{"id": "4", "lan_key": "' + $bridgeKey + '"}'),
            ('{"id": 5, "lan_key": "' + $key3 + '", "terminal_ip": "10.0.0.5"}'),
            ('{"id": 5, "lan_key": "' + $key3 + '", "portos_url": "localhost:3010"}'),
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
                @{ Spec = '[{"id": 3}, {"id": 4'; Want = "not valid JSON" },
                @{ Spec = '[{"id": 3}] trailing'; Want = "not valid JSON" },
                @{ Spec = '[{"id": 3},]'; Want = "not valid JSON" },
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

Describe "Set-DevBridgeLanApiFirewallRule (allowed on Private / Domain, blocked on Public)" {
    It "creates the allow rule and the Public block rule when there are none" {
        Mock Get-NetFirewallRule { $null }
        Mock New-NetFirewallRule { [pscustomobject]@{ DisplayName = "x" } }
        Mock Set-NetFirewallRule {}
        Set-DevBridgeLanApiFirewallRule -Port 9443 | Should -BeExactly "created"
        Should -Invoke New-NetFirewallRule -Times 1 -Exactly -ParameterFilter {
            $DisplayName -eq "DevBridge-LanApi" -and "$LocalPort" -eq "9443" -and $Action -eq "Allow" -and
            "$($PesterBoundParameters['Profile'])" -match "Domain" -and "$($PesterBoundParameters['Profile'])" -match "Private" -and
            "$($PesterBoundParameters['Profile'])" -notmatch "Public"
        }
        # the service binary's own allow rule covers every profile: block the port on Public
        Should -Invoke New-NetFirewallRule -Times 1 -Exactly -ParameterFilter {
            $DisplayName -eq "DevBridge-LanApi-Public" -and "$LocalPort" -eq "9443" -and $Action -eq "Block" -and
            "$($PesterBoundParameters['Profile'])" -eq "Public"
        }
        Should -Invoke New-NetFirewallRule -Times 2 -Exactly
        Should -Invoke Set-NetFirewallRule -Times 0 -Exactly
    }

    It "moves existing rules to the new port" {
        Mock Get-NetFirewallRule { [pscustomobject]@{ DisplayName = "DevBridge-E2E-LanApi" } }
        Mock New-NetFirewallRule {}
        Mock Set-NetFirewallRule {}
        Set-DevBridgeLanApiFirewallRule -Port 9243 -DisplayName "DevBridge-E2E-LanApi" | Should -BeExactly "updated"
        Should -Invoke Set-NetFirewallRule -Times 1 -Exactly -ParameterFilter {
            $DisplayName -eq "DevBridge-E2E-LanApi" -and "$LocalPort" -eq "9243" -and $Action -eq "Allow"
        }
        Should -Invoke Set-NetFirewallRule -Times 1 -Exactly -ParameterFilter {
            $DisplayName -eq "DevBridge-E2E-LanApi-Public" -and "$LocalPort" -eq "9243" -and $Action -eq "Block" -and
            "$($PesterBoundParameters['Profile'])" -eq "Public"
        }
        Should -Invoke New-NetFirewallRule -Times 0 -Exactly
    }
}

Describe "Remove-DevBridgeLanApiFirewallRule (the LAN API switched off)" {
    It "removes both rules that exist" {
        Mock Get-NetFirewallRule { if ($DisplayName -eq "DevBridge-LanApi-Public") { $null } else { [pscustomobject]@{ DisplayName = $DisplayName } } }
        Mock Remove-NetFirewallRule {}
        Remove-DevBridgeLanApiFirewallRule | Should -Be 1
        Should -Invoke Remove-NetFirewallRule -Times 1 -Exactly -ParameterFilter { $DisplayName -eq "DevBridge-LanApi" }
        Mock Get-NetFirewallRule { [pscustomobject]@{ DisplayName = $DisplayName } }
        Remove-DevBridgeLanApiFirewallRule -DisplayName "X" | Should -Be 2
        Should -Invoke Remove-NetFirewallRule -Times 1 -Exactly -ParameterFilter { $DisplayName -eq "X-Public" }
    }
}

Describe "Get-DevBridgeLanApiEffectiveProblems (the service's start-up check, before any change)" {
    BeforeAll {
        $lanOn = "[client.pokladna.lan_api]`nenabled = true`nhostname = `"pjsnvs.pos.example.sk`""
        $pokladnaOn = "[client.pokladna]`nenabled = true`nurl = `"https://erp.example.sk`"`napi_key = `"$bridgeKey`""
        $preserved = "[client]`nserver_address = `"x:1`"`nclient_id = `"PJSNVS`"`n`n$pokladnaOn`n`n$lanOn`n`n" +
            "[[client.pokladna.kasy]]`nid = 3`nlan_key = `"$key3`"`n`n[jobs]`nmax_retries = 3`n"
    }

    It "accepts a consistent result: fresh, and over a preserved config" {
        $p = Get-DevBridgeLanApiEffectiveProblems -ClientId "pjsnvs" -LanApiToml $lanOn -KasySpec $goodSpec `
            -PokladnaToml $pokladnaOn -ApiKey $bridgeKey
        $p.Count | Should -Be 0 -Because ($p -join "; ")
        $p = Get-DevBridgeLanApiEffectiveProblems -Existing $preserved -KasySpec $goodSpec
        $p.Count | Should -Be 0 -Because "client_id from the preserved config (case-insensitive prefix): $($p -join '; ')"
        (Get-DevBridgeLanApiEffectiveProblems -Existing $preserved).Count | Should -Be 0
    }

    It "is silent while the LAN API or the pokladna source is off" {
        $off = "[client.pokladna.lan_api]`nenabled = false"
        (Get-DevBridgeLanApiEffectiveProblems -LanApiToml $off -PokladnaToml $pokladnaOn).Count | Should -Be 0
        (Get-DevBridgeLanApiEffectiveProblems -LanApiToml $lanOn).Count | Should -Be 0 -Because "no [client.pokladna]"
        (Get-DevBridgeLanApiEffectiveProblems -Existing $preserved.Replace("[client.pokladna]`nenabled = true", "[client.pokladna]`nenabled = false")).Count |
            Should -Be 0
    }

    It "names what the service would refuse, never a key" {
        $text = (Get-DevBridgeLanApiEffectiveProblems -LanApiToml $lanOn -KasySpec '[{"id": 3}]' -PokladnaToml $pokladnaOn) -join "; "
        $text | Should -Match "the LAN API needs \[client\] client_id"
        $text | Should -Match "no till has a lan_key"
        $text = (Get-DevBridgeLanApiEffectiveProblems -ClientId "pjzav" -LanApiToml $lanOn -KasySpec $goodSpec -PokladnaToml $pokladnaOn) -join "; "
        $text | Should -Match "DEVBRIDGE_LAN_API_HOSTNAME 'pjsnvs.pos.example.sk' must be <client_id>.<zone> for client_id 'pjzav'"
        # a new bridge key equal to a preserved till's key
        $text = (Get-DevBridgeLanApiEffectiveProblems -Existing $preserved -PokladnaToml $pokladnaOn -ApiKey $key3) -join "; "
        $text | Should -Match "must not be the Odoo bridge key"
        $text | Should -Not -Match ([regex]::Escape($key3))
        # new tills whose key is the PRESERVED bridge key
        $spec = '[{"id": 3, "lan_key": "' + $bridgeKey + '"}]'
        ((Get-DevBridgeLanApiEffectiveProblems -Existing $preserved -KasySpec $spec) -join "; ") | Should -Match "must not be the Odoo bridge key"
        # a harness config without ACME (acme = false) needs no client_id
        $noAcme = $lanOn + "`nacme = false"
        (Get-DevBridgeLanApiEffectiveProblems -LanApiToml $noAcme -KasySpec $goodSpec -PokladnaToml $pokladnaOn).Count | Should -Be 0
    }
}

Describe "TOML readers of a preserved config" {
    It "reads one table, a string and a boolean" {
        $cfg = "[client]`r`nclient_id = `"a`"`r`n`r`n[client.pokladna]`r`nenabled = true # on`r`napi_key = `"k`"`r`n`r`n[client.pokladna.lan_api]`r`nenabled = false`r`n"
        $pok = Get-DevBridgeTomlTableText -Config $cfg -Table "client.pokladna"
        Get-DevBridgeTomlStringValue -Text $pok -Key "api_key" | Should -BeExactly "k"
        Get-DevBridgeTomlBoolValue -Text $pok -Key "enabled" | Should -BeTrue
        $lan = Get-DevBridgeTomlTableText -Config $cfg -Table "client.pokladna.lan_api"
        Get-DevBridgeTomlBoolValue -Text $lan -Key "enabled" -Default $true | Should -BeFalse
        Get-DevBridgeTomlBoolValue -Text $lan -Key "acme" -Default $true | Should -BeTrue
        Get-DevBridgeTomlBoolValue -Text $lan -Key "acme" | Should -BeFalse
        Get-DevBridgeTomlStringValue -Text $lan -Key "api_key" | Should -BeExactly ""
        Get-DevBridgeTomlStringValue -Text $lan -Key "api_key" -Default "none" | Should -BeExactly "none"
        Get-DevBridgeTomlStringValue -Text "x = `"`"" -Key "x" -Default "none" | Should -BeExactly "" -Because "an empty value is a value"
        Get-DevBridgeTomlTableText -Config $cfg -Table "client.odoo" | Should -BeExactly ""
        Get-DevBridgeTomlStringValue -Text (Get-DevBridgeTomlTableText -Config $cfg -Table "client") -Key "client_id" |
            Should -BeExactly "a"
    }
}

Describe "Get-DevBridgeLanApiRequest (the client install's LAN API part)" {
    It "builds the blocks, the firewall port and notes without a key" {
        $r = Get-DevBridgeLanApiRequest -KasySpec $goodSpec -Enabled "true" -Port "9443" -Hostname "pjsnvs.pos.example.sk" `
            -ClientId "pjsnvs" -PokladnaToml "[client.pokladna]`nenabled = true" -ApiKey $bridgeKey
        $r.Problems.Count | Should -Be 0 -Because ($r.Problems -join "; ")
        $r.KasyToml | Should -BeExactly (Get-DevBridgePokladnaKasyToml -Spec $goodSpec)
        $r.LanApiToml | Should -Match "(?m)^enabled = true$"
        $r.FirewallPort | Should -Be 9443
        $r.SwitchedOff | Should -BeFalse
        ($r.Notes -join "|") | Should -BeExactly ("Pokladna tills requested: 2 (LAN keys set, not shown)|" +
            "LAN API requested: enabled='true' port='9443' hostname='pjsnvs.pos.example.sk' (empty = enabled, port 9443)")
        ($r.Notes -join "|") | Should -Not -Match ([regex]::Escape($key3))
        (Get-DevBridgeLanApiRequest -Hostname "a.b" -ClientId "a").FirewallPort | Should -Be 9443 -Because "the default port"
    }

    It "switches the API off without a hostname" {
        $r = Get-DevBridgeLanApiRequest -Enabled "false"
        $r.Problems.Count | Should -Be 0
        $r.SwitchedOff | Should -BeTrue
        $r.FirewallPort | Should -Be 0
    }

    It "returns the request problems first, then the service's" {
        $r = Get-DevBridgeLanApiRequest -KasySpec '[{"id": 0}]' -Enabled "maybe"
        ($r.Problems -join "; ") | Should -Match "id must be the till's positive pos.config id"
        ($r.Problems -join "; ") | Should -Match "DEVBRIDGE_LAN_API_ENABLED 'maybe' must be true or false"
        $r.KasyToml | Should -BeExactly ""
        $r.LanApiToml | Should -BeExactly ""
        $r = Get-DevBridgeLanApiRequest -KasySpec $goodSpec -Hostname "pjsnvs.pos.example.sk" -ClientId "pjzav" `
            -PokladnaToml "[client.pokladna]`nenabled = true"
        ($r.Problems -join "; ") | Should -Match "for client_id 'pjzav'"
        # a bridge key rotation alone is checked against the preserved tills too
        $existing = "[client]`nclient_id = `"pjsnvs`"`n`n[client.pokladna]`nenabled = true`n`n" +
            "[client.pokladna.lan_api]`nenabled = true`nhostname = `"pjsnvs.pos.example.sk`"`n`n" +
            "[[client.pokladna.kasy]]`nid = 3`nlan_key = `"$key3`"`n"
        $r = Get-DevBridgeLanApiRequest -Existing $existing -PokladnaToml "[client.pokladna]`nenabled = true" -ApiKey $key3
        ($r.Problems -join "; ") | Should -Match "must not be the Odoo bridge key"
        (Get-DevBridgeLanApiRequest -Existing $existing).Problems.Count | Should -Be 0 -Because "nothing changes"
    }
}

Describe "Get-DevBridgeAcmeRequest and Get-DevBridgePublicNetworkWarnings" {
    It "returns the blocks and a note, or the problems" {
        $r = Get-DevBridgeAcmeRequest -Zone "pos.example.sk" -CloudflareZoneId "0123abcd" -ApiToken "tok-never-printed"
        $r.Problems.Count | Should -Be 0
        $r.Blocks.Count | Should -Be 2
        $r.Note | Should -BeExactly "[server.acme] requested: zone 'pos.example.sk', Cloudflare zone '0123abcd' (API token set, not shown)"
        $bad = Get-DevBridgeAcmeRequest -Zone "x" -CloudflareZoneId "0123abcd" -ApiToken "tok"
        ($bad.Problems -join "; ") | Should -Match "DEVBRIDGE_ACME_ZONE 'x'"
        $bad.Blocks.Count | Should -Be 0
    }

    It "warns about every Public network only" {
        Mock Get-NetConnectionProfile {
            @([pscustomobject]@{ InterfaceAlias = "Ethernet"; NetworkCategory = "Public" },
                [pscustomobject]@{ InterfaceAlias = "Wi-Fi"; NetworkCategory = "Private" })
        }
        $w = Get-DevBridgePublicNetworkWarnings
        $w.Count | Should -Be 1
        $w[0] | Should -Match "network 'Ethernet' is Public: the LAN API port is BLOCKED there"
        Mock Get-NetConnectionProfile { [pscustomobject]@{ InterfaceAlias = "Ethernet 2"; NetworkCategory = "Private" } }
        (Get-DevBridgePublicNetworkWarnings).Count | Should -Be 0
    }
}

Describe "Get-DevBridgeAcmeConfigProblems / Get-DevBridgeAcmeToml ([server.acme], pz-server)" {
    BeforeAll { $cfToken = "cf-token-never-printed-0123456789" }

    It "accepts a complete request and builds both tables" {
        $p = Get-DevBridgeAcmeConfigProblems -Zone "pos.example.sk" -ContactEmail "it@example.sk" -CloudflareZoneId "0123abcd" -ApiToken $cfToken
        $p.Count | Should -Be 0 -Because ($p -join "; ")
        $blocks = Get-DevBridgeAcmeToml -Zone "pos.example.sk" -ContactEmail "it@example.sk" -CloudflareZoneId "0123abcd" -ApiToken $cfToken
        $blocks.Count | Should -Be 2
        $blocks[0] | Should -BeExactly "[server.acme]`nenabled = true`nzone = `"pos.example.sk`"`ncontact_email = `"it@example.sk`""
        $blocks[1] | Should -BeExactly "[server.acme.cloudflare]`nzone_id = `"0123abcd`"`napi_token = `"$cfToken`""
        (Get-DevBridgeAcmeToml -Zone "a.b" -CloudflareZoneId "z" -ApiToken "t")[0] | Should -BeExactly "[server.acme]`nenabled = true`nzone = `"a.b`""
    }

    It "names every problem and never the token" {
        $text = (Get-DevBridgeAcmeConfigProblems -Zone "POS" -ContactEmail "a b@c" -CloudflareZoneId "zone id" -ApiToken "has space $cfToken") -join "; "
        $text | Should -Match "DEVBRIDGE_ACME_ZONE 'POS' must be a lower-case DNS name"
        $text | Should -Match "DEVBRIDGE_ACME_CONTACT_EMAIL 'a b@c' must be an e-mail address"
        $text | Should -Match "DEVBRIDGE_CLOUDFLARE_ZONE_ID 'zone id' must be the Cloudflare zone id"
        $text | Should -Match "DEVBRIDGE_CLOUDFLARE_API_TOKEN contains whitespace"
        $text | Should -Not -Match ([regex]::Escape($cfToken))
        ((Get-DevBridgeAcmeConfigProblems -Zone "a.b" -CloudflareZoneId "z") -join "; ") | Should -Match "DEVBRIDGE_CLOUDFLARE_API_TOKEN is not set"
        (Get-DevBridgeAcmeConfigProblems -Zone "a.b" -CloudflareZoneId "z" -ApiToken "t").Count | Should -Be 0 -Because "the e-mail is optional"
    }

    It "Test-DevBridgeConfigHasAcmeToken detects the token only" {
        $dir = New-TempDir
        try {
            $cfg = Join-Path $dir "config.toml"
            Test-DevBridgeConfigHasAcmeToken -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, "[server.acme]`nenabled = true`n")
            Test-DevBridgeConfigHasAcmeToken -Path $cfg | Should -BeFalse
            [System.IO.File]::WriteAllText($cfg, "[server.acme.cloudflare]`n  api_token = `"t`"`n")
            Test-DevBridgeConfigHasAcmeToken -Path $cfg | Should -BeTrue
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }

    It "merges both tables into a preserved server config, replacing them on rotation" {
        $dir = New-TempDir
        try {
            $cfg = Join-Path $dir "config.toml"
            $server = "[general]`nmode = `"server`"`n`n[server]`ngrpc_port = 50051`n`n[jobs]`nmax_retries = 3`n"
            [System.IO.File]::WriteAllText($cfg, $server)
            $first = Get-DevBridgeAcmeToml -Zone "pos.example.sk" -CloudflareZoneId "z1" -ApiToken "old-token"
            Merge-DevBridgeTomlTableIntoConfig -Path $cfg -Table "server.acme" -Block $first[0] | Should -BeExactly "added"
            Merge-DevBridgeTomlTableIntoConfig -Path $cfg -Table "server.acme.cloudflare" -Block $first[1] | Should -BeExactly "added"
            $second = Get-DevBridgeAcmeToml -Zone "pos.example.sk" -CloudflareZoneId "z1" -ApiToken "new-token"
            Merge-DevBridgeTomlTableIntoConfig -Path $cfg -Table "server.acme" -Block $second[0] | Should -BeExactly "replaced"
            Merge-DevBridgeTomlTableIntoConfig -Path $cfg -Table "server.acme.cloudflare" -Block $second[1] | Should -BeExactly "replaced"
            $out = [System.IO.File]::ReadAllText($cfg)
            $out | Should -Not -Match "old-token"
            ([regex]::Matches($out, '(?m)^\[server\.acme\]')).Count | Should -Be 1
            ([regex]::Matches($out, '(?m)^\[server\.acme\.cloudflare\]')).Count | Should -Be 1
            $out | Should -BeExactly $server.Replace("[jobs]", "$($second[0])`n`n$($second[1])`n`n[jobs]")
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
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
        Copy-Item $lanLibPath (Join-Path $script:workDir "DevBridgeLanApiLib.ps1")
        $script:cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($script:cfg, $sampleConfig)
    }
    AfterEach {
        Remove-Item Env:\DEVBRIDGE_POKLADNA_KASY -ErrorAction SilentlyContinue
        Remove-Item Env:\DEVBRIDGE_POKLADNA_API_KEY -ErrorAction SilentlyContinue
        Remove-Item Env:\DEVBRIDGE_ACME_ZONE -ErrorAction SilentlyContinue
        Remove-Item Env:\DEVBRIDGE_CLOUDFLARE_ZONE_ID -ErrorAction SilentlyContinue
        Remove-Item Env:\DEVBRIDGE_CLOUDFLARE_API_TOKEN -ErrorAction SilentlyContinue
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

    It "refuses a LAN API the service would refuse (hostname of another client_id)" {
        $env:DEVBRIDGE_POKLADNA_KASY = $goodSpec
        $hashBefore = (Get-FileHash -LiteralPath $script:cfg).Hash
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode client -LanApiEnabled:true -LanApiHostname pjzav.pos.example.sk -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "must be <client_id>.<zone> for client_id 'pjsnvs'"
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        (Get-FileHash -LiteralPath $script:cfg).Hash | Should -BeExactly $hashBefore
    }

    It "validates [server.acme] in server mode and never prints the token" {
        $token = "cf-token-never-printed-0123456789"
        $env:DEVBRIDGE_ACME_ZONE = "pos.example.sk"
        $env:DEVBRIDGE_CLOUDFLARE_ZONE_ID = "0123abcd"
        $env:DEVBRIDGE_CLOUDFLARE_API_TOKEN = $token
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
        $r.StdOut | Should -Match "\[server.acme\] requested: zone 'pos.example.sk', Cloudflare zone '0123abcd' \(API token set, not shown\)"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($token))
        $env:DEVBRIDGE_CLOUDFLARE_API_TOKEN = "bad token $token"
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "DEVBRIDGE_CLOUDFLARE_API_TOKEN contains whitespace"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($token))
    }
}
