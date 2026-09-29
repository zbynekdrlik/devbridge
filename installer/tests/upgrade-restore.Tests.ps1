# Pester v5 regression tests for issue #93: the 0.8.40 -> 0.8.41 auto-update
# left DevBridge STOPPED on 6 stores.
#
#   1. install.ps1 never stopped the tray app (devbridge-app.exe, one per user
#      session) before the Tauri NSIS installer, which aborts with exit code 2
#      in silent mode while the app runs.
#   2. Every failure branch after the service stop did `Write-Error` BEFORE
#      `Restore-DevBridgeService`. install.ps1 sets $ErrorActionPreference =
#      "Stop", so Write-Error is terminating and the restore never ran.
#   3. autoupdate.ps1's catch only logged "previous version remains installed"
#      and left the stopped service as it was.
#
# These tests run the REAL script bodies (install.ps1 / autoupdate.ps1) in
# process with every external effect mocked: GitHub, the download, NSIS
# (Start-Process), the scheduled task, processes, file hashes and the
# registry. Nothing touches a real DevBridge install, service or network.
# Pester only mocks commands that exist, so the install.ps1 helpers a test
# mocks are first defined here (AST-extracted, like the other suites); the
# mock then wins over the script's own definition. This file is pure ASCII
# (Windows PowerShell 5.1 runs it too).

