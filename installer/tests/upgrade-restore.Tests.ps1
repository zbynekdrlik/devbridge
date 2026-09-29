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

    $mockable = Get-FunctionSourceFromScript -ScriptPath $script:installScript `
        -Names @("Wait-DevBridgeBinaryUnlocked", "Get-DevBridgeInstalledVersion", "Get-DevBridgeMissingVcRuntimeDlls")
    foreach ($src in $mockable.Values) { . ([scriptblock]::Create($src)) }

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
            [object[]]$Trays = @()
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
        Mock Test-Path { $false } -ParameterFilter { "$Path" -like '*post-install.ps1' }
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
}
