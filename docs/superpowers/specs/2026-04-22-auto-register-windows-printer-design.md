# Auto-Register Windows Shared Printer on Server — Design

**Issue:** [#44](https://github.com/zbynekdrlik/devbridge/issues/44)
**Date:** 2026-04-22
**Status:** Design

## Problem

When a DevBridge client registers a virtual printer with the server, the server creates the virtual-printer database row but does **not** register a Windows shared printer on the server host. RDP users on `pz-server` therefore cannot see the virtual printer in the Windows Print Dialog until an operator manually runs `Add-Printer`. This manual step was missed during the 0.8.15 rollout to five new stores.

Two related problems compound it:

- `installer/post-install.ps1` Step 5 races the service startup. If the service is not ready when the post-install script queries `/api/virtual-printers`, the script falls back to a legacy "single DevBridge printer" mode and creates the wrong printer.
- Past versions of post-install.ps1 deleted existing Windows printers during upgrade, which caused three printer-wiping incidents during 0.8.15 debugging.

## Goal

Eliminate every manual printer-registration step on the server. The service process is the single source of truth for Windows-printer state; it registers printers automatically and never deletes existing printers.

## Non-Goals

- Changing IPP protocol behavior, driver choice, or port layout (still `http://127.0.0.1:<ipp_port>/printers/<ipp_name>` via Microsoft IPP Class Driver).
- Handling printer deletion when a virtual printer is removed from the dashboard — orphaned Windows printers remain; the operator removes them manually.
- Replacing the existing `deploy/register-virtual-printers.ps1` script; it stays and is invoked by the service.

## Architecture

The devbridge-service process owns Windows-printer reconciliation. A new Rust module spawns the existing `register-virtual-printers.ps1` PowerShell script on two triggers:

1. **Once at service startup**, after storage is loaded — catches reboots, upgrades, and any drift between the database and the Windows spooler.
2. **On every virtual-printer database insert / update event** — catches new client registrations without requiring a service restart.

A debounce window (500 ms) coalesces a burst of events into a single PS1 invocation. The reconciler is fire-and-forget: PS1 exit code is logged but never crashes the service, because a failed reconciliation is strictly better than a down service.

Post-install.ps1 and the existing `DevBridgeReconcilePrinters` AtStartup scheduled task are retired. Post-install.ps1 gains an upgrade-cleanup step that unregisters the stale scheduled task on older installs.

### Component diagram

```
┌────────────────────────────────┐
│ devbridge-service (Rust)       │
│                                │
│  ┌──────────────────────────┐  │
│  │ storage::insert_virtual  │──┼──► tokio::mpsc::Sender<()>
│  │ _printer / update_*      │  │         │
│  └──────────────────────────┘  │         ▼
│                                │  ┌─────────────────────┐
│  ┌──────────────────────────┐  │  │ reconciler task     │
│  │ service::run() startup   │──┼──┤ - recv + debounce   │
│  └──────────────────────────┘  │  │ - spawn PS1         │
│                                │  │ - log exit code     │
└────────────────────────────────┘  └──────────┬──────────┘
                                               │ Command::spawn
                                               ▼
                                 ┌─────────────────────────────┐
                                 │ powershell.exe              │
                                 │ register-virtual-printers.ps1│
                                 │   -InputJson <temp>.json    │
                                 └──────────────┬──────────────┘
                                                │
                                                ▼
                                      Windows Print Spooler
                                      (Add-Printer,
                                       Microsoft IPP Class Driver)
```

## Components

### `crates/devbridge-server/src/printer_reconciler.rs` (new module)

Public API:

```rust
pub struct Reconciler {
    data_dir: PathBuf,
    script_path: PathBuf,
    tx: mpsc::Sender<()>,
}

impl Reconciler {
    pub fn new(data_dir: PathBuf) -> (Self, mpsc::Receiver<()>);
    pub fn signal(&self);                       // non-blocking, used by storage
    pub async fn run(self, rx: mpsc::Receiver<()>, storage: Arc<Storage>);
    async fn reconcile_once(&self, storage: &Storage) -> Result<()>;
}
```

`run()` is a Tokio task. It immediately calls `reconcile_once()` on entry, then loops: on each event from the receiver, wait 500 ms draining any additional events, then call `reconcile_once()` again. Guarantees: startup always runs; bursts coalesce; never panics out of the loop on reconcile failure.

`reconcile_once()`:

1. Calls `storage.list_virtual_printers()`.
2. Serializes the list to `<data_dir>/reconcile-input.json` (atomic write via rename).
3. On Windows: spawns `powershell.exe -NoProfile -ExecutionPolicy Bypass -File <script_path> -InputJson <json_path>` with a 60 s timeout.
4. Logs stdout/stderr at info/warn level; non-zero exit code is logged as `warn`, never propagated.
5. On non-Windows (`cfg(not(target_os = "windows"))`): logs `"printer reconciler skipped (not Windows)"` and returns Ok. Allows the Rust code path to compile and unit-test on Linux CI.

### `crates/devbridge-server/src/storage.rs` (modification)

- `Storage` gains an `Option<mpsc::Sender<()>>` field set via `with_reconciler_signal(sender)`.
- `insert_virtual_printer` and `update_virtual_printer` call `tx.try_send(())` after the SQL write succeeds. Existing call sites that don't configure a sender behave as before — this is additive.

### `crates/devbridge-service/src/runtime.rs` (modification)

During server-mode startup, after storage is initialized and before the IPP and gRPC listeners start accepting traffic:

1. `let (reconciler, rx) = Reconciler::new(data_dir.clone());`
2. `storage.with_reconciler_signal(reconciler.tx.clone());`
3. `tokio::spawn(reconciler.run(rx, storage.clone()));`

The reconciler runs concurrently with the rest of startup. Its first reconcile completes asynchronously; we do not block service boot on it.

### `deploy/register-virtual-printers.ps1` (minor contract change)

Add an optional `-InputJson <path>` parameter. When supplied, the script reads the virtual-printer list from that JSON file instead of calling `/api/virtual-printers` over HTTP. All other logic (driver-store repair, idempotent Add-Printer, printer-existence check by port) is unchanged. The HTTP fallback remains for compatibility with any caller that omits `-InputJson`, but the service always passes it.

The **never-delete invariant** is hardened: no `Remove-Printer` call exists in any code path. This is already the case in the current version; the spec codifies it.

### `installer/post-install.ps1` (modification)

- **Remove** Step 5 (printer-registration block that queries `/api/virtual-printers` and falls back to legacy mode).
- **Remove** Step 8 (scheduled-task registration for `DevBridgeReconcilePrinters`).
- **Keep** the copy of `register-virtual-printers.ps1` to `C:\Program Files\DevBridge\` (the service needs to find it).
- **Add** upgrade-cleanup: `Unregister-ScheduledTask -TaskName DevBridgeReconcilePrinters -Confirm:$false -ErrorAction SilentlyContinue`. Idempotent; runs on every install/upgrade.

### `installer/register-printer.ps1` (deletion)

Delete the file. It is a legacy single-printer registration script not used in the 0.8.x code path. Verify with `grep -r register-printer.ps1` that no caller remains before removal.

### `installer/install.ps1`

No changes — it already invokes post-install.ps1 which handles the migration.

## Data Flow

**Startup path:**

1. Service binary starts; loads config and storage.
2. Runtime constructs `Reconciler`, wires the sender into storage, spawns the task.
3. Reconciler task's first iteration calls `reconcile_once()`: reads virtual printers, writes JSON, spawns PS1.
4. PS1 enumerates Windows printers, adds any from the JSON not already present, never deletes.
5. PS1 exits; reconciler task parks on the channel receiver waiting for events.

**Event path (new client registration):**

1. Client connects via gRPC with `virtual_printer_name` set.
2. Server-side pairing logic inserts / updates the `virtual_printers` row.
3. `Storage::insert_virtual_printer` fires `tx.try_send(())`.
4. Reconciler task receives, waits 500 ms draining any follow-up signals, then calls `reconcile_once()`.
5. New Windows printer appears on pz-server within ~1 second of the client handshake.

## Error Handling

- **Script missing** (`register-virtual-printers.ps1` not in `C:\Program Files\DevBridge\`): log warning, return Ok. The service continues; Windows printers are absent but the dashboard still works, and operators can run the script manually.
- **PowerShell spawn failure** (e.g., PowerShell not on PATH): log warning, return Ok.
- **PS1 non-zero exit**: log stdout/stderr at warn, return Ok.
- **PS1 timeout** (>60 s): kill the child process, log warn, return Ok.
- **Storage read failure** during `reconcile_once`: log error, return Ok. Never tear down the reconciler task on a transient DB blip.
- **Channel closed** (storage dropped before reconciler): task exits cleanly.

## Platform Gating

- `crates/devbridge-server/src/printer_reconciler.rs` compiles on all platforms.
- The spawn-PowerShell body is gated with `#[cfg(target_os = "windows")]`.
- The non-Windows body logs `"printer reconciler skipped (not Windows)"` and returns Ok.
- Unit tests run on Linux CI and exercise the non-Windows stub plus the debounce / event-plumbing logic with a mocked spawner trait.

## Never-Delete Invariant

The script must not remove Windows printers under any condition:

- Reconciling a shorter virtual-printer list (user removed a printer from the dashboard) leaves any orphaned Windows printer in place.
- Reconciling with an empty list leaves all existing Windows printers in place.
- Drift between the DB and the Windows spooler is resolved only by **addition**, never by removal.

Operators who need to remove a Windows printer delete it through the Windows spooler UI or `Remove-Printer`. This is documented in the operator notes.

## Migration / Rollout

1. Merge → CI deploys 0.8.20 to pz-server and pz-snv via the existing main-branch CI pipeline.
2. On startup, the service runs the reconciler once; all six existing virtual printers on pz-server are asserted (and the currently-registered Windows printers match already, so the script is a no-op beyond enumeration).
3. post-install.ps1 upgrade-cleanup unregisters the `DevBridgeReconcilePrinters` scheduled task on pz-server.
4. Remaining five clients (pjkeb, pjpos, pjkes, pjkkb, pjsln) pick up the change on their next `irm install.ps1 | iex`.
5. Any new client installation with `DEVBRIDGE_VIRTUAL_PRINTER_NAME` auto-registers its Windows printer on the server without manual intervention.

## Testing

### Unit tests (`crates/devbridge-server/src/printer_reconciler.rs`)

- **Non-Windows stub**: `reconcile_once()` on Linux returns Ok and produces the expected log line (capture with `tracing-test`).
- **Debounce**: using a mock spawner trait, send 10 signals within 100 ms → exactly 1 spawn.
- **Startup always reconciles**: spawning the task and never sending an event → exactly 1 spawn after task starts.
- **Missing script**: point `script_path` at a non-existent file → returns Ok, logs warning.
- **Channel closed**: drop sender → task exits without panic.

### Integration tests (`crates/devbridge-server/tests/printer_reconciler.rs`)

- Storage configured with a reconciler sender: `insert_virtual_printer` delivers one signal; subsequent `insert_virtual_printer` delivers another.
- Storage without a reconciler sender (existing call sites): insert succeeds with no signal — no panic, no observable change.

### PowerShell smoke tests (`deploy/tests/register-virtual-printers.smoke.ps1`, invoked manually on pz-server)

- `-InputJson` happy path: JSON with 2 printers → both registered with correct port URLs.
- Idempotency: running twice → no duplicates, no errors.
- Never-delete: pre-seed a Windows printer not in the JSON → it survives the run.

### E2E verification on pz-server (post-deploy, manual checklist in the plan)

1. After CI deploy, dashboard at `http://10.88.1.100:9120` lists all 6 virtual printers.
2. `Get-Printer | Where-Object { $_.PortName -like "*127.0.0.1:631/printers/*" }` shows all 6 Windows printers registered.
3. On an idle test box, run `irm install.ps1 | iex` with `DEVBRIDGE_VIRTUAL_PRINTER_NAME=test-auto-reg` → approve pairing on dashboard → within 5 s, `Get-Printer` on pz-server shows `test-auto-reg` without any manual `Add-Printer` step.
4. Delete a virtual printer via dashboard → `Get-Printer` on pz-server still shows the corresponding Windows printer (never-delete invariant).
5. Reboot pz-server → after service starts, all 6 Windows printers are still present (driver-phantom repair from existing PS1 still runs).

### Playwright

No new Playwright tests. The dashboard surface doesn't change for this feature. Existing dashboard tests continue to run.

## Acceptance Criteria

- [ ] New client install with `DEVBRIDGE_VIRTUAL_PRINTER_NAME` → Windows printer appears on server automatically within 5 seconds, no manual step.
- [ ] Server upgrade via `irm install.ps1 | iex` → all existing Windows printers remain (never-delete invariant verified).
- [ ] Service restart → missing Windows printers re-registered from DB.
- [ ] `DevBridgeReconcilePrinters` scheduled task is unregistered on upgrade.
- [ ] `installer/register-printer.ps1` is removed from the repo.
- [ ] Post-install.ps1 no longer references `/api/virtual-printers` or registers Windows printers.
- [ ] Unit + integration + PS1 smoke tests pass.
- [ ] E2E verification checklist on pz-server passes.

## Out-of-Scope / Follow-Ups

- Dashboard action to delete a Windows printer when its virtual printer is removed from the DB (would need an opt-in UI with a confirmation step — not in this design).
- Reconciler on the client side (clients do not register shared printers; scope is server-only).
- Rust-native spooler API (via `windows` crate). This design spawns the existing PS1 for pragmatism; if PS1 becomes a maintenance burden, a follow-up can replace it module-for-module without changing the service orchestration.