BeforeAll {
    $installerDir = Split-Path -Parent $PSScriptRoot
    $script:installScript = Join-Path $installerDir "install.ps1"
    $script:autoUpdateScript = Join-Path $installerDir "autoupdate.ps1"
    . (Join-Path (Split-Path -Parent $installerDir) "deploy/lib/Get-FunctionSourceFromScript.ps1")

    # install.ps1 helpers: the ones the script-level tests mock, plus the
    # guarded-upgrade functions the unit tests below call directly.
    $installFns = Get-FunctionSourceFromScript -ScriptPath $script:installScript `
        -Names @("Wait-DevBridgeBinaryUnlocked", "Get-DevBridgeInstalledVersion", "Get-DevBridgeMissingVcRuntimeDlls",
            "Test-DevBridgeBinarySwapOk", "Restore-DevBridgeService", "Stop-DevBridgeTrayApps",
            "Invoke-DevBridgePostInstallScript", "Install-DevBridgePackage")
    foreach ($src in $installFns.Values) { . ([scriptblock]::Create($src)) }
    $autoUpdateFns = Get-FunctionSourceFromScript -ScriptPath $script:autoUpdateScript `
        -Names @("Start-DevBridgeServiceIfStopped")
    foreach ($src in $autoUpdateFns.Values) { . ([scriptblock]::Create($src)) }

    # Mock bodies run in the scope of the script under test, where $script:
    # is THAT script's scope, so all mock state lives in one global table
    # (removed in AfterAll).
    $global:DB93 = @{ installerName = "DevBridge_0.8.42_x64-setup.exe" }

    # Snapshot + clear every DEVBRIDGE_* env var so the scripts under test see
    # a clean environment (an upgrade with no operator overrides).
    function Save-DevBridgeEnv {
        $saved = @{}
        Get-ChildItem Env: | Where-Object { $_.Name -like 'DEVBRIDGE_*' } | ForEach-Object {
            $saved[$_.Name] = $_.Value
            Remove-Item -Path ("Env:" + $_.Name)
        }
        return $saved
    }
    function Restore-DevBridgeEnv {
        param([hashtable]$Saved)
        Get-ChildItem Env: | Where-Object { $_.Name -like 'DEVBRIDGE_*' } | ForEach-Object {
            Remove-Item -Path ("Env:" + $_.Name)
        }
        foreach ($k in $Saved.Keys) { Set-Item -Path ("Env:" + $k) -Value $Saved[$k] }
    }

    # Mocks for one install.ps1 run. Every effect is recorded in
    # $global:DB93.calls in call order ("<Command>:<detail>").
    #   -NsisExitCode     what the NSIS installer returns
    #   -Unlocked         what the binary-unlock poll returns
    #   -BinaryExists     whether devbridge-service.exe is on disk
    #   -Hashes           SHA256 values returned in order (pre-, post-install)
    #   -Trays            tray processes running before the upgrade
    function Set-InstallMocks {
        param(
            [int]$NsisExitCode = 0,
            [bool]$Unlocked = $true,
            [bool]$BinaryExists = $false,
            [string[]]$Hashes = @(),
            [object[]]$Trays = @(),
            # $null = no post-install.ps1 on disk; a number = it exists and
            # exits with that code.
            $PostInstallExitCode = $null
        )
        $global:DB93.calls = New-Object System.Collections.ArrayList
        $global:DB93.nsisExitCode = $NsisExitCode
        $global:DB93.unlocked = $Unlocked
        $global:DB93.binaryExists = $BinaryExists
        $global:DB93.hashQueue = New-Object System.Collections.Queue
        foreach ($h in $Hashes) { $global:DB93.hashQueue.Enqueue($h) }
        $global:DB93.trays = New-Object System.Collections.ArrayList
        foreach ($t in $Trays) { [void]$global:DB93.trays.Add($t) }
        $global:DB93.taskState = "Running"
        $global:DB93.serviceUp = $true
        $global:DB93.postInstallExitCode = $PostInstallExitCode
        $global:DB93.postInstallArgs = $null

        Mock Invoke-RestMethod {
            [pscustomobject]@{
                tag_name = "v0.8.42"
                assets   = @([pscustomobject]@{
                        name                 = $global:DB93.installerName
                        browser_download_url = "https://example.invalid/$($global:DB93.installerName)"
                        updated_at           = "2026-09-29T00:00:00Z"
                    })
            }
        }
        Mock Invoke-WebRequest { [void]$global:DB93.calls.Add("Invoke-WebRequest:$Uri") }
        Mock Get-DevBridgeMissingVcRuntimeDlls { @() }
        Mock Get-DevBridgeInstalledVersion { "0.8.41" }
        Mock Wait-DevBridgeBinaryUnlocked { [void]$global:DB93.calls.Add("Wait-DevBridgeBinaryUnlocked"); $global:DB93.unlocked }
        Mock Test-Path { $global:DB93.binaryExists } -ParameterFilter { "$Path" -like '*devbridge-service.exe' }
        Mock Test-Path { $null -ne $global:DB93.postInstallExitCode } -ParameterFilter { "$Path" -like '*post-install.ps1' }
        Mock Invoke-DevBridgePostInstallScript {
            [void]$global:DB93.calls.Add("Invoke-DevBridgePostInstallScript:$(Split-Path -Leaf $ScriptPath)")
            $global:DB93.postInstallArgs = @($Arguments)
            $global:DB93.postInstallExitCode
        }
        Mock Test-Path { $false } -ParameterFilter { "$Path" -like '*config.toml' }
        Mock Get-FileHash {
            [pscustomobject]@{ Hash = [string]$global:DB93.hashQueue.Dequeue() }
        } -ParameterFilter { "$Path" -like '*devbridge-service.exe' }
        Mock Get-ScheduledTask { [pscustomobject]@{ TaskName = "DevBridgeService"; State = $global:DB93.taskState } }
        Mock Stop-ScheduledTask {
            [void]$global:DB93.calls.Add("Stop-ScheduledTask:$TaskName")
            $global:DB93.taskState = "Ready"
            $global:DB93.serviceUp = $false
        }
        Mock Start-ScheduledTask {
            [void]$global:DB93.calls.Add("Start-ScheduledTask:$TaskName")
            $global:DB93.taskState = "Running"
            $global:DB93.serviceUp = $true
        }
        Mock Get-Process { @($global:DB93.trays) } -ParameterFilter { @($Name) -contains 'devbridge-app' }
        Mock Get-Process {
            if ($global:DB93.serviceUp) { [pscustomobject]@{ Name = "devbridge-service"; Id = 4242; SessionId = 0 } }
        } -ParameterFilter { @($Name) -contains 'devbridge-service' }
        Mock Get-Process { }
        Mock Stop-Process {
            $target = if ($Id) { $Id } else { @($InputObject | ForEach-Object { $_.Id }) }
            foreach ($i in @($target)) {
                [void]$global:DB93.calls.Add("Stop-Process:$i")
                $hit = @($global:DB93.trays | Where-Object { $_.Id -eq $i })
                foreach ($h in $hit) { $global:DB93.trays.Remove($h) }
                if ($i -eq 4242) { $global:DB93.serviceUp = $false }
            }
        }
        Mock Start-Process {
            [void]$global:DB93.calls.Add("Start-Process:$(Split-Path -Leaf $FilePath)")
            [pscustomobject]@{ ExitCode = $global:DB93.nsisExitCode }
        }
        Mock Start-Sleep { }
    }

    function Get-CallIndex {
        param([string]$Like)
        for ($i = 0; $i -lt $global:DB93.calls.Count; $i++) {
            if ($global:DB93.calls[$i] -like $Like) { return $i }
        }
        return -1
    }
}

