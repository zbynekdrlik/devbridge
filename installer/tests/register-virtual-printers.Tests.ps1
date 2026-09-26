# Pester v5 tests for the per-virtual-printer Windows driver override in
# deploy/register-virtual-printers.ps1 (issue #88 -- RAW passthrough for label
# printers: the server-side Windows printer uses the vendor driver, e.g.
# "TSC ML241P", instead of the Microsoft IPP Class Driver).
#
# The reconciler script body touches the real spooler (Get-Printer,
# Remove-Printer, rundll32 printui.dll), so the tests extract ONLY its pure
# decision helpers via the shared AST extractor -- the body never runs.

BeforeAll {
    $repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    . (Join-Path $repoRoot "deploy/lib/Get-FunctionSourceFromScript.ps1")
    $script:reconciler = Join-Path $repoRoot "deploy/register-virtual-printers.ps1"
    $sources = Get-FunctionSourceFromScript -ScriptPath $script:reconciler `
        -Names @("Resolve-DevBridgeVpDriver", "Test-DevBridgePrinterUpToDate", "Test-DevBridgePrinterDriverInstalled")
    foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }

    # Mirrors what the service writes to reconcile-input.json (serde JSON of
    # VirtualPrinter) -- parsed the same way the script parses it.
    function ConvertFrom-VpJson([string]$Json) { return ($Json | ConvertFrom-Json) }
}

Describe "Resolve-DevBridgeVpDriver (issue #88)" {
    It "defaults to the IPP Class Driver when the entry has no driver field (pre-#88 JSON)" {
        $vp = ConvertFrom-VpJson '{"display_name":"pjsnvs printer","ipp_name":"pjsnvs-printer"}'
        Resolve-DevBridgeVpDriver -Vp $vp | Should -BeExactly "Microsoft IPP Class Driver"
    }

    It "defaults to the IPP Class Driver when driver is null" {
        $vp = ConvertFrom-VpJson '{"display_name":"pjsnvs printer","ipp_name":"pjsnvs-printer","driver":null}'
        Resolve-DevBridgeVpDriver -Vp $vp | Should -BeExactly "Microsoft IPP Class Driver"
    }

    It "defaults to the IPP Class Driver when driver is blank" {
        $vp = ConvertFrom-VpJson '{"display_name":"x","ipp_name":"x","driver":"   "}'
        Resolve-DevBridgeVpDriver -Vp $vp | Should -BeExactly "Microsoft IPP Class Driver"
    }

    It "returns the trimmed override" {
        $vp = ConvertFrom-VpJson '{"display_name":"spisska stitky","ipp_name":"spisska-stitky","driver":" TSC ML241P "}'
        Resolve-DevBridgeVpDriver -Vp $vp | Should -BeExactly "TSC ML241P"
    }

    It "honours a custom default" {
        $vp = ConvertFrom-VpJson '{"display_name":"x","ipp_name":"x"}'
        Resolve-DevBridgeVpDriver -Vp $vp -DefaultDriver "Generic / Text Only" | Should -BeExactly "Generic / Text Only"
    }

    It "throws on a double quote (printui /m argument break-out)" {
        $vp = ConvertFrom-VpJson '{"display_name":"x","ipp_name":"x","driver":"TSC\" /r \"http://evil"}'
        { Resolve-DevBridgeVpDriver -Vp $vp } | Should -Throw "*forbidden character*"
    }

    It "throws on a backslash" {
        $vp = ConvertFrom-VpJson '{"display_name":"x","ipp_name":"x","driver":"a\\b"}'
        { Resolve-DevBridgeVpDriver -Vp $vp } | Should -Throw "*forbidden character*"
    }

    It "throws on a control character" {
        $vp = ConvertFrom-VpJson '{"display_name":"x","ipp_name":"x","driver":"TSC\nML241P"}'
        { Resolve-DevBridgeVpDriver -Vp $vp } | Should -Throw "*forbidden character*"
    }
}

Describe "Test-DevBridgePrinterUpToDate (issue #88)" {
    BeforeAll {
        $script:url = "http://127.0.0.1:631/printers/pjsnvs-printer"
    }

    It "is false when the printer does not exist" {
        Test-DevBridgePrinterUpToDate -Existing $null -Url $script:url -Driver "Microsoft IPP Class Driver" | Should -BeFalse
    }

    It "is true for an existing store printer on the default driver (no change for production)" {
        $p = [pscustomobject]@{ PortName = $script:url; DriverName = "Microsoft IPP Class Driver" }
        Test-DevBridgePrinterUpToDate -Existing $p -Url $script:url -Driver "Microsoft IPP Class Driver" | Should -BeTrue
    }

    It "is false when the port differs" {
        $p = [pscustomobject]@{ PortName = "NUL"; DriverName = "Microsoft IPP Class Driver" }
        Test-DevBridgePrinterUpToDate -Existing $p -Url $script:url -Driver "Microsoft IPP Class Driver" | Should -BeFalse
    }

    It "is false when only the driver differs (override added -> re-register)" {
        $p = [pscustomobject]@{ PortName = $script:url; DriverName = "Microsoft IPP Class Driver" }
        Test-DevBridgePrinterUpToDate -Existing $p -Url $script:url -Driver "TSC ML241P" | Should -BeFalse
    }
}

Describe "Test-DevBridgePrinterDriverInstalled (issue #88)" {
    BeforeAll {
        $script:installed = @("Microsoft IPP Class Driver", "TSC ML241P", "Generic / Text Only")
    }

    It "finds an installed driver" {
        Test-DevBridgePrinterDriverInstalled -Driver "TSC ML241P" -InstalledDrivers $script:installed | Should -BeTrue
    }

    It "matches case-insensitively like Windows driver names" {
        Test-DevBridgePrinterDriverInstalled -Driver "tsc ml241p" -InstalledDrivers $script:installed | Should -BeTrue
    }

    It "is false for a driver that is not installed" {
        Test-DevBridgePrinterDriverInstalled -Driver "TSC TTP-244" -InstalledDrivers $script:installed | Should -BeFalse
    }

    It "does not treat the name as a wildcard pattern" {
        Test-DevBridgePrinterDriverInstalled -Driver "TSC*" -InstalledDrivers $script:installed | Should -BeFalse
    }

    It "is false when no drivers are installed" {
        Test-DevBridgePrinterDriverInstalled -Driver "TSC ML241P" -InstalledDrivers @() | Should -BeFalse
    }
}

Describe "register-virtual-printers.ps1 registration command (issue #88)" {
    It "passes the resolved driver to printui.dll, not a hard-coded IPP Class Driver" {
        $text = [System.IO.File]::ReadAllText($script:reconciler)
        $text | Should -Match '/m `"\$driver`"'
        $text | Should -Not -Match '/m `"Microsoft IPP Class Driver`"'
    }

    It "never installs a printer driver (only the Step 0 IPP Class Driver InfPath repair may)" {
        $tokens = $null; $errors = $null
        $ast = [System.Management.Automation.Language.Parser]::ParseFile($script:reconciler, [ref]$tokens, [ref]$errors)
        $adds = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.CommandAst] -and $n.GetCommandName() -eq "Add-PrinterDriver" }, $true)
        @($adds).Count | Should -Be 1
        @($adds)[0].Extent.Text | Should -Match 'Microsoft IPP Class Driver'
    }
}
