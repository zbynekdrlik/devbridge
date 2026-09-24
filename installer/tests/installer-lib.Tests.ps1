# Pester v5 tests for the installer upgrade-hardening helpers.
#
# The functions under test live in the production scripts: the config helpers
# in installer/DevBridgeInstallerLib.ps1 (the function library post-install.ps1
# dot-sources, issue #80) and the binary-swap helpers INLINE in
# installer/install.ps1 (it runs via irm|iex with no files on disk, so it can
# not dot-source a lib). To test the REAL code with zero drift, BeforeAll parses
# each script with the PowerShell AST and extracts the named function bodies
# without executing any script body.
#
# Covered: config preserve-vs-rewrite branch selection, permissive force-rewrite
# flag parse, snapshot creation, prune-to-5, the binary-swap file-unlock poll,
# SHA256 pre/post swap verification (including the same-version-reinstall
# carve-out, issue #71), the installed-version registry read, and the
# best-effort service restart on a failed-install path.
#
# Every test runs against a throwaway temp dir; nothing touches a real
# C:\ProgramData\DevBridge. CI runs this via Invoke-Pester on windows-latest.

BeforeAll {
    # We test the ACTUAL production code. install.ps1 (run via irm|iex) cannot
    # dot-source a lib at runtime, so its helpers are defined INLINE in it; the
    # post-install helpers live in DevBridgeInstallerLib.ps1 (issue #80). To
    # avoid testing a divergent copy, we parse each real script with the
    # PowerShell AST, pull out the named function definitions, and load THEM into
    # this scope. If a future edit changes the production function, these tests
    # exercise that exact change.
    $installerDir = Split-Path -Parent $PSScriptRoot

    # Get-FunctionSourceFromScript parses a real installer script and returns
    # @{ name = function-source-text } WITHOUT executing the script body (which
    # would trigger Stop-Service / scheduled tasks / network probes). It is the
    # SAME helper the E2E client setup uses to call the real serial-bridge merge
    # (issue #70), so it lives once in deploy/lib.
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    # Config helpers live in DevBridgeInstallerLib.ps1 (dot-sourced by
    # post-install.ps1, issue #80); binary-swap helpers in install.ps1.
    $functionSources = [ordered]@{}
    (Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "DevBridgeInstallerLib.ps1") `
        -Names @("Test-DevBridgeForceRewrite", "Get-DevBridgeConfigAction", "New-DevBridgeConfigSnapshot", "Get-DevBridgeClientConfigExtras", "Get-DevBridgeSerialBridgeToml", "Merge-DevBridgeSerialBridgeIntoConfig", "ConvertFrom-DevBridgeSerialBridgesSpec", "Get-DevBridgeServerSerialBridgesToml", "Add-DevBridgeServerSerialBridgesToConfig", "Merge-DevBridgeServerSerialBridgesIntoConfig", "Get-DevBridgeCom0comMissingPortWarnings")).GetEnumerator() |
        ForEach-Object { $functionSources[$_.Key] = $_.Value }
    (Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "install.ps1") `
        -Names @("Wait-DevBridgeBinaryUnlocked", "Test-DevBridgeBinarySwapOk", "Get-DevBridgeInstalledVersion", "Get-DevBridgeVersionFromAssetName", "Restore-DevBridgeService", "Get-DevBridgePostInstallArgs", "Assert-DevBridgeSerialBaud")).GetEnumerator() |
        ForEach-Object { $functionSources[$_.Key] = $_.Value }

    # Dot-source each extracted function body into THIS (BeforeAll/container)
    # scope so the It blocks can call them.
    foreach ($src in $functionSources.Values) {
        . ([scriptblock]::Create($src))
    }

    # Per-test temp DataDir helper.
    function New-TempDataDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbtest-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        return $dir
    }
}

Describe "Test-DevBridgeForceRewrite (permissive flag parse)" {
    It "treats '<value>' as force-rewrite=true" -ForEach @(
        @{ value = "true" }
        @{ value = "True" }
        @{ value = "TRUE" }
        @{ value = "1" }
        @{ value = "yes" }
        @{ value = "on" }
        @{ value = "  true  " }   # surrounding whitespace tolerated
        @{ value = "ON" }
    ) {
        Test-DevBridgeForceRewrite $value | Should -BeTrue
    }

    It "treats '<value>' as NOT force-rewrite (typo / unknown / empty)" -ForEach @(
        @{ value = "" }
        @{ value = "false" }
        @{ value = "0" }
        @{ value = "no" }
        @{ value = "off" }
        @{ value = "yep" }       # not in the allow-list
        @{ value = "truthy" }    # must NOT substring-match "true"
        @{ value = "10" }        # must NOT match bare "1"
        @{ value = $null }
    ) {
        Test-DevBridgeForceRewrite $value | Should -BeFalse
    }
}

Describe "Get-DevBridgeConfigAction (preserve-vs-rewrite branch selection)" {
    It "preserves when a config exists and force-rewrite is off" {
        Get-DevBridgeConfigAction -ExistingConfig $true -ForceRewrite $false | Should -Be "preserve"
    }

    It "rewrites the existing config when force-rewrite is on" {
        Get-DevBridgeConfigAction -ExistingConfig $true -ForceRewrite $true | Should -Be "rewrite-existing"
    }

    It "writes a fresh config when none exists (force-rewrite off)" {
        Get-DevBridgeConfigAction -ExistingConfig $false -ForceRewrite $false | Should -Be "write-fresh"
    }

    It "writes a fresh config when none exists even if force-rewrite is on" {
        # No existing config => nothing to snapshot/preserve regardless of flag.
        Get-DevBridgeConfigAction -ExistingConfig $false -ForceRewrite $true | Should -Be "write-fresh"
    }
}

