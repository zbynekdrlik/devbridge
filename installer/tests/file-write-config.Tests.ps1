# Pester v5 tests for the file.write folder of the device-job source
# (issue #130): DEVBRIDGE_POKLADNA_FILE_WRITE_DIR -> [client.pokladna.file_write],
# the SERVER-mode [client.pokladna] (the Codex server's bridge: fiscal = false,
# file.* jobs only), the preserved-config merge, and post-install.ps1 itself
# as a real child process (-ValidateOnly: validated before any change, the
# folder never created, the key never printed).
#
# Tests exercise the REAL functions, AST-extracted from the production scripts
# with the shared deploy/lib/Get-FunctionSourceFromScript.ps1. Pure ASCII
# (Windows PowerShell 5.1 runs it too). Everything runs in throwaway temp dirs.

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    $libPath = Join-Path $installerDir "DevBridgeInstallerLib.ps1"
    $lanLibPath = Join-Path $installerDir "DevBridgeLanApiLib.ps1"
    $postInstallPath = Join-Path $installerDir "post-install.ps1"
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    $lanSources = Get-FunctionSourceFromScript -ScriptPath $lanLibPath -Names @(
        "Resolve-DevBridgePokladnaFiscal", "Get-DevBridgeFileWriteDirProblems", "Get-DevBridgeFileWriteToml",
        "Get-DevBridgePokladnaRequest", "Get-DevBridgeExistingFileWrite", "Get-DevBridgeFileWriteVolumeProblems",
        "Initialize-DevBridgeFileWriteStaging")
    foreach ($src in $lanSources.Values) { . ([scriptblock]::Create($src)) }
    $sources = Get-FunctionSourceFromScript -ScriptPath $libPath -Names @(
        "ConvertTo-DevBridgeTomlString", "Get-DevBridgePokladnaConfigProblems", "Get-DevBridgePokladnaToml",
        "Merge-DevBridgeTomlTableIntoConfig", "Resolve-DevBridgePokladnaTestExecutor",
        "Resolve-DevBridgePokladnaStickyBool", "Get-DevBridgeTomlTableText", "Get-DevBridgeTomlBoolValue",
        "Get-DevBridgeTomlStringValue")
    foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }

    $secret = "server-bridge-k3y-never-printed-130"

    function New-TempDir {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("dbfile-" + [guid]::NewGuid().ToString("N"))
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

    # A preserved SERVER config (pz-server's shape: a [client] table too).
    $serverConfig = "[general]`nmode = `"server`"`n`n[server]`nipp_port = 631`n`n[client]`nserver_address = `"127.0.0.1:50051`"`ntarget_printer = `"unused`"`n`n[jobs]`nmax_retries = 3`n"
}

