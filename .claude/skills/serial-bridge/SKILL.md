---
name: devbridge-serial-bridge
description: >
  Serial bridge (barcode scanner) configuration for DevBridge — pjkeb scanner
  hardware specs, server-side COM port mapping, com0com pair setup, and Codex
  ERP port name requirements. Use when configuring, debugging, or deploying
  serial bridge features.
triggers:
  - serial
  - scanner
  - COM port
  - barcode
  - com0com
  - pjkeb
  - Codex
  - serial_bridge
  - pjsln
  - pjzav
  - Start.cd
  - E_EANPort
---

# DevBridge Serial Bridge Skill

## pjkeb scanner hardware (verified values — do NOT ask user)

- **Hardware:** USB-SERIAL CH341 (VID_1A86&PID_5523, wch.cn) on **COM5**
- **Baud:** **9600** (confirmed by live scan tests)
- **Also on pjkeb:** USB Serial Device STM32 (VID_0483&PID_5740) on COM6 — NOT the scanner

**Client config:**
```toml
[client.serial_bridge]
enabled = true
port = "COM5"
baud_rate = 9600
```

These values are proven. Never ask the user for port/baud. If a scan isn't captured, debug the pipeline (devbridge running, COM5 held, server online, pjkeb paired) — not port/baud.

