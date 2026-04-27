# Tray RDP Filter Leak Fix — Design

**Issue:** [#42 — Tray leaks other users' print notifications on shared RDP server](https://github.com/zbynekdrlik/devbridge/issues/42)

**Date:** 2026-04-27

## Problem

On `pz-server` (Windows terminal server, multiple concurrent RDP sessions), each logged-in user runs their own DevBridge tray instance. Each tray must only display events for jobs whose `requesting_user` matches the Windows session's `USERNAME`. The current logic implements this correctly when it works, but **fails open** when service detection fails at tray startup.

### Bug chain

1. `tray.rs:67` `run_event_loop` calls `detect_filter_user` once at startup with a 5s HTTP timeout.
2. `tray.rs:190` `detect_filter_user` returns `None` on **any** error path: HTTP timeout, JSON parse failure, `mode != "server"`, missing `USERNAME` env. The same `None` value is also the legitimate value for client mode.
3. `job_tracker.rs:61` `should_process` interprets `filter_user = None` as "no filter, pass all events".
4. WebSocket events arrive after the failed detection → all RDP users see all notifications.

The same `filter_user = None` value also leaks via `fetch_initial_jobs` (`tray.rs:211`) which queries `/api/jobs?limit=5` (no filter) and populates the tray's "Recent Jobs" submenu with **all users' jobs**.

### Why detection fails in production

- Tray is launched from `HKLM:\Run` on user login.
- Service may not have started yet (S4U auto-start delay, related issue #36) or may be mid-restart.
- 5s timeout is short on a busy server with many concurrent logins.
- No retry — filter stays `None` for the lifetime of the tray session.

## Architecture

### Filter state — fail-closed by default

Replace the ambiguous `Option<String>` with a typed enum that distinguishes "client mode" from "detection not yet complete":

```rust
pub enum FilterState {
    /// Server mode and filter not yet detected. Drop ALL events. Default at startup.
    Pending,
    /// Client mode confirmed. No filtering — pass all events.
    Disabled,
    /// Server mode confirmed. Pass only events whose requesting_user matches.
    User(String),
}
```

`JobTracker::new()` initializes to `Pending`. `should_process`:

| State | requesting_user | Result |
|---|---|---|
| `Pending` | * | `false` (drop) |
| `Disabled` | * | `true` (pass) |
| `User(u)` | `Some(u')` | `u.eq_ignore_ascii_case(u')` |
| `User(u)` | `None` | `false` |

The type system makes "drop until known" the default. There is no way to forget the fail-closed check.

### Detection — dedicated retry task

`tray.rs::run_event_loop` spawns a separate `detect_filter_loop` task on startup:

```text
loop {
    GET /api/status (5s timeout)
    on success:
        if mode == "client":
            tracker.set_filter_state(Disabled); return
        if mode == "server":
            if let Ok(user) = env::var("USERNAME").or(env::var("USER")):
                tracker.set_filter_state(User(user)); return
            else:
                log error; sleep backoff; continue   // never happens on Windows
        else:
            log warn (unknown mode); sleep backoff; continue
    on HTTP error:
        log warn; sleep backoff; continue
}
```

Backoff: exponential, 1s → 2s → 4s → … capped at 30s. The loop retries forever until it transitions out of `Pending`, then exits.

**No re-detection on WebSocket reconnect.** Once `mode` and `USERNAME` are known, they don't change for the lifetime of either the service config or the user's RDP session. A WS reconnect (which `ws_client.rs` already handles internally with its own backoff loop) does not require re-running detection.

### Initial-jobs leak — gate on filter known

`fetch_initial_jobs` builds the API URL from the tracker's filter state. Currently it runs immediately and uses whatever filter is set at that instant — which is `Pending` (or the old `None`) before detection completes, leading to a no-filter URL.

Change: `fetch_initial_jobs` waits for the tracker to leave `Pending` before constructing the URL. Implementation: a `tokio::sync::Notify` exposed via `JobTracker`. `set_filter_state` calls `notify_one()`. `fetch_initial_jobs` awaits the notify, re-reads the state, and proceeds.

URL construction:

| State at fetch time | URL |
|---|---|
| `Disabled` | `/api/jobs?limit=5` |
| `User(u)` | `/api/jobs?limit=5&requesting_user={u}` |
| `Pending` | (impossible — gated by notify) |

### Service-online state and tray icon during Pending

The current tray icon shows Gray = "Offline" until `set_online(true)` is called after `fetch_initial_jobs` returns. With this change, that "Offline" period extends to cover the detection window. This is acceptable: an RDP user logging in while the service is starting will see "Offline" for a few seconds, then "Online" once detection succeeds and the WS connects. This matches reality and gives no leak window.

## Data flow

```
tray startup
  ├── spawn detect_filter_loop ──► /api/status retry loop
  │                                     ├── client mode  → set_filter_state(Disabled) → notify
  │                                     └── server mode  → read USERNAME → set_filter_state(User(u)) → notify
  │
  └── spawn run_event_loop
         ├── await tracker filter known (notify)
         ├── fetch_initial_jobs (with correct filter)
         ├── set_online(true)
         └── consume WS events, drop those that fail should_process
```

## Files touched

| File | Change |
|---|---|
| `crates/devbridge-app/src/job_tracker.rs` | Replace `filter_user: Option<String>` with `filter_state: FilterState`. Add `Notify` for state-change signaling. Update existing tests; add `Pending`-state tests. |
| `crates/devbridge-app/src/tray.rs` | Replace one-shot `detect_filter_user` with retry-loop `detect_filter_loop`. Gate `fetch_initial_jobs` on filter-known notify. Update `setup_tray` to spawn the detector task. |

No changes to `ws_client.rs`, `ipc_client.rs`, dashboard API, or wire protocol.

## Test strategy

### Unit (`job_tracker.rs`)

New tests:
- `pending_drops_all_events` — `FilterState::Pending` returns `false` for `should_process` regardless of `requesting_user` value (None or any string). **This is the regression assertion.**
- `disabled_passes_all_events` — `FilterState::Disabled` returns `true` regardless.
- `user_filter_matches_case_insensitive` — already exists; updated to use `User(...)`.
- `user_filter_drops_none_requesting_user` — `User("alice")` + `requesting_user = None` → `false`.
- `set_filter_state_notifies` — calling `set_filter_state` triggers the internal notify.

Updated tests: any existing test that constructed `JobTracker::new(None)` or `JobTracker::new(Some(...))` is migrated to the new enum.

### Integration (`tray.rs` tests, with `tokio::time::pause`)

- `detect_filter_loop_retries_until_service_responds` — mock HTTP server returns 503 three times then 200 with `{"mode":"server"}`. Set `USERNAME=alice` env. Use `tokio::time::pause` + `advance` to simulate backoff intervals deterministically. Assert tracker eventually transitions to `User("alice")`.
- `detect_filter_loop_handles_client_mode` — mock returns 200 `{"mode":"client"}` immediately. Assert tracker transitions to `Disabled`. USERNAME env is not consulted.
- `detect_filter_loop_handles_missing_username` — mock returns `mode=server`, `USERNAME` env unset. Assert loop logs warning and stays `Pending` (never transitions). Then set `USERNAME` and verify next iteration transitions correctly.
- `fetch_initial_jobs_waits_for_filter` — start `fetch_initial_jobs` before setting filter; assert it does not query the API. Set filter to `User("bob")`; assert it queries with `requesting_user=bob`.

Use `tokio = { features = ["test-util"] }` (already in dev-deps from the reconciler tests) for paused-time control.

### Real verification on `pz-server` (E2E)

Run after CI deploys the dev build. Two RDP sessions on `pz-server`:

1. **Service-already-running case**: `pjsnvs` and `pjpos` are both logged in with their tray apps running. `pjpos` triggers a print job. **Assert:** `pjsnvs` sees no notification AND `pjsnvs`'s tray "Recent Jobs" menu does not list `pjpos`'s job.
2. **Service-cold-start case** (the actual bug): stop the DevBridge service. Both users RDP in (or restart their tray apps). Service is started ~5s later. `pjpos` prints. **Assert** as above.

Verification evidence: tray DevBridge log file (`%LOCALAPPDATA%\DevBridge\tray.log` or wherever the tracing subscriber writes) showing `should_process = false` for the dropped events; absence of any `Print Job Received` log entry on `pjsnvs`'s tray.

## Out of scope

- **Re-detection on WS reconnect** — unnecessary; mode and USERNAME don't change for a session.
- **UX feedback during Pending window** — tray icon stays Gray ("Offline") which is accurate.
- **Issue #36 (S4U auto-start)** — root cause of slow service startup; separate issue, separate fix.
- **Migrating other `Option<String>` filters elsewhere in the codebase** — the dashboard's filter is unrelated.

## Acceptance criteria

- ✅ `JobTracker::should_process` returns `false` when `filter_state == Pending`, even if `requesting_user` is set.
- ✅ Tray detection retries with backoff until `/api/status` responds.
- ✅ `fetch_initial_jobs` does not query the API until the filter is known.
- ✅ Real-world test on `pz-server`: `pjsnvs` does not see `pjpos`'s notifications or jobs in tray, in both cold-start and warm cases.
- ✅ All existing tray/job_tracker unit tests still pass after migration to `FilterState`.
- ✅ Mutation testing has zero surviving mutants in the new code paths.