Describe "New-DevBridgeConfigSnapshot (snapshot creation + prune)" {
    BeforeEach {
        $script:dataDir = New-TempDataDir
        $script:configPath = Join-Path $script:dataDir "config.toml"
        Set-Content -Path $script:configPath -Value "mode = `"client`"" -Encoding ASCII
    }
    AfterEach {
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "creates a preupgrade snapshot whose contents match the original config" {
        $backup = New-DevBridgeConfigSnapshot -ConfigPath $script:configPath -DataDir $script:dataDir `
            -Prefix "config.toml.preupgrade-" -KeepCount 5
        $backup | Should -Not -BeNullOrEmpty
        Test-Path $backup | Should -BeTrue
        (Get-Content $backup -Raw) | Should -Be (Get-Content $script:configPath -Raw)
        Split-Path -Leaf $backup | Should -BeLike "config.toml.preupgrade-*"
    }

    It "creates a 'replaced-' snapshot for the force-rewrite path" {
        $backup = New-DevBridgeConfigSnapshot -ConfigPath $script:configPath -DataDir $script:dataDir `
            -Prefix "config.toml.replaced-"
        Split-Path -Leaf $backup | Should -BeLike "config.toml.replaced-*"
        Test-Path $backup | Should -BeTrue
    }

    It "prunes to the 5 most recent preupgrade snapshots, deleting older ones" {
        # Seed 7 pre-existing snapshots with ascending timestamps, then create
        # one more via the function with KeepCount=5. Expect exactly 5 to remain.
        $base = (Get-Date).AddHours(-10)
        1..7 | ForEach-Object {
            $name = "config.toml.preupgrade-old{0:D2}" -f $_
            $p = Join-Path $script:dataDir $name
            Set-Content -Path $p -Value "old$_" -Encoding ASCII
            # Stagger LastWriteTime so Sort-Object -Descending is deterministic.
            (Get-Item $p).LastWriteTime = $base.AddMinutes($_)
        }

        $created = New-DevBridgeConfigSnapshot -ConfigPath $script:configPath -DataDir $script:dataDir `
            -Prefix "config.toml.preupgrade-" -KeepCount 5
        # The newly created snapshot has "now" mtime => newest => must survive.
        (Get-Item $created).LastWriteTime = (Get-Date)

        # Re-run prune semantics by creating once more is not needed; assert state.
        $remaining = @(Get-ChildItem -Path $script:dataDir -Filter "config.toml.preupgrade-*")
        $remaining.Count | Should -Be 5
        # The just-created snapshot must be among the survivors.
        ($remaining.Name -contains (Split-Path -Leaf $created)) | Should -BeTrue
        # The oldest seeded snapshots must have been pruned.
        ($remaining.Name -contains "config.toml.preupgrade-old01") | Should -BeFalse
        ($remaining.Name -contains "config.toml.preupgrade-old02") | Should -BeFalse
    }

    It "does NOT prune when KeepCount is 0 (force-rewrite snapshot path)" {
        1..4 | ForEach-Object {
            Set-Content -Path (Join-Path $script:dataDir ("config.toml.replaced-old{0}" -f $_)) -Value "x" -Encoding ASCII
        }
        New-DevBridgeConfigSnapshot -ConfigPath $script:configPath -DataDir $script:dataDir `
            -Prefix "config.toml.replaced-" | Out-Null
        # 4 seeded + 1 created = 5 (no prune).
        @(Get-ChildItem -Path $script:dataDir -Filter "config.toml.replaced-*").Count | Should -Be 5
    }

    It "returns `$null when the source config cannot be copied (snapshot failure is non-fatal)" {
        $missing = Join-Path $script:dataDir "does-not-exist.toml"
        $backup = New-DevBridgeConfigSnapshot -ConfigPath $missing -DataDir $script:dataDir `
            -Prefix "config.toml.preupgrade-" -KeepCount 5
        $backup | Should -BeNullOrEmpty
    }

    It "still prunes existing snapshots when the copy fails (prune is unconditional, matches pre-refactor)" {
        # Seed 7 old snapshots, then fail the copy (missing source). Prune must
        # still trim to KeepCount=5 even though no new snapshot was created.
        1..7 | ForEach-Object {
            $p = Join-Path $script:dataDir ("config.toml.preupgrade-old{0:00}" -f $_)
            Set-Content -Path $p -Value "x" -Encoding ASCII
            (Get-Item $p).LastWriteTime = (Get-Date).AddMinutes(-$_)
        }
        $missing = Join-Path $script:dataDir "does-not-exist.toml"
        $backup = New-DevBridgeConfigSnapshot -ConfigPath $missing -DataDir $script:dataDir `
            -Prefix "config.toml.preupgrade-" -KeepCount 5
        $backup | Should -BeNullOrEmpty
        @(Get-ChildItem -Path $script:dataDir -Filter "config.toml.preupgrade-*").Count | Should -Be 5
    }
}