AfterAll {
    Remove-Variable -Name DB93 -Scope Global -ErrorAction SilentlyContinue
}

Describe "install.ps1 restores DevBridgeService on every failure after the stop (issue #93)" {
    BeforeEach { $script:savedEnv = Save-DevBridgeEnv }
    AfterEach { Restore-DevBridgeEnv -Saved $script:savedEnv }

    It "restarts the service when NSIS exits 2 (tray app running) even with ErrorActionPreference Stop" {
        Set-InstallMocks -NsisExitCode 2
        $ErrorActionPreference = "Stop"

        { & $script:installScript 6>$null } | Should -Throw "*Installer exited with code 2*"

        Should -Invoke Start-ScheduledTask -Times 1 -Exactly -ParameterFilter { $TaskName -eq "DevBridgeService" }
        (Get-CallIndex "Start-ScheduledTask:DevBridgeService") | Should -BeGreaterThan (Get-CallIndex "Start-Process:$($global:DB93.installerName)")
    }

    It "restarts the service when the service binary stays locked" {
        Set-InstallMocks -Unlocked $false -BinaryExists $true -Hashes @("AAAA")
        $ErrorActionPreference = "Stop"

        { & $script:installScript 6>$null } | Should -Throw "*still locked*"

        Should -Invoke Start-ScheduledTask -Times 1 -Exactly -ParameterFilter { $TaskName -eq "DevBridgeService" }
        (Get-CallIndex "Start-Process:$($global:DB93.installerName)") | Should -Be -1
    }

    It "restarts the service when the binary hash is unchanged after NSIS (silent no-op swap)" {
        Set-InstallMocks -BinaryExists $true -Hashes @("AAAA", "AAAA")
        $ErrorActionPreference = "Stop"

        { & $script:installScript 6>$null } | Should -Throw "*SHA256 unchanged*"

        Should -Invoke Start-ScheduledTask -Times 1 -Exactly -ParameterFilter { $TaskName -eq "DevBridgeService" }
    }
}

Describe "install.ps1 stops the tray apps before NSIS (issue #93)" {
    BeforeEach { $script:savedEnv = Save-DevBridgeEnv }
    AfterEach { Restore-DevBridgeEnv -Saved $script:savedEnv }

    It "stops devbridge-app and the legacy DevBridge tray in every session before starting the installer" {
        Set-InstallMocks -Trays @(
            [pscustomobject]@{ Name = "devbridge-app"; Id = 501; SessionId = 3 },
            [pscustomobject]@{ Name = "devbridge-app"; Id = 502; SessionId = 7 },
            [pscustomobject]@{ Name = "DevBridge"; Id = 503; SessionId = 9 })

        & $script:installScript 6>$null

        $nsis = Get-CallIndex "Start-Process:$($global:DB93.installerName)"
        $nsis | Should -BeGreaterThan -1
        foreach ($trayId in @(501, 502, 503)) {
            $stop = Get-CallIndex "Stop-Process:$trayId"
            $stop | Should -BeGreaterThan -1 -Because "tray PID $trayId must be stopped"
            $stop | Should -BeLessThan $nsis -Because "tray PID $trayId must be stopped BEFORE NSIS runs"
        }
    }
}

