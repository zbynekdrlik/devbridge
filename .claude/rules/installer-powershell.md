---
paths:
  - "installer/**"
  - "deploy/*.ps1"
---

# Installer PowerShell — gotchas (auto-loads on installer/ and deploy/*.ps1)

- **Production runs Windows PowerShell 5.1 (as SYSTEM), not pwsh 7.** CI runs the installer Pester suite on BOTH (`installer-tests` job: pwsh 7 step + `shell: powershell` 5.1 step, #69). A test that passes only on pwsh 7 is a prod bug.
- **Never read a config with `Get-Content -Raw` and write it back as UTF-8.** PS 5.1 reads BOM-less UTF-8 as the ANSI code page, so the write-back double-encodes non-ASCII bytes. Use `[System.IO.File]::ReadAllText($Path)` + `WriteAllText($Path, $text, (New-Object System.Text.UTF8Encoding($false)))` (both serial-bridge merges do this since 0.8.36).
- **install.ps1 and post-install.ps1 cannot dot-source each other** (irm|iex / relocated Tauri resource). A helper needed in both is defined inline in both, and a Pester test asserts the two AST extents are byte-identical (`ConvertFrom-DevBridgeSerialBridgesSpec`). Edit one → copy verbatim to the other.
- **Array `Should -Be`**: compare `(@($x) -join ",") | Should -BeExactly "a,b"` — a piped array unrolls and is not a reliable element-wise compare.
- **PowerShell hashtables are case-INSENSITIVE**; Rust `HashMap` keys (client_id) are case-sensitive. Use `[System.Collections.Hashtable]::new([System.StringComparer]::Ordinal)` when mirroring Rust key semantics.
- **Smoke a PS change under real 5.1 before pushing:** pz-server has only Pester 3.4, so do not run the suite there. Push the commit to the wip ref, then on pz-server `Invoke-WebRequest https://raw.githubusercontent.com/zbynekdrlik/devbridge/<sha>/installer/post-install.ps1`, AST-extract the functions (same pattern as the Pester `Get-FunctionSourceFromScript`), and run them on a COPY of the live config in `C:\Windows\Temp\<dir>`. Delete the dir afterwards and confirm the live `config.toml` SHA256 is unchanged.
- The dev-branch CI E2E deploy replaces the binary on pz-snv and restarts the PRODUCTION `DevBridgeService` task too, so pjsnvs runs the dev build right after a green dev CI.