Describe "Wait-DevBridgeBinaryUnlocked (file-unlock poll)" {
    BeforeEach {
        $script:dataDir = New-TempDataDir
    }
    AfterEach {
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "returns true immediately for a fresh install (binary absent)" {
        $absent = Join-Path $script:dataDir "devbridge-service.exe"
        Wait-DevBridgeBinaryUnlocked -Path $absent -TimeoutSeconds 1 | Should -BeTrue
    }

    It "returns true for an unlocked, writable binary" {
        $bin = Join-Path $script:dataDir "devbridge-service.exe"
        Set-Content -Path $bin -Value "MZ" -Encoding ASCII
        Wait-DevBridgeBinaryUnlocked -Path $bin -TimeoutSeconds 2 -SleepMilliseconds 50 | Should -BeTrue
    }

    It "returns false within the timeout when the binary stays exclusively locked" {
        $bin = Join-Path $script:dataDir "devbridge-service.exe"
        Set-Content -Path $bin -Value "MZ" -Encoding ASCII
        # Hold an exclusive write handle (FileShare::None) for the whole probe.
        $lock = [System.IO.File]::Open($bin, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
        try {
            $result = Wait-DevBridgeBinaryUnlocked -Path $bin -TimeoutSeconds 2 -SleepMilliseconds 50
            $result | Should -BeFalse
        } finally {
            $lock.Close()
        }
    }

    It "retries past a transient lock and returns true once it is released mid-poll" {
        # Deterministic, race-free version of "lock released part-way through the
        # poll": a background job acquires an exclusive write lock, signals via a
        # marker file that the lock is genuinely held, then releases it ~600 ms
        # later. We block until the marker exists so the FIRST probe iteration is
        # guaranteed to hit a held lock -- proving the retry loop recovers rather
        # than the probe trivially succeeding on iteration 1.
        $bin = Join-Path $script:dataDir "devbridge-service.exe"
        Set-Content -Path $bin -Value "MZ" -Encoding ASCII
        $markerHeld = Join-Path $script:dataDir "lock-held.marker"
        $job = Start-Job -ScriptBlock {
            param($p, $marker)
            $h = [System.IO.File]::Open($p, [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            # Announce the lock is held ONLY after Open() returned.
            Set-Content -Path $marker -Value "held" -Encoding ASCII
            Start-Sleep -Milliseconds 600
            $h.Close()
        } -ArgumentList $bin, $markerHeld
        try {
            # Block until the job has actually acquired the lock (bounded wait).
            $waited = 0
            while (-not (Test-Path $markerHeld) -and $waited -lt 5000) {
                Start-Sleep -Milliseconds 25
                $waited += 25
            }
            (Test-Path $markerHeld) | Should -BeTrue   # lock is genuinely held now
            # Probe must fail initially (lock held) then succeed after release.
            # Timeout (8s) comfortably exceeds the 600 ms remaining hold.
            Wait-DevBridgeBinaryUnlocked -Path $bin -TimeoutSeconds 8 -SleepMilliseconds 100 | Should -BeTrue
        } finally {
            Wait-Job -Job $job -Timeout 10 | Out-Null
            Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
        }
    }
}

Describe "Get-DevBridgeClientConfigExtras (issue #68 -- [client.serial_bridge] emission)" {
    It "emits enabled/port/baud_rate when -SerialPort is set" {
        $extras = Get-DevBridgeClientConfigExtras -SerialPort "COM4" -SerialBaudRate 9600
        $extras | Should -Match '(?m)^\[client\.serial_bridge\]$'
        $extras | Should -Match '(?m)^enabled = true$'
        $extras | Should -Match '(?m)^port = "COM4"$'
        $extras | Should -Match '(?m)^baud_rate = 9600$'
    }

    It "omits [client.serial_bridge] entirely when -SerialPort is not given" {
        $extras = Get-DevBridgeClientConfigExtras -ClientId "some-client"
        $extras | Should -Not -Match 'serial_bridge'
        $extras | Should -Not -Match 'baud_rate'
    }

    It "defaults SerialBaudRate to 9600 when only -SerialPort is given" {
        $extras = Get-DevBridgeClientConfigExtras -SerialPort "COM4"
        $extras | Should -Match '(?m)^baud_rate = 9600$'
    }

    It "still emits the pre-existing optional fields unchanged (no regression)" {
        $extras = Get-DevBridgeClientConfigExtras -ClientId "pjkeb-client" -PrintBackend "direct_ipp"
        $extras | Should -Match '(?m)^client_id = "pjkeb-client"$'
        $extras | Should -Match '(?m)^print_backend = "direct_ipp"$'
        $extras | Should -Not -Match 'serial_bridge'
    }

    It "places [client.serial_bridge] LAST, after every scalar field (review finding F7 invariant)" {
        $extras = Get-DevBridgeClientConfigExtras -ClientId "some-client" -PrintBackend "direct_ipp" `
            -SerialPort "COM4" -SerialBaudRate 9600
        $idxClientId = $extras.IndexOf('client_id')
        $idxBackend = $extras.IndexOf('print_backend')
        $idxSerial = $extras.IndexOf('[client.serial_bridge]')
        $idxClientId | Should -BeGreaterThan -1
        $idxBackend | Should -BeGreaterThan -1
        $idxSerial | Should -BeGreaterThan -1
        $idxSerial | Should -BeGreaterThan $idxClientId
        $idxSerial | Should -BeGreaterThan $idxBackend
        # Nothing may follow the serial_bridge block -- it is spliced
        # immediately before [jobs] at the call site with no trailer.
        $extras.TrimEnd().EndsWith("baud_rate = 9600") | Should -BeTrue
    }
}

Describe "Get-DevBridgeSerialBridgeToml (issue #68 -- shared TOML-block builder)" {
    It "returns the exact 4-line [client.serial_bridge] block" {
        $toml = Get-DevBridgeSerialBridgeToml -SerialPort "COM4" -SerialBaudRate 19200
        $lines = $toml -split "`n"
        $lines.Count | Should -Be 4
        $lines[0] | Should -Be '[client.serial_bridge]'
        $lines[1] | Should -Be 'enabled = true'
        $lines[2] | Should -Be 'port = "COM4"'
        $lines[3] | Should -Be 'baud_rate = 19200'
    }

    It "defaults SerialBaudRate to 9600" {
        $toml = Get-DevBridgeSerialBridgeToml -SerialPort "COM9"
        ($toml -split "`n")[3] | Should -Be 'baud_rate = 9600'
    }
}

Describe "Merge-DevBridgeSerialBridgeIntoConfig (issue #68 review F1 -- preserve-branch merge)" {
    BeforeEach {
        $script:dataDir = New-TempDataDir
        $script:configPath = Join-Path $script:dataDir "config.toml"
    }
    AfterEach {
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "adds [client.serial_bridge] before [jobs] when the section is absent, UTF-8 no BOM" {
        $original = "[general]`nmode = `"client`"`n`n[client]`nserver_address = `"1.2.3.4:50051`"`n`n[jobs]`nmax_retries = 3`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII

        $result = Merge-DevBridgeSerialBridgeIntoConfig -Path $script:configPath -SerialPort "COM4" -SerialBaudRate 19200
        $result | Should -Be "added"

        $content = Get-Content -Path $script:configPath -Raw
        $content | Should -Match '(?m)^\[client\.serial_bridge\]$'
        $content | Should -Match '(?m)^enabled = true$'
        $content | Should -Match '(?m)^port = "COM4"$'
        $content | Should -Match '(?m)^baud_rate = 19200$'
        # The block must land BEFORE [jobs], and the rest of the file survives.
        $content.IndexOf("[client.serial_bridge]") | Should -BeLessThan $content.IndexOf("[jobs]")
        $content | Should -Match 'server_address = "1\.2\.3\.4:50051"'
        $content | Should -Match 'max_retries = 3'

        $bytes = [System.IO.File]::ReadAllBytes($script:configPath)
        ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) | Should -BeFalse
    }

    It "keeps the file byte-identical and returns 'kept' when [client.serial_bridge] already exists" {
        $original = "[general]`nmode = `"client`"`n`n[client]`nserver_address = `"1.2.3.4:50051`"`n`n[client.serial_bridge]`nenabled = true`nport = `"COM9`"`nbaud_rate = 4800`n`n[jobs]`nmax_retries = 3`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII
        $before = Get-Content -Path $script:configPath -Raw

        $result = Merge-DevBridgeSerialBridgeIntoConfig -Path $script:configPath -SerialPort "COM4" -SerialBaudRate 19200
        $result | Should -Be "kept"

        $after = Get-Content -Path $script:configPath -Raw
        $after | Should -Be $before
        # The pre-existing values must NOT be overwritten by the new args.
        $after | Should -Match 'port = "COM9"'
        $after | Should -Not -Match 'port = "COM4"'
    }

    It "leaves the file byte-identical and returns 'skipped' when -SerialPort is not given" {
        $original = "[general]`nmode = `"client`"`n`n[client]`nserver_address = `"1.2.3.4:50051`"`n`n[jobs]`nmax_retries = 3`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII
        $before = Get-Content -Path $script:configPath -Raw

        $result = Merge-DevBridgeSerialBridgeIntoConfig -Path $script:configPath -SerialPort ""
        $result | Should -Be "skipped"

        $after = Get-Content -Path $script:configPath -Raw
        $after | Should -Be $before
        $after | Should -Not -Match 'serial_bridge'
    }

    It "appends at the end when [jobs] is absent, preserving CRLF line endings" {
        $original = "[general]`r`nmode = `"client`"`r`n`r`n[client]`r`nserver_address = `"1.2.3.4:50051`"`r`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII

        $result = Merge-DevBridgeSerialBridgeIntoConfig -Path $script:configPath -SerialPort "COM4" -SerialBaudRate 9600
        $result | Should -Be "added"

        $raw = [System.IO.File]::ReadAllText($script:configPath)
        $raw.Contains("[client.serial_bridge]`r`n") | Should -BeTrue
        $raw.Contains("port = `"COM4`"`r`n") | Should -BeTrue
        $raw.Contains("`n`n") | Should -BeFalse   # no bare LF was introduced into a CRLF file
    }
}

Describe "Get-DevBridgePostInstallArgs (install.ps1 env -> post-install.ps1 args mapping, issue #68)" {
    It "maps DEVBRIDGE_SERIAL_PORT/DEVBRIDGE_SERIAL_BAUD to -SerialPort/-SerialBaudRate" {
        $envSnapshot = @{ DEVBRIDGE_SERIAL_PORT = "COM4"; DEVBRIDGE_SERIAL_BAUD = "19200" }
        $args = Get-DevBridgePostInstallArgs -Mode "client" -Env $envSnapshot
        $idxPort = [array]::IndexOf($args, "-SerialPort")
        $idxPort | Should -BeGreaterThan -1
        $args[$idxPort + 1] | Should -Be "COM4"
        $idxBaud = [array]::IndexOf($args, "-SerialBaudRate")
        $idxBaud | Should -BeGreaterThan -1
        $args[$idxBaud + 1] | Should -Be "19200"
    }

    It "omits -SerialPort/-SerialBaudRate when neither env var is set" {
        $envSnapshot = @{ DEVBRIDGE_TARGET_PRINTER = "Canon MG3600" }
        $args = Get-DevBridgePostInstallArgs -Mode "client" -Env $envSnapshot
        $args | Should -Not -Contain "-SerialPort"
        $args | Should -Not -Contain "-SerialBaudRate"
        $args | Should -Contain "-TargetPrinter"
    }

    It "still maps pre-existing env vars unchanged (no regression)" {
        $envSnapshot = @{
            DEVBRIDGE_SERVER_HOST    = "print-server.lan"
            DEVBRIDGE_TARGET_PRINTER = "Canon MG3600"
            DEVBRIDGE_PRINTER_TLS    = "true"
        }
        $args = Get-DevBridgePostInstallArgs -Mode "client" -Env $envSnapshot
        $args | Should -Contain "-Mode"
        $args | Should -Contain "client"
        $args | Should -Contain "-ServerHost"
        $args | Should -Contain "print-server.lan"
        $args | Should -Contain "-TargetPrinter"
        $args | Should -Contain "Canon MG3600"
        $args | Should -Contain "-PrinterTls"
    }

    It "maps DEVBRIDGE_SERIAL_BRIDGES to -SerialBridges in server mode (issue #69)" {
        $envSnapshot = @{ DEVBRIDGE_SERIAL_BRIDGES = "pjkeb-client=COM20,pjsln-client=COM22" }
        $args = Get-DevBridgePostInstallArgs -Mode "server" -Env $envSnapshot
        $idx = [array]::IndexOf($args, "-SerialBridges")
        $idx | Should -BeGreaterThan -1
        $args[$idx + 1] | Should -BeExactly "pjkeb-client=COM20,pjsln-client=COM22"
    }

    It "does NOT forward DEVBRIDGE_SERIAL_BRIDGES in client mode (issue #69)" {
        $envSnapshot = @{ DEVBRIDGE_SERIAL_BRIDGES = "pjkeb-client=COM20" }
        $args = Get-DevBridgePostInstallArgs -Mode "client" -Env $envSnapshot
        $args | Should -Not -Contain "-SerialBridges"
        $args | Should -Not -Contain "pjkeb-client=COM20"
    }

    It "omits -SerialBridges in server mode when the env var is not set (issue #69)" {
        $args = Get-DevBridgePostInstallArgs -Mode "server" -Env @{ DEVBRIDGE_DASHBOARD_PORT = "9120" }
        $args | Should -Not -Contain "-SerialBridges"
        $args | Should -Contain "-DashboardPort"
    }

    It "omits -SerialBaudRate (and -SerialPort) when only DEVBRIDGE_SERIAL_BAUD is set (review finding F4)" {
        $envSnapshot = @{ DEVBRIDGE_SERIAL_BAUD = "19200" }
        $args = Get-DevBridgePostInstallArgs -Mode "client" -Env $envSnapshot
        $args | Should -Not -Contain "-SerialBaudRate"
        $args | Should -Not -Contain "-SerialPort"
    }
}

Describe "ConvertFrom-DevBridgeSerialBridgesSpec (issue #69 -- DEVBRIDGE_SERIAL_BRIDGES parse)" {
    It "parses a two-entry spec, defaulting baud to 9600" {
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20,pjsln-client=COM22")
        $entries.Count | Should -Be 2
        $entries[0].ClientId | Should -Be "pjkeb-client"
        $entries[0].VirtualPort | Should -Be "COM20"
        $entries[0].BaudRate | Should -Be 9600
        $entries[1].ClientId | Should -Be "pjsln-client"
        $entries[1].VirtualPort | Should -Be "COM22"
        $entries[1].BaudRate | Should -Be 9600
    }

    It "honours an explicit baud, tolerates whitespace, upper-cases the port" {
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec " store-a = com24 : 19200 , ")
        $entries.Count | Should -Be 1
        $entries[0].ClientId | Should -Be "store-a"
        $entries[0].VirtualPort | Should -Be "COM24"
        $entries[0].BaudRate | Should -Be 19200
    }

    It "returns no entries for '<value>'" -ForEach @(
        @{ value = "" }
        @{ value = "   " }
        @{ value = $null }
    ) {
        @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec $value).Count | Should -Be 0
    }

    It "throws a descriptive error for malformed entry '<value>'" -ForEach @(
        @{ value = "pjkeb-client" }              # no port
        @{ value = "pjkeb-client=COMX" }         # non-numeric port
        @{ value = "pjkeb-client=/dev/ttyS0" }   # not a COM port
        @{ value = "=COM20" }                    # empty client_id
        @{ value = "pj keb=COM20" }              # space inside client_id
        @{ value = "pjkeb-client=COM20:fast" }   # non-numeric baud
        @{ value = 'pj"keb=COM20' }              # would break the TOML string
        @{ value = "pjkeb-client=COM0" }         # no COM0 on Windows
        @{ value = "pjkeb-client=COM020" }       # leading zero would dodge the duplicate/SERIALCOMM checks
        @{ value = "pjkeb-client=COM1234" }      # out of range
    ) {
        { ConvertFrom-DevBridgeSerialBridgesSpec -Spec $value } | Should -Throw "*DEVBRIDGE_SERIAL_BRIDGES entry*malformed*"
    }

    It "throws for a zero baud" {
        { ConvertFrom-DevBridgeSerialBridgesSpec -Spec "a=COM20:0" } | Should -Throw "*invalid baud rate*"
    }

    It "throws for a duplicate client_id" {
        { ConvertFrom-DevBridgeSerialBridgesSpec -Spec "a=COM20,a=COM22" } | Should -Throw "*client_id 'a' more than once*"
    }

    It "accepts client_ids differing only by case (distinct keys for the Rust HashMap)" {
        @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "store=COM20,STORE=COM22").Count | Should -Be 2
    }

    It "throws for a duplicate virtual port (case-insensitive)" {
        { ConvertFrom-DevBridgeSerialBridgesSpec -Spec "a=COM20,b=com20" } | Should -Throw "*virtual port COM20 more than once*"
    }

    It "is byte-identical in install.ps1 and DevBridgeInstallerLib.ps1 (no drift between the two copies)" {
        $fromInstall = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "install.ps1") `
            -Names @("ConvertFrom-DevBridgeSerialBridgesSpec")
        $fromLib = Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "DevBridgeInstallerLib.ps1") `
            -Names @("ConvertFrom-DevBridgeSerialBridgesSpec")
        $fromInstall["ConvertFrom-DevBridgeSerialBridgesSpec"] | Should -BeExactly $fromLib["ConvertFrom-DevBridgeSerialBridgesSpec"]
    }
}

Describe "Get-DevBridgeServerSerialBridgesToml (issue #69 -- [[server.serial_bridges]] builder)" {
    It "returns the exact blocks, blank line between entries" {
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20,pjsln-client=COM22:19200")
        $toml = Get-DevBridgeServerSerialBridgesToml -Entries $entries
        $expected = @(
            '[[server.serial_bridges]]',
            'client_id = "pjkeb-client"',
            'virtual_port = "COM20"',
            'baud_rate = 9600',
            '',
            '[[server.serial_bridges]]',
            'client_id = "pjsln-client"',
            'virtual_port = "COM22"',
            'baud_rate = 19200'
        ) -join "`n"
        $toml | Should -BeExactly $expected
    }

    It "returns an empty string for no entries" {
        Get-DevBridgeServerSerialBridgesToml -Entries @() | Should -BeExactly ""
    }
}

Describe "Add-DevBridgeServerSerialBridgesToConfig (issue #69 -- fresh server config)" {
    It "appends the blocks at the END of the config, after [jobs]" {
        $config = "[general]`nmode = `"server`"`n`n[server]`nipp_port = 631`n`n[jobs]`nmax_retries = 3`nprint_timeout_secs = 1800"
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20")
        $out = Add-DevBridgeServerSerialBridgesToConfig -Config $config -Entries $entries
        $out.StartsWith($config) | Should -BeTrue
        $out.IndexOf("[[server.serial_bridges]]") | Should -BeGreaterThan $out.IndexOf("[jobs]")
        $out.IndexOf("[[server.serial_bridges]]") | Should -BeGreaterThan $out.IndexOf("print_timeout_secs = 1800")
        $out.TrimEnd().EndsWith('baud_rate = 9600') | Should -BeTrue
        $out | Should -Match '(?m)^client_id = "pjkeb-client"$'
    }

    It "returns the config unchanged when there are no entries" {
        $config = "[general]`nmode = `"server`"`n`n[jobs]`nmax_retries = 3`n"
        Add-DevBridgeServerSerialBridgesToConfig -Config $config -Entries @() | Should -BeExactly $config
    }
}

Describe "Merge-DevBridgeServerSerialBridgesIntoConfig (issue #69 -- preserve-branch merge)" {
    BeforeEach {
        $script:dataDir = New-TempDataDir
        $script:configPath = Join-Path $script:dataDir "config.toml"
        # Shape of the live pz-server config: CRLF, mappings at the end.
        $script:liveLike = (@(
            '[general]',
            'mode = "server"',
            '',
            '[jobs]',
            'max_retries = 3',
            '',
            '[[server.serial_bridges]]',
            'client_id = "pjkeb-client"',
            'virtual_port = "COM20"',
            'baud_rate = 9600',
            '',
            '[[server.serial_bridges]]',
            'client_id = "pjsln-client"',
            'virtual_port = "COM22"',
            'baud_rate = 9600',
            ''
        ) -join "`r`n")
    }
    AfterEach {
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "keeps the file byte-identical when every requested client_id is already mapped" {
        Set-Content -Path $script:configPath -Value $script:liveLike -NoNewline -Encoding ASCII
        $before = [System.IO.File]::ReadAllBytes($script:configPath)

        # Different port/baud requested for pjkeb -- an existing mapping must NEVER change.
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM30:19200,pjsln-client=COM22")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        @($r.Added).Count | Should -Be 0
        (@($r.Kept) -join ",") | Should -BeExactly (@("pjkeb-client", "pjsln-client") -join ",")
        @($r.Conflicts).Count | Should -Be 0
        $after = [System.IO.File]::ReadAllBytes($script:configPath)
        [System.Convert]::ToBase64String($after) | Should -BeExactly ([System.Convert]::ToBase64String($before))
    }

    It "adds a missing mapping after the last existing block, keeps the rest, CRLF + UTF-8 no BOM" {
        Set-Content -Path $script:configPath -Value $script:liveLike -NoNewline -Encoding ASCII
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20,store-new=COM24")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        (@($r.Added) -join ",") | Should -BeExactly (@("store-new") -join ",")
        (@($r.Kept) -join ",") | Should -BeExactly (@("pjkeb-client") -join ",")
        $raw = [System.IO.File]::ReadAllText($script:configPath)
        $raw.StartsWith($script:liveLike.TrimEnd()) | Should -BeTrue
        $raw.Contains("[[server.serial_bridges]]`r`nclient_id = `"store-new`"`r`nvirtual_port = `"COM24`"`r`nbaud_rate = 9600`r`n") | Should -BeTrue
        $raw.IndexOf('client_id = "store-new"') | Should -BeGreaterThan $raw.IndexOf('client_id = "pjsln-client"')
        ([regex]::Matches($raw, '\[\[server\.serial_bridges\]\]')).Count | Should -Be 3
        ($raw -replace "`r`n", "") -match "`n" | Should -BeFalse   # no bare LF introduced
        $bytes = [System.IO.File]::ReadAllBytes($script:configPath)
        ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) | Should -BeFalse
    }

    It "inserts after the last mapping (not at EOF) when another table follows it" {
        $original = "[general]`nmode = `"server`"`n`n[[server.serial_bridges]]`nclient_id = `"a`"`nvirtual_port = `"COM20`"`nbaud_rate = 9600`n`n[jobs]`nmax_retries = 3`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "b=COM22")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        (@($r.Added) -join ",") | Should -BeExactly (@("b") -join ",")
        $raw = [System.IO.File]::ReadAllText($script:configPath)
        $raw.IndexOf('client_id = "b"') | Should -BeGreaterThan $raw.IndexOf('client_id = "a"')
        $raw.IndexOf('client_id = "b"') | Should -BeLessThan $raw.IndexOf('[jobs]')
        $raw.EndsWith("[jobs]`nmax_retries = 3`n") | Should -BeTrue
    }

    It "appends at the end when the config has no mappings yet" {
        $original = "[general]`nmode = `"server`"`n`n[jobs]`nmax_retries = 3`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20,pjsln-client=COM22")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        (@($r.Added) -join ",") | Should -BeExactly (@("pjkeb-client", "pjsln-client") -join ",")
        $raw = [System.IO.File]::ReadAllText($script:configPath)
        $raw.StartsWith("[general]`nmode = `"server`"`n`n[jobs]`nmax_retries = 3`n`n[[server.serial_bridges]]") | Should -BeTrue
        $raw.EndsWith("virtual_port = `"COM22`"`nbaud_rate = 9600`n") | Should -BeTrue
    }

    It "does not add a mapping whose virtual port is already used by another client_id (conflict), file byte-identical" {
        Set-Content -Path $script:configPath -Value $script:liveLike -NoNewline -Encoding ASCII
        $before = [System.Convert]::ToBase64String([System.IO.File]::ReadAllBytes($script:configPath))
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "store-x=COM20")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        @($r.Conflicts).Count | Should -Be 1
        $r.Conflicts[0].ClientId | Should -BeExactly "store-x"
        $r.Conflicts[0].VirtualPort | Should -BeExactly "COM20"
        $r.Conflicts[0].ExistingClientId | Should -BeExactly "pjkeb-client"
        @($r.Added).Count | Should -Be 0
        [System.Convert]::ToBase64String([System.IO.File]::ReadAllBytes($script:configPath)) | Should -BeExactly $before
    }

    It "reports Added, Kept and Conflicts from ONE call and writes only the added mapping" {
        Set-Content -Path $script:configPath -Value $script:liveLike -NoNewline -Encoding ASCII
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20,store-x=COM22,store-new=COM24")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        (@($r.Added) -join ",") | Should -BeExactly "store-new"
        (@($r.Kept) -join ",") | Should -BeExactly "pjkeb-client"
        (@($r.Conflicts | ForEach-Object { "$($_.ClientId)->$($_.ExistingClientId)" }) -join ",") | Should -BeExactly "store-x->pjsln-client"
        $raw = [System.IO.File]::ReadAllText($script:configPath)
        $raw | Should -Not -Match 'store-x'
        ([regex]::Matches($raw, '\[\[server\.serial_bridges\]\]')).Count | Should -Be 3
    }

    It "treats a literal-string (single-quoted) existing mapping as present -- never appends a duplicate client_id" {
        $original = "[general]`nmode = 'server'`n`n[[server.serial_bridges]]`nclient_id = 'pjkeb-client'`nvirtual_port = 'COM20'`nbaud_rate = 9600`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM30,store-y=COM20")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        (@($r.Kept) -join ",") | Should -BeExactly "pjkeb-client"
        $r.Conflicts[0].ExistingClientId | Should -BeExactly "pjkeb-client"
        @($r.Added).Count | Should -Be 0
        [System.IO.File]::ReadAllText($script:configPath) | Should -BeExactly $original
    }

    It "refuses to merge (file untouched) when serial_bridges is declared inline under [server]" {
        $original = "[server]`nipp_port = 631`nserial_bridges = [ { client_id = `"a`", virtual_port = `"COM20`" } ]`n`n[jobs]`nmax_retries = 3`n"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "b=COM22")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries

        $r.Refused | Should -BeTrue
        @($r.Added).Count | Should -Be 0
        [System.IO.File]::ReadAllText($script:configPath) | Should -BeExactly $original
    }

    It "treats client_ids case-sensitively (Rust HashMap semantics): 'PJKEB-client' is NOT 'pjkeb-client'" {
        Set-Content -Path $script:configPath -Value $script:liveLike -NoNewline -Encoding ASCII
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "PJKEB-client=COM24")
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $entries
        (@($r.Added) -join ",") | Should -BeExactly "PJKEB-client"
        @($r.Kept).Count | Should -Be 0
    }

    It "appends after a last mapping that has no trailing newline, terminating the file with the EOL" {
        $original = "[jobs]`r`nmax_retries = 3`r`n`r`n[[server.serial_bridges]]`r`nclient_id = `"a`"`r`nvirtual_port = `"COM20`"`r`nbaud_rate = 9600"
        Set-Content -Path $script:configPath -Value $original -NoNewline -Encoding ASCII
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "b=COM22")

        (@($r.Added) -join ",") | Should -BeExactly "b"
        [System.IO.File]::ReadAllText($script:configPath) | Should -BeExactly ($original + "`r`n`r`n[[server.serial_bridges]]`r`nclient_id = `"b`"`r`nvirtual_port = `"COM22`"`r`nbaud_rate = 9600`r`n")
    }

    It "round-trips non-ASCII (UTF-8) content byte-exactly when it adds a mapping" {
        $original = "# Pekarova zena -- " + [char]0x0161 + [char]0x010D + [char]0x0165 + " ##`n[general]`nmode = `"server`"`n"
        [System.IO.File]::WriteAllText($script:configPath, $original, (New-Object System.Text.UTF8Encoding($false)))
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "a=COM20")

        (@($r.Added) -join ",") | Should -BeExactly "a"
        $expected = [System.Text.Encoding]::UTF8.GetBytes($original + "`n[[server.serial_bridges]]`nclient_id = `"a`"`nvirtual_port = `"COM20`"`nbaud_rate = 9600`n")
        [System.Convert]::ToBase64String([System.IO.File]::ReadAllBytes($script:configPath)) | Should -BeExactly ([System.Convert]::ToBase64String($expected))
    }

    It "does nothing (file byte-identical) for no entries or `$null entries" {
        Set-Content -Path $script:configPath -Value $script:liveLike -NoNewline -Encoding ASCII
        $before = [System.Convert]::ToBase64String([System.IO.File]::ReadAllBytes($script:configPath))
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries @()
        @($r.Added).Count | Should -Be 0
        $r = Merge-DevBridgeServerSerialBridgesIntoConfig -Path $script:configPath -Entries $null
        @($r.Added).Count | Should -Be 0
        [System.Convert]::ToBase64String([System.IO.File]::ReadAllBytes($script:configPath)) | Should -BeExactly $before
    }
}

