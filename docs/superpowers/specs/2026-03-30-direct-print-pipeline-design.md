# Direct Print Pipeline with Full Audit Trail

**Date:** 2026-03-30
**Status:** Approved
**Scope:** Replace SumatraPDF+Windows Spooler black box with a fully audited, direct-to-printer pipeline using Ghostscript for rendering and native network protocols for delivery.

## Problem

The current print flow has a black box between the DevBridge client and the physical printer:

```
Server → gRPC → Client → [SumatraPDF → Windows Spooler → ???] → Printer
```

After SumatraPDF is called, DevBridge has no visibility into what happens. The `printer_status` and `spooler_status` fields in the proto exist but are never populated. The spooler verification polls for 60 seconds but can only report "empty queue" (assumed success), "error state", or "timeout" — none of which confirm ink hit paper.

## Solution

A new print pipeline where DevBridge controls every step, from PDF rendering to network delivery, with each step recorded as an auditable event.

```
Server → gRPC → Client → Ghostscript → Direct network send → Printer
                  ↑ event    ↑ event       ↑ event            ↑ event
```

## Architecture

### Print Backends

A `PrintBackend` trait with three implementations, selected per-printer in config:

```rust
trait PrintBackend {
    async fn print(&self, job: &PrintJob, pdf_path: &Path, events: &EventEmitter) -> Result<()>;
}
```

| Backend | Config value | Rendering | Delivery | Use case |
|---|---|---|---|---|
| `DirectIpp` | `direct_ipp` | Ghostscript → PWG-Raster | HTTP POST IPP Print-Job | Canon MG3600, any IPP printer |
| `DirectRaw` | `direct_raw` | Ghostscript → raw raster | TCP socket port 9100 | Epson L3270, RAW-capable printers |
| `WindowsSpooler` | `windows_spooler` | SumatraPDF (existing) | Windows print spooler | Fallback for unsupported printers |

### Configuration

```toml
[client]
target_printer = "EPSON L3270"
print_backend = "direct_raw"          # "direct_ipp" | "direct_raw" | "windows_spooler"
printer_address = "10.78.5.9:9100"    # IP:port for direct backends

# Ghostscript settings (direct backends only)
ghostscript_device = "ppmraw"         # "pwgraster" for IPP, "ppmraw" for RAW
ghostscript_resolution = 600          # DPI
```

All new fields use `#[serde(default)]` so existing configs without them default to `windows_spooler` (backward compatible).

## Audit Trail — Job Events

### Event Stages

Every print job records a timeline of events, each with timestamp, stage, success/failure, and detail string.

| Stage | Meaning | Recorded by |
|---|---|---|
| `received` | Server accepted IPP job | Server |
| `routed` | Matched to virtual printer + client | Server |
| `downloading` | Client started payload download | Client |
| `downloaded` | Payload complete, SHA256 verified | Client |
| `rendering` | Ghostscript started PDF→raster conversion | Client |
| `rendered` | Raster output ready (format, size, pages) | Client |
| `sending` | Data transmission to printer started | Client |
| `sent` | All bytes delivered to printer | Client |
| `acknowledged` | Printer confirmed receipt (IPP only) | Client |
| `completed` | Job finished successfully | Client |
| `failed` | Error at any stage | Client/Server |
| `retrying` | Requeued for retry | Server |

### Storage

New SQLite table alongside existing `jobs`:

```sql
CREATE TABLE job_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id TEXT NOT NULL,
  stage TEXT NOT NULL,
  success INTEGER NOT NULL,
  detail TEXT DEFAULT '',
  timestamp TEXT NOT NULL,
  FOREIGN KEY (job_id) REFERENCES jobs(job_id)
);
CREATE INDEX idx_job_events_job_id ON job_events(job_id);
```

### API

- `GET /api/jobs/{id}/events` — returns ordered timeline for a job
- WebSocket broadcasts each event as it happens (extends existing `JobEvent` enum)
- `JobCompletion` proto message now populates `printer_status` and `spooler_status` with real data

