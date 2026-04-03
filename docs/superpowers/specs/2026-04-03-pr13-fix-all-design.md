# PR #13 Fix-All: Security, Resource Leaks, Code Quality

**Date:** 2026-04-03
**PR:** #13 — Add direct print pipeline with full audit trail + dashboard redesign

## Context

Code review of PR #13 identified 23 issues (4 critical, 12 warning, 7 info). All must be fixed before merge.

## 1. Security — `.mcp.json` Token Leak (CRITICAL)

**Problem:** 6 Bearer tokens for production Windows MCP servers are committed to a public repo.

**Fix:**
- `git rm --cached .mcp.json` — remove from tracking, keep on disk
- Add `.mcp.json` to `.gitignore`
- Commit `.mcp.json.example` with placeholder tokens documenting the expected structure
- Tokens must be rotated on Windows machines separately (out of scope for this PR)

## 2. Resource Leaks (CRITICAL)

### 2a. Raster file leak on print failure

**Files:** `backend_direct_ipp.rs`, `backend_direct_raw.rs`

**Problem:** Rendered raster files (`.pwg`, `.raw`) are only cleaned up on success path. Error paths leave files on disk.

**Fix:** Add cleanup in both success and error paths. Use a guard pattern or explicit cleanup before each `bail!`.

### 2b. WebSocket connection leak on navigation

**Files:** `pages/dashboard.rs`, `ws_listener.rs`

**Problem:** `use_live_jobs()` spawns infinite `spawn_local` loops that never cancel. Each Dashboard visit adds another WS connection. Also, two independent WS connections exist per page load (app-level `ws_listener` + page-level `use_live_jobs`).

**Fix:**
- Remove the duplicate WS connection in `use_live_jobs()`. Instead, have the app-level `ws_listener` update shared signals that the dashboard reads.
- If `use_live_jobs` must keep its own connection, add `on_cleanup` with a cancellation flag to break the loop.

### 2c. Pending client spin-loop

**File:** `dispatch.rs:148-157`

**Problem:** Pending (unapproved) clients cause a tight 1-second poll loop querying SQLite.

**Fix:** Add a `tokio::sync::Notify` to the queue/state. Trigger it when pairing state changes (approval/rejection). The delivery task `notified().await`s instead of polling.

## 3. Backend Fixes (WARNING)

### 3a. gRPC connect timeout
**File:** `receiver.rs:71`
**Fix:** Add `.connect_timeout(Duration::from_secs(10))` to the endpoint builder.

### 3b. Reprint spool_path sharing
**File:** `api/jobs.rs:153`
**Fix:** Copy the spool file to a new path for the reprinted job so each job owns its own file.

### 3c. Mutex poisoning
**File:** `queue.rs`
**Fix:** Replace `.lock().unwrap()` with `.lock().expect("queue lock poisoned")` for clear diagnostics.

### 3d. Printer status mismatch
**File:** `printer.rs:204-222`
**Fix:** Add explicit match arms for status 3 (pending deletion → not ready) and 6 (manual feed → not ready) in `check_printer_ready`.

### 3e. Cargo audit justifications
**File:** `.github/workflows/ci.yml:127`
**Fix:** Add comments explaining why each `--ignore RUSTSEC-*` is acceptable.

## 4. Frontend Fixes (WARNING)

### 4a. N+1 events query
**Files:** `api.rs:203-216`, `api/job_events.rs`
**Fix:** Add batch endpoint `GET /api/jobs/events` returning all events keyed by job ID. Frontend fetches once instead of N times.

### 4b. WS state_changed stale timeline
**File:** `pages/dashboard.rs:108-128`
**Fix:** On `state_changed`, also append a synthetic event entry to the job's event list so the timeline updates without a full refetch.

### 4c. Service worker stale cache
**File:** `sw.js`
**Fix:** Include build version in `CACHE_NAME` (e.g., inject from environment or use a build-time hash).

### 4d. Reprint feedback banner
**File:** `pages/dashboard.rs:548-567`
**Fix:** Add `set_timeout` to clear the feedback signal after 5 seconds, matching toast behavior.

## 5. E2E Test Hardening (WARNING)

### 5a. Remove "old server" escape hatches
**File:** `devbridge-e2e/src/main.rs` (tests 24-30)
**Fix:** Remove silent-pass fallbacks. These features are now deployed; the tests must fail if the endpoints are missing.

### 5b. Test 25 WS timeout must fail
**Fix:** Change timeout from pass to fail — broken WS should not pass CI.

### 5c. `signal_e2e_done` hardcoded hostname
**File:** `devbridge-e2e/src/main.rs:939`
**Fix:** Read `E2E_CLIENT_HOST` env var instead of hardcoded `print-client.lan`.

### 5d. Spooler clear safety
**File:** `deploy/e2e-setup-client-local.ps1:146-151`
**Fix:** Check for active non-test print jobs before clearing the spooler.

## 6. Minor Cleanups (INFO)

### 6a. Extract `format_download_size`
Move from `receiver.rs` and `ipp_service.rs` to `devbridge-core`.

### 6b. Log render result in DirectIpp
Remove `_` prefix, log page count and size.

### 6c. Fix `std::mem::forget(dir)` in tests
Store `TempDir` in the returned test state struct instead of leaking.

### 6d. Add pagination to `get_all_jobs`
Add `?limit=N` query parameter (default 100).

### 6e. Check `clear_jobs` response status
Log on non-2xx response.

### 6f. Document `danger_accept_invalid_certs`
Add comment explaining this is intentional for Epson self-signed certs.

### 6g. E2E cleanup gate
Add `e2e-cleanup-client` result to `all-pass` gate (allow `skipped` or `success`).

## Out of Scope

- Token rotation on Windows machines (manual step, user will handle)
- Git history rewriting (tokens will be rotated instead)