Describe "Merge-DevBridgeSerialBridgeIntoConfig UTF-8 read (issue #69 review -- PS 5.1 would read BOM-less UTF-8 as ANSI)" {
    It "round-trips non-ASCII content byte-exactly when it adds [client.serial_bridge]" {
        $dir = New-TempDataDir
        try {
            $path = Join-Path $dir "config.toml"
            $original = "# " + [char]0x0161 + [char]0x010D + "`n[client]`nserver_address = `"1.2.3.4:50051`"`n`n[jobs]`nmax_retries = 3`n"
            [System.IO.File]::WriteAllText($path, $original, (New-Object System.Text.UTF8Encoding($false)))
            Merge-DevBridgeSerialBridgeIntoConfig -Path $path -SerialPort "COM4" | Should -Be "added"
            $raw = [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)
            $raw.StartsWith("# " + [char]0x0161 + [char]0x010D + "`n") | Should -BeTrue
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Get-DevBridgeCom0comMissingPortWarnings (issue #69 -- warn-only com0com check)" {
    It "warns with the exact setupc command (B = A + 1) for a port that does not exist" {
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20,store-new=COM24")
        $w = @(Get-DevBridgeCom0comMissingPortWarnings -Entries $entries -ExistingPorts @("COM1", "COM20", "COM21"))
        $w.Count | Should -Be 1
        $w[0] | Should -Match "COM24"
        $w[0] | Should -Match "store-new"
        $w[0] | Should -Match ([regex]::Escape("setupc.exe --silent install PortName=COM24,EmuBR=yes PortName=COM25,EmuBR=yes"))
    }

    It "returns no warnings when every port exists" {
        $entries = @(ConvertFrom-DevBridgeSerialBridgesSpec -Spec "pjkeb-client=COM20,pjsln-client=COM22")
        @(Get-DevBridgeCom0comMissingPortWarnings -Entries $entries -ExistingPorts @("COM20", "COM21", "COM22", "COM23")).Count | Should -Be 0
    }
}

Describe "Assert-DevBridgeSerialBaud (issue #68 review F4 -- validated before any binary swap)" {
    It "does not throw for '<value>'" -ForEach @(
        @{ value = "" }
        @{ value = $null }
        @{ value = "9600" }
        @{ value = "19200" }
        @{ value = "0" }
    ) {
        { Assert-DevBridgeSerialBaud -Value $value } | Should -Not -Throw
    }

    It "throws a descriptive error for a non-numeric value" {
        { Assert-DevBridgeSerialBaud -Value "abc" } | Should -Throw "*DEVBRIDGE_SERIAL_BAUD must be a positive integer, got 'abc'*"
    }

    It "throws for a negative number" {
        { Assert-DevBridgeSerialBaud -Value "-9600" } | Should -Throw "*DEVBRIDGE_SERIAL_BAUD must be a positive integer*"
    }

    It "throws for a value with trailing garbage" {
        { Assert-DevBridgeSerialBaud -Value "9600baud" } | Should -Throw "*DEVBRIDGE_SERIAL_BAUD must be a positive integer*"
    }
}

Describe "Get-DevBridgeVersionFromAssetName (review finding F2 -- real target semver from the installer asset filename, issue #71)" {
    It "extracts the semver from a stable-channel asset name" {
        Get-DevBridgeVersionFromAssetName -Name "DevBridge_0.8.33_x64-setup.exe" | Should -Be "0.8.33"
    }

    It "extracts the semver from a dev-channel asset name (release tag_name is the literal 'dev-latest', not a usable semver)" {
        # The dev-latest pre-release still ships an installer built from the
        # real workspace version (e.g. Cargo.toml 0.8.34), embedded in the
        # asset filename exactly like a stable release -- only the release's
        # tag_name is the non-semver literal "dev-latest". This is the whole
        # point of F2: parse the filename, never $release.tag_name.
        Get-DevBridgeVersionFromAssetName -Name "DevBridge_0.8.34_x64-setup.exe" | Should -Be "0.8.34"
    }

    It "returns `$null for a name with no embedded semver (e.g. the checksum asset)" {
        Get-DevBridgeVersionFromAssetName -Name "SHA256SUMS" | Should -BeNullOrEmpty
    }
}

Describe "Test-DevBridgeBinarySwapOk (SHA256 pre/post swap + same-version reinstall, issue #71)" {
    It "reports fresh-install OK when no prior binary existed (empty pre-hash)" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "" -PostHash "ABC123"
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "fresh-install"
    }

    It "reports updated OK when the hash changed (real swap happened)" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "AAAA" -PostHash "BBBB"
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "updated"
    }

    It "reports updated OK on a real hash change even when version info is absent" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "AAAA" -PostHash "BBBB" -InstalledVersion "" -TargetVersion ""
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "updated"
    }

    It "FAILS (Ok=false) when a real binary existed, hash is unchanged, and no version info was supplied" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "DEADBEEF" -PostHash "DEADBEEF"
        $r.Ok | Should -BeFalse
        $r.Reason | Should -Be "unchanged"
    }

    It "(a) reports OK (same-version) when the hash is unchanged but the installed version already matches the target" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "DEADBEEF" -PostHash "DEADBEEF" `
            -InstalledVersion "0.8.32" -TargetVersion "v0.8.32"
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "same-version"
    }

    It "(a) matches versions regardless of which side carries the leading 'v'" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "DEADBEEF" -PostHash "DEADBEEF" `
            -InstalledVersion "v0.8.32" -TargetVersion "0.8.32"
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "same-version"
    }

    It "(b) FAILS (Ok=false) when the hash is unchanged and the installed version differs from the target" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "DEADBEEF" -PostHash "DEADBEEF" `
            -InstalledVersion "0.8.31" -TargetVersion "v0.8.32"
        $r.Ok | Should -BeFalse
        $r.Reason | Should -Be "unchanged"
    }

    It "(c) reports updated OK on a real hash change even when installed/target versions differ" {
        $r = Test-DevBridgeBinarySwapOk -PreHash "AAAA" -PostHash "BBBB" `
            -InstalledVersion "0.8.31" -TargetVersion "v0.8.32"
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "updated"
    }

    It "computes real Get-FileHash values and detects a genuine swap end-to-end" {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbswap-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        try {
            $bin = Join-Path $dir "devbridge-service.exe"
            Set-Content -Path $bin -Value "OLD-BINARY-CONTENT" -Encoding ASCII
            $pre = (Get-FileHash $bin -Algorithm SHA256).Hash

            # Same content again => simulates NSIS silently no-oping the overwrite,
            # with the registry still showing the OLD version (a genuine failure).
            $postSame = (Get-FileHash $bin -Algorithm SHA256).Hash
            (Test-DevBridgeBinarySwapOk -PreHash $pre -PostHash $postSame `
                -InstalledVersion "0.8.31" -TargetVersion "v0.8.32").Ok | Should -BeFalse

            # Now actually replace the bytes => a real upgrade.
            Set-Content -Path $bin -Value "NEW-BINARY-CONTENT-v2" -Encoding ASCII
            $postNew = (Get-FileHash $bin -Algorithm SHA256).Hash
            $postNew | Should -Not -Be $pre
            $swap = Test-DevBridgeBinarySwapOk -PreHash $pre -PostHash $postNew `
                -InstalledVersion "0.8.31" -TargetVersion "v0.8.32"
            $swap.Ok | Should -BeTrue
            $swap.Reason | Should -Be "updated"
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Get-DevBridgeInstalledVersion (registry DisplayVersion read, issue #71 review finding F4)" {
    It "returns DisplayVersion when it is present on the FIRST (non-WOW6432Node) key" {
        Mock Get-ItemProperty {
            [pscustomobject]@{ DisplayVersion = "0.8.30" }
        } -ParameterFilter { $Path -eq "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge" }
        Mock Get-ItemProperty {
            throw "must not be reached -- the first key already answered"
        } -ParameterFilter { $Path -eq "HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge" }

        Get-DevBridgeInstalledVersion | Should -Be "0.8.30"
    }

    It "falls through to the WOW6432Node key and returns its DisplayVersion when the first key THROWS" {
        Mock Get-ItemProperty {
            throw "key not found"
        } -ParameterFilter { $Path -eq "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge" }
        Mock Get-ItemProperty {
            [pscustomobject]@{ DisplayVersion = "0.8.32" }
        } -ParameterFilter { $Path -eq "HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge" }

        Get-DevBridgeInstalledVersion | Should -Be "0.8.32"
    }

    It "falls through to the WOW6432Node key and returns its DisplayVersion when the first key RETURNS `$null" {
        Mock Get-ItemProperty {
            $null
        } -ParameterFilter { $Path -eq "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge" }
        Mock Get-ItemProperty {
            [pscustomobject]@{ DisplayVersion = "0.8.32" }
        } -ParameterFilter { $Path -eq "HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\DevBridge" }

        Get-DevBridgeInstalledVersion | Should -Be "0.8.32"
    }

    It "returns `$null when BOTH keys throw" {
        Mock Get-ItemProperty { throw "key not found" }

        Get-DevBridgeInstalledVersion | Should -BeNullOrEmpty
    }

    It "does not throw and returns a string or null on a machine with no DevBridge Uninstall key (real, unmocked registry read)" {
        # windows-latest CI runners never have DevBridge installed, so the
        # registry key is absent -- this proves the function fails soft
        # (returns $null) rather than throwing and aborting the installer.
        { $script:installedVersionResult = Get-DevBridgeInstalledVersion } | Should -Not -Throw
        ($null -eq $script:installedVersionResult -or $script:installedVersionResult -is [string]) | Should -BeTrue
    }
}

Describe "Restore-DevBridgeService (best-effort restart on a post-stop failure path, issue #71 review finding F3)" {
    It "starts the DevBridgeService scheduled task" {
        Mock Start-ScheduledTask {}
        Mock Get-ScheduledTask { [pscustomobject]@{ TaskName = "DevBridgeService"; State = "Running" } }
        Mock Get-Process {}
        Restore-DevBridgeService
        Should -Invoke Start-ScheduledTask -Times 1 -Exactly -ParameterFilter { $TaskName -eq "DevBridgeService" }
    }

    It "logs the restart line (no warning) when the scheduled task reports State=Running after starting it" {
        Mock Start-ScheduledTask {}
        Mock Get-ScheduledTask { [pscustomobject]@{ TaskName = "DevBridgeService"; State = "Running" } }
        Mock Get-Process {}

        $warnings = @(Restore-DevBridgeService 3>&1) | Where-Object { $_ -is [System.Management.Automation.WarningRecord] }
        $warnings.Count | Should -Be 0
    }

    It "logs the restart line (no warning) when the task state is unreadable but devbridge-service.exe is actually running" {
        Mock Start-ScheduledTask {}
        Mock Get-ScheduledTask { $null }
        Mock Get-Process { [pscustomobject]@{ Name = "devbridge-service"; Id = 4242 } }

        $warnings = @(Restore-DevBridgeService 3>&1) | Where-Object { $_ -is [System.Management.Automation.WarningRecord] }
        $warnings.Count | Should -Be 0
    }

    It "does not throw when the scheduled task no longer exists (a non-terminating error, suppressed by -ErrorAction SilentlyContinue)" {
        Mock Start-ScheduledTask { Write-Error "No MSFT_ScheduledTask objects found with property 'TaskName' equal to 'DevBridgeService'" }
        Mock Get-ScheduledTask { $null }
        Mock Get-Process { $null }
        { Restore-DevBridgeService } | Should -Not -Throw
    }

    It "warns (instead of falsely claiming success) when the scheduled task no longer exists and no process is running" {
        Mock Start-ScheduledTask { Write-Error "No MSFT_ScheduledTask objects found with property 'TaskName' equal to 'DevBridgeService'" }
        Mock Get-ScheduledTask { $null }
        Mock Get-Process { $null }

        $warnings = @(Restore-DevBridgeService 3>&1) | Where-Object { $_ -is [System.Management.Automation.WarningRecord] }
        $warnings.Count | Should -Be 1
        $warnings[0].Message | Should -Match "Could not restart DevBridgeService"
    }

    It "warns when the scheduled task exists but is NOT in the Running state and no process is found" {
        Mock Start-ScheduledTask {}
        Mock Get-ScheduledTask { [pscustomobject]@{ TaskName = "DevBridgeService"; State = "Ready" } }
        Mock Get-Process { $null }

        $warnings = @(Restore-DevBridgeService 3>&1) | Where-Object { $_ -is [System.Management.Automation.WarningRecord] }
        $warnings.Count | Should -Be 1
        $warnings[0].Message | Should -Match "Could not restart DevBridgeService"
    }
}

Describe "install.ps1 script ordering (review finding F1 -- installed version captured BEFORE NSIS runs, issue #71)" {
    BeforeAll {
        $script:installScriptContent = Get-Content -Path (Join-Path $installerDir "install.ps1") -Raw
    }

    It "assigns `$installedVersion = Get-DevBridgeInstalledVersion BEFORE Start-Process runs the NSIS installer" {
        # Tauri's NSIS writes DisplayVersion=<target> to the Uninstall
        # registry key even when it silently no-oped the binary swap because
        # the file was locked -- reading the registry AFTER NSIS runs would
        # always read the TARGET version, making the "hash unchanged"
        # failure branch unreachable for a genuinely failed swap. The
        # capture must happen BEFORE Start-Process runs the installer.
        $versionCaptureIdx = $script:installScriptContent.IndexOf('$installedVersion = Get-DevBridgeInstalledVersion')
        $nsisRunIdx = $script:installScriptContent.IndexOf('Start-Process -FilePath $installerPath')

        $versionCaptureIdx | Should -BeGreaterThan -1
        $nsisRunIdx | Should -BeGreaterThan -1
        $versionCaptureIdx | Should -BeLessThan $nsisRunIdx
    }

    It "captures Get-DevBridgeInstalledVersion exactly ONCE (no leftover post-NSIS re-read)" {
        # Exactly 2 occurrences of the bare function name in the whole file:
        # its own `function Get-DevBridgeInstalledVersion {` definition, and
        # the single pre-NSIS call site asserted above. A 3rd occurrence
        # would mean a stale post-install re-read crept back in.
        $matches = [regex]::Matches($script:installScriptContent, [regex]::Escape("Get-DevBridgeInstalledVersion"))
        $matches.Count | Should -Be 2
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
        $script:postInstallText | Should -Not -Match ([regex]::Escape('VisualStudio\14.0\VC\Runtimes'))
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