## Ghostscript Integration

### Bundling

Ghostscript portable (~30MB) is downloaded during the CI Windows Build step and bundled into the NSIS installer at `C:\Program Files\DevBridge\ghostscript\gswin64c.exe`. No system-wide install, no registry, no PATH modification.

### Rendering Call

```
gswin64c.exe -dNOPAUSE -dBATCH -dSAFER
  -sDEVICE=<device> -r<resolution>
  -sOutputFile=<output_path> <input_pdf>
```

### Device Selection

| Target Printer | Ghostscript device | Output format |
|---|---|---|
| Canon MG3600 (IPP) | `pwgraster` | PWG-Raster |
| Epson L3270 (RAW) | `ppmraw` | Portable Pixmap |
| Fallback | n/a | PDF via SumatraPDF |

### Tracked During Rendering

- Ghostscript exit code (0 = success)
- Output file size (must be > 0)
- Page count (parsed from Ghostscript stderr: `Page 1`, `Page 2`, etc.)
- Duration (start→finish)
- All captured in `rendered` event: `"3 pages, 4.2MB, 1.3s, device=pwgraster"`

### Error Handling

- Ghostscript not found → `failed` event, fall back to `windows_spooler` if available
- Corrupt PDF → Ghostscript non-zero exit → `failed` event with stderr
- Disk full → output file missing/empty → `failed` event

## Direct IPP Print-Job (Canon Path)

### Flow

1. Ghostscript renders PDF → PWG-Raster temp file
2. Open HTTP connection to printer's IPP endpoint (e.g., `http://10.78.2.9:631/ipp/print`)
3. Build IPP `Print-Job` request (operation 0x0002) with attributes:
   - `document-format`: `image/pwg-raster`
   - `copies`, `sides`, `printer-uri`
4. Append PWG-Raster file as request body
5. Parse IPP response: printer's `job-id`, `job-state`, `job-state-reasons`
6. Poll `Get-Job-Attributes` every 2s for up to 60s to confirm `completed` state

### What IPP Gives Us (not available today)

- Printer's own job-id for correlation
- `job-state-reasons`: `none`, `media-empty`, `marker-supply-low`, `document-format-error`
- Confirmation that the printer processed the job (not just received it)

### Audit Events

```
rendering    → "Ghostscript pwgraster 600dpi started"
rendered     → "3 pages, 4.2MB, 1.3s"
sending      → "IPP Print-Job to 10.78.2.9:631"
acknowledged → "printer job-id=42, state=processing"
completed    → "printer job-id=42, state=completed"
```

### Implementation

Use `reqwest` (existing dependency) for HTTP. IPP binary encoding/decoding implemented manually — the format is straightforward (~200 lines). No new crate needed.

## Direct RAW Print (Epson Path)

### Flow

1. Ghostscript renders PDF → PPM raster temp file
2. Open TCP socket to printer:9100
3. Stream all bytes to socket
4. Close socket, verify clean close (no TCP RST)

### Limitations

RAW protocol has no feedback channel. We can confirm:
- TCP connection established (printer alive)
- All bytes written (no TCP errors)
- Socket closed cleanly (not reset)

We cannot confirm:
- Printer actually printed the pages
- Paper jam, out of ink, offline status

### Audit Events

```
rendering  → "Ghostscript ppmraw 600dpi started"
rendered   → "3 pages, 12.1MB, 2.1s"
sending    → "RAW TCP to 10.78.5.9:9100, 12.1MB"
sent       → "12.1MB delivered, socket closed cleanly"
completed  → "delivered to printer (no ACK via RAW)"
```

The `completed` event is honest — "delivered" not "printed".

### Epson RAW Format Risk

