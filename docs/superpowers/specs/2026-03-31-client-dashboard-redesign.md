# Client Dashboard Redesign — Full Audit Trail with Identity

**Date:** 2026-03-31
**Status:** Approved
**Scope:** Fix the client dashboard to show clear identity, full end-to-end audit trail with printer names, and stream events back to server for unified view.

## Problem

The client dashboard shows raw technical data that doesn't help admins or managers understand the print flow:

- No client identity (which machine, which printer)
- Audit timestamps show only HH:mm, no seconds
- Audit shows IP addresses instead of printer names ("IPP Print-Job to 10.78.2.9:631")
- No server-side events (received, routed) — job appears to start printing from nowhere
- Server dashboard has no audit trail at all — events only stored on client
- Printer list shows only Windows printers, not the configured direct print target
- No connection status to server

## Solution

### 1. Config — Add `printer_display_name`

New optional field in `[client]` config:

```toml
[client]
printer_display_name = "Canon MG3600"
```

Used in dashboard header and audit event messages instead of raw IP. Falls back to `target_printer` if not set.

### 2. Server-Side Event Emission

The server emits events when jobs arrive and get routed. These are stored in the server's `job_events` table.

**In `ipp_service.rs` (`JobHandler::handle_document`):**
- Emit `received` event: `"Print job received (52KB)"`

**In `queue.rs` (`push()` after routing):**
- Emit `routed` event: `"pjsnvs printer → pz-snv (Canon MG3600)"`
  - Virtual printer display name, client hostname, and physical printer name (from client registration or virtual printer pairing metadata)

### 3. Client Reports Events to Server via gRPC Stream

The `ReportStatus` gRPC stream (already defined in proto) is used to send `PrintJobEvent` data back to the server in real-time.

**Flow:**
1. Client's event persistence task (in `receiver.rs`) stores events locally AND streams them to server
2. Server receives events via `ReportStatus` stream handler in `dispatch.rs`
3. Server stores received client events in its own `job_events` table
4. Server dashboard now has the complete timeline: server events + client events

**Proto change:** Add a new `JobStatusUpdate` variant or use the existing `message` field to carry serialized `PrintJobEvent` JSON. The `state` field maps to the current job state, the `message` field carries the event detail.

### 4. Client Dashboard Header

Always visible at the top of the client dashboard:

```
DevBridge Client: pz-snv
Printer: Canon MG3600 (10.78.2.9) — direct_ipp
Server: 10.88.1.100 — connected
```

**Data sources:**
- Client name: `client_id` from config (already exists)
- Printer name: `printer_display_name` from config (new)
- Printer address: `printer_address` from config
- Backend type: `print_backend` from config
- Server address: `server_address` from config
- Connection status: from `/api/status` `connected_clients` or gRPC stream state

### 5. Audit Timeline Format

Every job shows its full timeline inline on both dashboards. Timestamp format: **HH:mm:ss** (seconds precision, no milliseconds).

```
job-fa5b5ee2 — completed                                09:36:41
  09:36:37  OK  received       Print job received (52KB)
  09:36:37  OK  routed         pjsnvs printer → pz-snv (Canon MG3600)
  09:36:38  OK  downloading    Client started payload download
  09:36:39  OK  downloaded     SHA256 verified (52KB, 1.1s)
  09:36:41  OK  rendering      Ghostscript jpeg 600dpi
  09:36:43  OK  rendered       1 page, 0.6MB, 1.9s
  09:36:43  OK  sending        IPP Print-Job → Canon MG3600 (10.78.2.9)
  09:36:43  OK  acknowledged   printer job-id=5, processing
  09:37:04  OK  completed      Canon MG3600 confirmed printed
```

Failed job shows exactly where it broke:

```
job-3429e50a — failed                                    08:07:45
  08:07:44  OK  received       Print job received (588B)
  08:07:44  OK  routed         pjsnvs printer → pz-snv (Canon MG3600)
  08:07:45  OK  rendering      Ghostscript urfrgb 600dpi
  08:07:46  FAIL failed        Ghostscript exit code 1: unknown device
```