Describe "autoupdate.ps1 brings a stopped service back after a failed install (issue #93)" {
    BeforeEach {
        $script:savedEnv = Save-DevBridgeEnv
        # Pin the target so the run never asks GitHub for the latest release.
        $env:DEVBRIDGE_VERSION = "v0.8.42"
        $global:DB93.log = New-Object System.Collections.ArrayList
        $global:DB93.taskState = "Ready"
        $global:DB93.serviceUp = $false

        Mock Test-Path { $false } -ParameterFilter { "$Path" -like '*autoupdate.disabled' }
        Mock Test-Path { $true }
        Mock New-Item { }
        Mock Add-Content { [void]$global:DB93.log.Add([string]$Value) }
        Mock Get-ItemProperty { [pscustomobject]@{ DisplayVersion = "0.8.41" } }
        Mock Invoke-RestMethod { [pscustomobject]@{ status = "running"; active_jobs = 0 } }
        # The downloaded install.ps1 fails the way the pre-#93 installer did:
        # a terminating error with the service left stopped.
        Mock New-Object {
            $client = [pscustomobject]@{}
            $client | Add-Member -MemberType ScriptMethod -Name DownloadString -Value {
                param($Url)
                'Write-Error "Installer exited with code 2"'
            }
            $client
        } -ParameterFilter { $TypeName -eq "System.Net.WebClient" }
        Mock Get-ScheduledTask { [pscustomobject]@{ TaskName = "DevBridgeService"; State = $global:DB93.taskState } }
        Mock Start-ScheduledTask { $global:DB93.taskState = "Running"; $global:DB93.serviceUp = $true }
        Mock Get-Process {
            if ($global:DB93.serviceUp) { [pscustomobject]@{ Name = "devbridge-service"; Id = 4242; SessionId = 0 } }
        } -ParameterFilter { @($Name) -contains 'devbridge-service' }
        Mock Start-Sleep { }
    }
    AfterEach { Restore-DevBridgeEnv -Saved $script:savedEnv }

    It "starts DevBridgeService when the failed install left it stopped, logs it and still exits 1" {
        & $script:autoUpdateScript 6>$null
        $code = $LASTEXITCODE

        $code | Should -Be 1
        Should -Invoke Start-ScheduledTask -Times 1 -Exactly -ParameterFilter { $TaskName -eq "DevBridgeService" }
        ($global:DB93.log -join "`n") | Should -Match "Auto-update FAILED"
        ($global:DB93.log -join "`n") | Should -Match "safety net.*started"
    }

    It "leaves a service that is already running alone (no second start)" {
        $global:DB93.taskState = "Running"
        $global:DB93.serviceUp = $true

        & $script:autoUpdateScript 6>$null

        $LASTEXITCODE | Should -Be 1
        Should -Invoke Start-ScheduledTask -Times 0 -Exactly
        ($global:DB93.log -join "`n") | Should -Match "safety net.*already-running"
    }
}

Describe "Install-DevBridgePackage (the guarded upgrade, issue #93)" {
    It "runs post-install with the given arguments and never calls the restore on success" {
        Set-InstallMocks -PostInstallExitCode 0 -Trays @([pscustomobject]@{ Name = "devbridge-app"; Id = 601; SessionId = 2 })

        Install-DevBridgePackage -InstallerPath "C:\t\$($global:DB93.installerName)" -TargetVersion "0.8.42" `
            -InstallDir "C:\Program Files\DevBridge" -PostInstallArgs @("-Mode", "client") 6>$null

        (@($global:DB93.postInstallArgs) -join ",") | Should -BeExactly "-Mode,client"
        Should -Invoke Start-ScheduledTask -Times 0 -Exactly
        $order = @("Stop-ScheduledTask:DevBridgeService", "Stop-Process:601",
            "Start-Process:$($global:DB93.installerName)", "Invoke-DevBridgePostInstallScript:post-install.ps1")
        $last = -1
        foreach ($step in $order) {
            $idx = Get-CallIndex $step
            $idx | Should -BeGreaterThan $last -Because "$step must follow the previous upgrade step"
            $last = $idx
        }
    }

    It "restores the service when post-install.ps1 exits non-zero" {
        Set-InstallMocks -PostInstallExitCode 1
        $ErrorActionPreference = "Stop"

        { Install-DevBridgePackage -InstallerPath "C:\t\$($global:DB93.installerName)" -TargetVersion "0.8.42" `
                -InstallDir "C:\Program Files\DevBridge" 6>$null } | Should -Throw "*post-install.ps1 exited with code 1*"

        Should -Invoke Start-ScheduledTask -Times 1 -Exactly -ParameterFilter { $TaskName -eq "DevBridgeService" }
    }

    It "still surfaces the ORIGINAL error when the restore itself fails" {
        Set-InstallMocks -NsisExitCode 2
        Mock Restore-DevBridgeService { throw "task scheduler unavailable" }
        $ErrorActionPreference = "Stop"

        { Install-DevBridgePackage -InstallerPath "C:\t\$($global:DB93.installerName)" -TargetVersion "0.8.42" `
                -InstallDir "C:\Program Files\DevBridge" 6>$null 3>$null } |
            Should -Throw "*Installer exited with code 2*"
        Should -Invoke Restore-DevBridgeService -Times 1 -Exactly
    }

    It "accepts a same-version reinstall (hash unchanged, installed version == target)" {
        Set-InstallMocks -BinaryExists $true -Hashes @("AAAA", "AAAA") -PostInstallExitCode 0
        Mock Get-DevBridgeInstalledVersion { "0.8.42" }

        { Install-DevBridgePackage -InstallerPath "C:\t\$($global:DB93.installerName)" -TargetVersion "0.8.42" `
                -InstallDir "C:\Program Files\DevBridge" 6>$null } | Should -Not -Throw
        Should -Invoke Start-ScheduledTask -Times 0 -Exactly
    }
}