Describe "Get-DevBridgeFileWriteDirProblems (the folder, checked before any change)" {
    It "accepts an existing absolute local folder" {
        $dir = New-TempDir
        try {
            if ($dir -match '^[A-Za-z]:[\\/]') {
                (Get-DevBridgeFileWriteDirProblems -Dir $dir).Count | Should -Be 0
            } else {
                # a Linux pwsh temp dir has no drive letter: the service's rule is Windows-shaped
                (Get-DevBridgeFileWriteDirProblems -Dir $dir)[0] | Should -Match "must be an absolute path"
            }
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }

    It "refuses relative, share, traversal, quoted and missing folders" {
        $cases = @(
            @("data\in", "must be an absolute path"),
            @("", "must be an absolute path"),
            @("C:in", "must be an absolute path"),
            @("\\nas\share\in", "network share"),
            @("//nas/share", "network share"),
            @("C:\ORION\..\Windows", "'..' component"),
            @("C:\in`"x", "quote or a control character"),
            @("C:\in`n", "quote or a control character"),
            @("Z:\devbridge-never-exists-130\in", "does not exist")
        )
        foreach ($case in $cases) {
            $p = Get-DevBridgeFileWriteDirProblems -Dir $case[0]
            $p.Count | Should -Be 1 -Because "dir '$($case[0])'"
            $p[0] | Should -Match ([regex]::Escape($case[1])) -Because "dir '$($case[0])'"
        }
    }
}

Describe "Get-DevBridgeFileWriteToml ([client.pokladna.file_write])" {
    It "writes the folder as an escaped TOML string" {
        Get-DevBridgeFileWriteToml -Dir "C:\ORION\COMMUNICATOR\data\in" |
            Should -BeExactly "[client.pokladna.file_write]`ntarget_dir = `"C:\\ORION\\COMMUNICATOR\\data\\in`""
    }
}

Describe "Get-DevBridgePokladnaRequest (client + server mode, issue #130)" {
    BeforeAll {
        Mock Get-DevBridgeFileWriteDirProblems { return ,@() }
        $dataDir = "C:\ProgramData\DevBridge"
    }

    It "builds the server's block: fiscal = false and the folder, no test mode" {
        $r = Get-DevBridgePokladnaRequest -Mode server -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret -FileWriteDir "C:\ORION\COMMUNICATOR\data\in"
        $r.Problems.Count | Should -Be 0
        $r.Toml | Should -BeExactly "[client.pokladna]`nenabled = true`nurl = `"https://erp.example.sk`"`napi_key = `"$secret`"`nfiscal = false"
        $r.FileWriteToml | Should -Match '^\[client\.pokladna\.file_write\]'
        ($r.Notes -join "|") | Should -Match "file.write folder: 'C:\\ORION\\COMMUNICATOR\\data\\in'"
        ($r.Notes -join "|") | Should -Match "server mode"
        ($r.Notes -join "|") | Should -Not -Match ([regex]::Escape($secret))
    }

    It "refuses a server without a folder, with the test mode or with fiscal = true" {
        $none = Get-DevBridgePokladnaRequest -Mode server -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret
        ($none.Problems -join "|") | Should -Match "runs file.\* jobs only: give DEVBRIDGE_POKLADNA_FILE_WRITE_DIR"
        $none.Toml | Should -BeExactly ""
        $store = Get-DevBridgePokladnaRequest -Mode server -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret `
            -FileWriteDir "C:\in" -TestExecutor "true" -Fiscal "True"
        $store.Problems.Count | Should -Be 2
        ($store.Problems -join "|") | Should -Match "DEVBRIDGE_POKLADNA_TEST_EXECUTOR is a store setting"
        ($store.Problems -join "|") | Should -Match "DEVBRIDGE_POKLADNA_FISCAL=true is a store setting"
        $explicitOff = Get-DevBridgePokladnaRequest -Mode server -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret -FileWriteDir "C:\in" -Fiscal "false"
        $explicitOff.Problems.Count | Should -Be 0
    }

    It "a key rotation or a forced rewrite without the folder KEEPS the existing one (both modes)" {
        $existing = $serverConfig.Replace("[jobs]", "[client.pokladna.file_write]`ntarget_dir = `"C:\\ORION\\in`"`n`n[jobs]")
        foreach ($mode in @("server", "client")) {
            $r = Get-DevBridgePokladnaRequest -Mode $mode -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey "rotated-$secret" -Existing $existing
            $r.Problems.Count | Should -Be 0 -Because $mode
            $r.FileWriteDir | Should -BeExactly "C:\ORION\in" -Because $mode
            $r.FileWriteToml | Should -BeExactly "[client.pokladna.file_write]`ntarget_dir = `"C:\\ORION\\in`"" -Because "re-written, so a forced rewrite keeps it ($mode)"
        }
        $none = Get-DevBridgePokladnaRequest -Mode client -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret -Existing $serverConfig
        $none.FileWriteToml | Should -BeExactly ""
        $none.FileWriteDir | Should -BeExactly ""
    }

    It "refuses a folder on another drive than the data dir (the hard link needs one volume)" {
        $r = Get-DevBridgePokladnaRequest -Mode server -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret -FileWriteDir "D:\in"
        ($r.Problems -join "|") | Should -Match "must be on the drive of the DevBridge data dir"
        $r.Toml | Should -BeExactly ""
        (Get-DevBridgeFileWriteVolumeProblems -Dir "c:\ORION\in" -DataDir "C:\ProgramData\DevBridge").Count | Should -Be 0
        (Get-DevBridgeFileWriteVolumeProblems -Dir "D:\in" -DataDir "C:\ProgramData\DevBridge").Count | Should -Be 1
    }

    It "a store may get a folder too, with its own test mode / fiscal kept" {
        $r = Get-DevBridgePokladnaRequest -Mode client -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret -FileWriteDir "C:\in"
        $r.Problems.Count | Should -Be 0
        $r.Toml | Should -Not -Match 'fiscal'
        $r.FileWriteToml | Should -Match 'target_dir = "C:\\\\in"'
    }

    It "refuses a folder without URL and key, in both modes" {
        foreach ($mode in @("client", "server")) {
            $r = Get-DevBridgePokladnaRequest -Mode $mode -DataDir $dataDir -FileWriteDir "C:\in"
            ($r.Problems -join "|") | Should -Match "FILE_WRITE_DIR needs DEVBRIDGE_POKLADNA_URL" -Because $mode
        }
    }

    It "is silent when nothing pokladna is asked for" {
        $r = Get-DevBridgePokladnaRequest -Mode server
        $r.Problems.Count | Should -Be 0
        $r.Toml | Should -BeExactly ""
        $r.Warnings.Count | Should -Be 0
    }

    It "reports the folder problem itself (real check)" {
        Mock Get-DevBridgeFileWriteDirProblems { return ,@("DEVBRIDGE_POKLADNA_FILE_WRITE_DIR 'x' must be an absolute path") }
        $r = Get-DevBridgePokladnaRequest -Mode server -DataDir $dataDir -Url "https://erp.example.sk" -ApiKey $secret -FileWriteDir "x"
        ($r.Problems -join "|") | Should -Match "must be an absolute path"
        $r.Toml | Should -BeExactly ""
    }
}

Describe "Merge of [client.pokladna] + [client.pokladna.file_write] into a preserved server config" {
    It "adds both blocks, and a later key rotation keeps the folder" {
        $dir = New-TempDir
        try {
            $cfg = Join-Path $dir "config.toml"
            [System.IO.File]::WriteAllText($cfg, $serverConfig)
            $block = Get-DevBridgePokladnaToml -Url "https://erp.example.sk" -ApiKey "k1" -Fiscal $false
            Merge-DevBridgeTomlTableIntoConfig -Path $cfg -Table "client.pokladna" -Block $block | Should -Be "added"
            $folder = Get-DevBridgeFileWriteToml -Dir "C:\ORION\COMMUNICATOR\data\in"
            Merge-DevBridgeTomlTableIntoConfig -Path $cfg -Table "client.pokladna.file_write" -Block $folder | Should -Be "added"
            $rotated = Get-DevBridgePokladnaToml -Url "https://erp.example.sk" -ApiKey "k2" -Fiscal $false
            Merge-DevBridgeTomlTableIntoConfig -Path $cfg -Table "client.pokladna" -Block $rotated | Should -Be "replaced"
            $text = [System.IO.File]::ReadAllText($cfg)
            $text | Should -Match 'api_key = "k2"'
            $text | Should -Not -Match 'api_key = "k1"'
            $text | Should -Match '(?m)^\[client\.pokladna\.file_write\]\r?\ntarget_dir = "C:\\\\ORION\\\\COMMUNICATOR\\\\data\\\\in"'
            ([regex]::Matches($text, '(?m)^\[client\.pokladna\.file_write\]')).Count | Should -Be 1
            $text | Should -Match '(?m)^\[general\]\r?\nmode = "server"'
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "Initialize-DevBridgeFileWriteStaging (its own ACL, never a link, issue #130)" {
    It "creates the folder: owner Administrators, protected, SYSTEM + Administrators full, Users Modify on files only" {
        $dir = New-TempDir
        try {
            $path = Initialize-DevBridgeFileWriteStaging -DataDir $dir
            $path | Should -BeExactly (Join-Path $dir "file-write-staging")
            $acl = Get-Acl -LiteralPath $path
            $acl.AreAccessRulesProtected | Should -BeTrue
            $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value | Should -BeExactly "S-1-5-32-544"
            $rules = @($acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier]))
            $rules.Count | Should -Be 3
            foreach ($sid in @("S-1-5-18", "S-1-5-32-544")) {
                $rule = @($rules | Where-Object { $_.IdentityReference.Value -eq $sid })[0]
                $rule.FileSystemRights.ToString() | Should -Match "FullControl"
                $rule.InheritanceFlags.ToString() | Should -BeExactly "ContainerInherit, ObjectInherit"
            }
            $users = @($rules | Where-Object { $_.IdentityReference.Value -eq "S-1-5-32-545" })[0]
            $users.PropagationFlags.ToString() | Should -BeExactly "InheritOnly"
            $users.InheritanceFlags.ToString() | Should -BeExactly "ObjectInherit"
            # idempotent on an existing folder
            Initialize-DevBridgeFileWriteStaging -DataDir $dir | Should -BeExactly $path
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }

    It "replaces a planted junction (the LINK only - its target untouched)" {
        $dir = New-TempDir
        $elsewhere = New-TempDir
        try {
            [System.IO.File]::WriteAllText((Join-Path $elsewhere "keep.txt"), "precious")
            New-Item -ItemType Junction -Path (Join-Path $dir "file-write-staging") -Target $elsewhere | Out-Null
            $path = Initialize-DevBridgeFileWriteStaging -DataDir $dir
            ((Get-Item -LiteralPath $path -Force).Attributes -band [System.IO.FileAttributes]::ReparsePoint) | Should -Be 0
            [System.IO.File]::ReadAllText((Join-Path $elsewhere "keep.txt")) | Should -BeExactly "precious"
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
            Remove-Item -Recurse -Force $elsewhere -ErrorAction SilentlyContinue
        }
    }

    It "refuses a FILE where the folder must be" {
        $dir = New-TempDir
        try {
            [System.IO.File]::WriteAllText((Join-Path $dir "file-write-staging"), "x")
            { Initialize-DevBridgeFileWriteStaging -DataDir $dir } | Should -Throw "*is not a folder*"
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "post-install.ps1 server mode (real child process, -ValidateOnly, issue #130)" {
    BeforeEach {
        $script:workDir = New-TempDir
        $script:dataDir = New-TempDir
        Copy-Item $postInstallPath (Join-Path $script:workDir "post-install.ps1")
        Copy-Item $libPath (Join-Path $script:workDir "DevBridgeInstallerLib.ps1")
        Copy-Item $lanLibPath (Join-Path $script:workDir "DevBridgeLanApiLib.ps1")
        $env:DEVBRIDGE_POKLADNA_API_KEY = $secret
    }
    AfterEach {
        Remove-Item Env:\DEVBRIDGE_POKLADNA_API_KEY -ErrorAction SilentlyContinue
        Remove-Item Env:\DEVBRIDGE_POKLADNA_FILE_WRITE_DIR -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:workDir -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $script:dataDir -ErrorAction SilentlyContinue
    }

    It "refuses a server bridge without a folder before any change, key never printed" {
        $cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($cfg, $serverConfig)
        $hashBefore = (Get-FileHash -LiteralPath $cfg).Hash
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -PokladnaUrl https://erp.example.sk -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        ($r.StdOut + $r.StdErr) | Should -Match "give DEVBRIDGE_POKLADNA_FILE_WRITE_DIR"
        ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($secret))
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        (Get-FileHash -LiteralPath $cfg).Hash | Should -BeExactly $hashBefore
    }

    It "refuses a folder that does not exist and never creates it" {
        $missing = Join-Path $script:dataDir "no-such-in"
        $env:DEVBRIDGE_POKLADNA_FILE_WRITE_DIR = $missing
        $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
            -Arguments "-ValidateOnly -Mode server -PokladnaUrl https://erp.example.sk -DataDir `"$($script:dataDir)`""
        $r.ExitCode | Should -Be 1
        $r.StdOut | Should -Not -Match "VALIDATE-OK"
        Test-Path -LiteralPath $missing | Should -BeFalse
    }

    It "accepts the server bridge with an existing folder, changes nothing, prints no key" {
        $cfg = Join-Path $script:dataDir "config.toml"
        [System.IO.File]::WriteAllText($cfg, $serverConfig)
        $hashBefore = (Get-FileHash -LiteralPath $cfg).Hash
        $folder = New-TempDir
        try {
            $env:DEVBRIDGE_POKLADNA_FILE_WRITE_DIR = $folder
            $r = Invoke-ChildScript -ScriptPath (Join-Path $script:workDir "post-install.ps1") `
                -Arguments "-ValidateOnly -Mode server -PokladnaUrl https://erp.example.sk -DataDir `"$($script:dataDir)`""
            if ($folder -match '^[A-Za-z]:[\\/]') {
                $r.ExitCode | Should -Be 0 -Because "stdout: $($r.StdOut) stderr: $($r.StdErr)"
                $r.StdOut | Should -Match "VALIDATE-OK"
                $r.StdOut | Should -Match "Odoo device-job source \(pokladna\) requested: https://erp.example.sk"
                $r.StdOut | Should -Match "server mode"
                @(Get-ChildItem -LiteralPath $folder -Force).Count | Should -Be 0 -Because "nothing is written into the folder"
            } else {
                # Linux pwsh: no drive letter, refused like the service would
                ($r.StdOut + $r.StdErr) | Should -Match "must be an absolute path"
            }
            ($r.StdOut + $r.StdErr) | Should -Not -Match ([regex]::Escape($secret))
            (Get-FileHash -LiteralPath $cfg).Hash | Should -BeExactly $hashBefore
        } finally {
            Remove-Item -Recurse -Force $folder -ErrorAction SilentlyContinue
        }
    }
}