### 6. Event Detail Messages — Use Printer Names

Audit event messages must use human-readable printer names, not raw IPs:

| Stage | Current message | New message |
|-------|----------------|-------------|
| received | (not emitted) | `Print job received (52KB)` |
| routed | (not emitted) | `pjsnvs printer → pz-snv (Canon MG3600)` |
| sending (IPP) | `IPP Print-Job to 10.78.2.9:631` | `IPP Print-Job → Canon MG3600 (10.78.2.9)` |
| sending (spooler) | `Windows spooler to Microsoft Print to PDF` | same (already uses name) |
| completed (IPP) | `printer job-id=5, state=completed` | `Canon MG3600 confirmed printed` |
| completed (spooler) | `printed via Windows spooler to X` | same (already uses name) |

The `printer_display_name` config value is passed to the PrintBackend so it can use the name in event messages.

### 7. Printer List Page on Client

Currently shows only Windows printers. For direct backends, the physical printer is not a Windows printer. The printer list should show the configured target printer:

```
Printers
  Canon MG3600 (10.78.2.9)    direct_ipp    target
```

Data comes from config fields, not Windows `Get-Printer`. Show backend type and mark as target.

### 8. Server Dashboard — Full Timeline

The server dashboard's job list should also show audit timelines (same as client). Since the server now has all events (server-side + client-reported), each job shows the complete flow.

### 9. Timestamp Component Fix

The `TimeOnly` component currently formats as HH:mm. Change to **HH:mm:ss** to show seconds precision in audit timelines.

For the job header row, keep HH:mm (less noisy). Only the audit timeline rows use HH:mm:ss.

Create a new `TimeWithSeconds` component or add a `seconds` prop to `TimeOnly`.

## Files to Modify

### Backend
- `crates/devbridge-core/src/config.rs` — Add `printer_display_name: Option<String>` to `ClientConfig`
- `crates/devbridge-server/src/ipp_service.rs` — Emit `received` event in `handle_document()`
- `crates/devbridge-server/src/queue.rs` — Emit `routed` event in `push()` after routing
- `crates/devbridge-server/src/dispatch.rs` — Handle incoming client events via `ReportStatus` stream, store in `job_events`
- `crates/devbridge-client/src/receiver.rs` — Stream events to server via `ReportStatus` in addition to local storage
- `crates/devbridge-client/src/backend_direct_ipp.rs` — Use `printer_display_name` in event messages
- `crates/devbridge-client/src/backend_direct_raw.rs` — Use `printer_display_name` in event messages
- `crates/devbridge-client/src/backend_windows_spooler.rs` — Use `printer_display_name` in event messages
- `crates/devbridge-client/src/print_backend.rs` — Add `printer_display_name` to `PrintJobInfo`

### Frontend
- `crates/devbridge-ui/src/pages/dashboard.rs` — Add identity header to client view, show timeline on server view
- `crates/devbridge-ui/src/components/time_display.rs` — Add `TimeWithSeconds` component (HH:mm:ss)
- `crates/devbridge-ui/src/pages/printers.rs` — Show configured direct printer on client mode
- `crates/devbridge-ui/src/api.rs` — Add `fetch_client_info()` for header data (or extend `/api/status`)

### Config
- `config/default.toml` — Add `printer_display_name` comment
- `installer/post-install.ps1` — Add `printer_display_name` to client config template

### API
- Extend `GET /api/status` with `client_id`, `printer_display_name`, `printer_address`, `print_backend`, `server_address` fields (client mode only)

## Testing

### Unit Tests
- Config parsing with `printer_display_name` (present and absent)
- Event detail messages use printer name when available
- Server stores client-reported events

### E2E Tests
- Print job → server has complete timeline (received through completed)
- Client dashboard header shows identity info
- Audit timestamps show seconds

### Post-Deploy Verification
- pz-snv (10.78.2.10:9120): header shows "Canon MG3600 (10.78.2.9) — direct_ipp"
- pz-server (10.88.1.100:9120): job timeline shows all stages including client events
- Timestamp shows HH:mm:ss in audit rows