Describe "Stop-DevBridgeTrayApps (issue #93)" {
    It "stops every devbridge-app and legacy DevBridge process and returns how many" {
        Set-InstallMocks -Trays @(
            [pscustomobject]@{ Name = "devbridge-app"; Id = 701; SessionId = 1 },
            [pscustomobject]@{ Name = "DevBridge"; Id = 702; SessionId = 4 })

        $n = Stop-DevBridgeTrayApps 6>$null

        $n | Should -Be 2
        Should -Invoke Stop-Process -Times 1 -Exactly -ParameterFilter { $Id -eq 701 }
        Should -Invoke Stop-Process -Times 1 -Exactly -ParameterFilter { $Id -eq 702 }
        Should -Invoke Get-Process -ParameterFilter { (@($Name) -contains 'devbridge-app') -and (@($Name) -contains 'DevBridge') }
    }

    It "returns 0 and stops nothing when no tray app runs" {
        Set-InstallMocks

        $n = Stop-DevBridgeTrayApps 6>$null

        $n | Should -Be 0
        Should -Invoke Stop-Process -Times 0 -Exactly
    }

    It "warns (does not throw) when a tray app survives the stop" {
        Set-InstallMocks -Trays @([pscustomobject]@{ Name = "devbridge-app"; Id = 801; SessionId = 5 })
        Mock Stop-Process { }   # the process refuses to die

        $warnings = @(Stop-DevBridgeTrayApps -TimeoutSeconds 0 3>&1 6>$null) |
            Where-Object { $_ -is [System.Management.Automation.WarningRecord] }

        $warnings.Count | Should -Be 1
        $warnings[0].Message | Should -Match "devbridge-app PID 801"
    }
}

Describe "Invoke-DevBridgePostInstallScript (real Windows PowerShell child process)" {
    It "returns the child's exit code, not its output lines" {
        $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("db93-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        try {
            $child = Join-Path $dir "post-install.ps1"
            Set-Content -Path $child -Encoding ASCII -Value @(
                'param([string]$Mode)',
                'Write-Output "stdout line mode=$Mode"',
                '[Console]::Error.WriteLine("ERROR: a stderr line")',
                'exit 3')
            $ErrorActionPreference = "Stop"

            $code = Invoke-DevBridgePostInstallScript -ScriptPath $child -Arguments @("-Mode", "client") 6>$null

            @($code).Count | Should -Be 1
            $code | Should -Be 3
        } finally {
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
        }
    }
}

