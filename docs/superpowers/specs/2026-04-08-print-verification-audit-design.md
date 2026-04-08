# Print Verification & Audit Evidence

**Date:** 2026-04-08
**Issues:** #19 (False 'completed' status), #20 (Print audit log)
**Status:** Approved

## Problem

DevBridge marks jobs as "completed" based on spooler acceptance or gRPC
completion reports, without verifying that paper actually came out of the
printer. This led to repeated incidents where the dashboard showed green
checkmarks while printers never printed. Jobs were silently re-dispatched
to wrong clients, and false "completed" status wasted hours of user time.

## Goal

Every print job must carry machine-verifiable evidence of physical delivery
before being marked completed. Verification failures trigger auto-retry via
the existing retry mechanism. The audit trail shows exactly what happened,
including which client completed the job and what physical evidence exists.

## Design

### 1. Proto Changes (`proto/devbridge.proto`)

Extend `JobCompletion` with three new fields:

```proto
message JobCompletion {
  string job_id = 1;
  bool success = 2;
  string error_detail = 3;
  uint32 pages_printed = 4;
  string printer_status = 5;
  string spooler_status = 6;
  string verification_method = 7;   // "eventid_307", "ipp_job_state", "cups_lpstat", "virtual_printer", "none"
  string verification_evidence = 8; // Human-readable proof string
  string client_id = 9;             // Which client completed this job
}
```

Field semantics:
- `verification_method`: Machine-readable tag identifying how delivery was confirmed.
- `verification_evidence`: Human-readable proof string for the audit trail.
- `client_id`: The machine_id of the client that executed the print. Used by
  the server to detect misrouted completions.

### 2. Event Enrichment (`devbridge-core/src/job_event.rs`)

Add two fields to `PrintJobEvent`:

```rust
pub verification_method: String,
pub verification_evidence: String,
```

Add a new variant to `PrintStage`:

```rust
pub enum PrintStage {
    Received,
    Routed,
    Downloading,
    Downloaded,
    Rendering,
    Rendered,
    Sending,
    Sent,
    Verified,      // NEW — physical delivery confirmed
    Acknowledged,
    Completed,
    Failed,
    Retrying,
}
```

`Verified` is emitted between `Sent` and `Completed` when the backend has
physical evidence of delivery. If verification fails, `Failed` is emitted
with the specific error and the existing retry mechanism kicks in.

### 3. Backend Verification

#### windows_spooler

After SumatraPDF submits to the Windows spooler:

1. Ensure `Microsoft-Windows-PrintService/Operational` log is enabled
   (one-time `wevtutil sl` call, idempotent).
2. Poll for EventID 307 matching the target printer name.
   - Interval: 2 seconds
   - Timeout: 60 seconds
   - Match criteria: `$_.Id -eq 307 -and $_.Message -match "<printer_name>"`
3. On EventID 307 found:
   - Emit `PrintStage::Verified` event
   - `verification_method = "eventid_307"`
   - `verification_evidence = "EventID 307: Document <N>, <printer>, <port>, <size>"`
   - Return `Ok(())`
4. On timeout (no EventID 307):
   - Check for error events (EventID 372, 842) for specific failure reason
   - Emit `PrintStage::Failed` event with error detail
   - Return `Err("No physical delivery confirmation (EventID 307) within 60s for <printer>. <error_detail>")`
5. Virtual printers (PDF, XPS, Fax, OneNote):
   - Skip EventID 307 verification
   - `verification_method = "virtual_printer"`
   - `verification_evidence = "Virtual printer — no physical delivery"`

Implementation: The backend runs a PowerShell command via `std::process::Command`
to query the event log. This is Windows-only code behind `#[cfg(target_os = "windows")]`.

On Linux (CI), the windows_spooler module compiles but is not exercised. The
EventID 307 verification code is behind the same `#[cfg(target_os = "windows")]`
gate and does not affect CI.

#### direct_ipp

Already polls `Get-Job-Attributes` for completion. Enrich the existing flow:

- On IPP job-state 9 (completed):
  - Emit `PrintStage::Verified`
  - `verification_method = "ipp_job_state"`
  - `verification_evidence = "IPP job-state=9 (completed), job-id=<N>"`
