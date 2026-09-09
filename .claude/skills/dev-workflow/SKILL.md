---
name: devbridge-dev-workflow
description: >
  Development workflow for DevBridge — test PowerShell/Windows changes via MCP
  before pushing to CI, CI runner constraints (PowerShell 5.1 / LocalSystem),
  and no-local-build discipline to avoid target/ disk bloat. Use before
  pushing any PowerShell, installer, or Windows-service changes.
triggers:
  - PowerShell
  - MCP test
  - CI runner
  - installer
  - post-install
  - test before push
  - local build
  - cargo build
---

# DevBridge Dev Workflow Skill

## Always test PowerShell scripts on Windows via MCP before pushing

Before pushing any PowerShell script or Windows-specific change to CI, run the key sections directly on the target machine via MCP:

```
mcp__win-pz-server__Shell  — PowerShell on pz-server (10.88.1.100)
mcp__win-pz-snv__Shell     — PowerShell on pz-snv (10.78.2.10)
mcp__win-pz-holla__Shell   — PowerShell on pz-holla (10.88.1.105)
```

Test `sc.exe` / `New-Service` / `Set-Printer` / service registration directly on the machines.

**Why:** Iterating through CI for PowerShell fixes wastes ~25 min per cycle. Testing locally via MCP catches issues in seconds.

## CI runner constraints (PowerShell 5.1 / LocalSystem)

Self-hosted runners run as **LocalSystem** and use **PowerShell 5.1** (NOT PowerShell 7). Features absent in 5.1:
- if-expressions as values
- `Set-Service -BinaryPathName`
- `Remove-Service` (use `sc.exe delete` instead)

CI runners run as LocalSystem on both machines (reconfigured 2026-03-22). Test scripts with the same user context when verifying via MCP.
Verified 2026-09-09: both are Windows services under LocalSystem — `actions.runner.zbynekdrlik-devbridge.pz-server` and `actions.runner.zbynekdrlik-devbridge.pz-snv`, dir `C:\actions-runner`. pz-snv had drifted to a scheduled task `GitHubActionsRunner` (`run.cmd` as user `pz`, interactive) that died with `STATUS_CONTROL_C_EXIT` after the runner self-update; it was re-registered as a service (`--runasservice --windowslogonaccount "NT AUTHORITY\SYSTEM"`) and the task is Disabled.

**Runner registration expires after 14 days without contact** (#68 finding) — see the next section.

## Runner deregistered → stuck CI (#68, 2026-09-09)
No CI run for weeks → GitHub deletes the runner; `C:\actions-runner\_diag\Runner_*.log` says "runner registration has been deleted from the server". Symptom: run stuck `queued` on `pz-client`; with `concurrency: ci-${{ github.ref }}` the NEXT run sits `pending` with 0 jobs. `gh run cancel` did not free it, `gh api -X POST repos/zbynekdrlik/devbridge/actions/runs/<id>/force-cancel` did.
Re-register: `config.cmd remove --local` (or move `.runner` + `.credentials*` aside), mint `gh api -X POST repos/zbynekdrlik/devbridge/actions/runners/registration-token`, on the box `config.cmd --unattended --url https://github.com/zbynekdrlik/devbridge --token <t> --name pz-snv --labels pz-client --work _work --replace --runasservice --windowslogonaccount "NT AUTHORITY\SYSTEM"` — the service starts itself. Check anytime: `gh api repos/zbynekdrlik/devbridge/actions/runners`.

## No local builds — cargo fmt only

**NEVER run locally:**
```
cargo build
cargo check
cargo test
cargo clippy
cargo build -p <pkg>
```

The workspace excludes `devbridge-app` (Tauri, ~5.6GB), `devbridge-ui` (Leptos WASM, ~460MB), and `devbridge-e2e` (~584MB). Running ANY cargo compile command (even `-p <pkg>`) compiles those into their own `target/` directories outside the main workspace target, rapidly consuming gigabytes of disk.

**The only allowed local cargo command:**
```bash
cargo fmt --all --check   # and its fixer: cargo fmt --all
```

Push and let CI compile. Trust CI output.

When dispatching implementer subagents, explicitly instruct them NOT to run local builds.

## MCP disconnected — stop and ask user immediately

If any `mcp__win-*` tool returns an error (Session not found, connection error, etc.), STOP and ask the user to reconnect MCP. Do NOT continue working and try to diagnose through CI logs alone — direct MCP access is 200× faster.