Describe "install.ps1 DEVBRIDGE_INSTALLER_PATH (local installer, issue #93)" {
    BeforeEach {
        $script:savedEnv = Save-DevBridgeEnv
        $script:localDir = Join-Path ([System.IO.Path]::GetTempPath()) ("db93-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $script:localDir | Out-Null
    }
    AfterEach {
        Restore-DevBridgeEnv -Saved $script:savedEnv
        Remove-Item -Recurse -Force $script:localDir -ErrorAction SilentlyContinue
    }

    It "installs the local file: no GitHub fetch, no download, target version from the file name" {
        Set-InstallMocks -BinaryExists $true -Hashes @("AAAA", "AAAA")
        Mock Get-DevBridgeInstalledVersion { "0.8.42" }   # same-version reinstall -> must be accepted
        $local = Join-Path $script:localDir "DevBridge_0.8.42_x64-setup.exe"
        Set-Content -Path $local -Value "not a real installer" -Encoding ASCII
        $env:DEVBRIDGE_INSTALLER_PATH = $local

        & $script:installScript 6>$null

        Should -Invoke Invoke-RestMethod -Times 0 -Exactly
        Should -Invoke Invoke-WebRequest -Times 0 -Exactly
        Should -Invoke Start-Process -Times 1 -Exactly -ParameterFilter { $FilePath -eq (Resolve-Path -LiteralPath $local).Path }
        Test-Path -LiteralPath $local | Should -BeTrue -Because "a caller-provided installer is never deleted"
    }

    It "refuses a missing file BEFORE stopping anything" {
        Set-InstallMocks
        $env:DEVBRIDGE_INSTALLER_PATH = Join-Path $script:localDir "DevBridge_0.8.42_x64-setup.exe"

        { & $script:installScript 6>$null } | Should -Throw "*DEVBRIDGE_INSTALLER_PATH*not a file*"

        Should -Invoke Stop-ScheduledTask -Times 0 -Exactly
        Should -Invoke Start-Process -Times 0 -Exactly
    }

    It "refuses a file name without a version BEFORE stopping anything" {
        Set-InstallMocks
        $local = Join-Path $script:localDir "setup.exe"
        Set-Content -Path $local -Value "x" -Encoding ASCII
        $env:DEVBRIDGE_INSTALLER_PATH = $local

        { & $script:installScript 6>$null } | Should -Throw "*carries no version*"

        Should -Invoke Stop-ScheduledTask -Times 0 -Exactly
    }
}

Describe "Start-DevBridgeServiceIfStopped (autoupdate.ps1 safety net, issue #93)" {
    BeforeEach {
        $global:DB93.taskState = "Ready"
        $global:DB93.serviceUp = $false
        Mock Get-ScheduledTask { [pscustomobject]@{ TaskName = "DevBridgeService"; State = $global:DB93.taskState } }
        Mock Start-ScheduledTask { $global:DB93.taskState = "Running"; $global:DB93.serviceUp = $true }
        Mock Get-Process {
            if ($global:DB93.serviceUp) { [pscustomobject]@{ Name = "devbridge-service"; Id = 4242; SessionId = 0 } }
        } -ParameterFilter { @($Name) -contains 'devbridge-service' }
        Mock Start-Sleep { }
    }

    It "does nothing when the DevBridgeService task is already Running" {
        $global:DB93.taskState = "Running"

        $r = Start-DevBridgeServiceIfStopped

        $r.Action | Should -Be "already-running"
        $r.Running | Should -BeTrue
        Should -Invoke Start-ScheduledTask -Times 0 -Exactly
    }

    It "starts a stopped task and reports the running process" {
        $r = Start-DevBridgeServiceIfStopped

        $r.Action | Should -Be "started"
        $r.Running | Should -BeTrue
        $r.Detail | Should -Match "PID 4242"
        Should -Invoke Start-ScheduledTask -Times 1 -Exactly -ParameterFilter { $TaskName -eq "DevBridgeService" }
    }

    It "reports start-failed when Start-ScheduledTask throws (task deleted)" {
        Mock Get-ScheduledTask { $null }
        Mock Start-ScheduledTask { throw "No MSFT_ScheduledTask objects found" }

        $r = Start-DevBridgeServiceIfStopped

        $r.Action | Should -Be "start-failed"
        $r.Running | Should -BeFalse
        $r.Detail | Should -Match "task state was missing"
    }

    It "reports start-failed when no process appears after the start" {
        Mock Start-ScheduledTask { }

        $r = Start-DevBridgeServiceIfStopped -WaitSeconds 2

        $r.Action | Should -Be "start-failed"
        $r.Running | Should -BeFalse
        Should -Invoke Start-Sleep -Times 2 -Exactly
    }
}