- On IPP job-state 7 (canceled) or 8 (aborted):
  - Emit `PrintStage::Failed`
  - `verification_evidence = "IPP job-state=<N> (<state_name>), job-id=<id>"`
- On timeout:
  - Emit `PrintStage::Failed`
  - `verification_evidence = "IPP job-state polling timeout after 60s, last state=<N>"`

#### cups

After `lp` submission, enrich the existing `verify_print_completion`:

- On job disappearance from `lpstat`:
  - Emit `PrintStage::Verified`
  - `verification_method = "cups_lpstat"`
  - `verification_evidence = "CUPS job <id> completed on <printer>"`
- On timeout:
  - Emit `PrintStage::Failed`
  - `verification_evidence = "CUPS job <id> still in queue after 180s on <printer>"`

#### print_proxy

Proxy backend forwards to a remote endpoint. No local verification possible:
- `verification_method = "none"`
- `verification_evidence = "Proxied to <url> — no local verification"`

### 4. Client Completion Report (`receiver.rs`)

After backend execution, the client populates `JobCompletion` with:
- `client_id = machine_id` (from client config)
- `verification_method` and `verification_evidence` from the last `Verified`
  or `Failed` event emitted by the backend

The `EventEmitter` is extended to track verification state so the receiver
can extract it after backend execution.

### 5. Server-Side Client Validation (`dispatch.rs`)

In `complete_job`, after receiving `JobCompletion`:

1. Compare `completion.client_id` against `job.target_client_id`:
   - If match or job was unpaired (`target_client_id` is None): proceed normally.
   - If mismatch: insert a WARNING event before the completion event:
     ```
     stage: Completed
     detail: "Completed by <actual_client> (originally routed to <expected_client>)"
     verification_method: "client_mismatch"
     ```
2. Store `verification_method` and `verification_evidence` from the completion
   in the final `Completed`/`Failed` event.
3. If `success = false`: existing retry logic handles requeue.

### 6. Database Migration

Add columns to `job_events` table (incremental, production-safe):

```sql
ALTER TABLE job_events ADD COLUMN verification_method TEXT NOT NULL DEFAULT '';
ALTER TABLE job_events ADD COLUMN verification_evidence TEXT NOT NULL DEFAULT '';
```

Empty strings for existing events (backward compatible). New events populate
both fields.

### 7. Dashboard API

No new endpoints needed. Existing `GET /api/jobs/{id}/events` and
`GET /api/jobs/events` already return all event fields. The new
`verification_method` and `verification_evidence` fields appear
automatically in the JSON response.

The dashboard UI event timeline already renders `stage`, `success`, and
`detail` for each event. The `Verified` stage events appear in the timeline
with their evidence strings, providing a clear audit trail.

### 8. Testing Strategy

#### Unit Tests
- `PrintStage::Verified` serialization/deserialization
- `PrintJobEvent` with verification fields round-trips through DB
- Server client validation logic (match, mismatch, unpaired)
- Event enrichment in each backend (mock the system calls)

#### Integration Tests (CI, ubuntu)
- `direct_ipp` verification flow (mock IPP server returning job-state 9)
- `cups` verification flow (mock `lpstat` output)
- Server accepts `JobCompletion` with verification fields
- Migration adds columns without data loss

#### E2E Tests (self-hosted runners)
- **pz-holla** (windows_spooler): Print → verify EventID 307 appears in
  job events on dashboard
- **pz-snv** (direct_ipp): Print → verify IPP job-state evidence in events
- Reprint a failed job → verify retry events have verification detail

#### Mutation Testing
- Verification timeout path must be tested (mock slow responses)
- Client mismatch detection must be tested
- Evidence string construction must be tested

### 9. Scope Exclusions

The following are NOT in scope for this change:
- Printer health pre-check (Issue #17 — separate work)
- New /audit dashboard page (not needed, existing timeline is sufficient)
- Server-side remote verification via MCP (client-side is sufficient)
- Tray app notifications (Issue #11 — separate work)

### 10. Rollout

1. Deploy to dev, run CI (mutation + Playwright)
2. Deploy to pz-holla first (windows_spooler, most problematic printer)
3. Verify EventID 307 evidence appears in job events
4. Deploy to pz-snv (direct_ipp)
5. Verify IPP job-state evidence
6. Deploy to remaining machines
