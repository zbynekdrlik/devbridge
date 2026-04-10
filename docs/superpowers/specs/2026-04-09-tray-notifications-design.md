# Tray App Print Notifications & Job Tracking

**Issue:** #11
**Date:** 2026-04-09
**Status:** Approved

## Goal

Add real-time print job notifications, status tracking, and job history to the DevBridge tray app. Employees at store branches see when documents arrive and print (or fail). RDP users on the terminal server see only their own jobs.

## Architecture

### Data Pipeline: IPP User & Job Name Capture

Currently DevBridge discards the IPP `requesting-user-name` and `job-name` attributes. Both must be captured to enable per-user filtering and meaningful notification text.

**Changes:**

1. **`crates/devbridge-server/src/ipp_service.rs`** — extract `requesting-user-name` and `job-name` from IPP job attributes in `JobHandler::handle_document()`
2. **`crates/devbridge-core/src/job.rs`** — add `requesting_user: Option<String>` and update `document_name` to use the real IPP job name (fallback to `job-{uuid}` if absent)
3. **`crates/devbridge-server/src/storage.rs`** — incremental migration: `ALTER TABLE jobs ADD COLUMN requesting_user TEXT`
4. **`crates/devbridge-dashboard/src/api/jobs.rs`** — expose `requesting_user` in `/api/jobs` response
5. **WebSocket events** — include `requesting_user` in `JobEvent` and `PrintJobEvent` broadcasts

### Real-Time Communication: WebSocket

The tray app connects to the existing `/api/ws` endpoint for instant event delivery.

**Flow:**
1. On startup, fetch `/api/jobs?limit=5` to populate the tray menu
2. Connect WebSocket to `/api/ws`
3. On each event: update menu, show balloon, update icon
4. On disconnect: exponential backoff reconnect (same pattern as gRPC client)

### Per-User Filtering (Terminal Server)

On the server (terminal server with RDP users), each user's tray instance filters by their Windows username:

1. On startup, detect `mode == "server"` from `/api/status`
2. Get current Windows username via `std::env::var("USERNAME")`
3. Filter `/api/jobs?requesting_user=alice` — server-side query parameter filters jobs by username
4. Filter WebSocket events client-side by matching `requesting_user` field in event payload

On client machines (single user per machine), no filtering — all jobs are shown.

## Tray Menu Layout

```
┌──────────────────────────────────┐
│ RECENT JOBS                      │
│ ✓ Invoice_March.pdf      2m ago  │
│ ✓ Receipt_0412.pdf      15m ago  │
│ ✗ Report_Q1.pdf          1h ago  │
│ ⏳ Label_batch.pdf        1h ago  │
│ ✓ PO_2026_041.pdf        2h ago  │
├──────────────────────────────────┤
│ 🖨 Open Dashboard                │
├──────────────────────────────────┤
│ ● Online — Canon MG3600 — v0.8  │
│ ▶ Start   ■ Stop                 │
│ Quit                             │
└──────────────────────────────────┘
```

**Job status icons:**
- `✓` — completed (green)
- `✗` — failed (red)
- `⏳` — in progress / pending (yellow)

## Tray Icon States

| State | Color | Trigger | Clears when |
|-------|-------|---------|-------------|
| Idle/OK | Green | Default, after successful job | — |
| Printing | Yellow | Job received or printing | Job completes or fails |
| Error | Red | Job failed | Next successful job or user opens dashboard |
| Offline | Gray | Service unreachable | Service responds again |

Four icon variants bundled in `assets/icons/`: `tray-icon-green.png`, `tray-icon-yellow.png`, `tray-icon-red.png`, `tray-icon-gray.png`.

## Balloon Notifications

Lightweight system tray balloons via `tauri-plugin-notification`. Shown for key job transitions:

| Event | Client message | Server message |
|-------|---------------|----------------|
| Job received | `Receiving: {document_name}` | `Sent: {document_name} → {printer_name}` |
| Printing | `Printing: {document_name} → {printer}` | — |
| Completed | `Printed: {document_name} ✓` | `Delivered: {document_name} → {client_id}` |
| Failed | `Failed: {document_name} — {error}` | `Failed: {document_name} — {error}` |

## Implementation Stack

- **WebSocket client:** `tokio-tungstenite` in background Tokio task, events via `mpsc` channel
- **Tray menu updates:** Tauri `tray.set_menu()` for runtime menu rebuilding
- **Notifications:** `tauri-plugin-notification` (cross-platform native balloons)
- **Icon switching:** `tray.set_icon()` with bundled icon variants
- **Username detection:** `std::env::var("USERNAME")` on Windows, `std::env::var("USER")` on Unix

No new ports, no new services. Everything goes through the existing dashboard HTTP/WS API.

## Database Migration

Incremental migration (production data exists on deployed machines):

```sql
ALTER TABLE jobs ADD COLUMN requesting_user TEXT;
```

Nullable — existing jobs will have `NULL` requesting_user. New jobs will have the IPP username populated.

## Scope Exclusions

- No job actions from tray (reprint, cancel, delete) — dashboard only
- No sound notifications
- No notification preferences/settings UI
- No macOS notification center integration beyond standard Tauri API

## Testing

- **Unit tests:** WebSocket event filtering by username, icon state machine transitions, menu building from job list
- **Integration tests:** IPP job with `requesting-user-name` → stored in DB → exposed in API → delivered via WebSocket
- **Playwright E2E:** Verify `requesting_user` appears in dashboard job list and API responses
- **Manual verification:** Balloon notifications on pz-snv/pz-holla after deploy, per-user filtering on pz-server via RDP
