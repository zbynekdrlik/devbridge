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
  - hp laserjet
  - PCLm
  - urf
  - pagecount
  - title-case headers
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

## HP LaserJet M1xx (M110w) — needs image/urf (urfgray); PCLm is silently discarded

Verified live 2026-09-09, store pjzav, HP LaserJet M110w at `10.78.9.9`
(product `7MD66A`, firmware `20250324`, `ipp://10.78.9.9:631/ipp/print`).
The `jpeg` rule above is Canon/Epson-specific — it does **NOT** apply to
this printer family.

**PCLm is a trap on this printer (issue #71).** The M110w *advertises*
`application/PCLm` via IPP, *accepts* a PCLm Print-Job, and reports
`job-state 9 completed` within the same second — but prints **nothing**:
`job-impressions-completed` stays `0`, the PJL `@PJL INFO PAGECOUNT` counter
stays `0`, and `DevMgmt/ProductUsageDyn.xml`'s `PrinterSubunit/TotalImpressions`
stays `0`. This is true for Ghostscript's `pclm` device AND for Windows'
Microsoft IPP Class Driver (which also picks PCLm for this printer) — it is
not a DevBridge-specific bug, the printer itself silently discards the job.
PJL `@PJL INFO CONFIG` lists supported page-description languages as
`PWG_RASTER`, `URP` only — PCLm isn't even in that list, despite IPP
advertising it.

**What actually prints:** `image/urf` via Ghostscript's `urfgray` device.
`urfgray -r600` produced job 19, `job-state 9 completed`, `PAGECOUNT` went
`0 → 1`, `TotalImpressions` went `0 → 1` — genuine physical output.

Every `direct_ipp` client config targeting an HP LaserJet M1xx MUST have:
```toml
ghostscript_device = "urfgray"
ghostscript_resolution = 600
```
(DevBridge maps `urfgray` → `image/urf`.)

**Note:** the bundled Ghostscript 10.04 has **no `pwgraster` device** — only
`pclm`, `pclm8`, `urfgray`, `urfrgb`, `jpeg`, etc. Don't reach for
`pwgraster` expecting PWG Raster; use `urfgray` (or `urfrgb` for colour).

**Header case-sensitivity (issue #71) — still required.** the M110w's
embedded HTTP parser treats header *names* case-sensitively. hyper
(reqwest's HTTP/1.1 engine) sends header names lowercase by default —
`content-type:`, `content-length:`, `accept:`, `host:`. The M110w accepts
the Print-Job request (assigns a job-id, job-state 3 pending) but never
recognizes the lowercase `content-length:`, so it never reads the document
body, and aborts the job (job-state 8, aborted-by-system). A byte-identical
request with `Content-Type:` / `Content-Length:` / `Accept:` / `Host:`
title-cased completes normally (job-state 9). Canon/Epson tolerated
lowercase headers, which is why `direct_ipp` worked for those printers
before this was found.

**DevBridge ≥ 0.8.32 title-cases HTTP/1.1 headers for every `direct_ipp`
request** (`reqwest::blocking::ClientBuilder::http1_title_case_headers()`,
in `crates/devbridge-client/src/backend_direct_ipp.rs`'s shared
`http_client` helper). A client on an older version printing to an HP
LaserJet will see jobs accepted then silently aborted — upgrade it.

**Verification rule — `job-state 9 completed` is NOT proof of output on
HP.** The PCLm trap above shows the IPP job-state alone is worthless for
this printer family: it reaches `completed` whether or not anything printed.
Before/after every test, check one of:
- PJL page counter over port 9100: send
  `` ESC%-12345X@PJL INFO PAGECOUNT\r\nESC%-12345X `` (raw TCP to port 9100)
  and read the returned count.
- `http://<printer-ip>/DevMgmt/ProductUsageDyn.xml` →
  `PrinterSubunit/TotalImpressions`.

A job is only genuinely printed if one of these counters incremented.

**Diagnostic scripts on pjsln** (`C:\ProgramData\DevBridge\`):
- `ipp-printer-all.ps1` — Get-Printer-Attributes, all attributes (confirms
  supported document formats)
- `ipp-jobs-all.ps1` — Get-Jobs, all attributes (confirms job-state / abort
  reason)
- `ipp-print2.ps1` — Print-Job with any MIME type + polls job-state, for
  ad-hoc format testing
- `ipp-rawreplay-title.ps1` — raw HTTP replay of a captured Print-Job request
  with headers forced Title-Case, to reproduce/confirm the header fix
  independent of the DevBridge binary

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
| `direct_ipp` | IPP job-state must reach `"completed"` (not just `"processing"`) — **exception: HP LaserJet M1xx, see below** |
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
