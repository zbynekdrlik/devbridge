---
name: devbridge-print-config
description: >
  Print configuration gotchas for DevBridge — ghostscript device selection
  for direct_ipp backends, print verification via EventID 307, and E2E test
  requirements for realistic print flows. Use when configuring clients,
  writing E2E tests, or verifying physical print output.
triggers:
  - ghostscript
  - direct_ipp
  - canon
  - epson
  - print_backend
  - EventID 307
  - print verification
  - E2E print
  - spooler
---

# DevBridge Print Config Skill

## Ghostscript device for direct_ipp — MUST be jpeg, never ppmraw

Canon MG3600 and Epson L3260 (and all tested direct_ipp printers) reject raw PPM pixel data.

Every `direct_ipp` client config MUST have:
```toml
ghostscript_device = "jpeg"
ghostscript_resolution = 600   # Canon; use 360 for Epson L3260
```

**Why:** Ghostscript `ppmraw` produces ~100MB Portable Pixmap data per page which is NOT a printer language. Canon/Epson only accept `image/jpeg` via IPP. JPEG is also 200× smaller (~0.5MB vs ~99.6MB).

The default `ppmraw` device **NEVER works** with real printers. When setting up any new direct_ipp client, always set `ghostscript_device = "jpeg"`.

## Print verification — EventID 307 is the only reliable signal

NEVER claim a print job "completed" or "verified" based only on:
- Spooler queue status showing empty
- DevBridge job API returning "completed"
- gRPC dispatch success

These are unreliable. Jobs can show "completed" while paper never comes out.

**The correct verification by backend:**

| Backend | Verification |
|---|---|
| `windows_spooler` | Windows Print Service Operational log **EventID 307** (data physically delivered to printer port) |
| `direct_ipp` | IPP job-state must reach `"completed"` (not just `"processing"`) |
| `cups` | `lpstat` shows the job completed |

**Query EventID 307 on the client machine:**
```powershell
Get-WinEvent -LogName "Microsoft-Windows-PrintService/Operational" -FilterHashtable @{Id=307} -MaxEvents 5 | Select-Object TimeCreated, Message
```

Also always verify on the CLIENT side (not just server side): check client dashboard `/jobs`, check the physical printer queue.

## E2E tests must use realistic print flows

`Out-Printer` with plain text is NOT a real E2E test. Real users print documents (PDF, web pages, Print Test Page) which send XPS/EMF through the Windows spooler.

**Correct E2E spooler test:**
```powershell
# Print Test Page via IPP Class Driver (realistic XPS flow)
rundll32 printui.dll,PrintUIEntry /k /n "DevBridge"

# OR print an actual PDF:
& "C:\Program Files\SumatraPDF\SumatraPDF.exe" -print-to "DevBridge" -print-settings "noscale" "C:\path\to\test.pdf"
```

**Do NOT use:**
```powershell
"test" | Out-Printer -Name "DevBridge"  # Not realistic, doesn't exercise the full IPP pipeline
```