Epson inkjets on port 9100 may not accept arbitrary PPM raster — they may require ESC/P-R (proprietary) or PCL. During implementation, we must test `ppmraw` output against the actual Epson L3270. If it doesn't print correctly, alternatives:
1. Try Ghostscript `epsonfx` device (ESC/P dot matrix — may work for basic output)
2. Try `pcl3` device (some Epson inkjets accept PCL)
3. Fall back to `windows_spooler` for Epson until a working device is found

The direct_raw backend is designed to be device-agnostic — changing the Ghostscript device is a config change, not a code change.

### Future Improvement

The Epson has port 631 open but requires TLS (426 Upgrade Required). If the IPPS endpoint can be discovered (non-standard path), we could upgrade to IPP with full job tracking. This is a future investigation, not part of this spec.

## Dashboard Job Detail View

Clicking a job in the dashboard opens a timeline view:

```
Job: Invoice_2026-03-30.pdf
Printer: pjpos printer → EPSON L3270 (pjpos-client)

  14:01:03.120  ✅ received      Server accepted IPP job (234KB)
  14:01:03.250  ✅ routed        → pjpos-printer → pjpos-client
  14:01:03.890  ✅ downloading   Payload transfer started
  14:01:05.102  ✅ downloaded    SHA256 verified (234KB, 1.2s)
  14:01:05.150  ✅ rendering     Ghostscript ppmraw 600dpi
  14:01:06.480  ✅ rendered      3 pages, 12.1MB, 1.3s
  14:01:06.510  ✅ sending       RAW TCP to 10.78.5.9:9100
  14:01:08.230  ✅ sent          12.1MB delivered, socket closed
  14:01:08.240  ✅ completed     Delivered to printer
```

Failed job shows exactly where it broke:

```
  14:01:05.150  ✅ rendering     Ghostscript ppmraw 600dpi
  14:01:05.400  ❌ failed        Ghostscript exit code 1: /tmp/job.pdf is corrupted
```

## Tray App Notifications

Extend existing tray app toast system with key events only (not every intermediate step):

- **Job received:** `"Invoice.pdf → EPSON L3270"`
- **Completed:** `"Invoice.pdf printed successfully"`
- **Failed:** `"Invoice.pdf FAILED: printer offline"`

## WebSocket Events

Extend existing `JobEvent` enum to carry the new event stages. Client dashboard auto-updates the job detail timeline in real-time.

## Backward Compatibility

- All new config fields default to `windows_spooler` behavior via `#[serde(default)]`
- Existing deployments without config changes continue using SumatraPDF
- Migration is per-printer: change `print_backend` in config, restart service
- `windows_spooler` backend also gains audit events (wraps existing SumatraPDF flow with event emission at each stage)

## Target Printer Configuration

| Printer | Location | Backend | Device | Address |
|---|---|---|---|---|
| Canon MG3600 | pz-snv | `direct_ipp` | `pwgraster` | `10.78.2.9:631` |
| Epson L3270 | pjpos | `direct_raw` | `ppmraw` | `10.78.5.9:9100` |
| Microsoft Print to PDF | CI test | `windows_spooler` | n/a | n/a |

## Testing

### Unit Tests
- IPP request/response encoding/decoding
- Ghostscript output parsing (page count, exit code)
- Event emitter (correct stages, timestamps, ordering)
- Config parsing with new fields (backward compat)

### Integration Tests
- Full pipeline with mock printer (TCP listener that accepts data)
- Ghostscript rendering a real PDF → verify output is valid PWG-Raster/PPM
- Event timeline completeness (all stages present for success/failure paths)

### E2E Tests
- Print to Canon via IPP → verify job completed on printer
- Print to Epson via RAW → verify data delivered
- Fallback: direct backend fails → windows_spooler takes over (if configured)
- Dashboard: job detail view shows full timeline
- WebSocket: events arrive in real-time during print

### Post-Deploy Verification
- Print test page from pz-server "pjpos printer" → verify Epson prints via direct_raw
- Print test page from pz-server "pjsnvs printer" → verify Canon prints via direct_ipp
- Check dashboard timeline shows all stages with timestamps
- Verify tray app shows received/completed notifications