**Installer support (since #68):** `install.ps1`/`post-install.ps1` write this block for you — set `$env:DEVBRIDGE_SERIAL_PORT = "COM5"` (`$env:DEVBRIDGE_SERIAL_BAUD` optional, defaults to 9600) before the `irm|iex` one-liner. pjkeb's config was hand-edited before this existed; a fresh install/upgrade no longer needs that.

## pjsln / pjzav scanner (deployed 2026-09-09, #68)

Store **pjsln** (being renamed **pjzav**; `client_id` stays `pjsln-client` until re-paired) has the SAME
scanner hardware as pjkeb: USB-SERIAL CH341 (VID_1A86&PID_5523) on **COM4**, 9600. Also present: STM32
USB Serial Device on COM3 — NOT the scanner.

- Client config: `[client.serial_bridge] enabled = true, port = "COM4", baud_rate = 9600`
- Server mapping: `client_id = "pjsln-client"`, `virtual_port = "COM22"` — com0com pair **CNCA1=COM22 ↔ CNCB1=COM23**
- Codex (user `pjsln`): `E_EANPort=\\.\COM23`
- Before #68 the scanner ran over RDP COM redirection (`redirectcomports:i:1`) — flaky, that is what "scanner stopped working" was.

## Adding a scanner to a NEW store — the full checklist

1. **Client:** `$env:DEVBRIDGE_SERIAL_PORT = "COMx"` + `irm|iex` (or, on an existing install, add the
   `[client.serial_bridge]` block and restart the `DevBridgeService` scheduled task — config is read ONLY at
   start, no hot-reload). Verify in `C:\ProgramData\DevBridge\logs\service.<date>.log`:
   `serial port opened (ready to read barcode data)` + `StreamSerialData RPC established`.
2. **Client RDP file** (`C:\Users\<user>\Desktop\*.rdp`): set `redirectcomports:i:0` — otherwise mstsc and
   devbridge fight over the COM port.
3. **pz-server com0com pair:** tools live in `C:\ProgramData\DevBridge\tools\com0com\` (`setupc.exe`,
   `setup.dll`, `cncport.inf`, `comport.inf`, plus the SIGNED `com0com.inf/.cat/.sys` copied from
   `C:\Windows\System32\DriverStore\FileRepository\com0com.inf_amd64_*`). Run from that dir:
   `setupc.exe --silent install PortName=COM2A,EmuBR=yes PortName=COM2B,EmuBR=yes` (next free pair:
   COM24/COM25 …). Works without reboot despite the "Reboot required" line; verify with `setupc.exe list`
   and a PowerShell loopback (`SerialPort` write on A, `ReadExisting` on B).
   Sourceforge blocks scripted downloads (returns the literal body `no`); the x64 `setupc.exe` was
   extracted with `7z x` from the NSIS `setup.exe` mirrored at GitHub `0x8DEADF00D/obd2NET/tools/com0com-3.0.0.0-i386-and-x64/`.
4. **pz-server config:** append `[[server.serial_bridges]]` for the new `client_id` → `COM2A`, then restart
   the `DevBridgeService` scheduled task (Stop-ScheduledTask / kill `devbridge-service` / Start-ScheduledTask).
   Log must show `serial bridge manager initialized with configured mappings count=N clients=...`.
5. **Codex port:** per-user file `C:\Users\<user>\UzivatelCodex\Start.cd`, line `E_EANPort=` → `\\.\COM2B`
   (DOS-device form, see below). Plain cp1250 text with CRLF — rewrite with `[IO.File]::WriteAllLines(..., GetEncoding(1250))`.
   Codex reads it ONLY at start → restart Codex (`START.EXE` + `CPanel.exe`) in that user's RDP session.
   Without the user's password: `Register-ScheduledTask -Principal (New-ScheduledTaskPrincipal -UserId <user> -LogonType Interactive)`
   with the action `C:\Users\<user>\UzivatelCodex\START.EXE`, then `Start-ScheduledTask` — it runs in the
   logged-on session. START.EXE then sits on the login form (`TOtvorForm` "Otváram ekonomický projekt CODEX")
   until the cashier logs in; only after login does Codex open the COM port (`CPanel.exe` appears).
   Same trick (interactive task running a script) is how you enumerate that session's windows; Defender AMSI
   blocks an inline `CopyFromScreen` screenshot script, so read window titles/classes instead.
6. **End-to-end proof:** client log `chunk read from port`, server log `wrote N bytes to COMxx successfully
   client_id=<id>`, and the barcode lands in Codex. Server-side `COM2B` shows "Access denied" once Codex holds it.

## Server-side COM port mapping (pz-server)

```toml
[[server.serial_bridges]]
client_id = "pjkeb-client"
virtual_port = "COM20"
baud_rate = 9600

[[server.serial_bridges]]
client_id = "pjsln-client"
virtual_port = "COM22"
baud_rate = 9600
```

com0com pairs on pz-server: **CNCA0=COM20 ↔ CNCB0=COM21** (pjkeb), **CNCA1=COM22 ↔ CNCB1=COM23** (pjsln).
The A side is where devbridge writes (opened lazily, only when bytes arrive); the B side is what Codex reads.
An unmapped `client_id` sending SerialData gets `SerialAck{ok:false}` and a server WARN — data is dropped, never queued.

**RDP note:** `.rdp` files on client machines have `redirectcomports:i:0` — mstsc does NOT grab the scanner port. Codex runs locally on pz-server and reads the B-side port directly.

**Codex config location:** `C:\Users\<codex-user>\UzivatelCodex\Start.cd` → `E_EANPort=` (per Windows user on pz-server; the RDP user = the store).

## Codex ERP — COM21 port name MUST use DOS-device form

Codex (2009/2020 vintage Delphi/FPC binary) opens serial devices via Win32 DOS namespace directly (NOT `System.IO.Ports.SerialPort`).

**Rule:** For COM ports numbered **≥ 10**, the name MUST be the full DOS-device path:
```
\\.\COM21   ← correct
COM21       ← WRONG — CreateFile returns INVALID_HANDLE_VALUE silently
```

**Correct Codex config:** `\\.\COM21`, 9600 8N1, no flow control.

Symptom when set wrong: Codex silently opens nothing; barcodes never arrive in ERP even though every other pipeline link works.

## Monitor script

`C:\ProgramData\DevBridge\com21-monitor.ps1` on pz-server reads COM21 with 2s ReadTimeout and infinite loop; logs to `com21-monitor.log`.

**Do NOT put a deadline** — past bug was a 10-min auto-exit that made scanners stop working overnight.
