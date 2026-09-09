---
name: devbridge-deploy
description: >
  DevBridge deployment procedures — irm|iex installer pattern, dev-branch
  verification on pz machines, printer name gotchas, VC++ Redist requirement,
  and fleet auto-update behaviour. Use whenever deploying, upgrading, or
  setting up DevBridge on any pz/pj machine.
triggers:
  - deploy
  - upgrade
  - install
  - irm
  - pz-server
  - pz-snv
  - pz-holla
  - pz-david
  - fleet
  - installer
---

# DevBridge Deploy Skill

## The one-liner rule — ALL deploys use irm|iex

NEVER download artifacts manually, copy files, or use any other method. Use the install.ps1 script exclusively:

```powershell
# Stable (latest release):
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex

# Dev build (dev-latest pre-release):
$env:DEVBRIDGE_VERSION="dev"; irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex

# Specific version:
$env:DEVBRIDGE_VERSION="v0.8.30"; irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

While install.ps1 is on dev branch only: use the dev branch URL. After merge to main, use main URL.

**NEVER:**
- Download CI artifacts manually via `gh api` or `Invoke-WebRequest` artifact URLs
- Copy installer files between machines
- Run NSIS installer directly (bypasses post-install config, VC++ redist check, etc.)

## Dev-branch verification flow

**The pz machines are the verification environment, not production-only.** The CI dev-branch run (`E2E Deploy`, `E2E Test`, `Dev Release` jobs on self-hosted runners) already installs the new binary on pz-server + pz-snv. After dev CI reaches terminal success:

1. IMMEDIATELY verify on pz-server/pz-snv via MCP — do NOT wait for merge first.
2. Use `mcp__win-pz-server__Shell` / `mcp__win-pz-snv__Shell` to check service state, version, and feature-specific output.
3. Only after concrete post-deploy evidence request merge — and frame it as "dev deployment is verified working on pz-server, requesting merge to promote to main."

"Wait for merge to verify on production" is a mistake. pz-server IS the verification environment.

## pz-holla printer name gotcha

pz-holla has TWO entries for the Brother DCP-1610W:
- **"eholla printer"** — Brother Laser Type1 Class Driver on USB002 — **THIS ONE WORKS**
- **"Brother DCP-1610W series"** — model-specific driver on USB001 — **BROKEN** (creates 139MB spooler jobs that error)

The model-specific driver rasterizes at 600 DPI creating 35MB bitmaps that overwhelm USB. Always use `"eholla printer"` for pz-holla config.

## VC++ 2015-2022 Redistributable requirement

Bundled Ghostscript (`gsdll64.dll`) needs `msvcp140.dll` + `vcruntime140.dll`. Fresh Windows may not have them.

**Symptom:** `Ghostscript exit code -1073741515` / `Can't load Ghostscript DLL, LoadLibrary error code 126`.

**Resolution:** `install.ps1` auto-installs VC++ Redist if missing. If the symptom appears on a machine with an old install, run:
```powershell
vc_redist.x64.exe /install /quiet /norestart
```
No reinstall of DevBridge required.

## Fleet auto-update (since v0.8.30)

Since 2026-06-15 (v0.8.30), every ONLINE DevBridge machine self-upgrades via `DevBridgeAutoUpdate` (Windows scheduled task / macOS launchd). It checks GitHub `releases/latest` and applies PATCH-only updates automatically.

**Manual upgrade is still required for:**
- Machines that were OFFLINE when 0.8.30 was first deployed (no task registered yet)
- Brand-new installs
- Minor/major version bumps (auto-update is patch-only)

The CI deploy only touches pz-server + pz-snv (via e2e-setup, which bypasses post-install).

## pz-david — two instances, don't forget

pz-david (10.88.1.104, macOS arm64, MCP: `mac-pz-david`) runs TWO DevBridge client instances under user `grena`:
- `com.devbridge.wifi` — dashboard port 9120, client_id `david-wifi`, target `EPSON_L4260_Series`
- `com.devbridge.usb` — dashboard port 9121, client_id `david-usb`, target `EPSON_L4260_Series_2`

**Upgrade path:** download `DevBridge_X.Y.Z_aarch64.dmg`, unload both launchd agents, replace `/Applications/DevBridge.app`, reload agents. Configs in `~/Library/Application Support/DevBridge-{wifi,usb}/` are preserved.

Whenever deploying to "all pz computers", include pz-david via `mac-pz-david` MCP.

## New client deployment

```powershell
$env:DEVBRIDGE_MODE = "client"
$env:DEVBRIDGE_SERVER_HOST = "10.88.1.100"
$env:DEVBRIDGE_CLIENT_ID = "store-name"
$env:DEVBRIDGE_TARGET_PRINTER = "Printer Name"
$env:DEVBRIDGE_PRINT_BACKEND = "windows_spooler"
$env:DEVBRIDGE_VIRTUAL_PRINTER_NAME = "store printer"
$env:DEVBRIDGE_SERIAL_PORT = "COM4"   # only for a client with a serial barcode scanner
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
# Then approve on server dashboard
```

`DEVBRIDGE_SERIAL_PORT` (e.g. `COM4`) and `DEVBRIDGE_SERIAL_BAUD` (default `9600` when the port is set) write a `[client.serial_bridge]` block into config.toml. On a fresh install it's part of the new config; on an **existing** install (config.toml preserved on upgrade) the installer **ADDS** the block if none exists yet, or **KEEPS** an existing `[client.serial_bridge]` section untouched (values not overwritten — set `DEVBRIDGE_FORCE_CONFIG_REWRITE=true` to regenerate). See `.claude/skills/serial-bridge/SKILL.md` for the full serial-bridge feature (issue #68).

Since 0.8.33, re-running `install.ps1` with `DEVBRIDGE_FORCE_CONFIG_REWRITE=true` against a machine that already has the target version installed is the sanctioned way to rewrite config only — the installer recognizes the unchanged binary hash as a same-version reinstall (not a failed swap) and keeps going into post-install instead of aborting with the service left stopped (issue #71).

NEVER manually write config.toml, copy certs, install SumatraPDF, or create scheduled tasks by hand. If the installer doesn't handle it, fix the installer.
