# Shared AST function extractor for test/CI tooling (issue #70).
#
# The installer helpers are defined INLINE in the production scripts
# (installer/install.ps1 runs via irm|iex with no files on disk,
# installer/post-install.ps1 is a relocated lone Tauri resource, and
# installer/autoupdate.ps1 is the scheduled-task body), so they cannot be a
# shared dot-sourced lib at RUNTIME. Test and CI code that wants to exercise the
# REAL production functions parses the script with the PowerShell AST and
# extracts the named function definitions WITHOUT executing the script body
# (which would stop services, touch the live data dir, or hit the network).
#
# Consumers (dot-source this file, then dot-source each returned body in the
# caller's own scope so the functions become callable there):
#   installer/tests/installer-lib.Tests.ps1, installer/tests/autoupdate.Tests.ps1
#   deploy/e2e-setup-client-local.ps1 (real Merge-DevBridgeSerialBridgeIntoConfig
#   on the isolated E2E config)
#
#   . (Join-Path $repoRoot "deploy\lib\Get-FunctionSourceFromScript.ps1")
#   $sources = Get-FunctionSourceFromScript -ScriptPath <script> -Names @("A", "B")
#   foreach ($src in $sources.Values) { . ([scriptblock]::Create($src)) }
#
# Must stay Windows PowerShell 5.1 compatible (CI runs the Pester suites on 5.1
# and pwsh 7; the E2E client setup runs under 5.1).

# Parse $ScriptPath and return an ordered @{ name = function-source-text } for
# every requested function. Throws on a parse error or a missing function so a
# rename/removal fails loudly instead of silently testing nothing.
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
