# Pester v5 tests for the installer upgrade-hardening helpers.
#
# The functions under test are defined INLINE inside the production scripts
# installer/install.ps1 and installer/post-install.ps1 (they cannot be a shared
# dot-sourced lib -- install.ps1 runs via irm|iex with no files on disk, and
# post-install.ps1 is a relocated lone Tauri resource). To test the REAL code
# with zero drift, BeforeAll parses each script with the PowerShell AST and
# extracts the named function bodies without executing the script.
#
# Covered: config preserve-vs-rewrite branch selection, permissive force-rewrite
# flag parse, snapshot creation, prune-to-5, the binary-swap file-unlock poll,
# and SHA256 pre/post swap verification.
#
# Every test runs against a throwaway temp dir; nothing touches a real
# C:\ProgramData\DevBridge. CI runs this via Invoke-Pester on windows-latest.

BeforeAll {
    # We test the ACTUAL production code. install.ps1 (run via irm|iex) and
    # post-install.ps1 (a relocated Tauri resource) cannot dot-source a sibling
    # lib at runtime, so the helper functions are defined INLINE in each script.
    # To avoid testing a divergent copy, we parse each real script with the
    # PowerShell AST, pull out the named function definitions, and load THEM into
    # this scope. If a future edit changes the inline production function, these
    # tests exercise that exact change.
    $installerDir = Split-Path -Parent $PSScriptRoot

    # Parse a real installer script and return @{ name = function-source-text }
    # for the requested functions WITHOUT executing the script body (which would
    # trigger Stop-Service / scheduled tasks / network probes). The caller
    # dot-sources the returned text into the test scope so the production
    # function bodies become callable here.
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

    # Config helpers live in post-install.ps1; binary-swap helpers in install.ps1.
    $functionSources = [ordered]@{}
    (Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "post-install.ps1") `
        -Names @("Test-DevBridgeForceRewrite", "Get-DevBridgeConfigAction", "New-DevBridgeConfigSnapshot")).GetEnumerator() |
        ForEach-Object { $functionSources[$_.Key] = $_.Value }
    (Get-FunctionSourceFromScript -ScriptPath (Join-Path $installerDir "install.ps1") `
        -Names @("Wait-DevBridgeBinaryUnlocked", "Test-DevBridgeBinarySwap")).GetEnumerator() |
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

Describe "Test-DevBridgeBinarySwap (SHA256 pre/post swap verification)" {
    It "reports fresh-install OK when no prior binary existed (empty pre-hash)" {
        $r = Test-DevBridgeBinarySwap -PreHash "" -PostHash "ABC123"
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "fresh-install"
    }

    It "reports updated OK when the hash changed (real swap happened)" {
        $r = Test-DevBridgeBinarySwap -PreHash "AAAA" -PostHash "BBBB"
        $r.Ok | Should -BeTrue
        $r.Reason | Should -Be "updated"
    }

    It "FAILS (Ok=false) when a real binary existed but the hash is unchanged (NSIS silent no-op)" {
        $r = Test-DevBridgeBinarySwap -PreHash "DEADBEEF" -PostHash "DEADBEEF"
        $r.Ok | Should -BeFalse
        $r.Reason | Should -Be "unchanged"
    }

    It "computes real Get-FileHash values and detects a genuine swap end-to-end" {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbswap-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        try {
            $bin = Join-Path $dir "devbridge-service.exe"
            Set-Content -Path $bin -Value "OLD-BINARY-CONTENT" -Encoding ASCII
            $pre = (Get-FileHash $bin -Algorithm SHA256).Hash

            # Same content again => simulates NSIS silently no-oping the overwrite.
            $postSame = (Get-FileHash $bin -Algorithm SHA256).Hash
            (Test-DevBridgeBinarySwap -PreHash $pre -PostHash $postSame).Ok | Should -BeFalse

            # Now actually replace the bytes => a real upgrade.
            Set-Content -Path $bin -Value "NEW-BINARY-CONTENT-v2" -Encoding ASCII
            $postNew = (Get-FileHash $bin -Algorithm SHA256).Hash
            $postNew | Should -Not -Be $pre
            $swap = Test-DevBridgeBinarySwap -PreHash $pre -PostHash $postNew
            $swap.Ok | Should -BeTrue
            $swap.Reason | Should -Be "updated"
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}
