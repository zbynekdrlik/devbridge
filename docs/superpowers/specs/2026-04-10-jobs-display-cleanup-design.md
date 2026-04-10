# Jobs Display Cleanup — Design Spec

**Issue:** [#27](https://github.com/zbynekdrlik/devbridge/issues/27) — "jobs in dashboard should have this 3 base information time with sec, what user call print, on what printer"

**Date:** 2026-04-10

**Goal:** Replace cluttered job rows showing the worthless string `job-{uuid}` with the three pieces of information the user actually wants: **time**, **requesting user**, **target printer**.

---

## Problem

Today, both the Server Dashboard's "Recent Jobs" cards and the `/jobs` page table show:

- The hardcoded string `job-7d70b087-9021-45fd-bf90-676fd4ce83e8` (the `document_name` field, which `ipp_service.rs:248` hardcodes to `"job-{job_id}"` because the upstream IPP library doesn't expose the real document name)
- A short truncated UUID at the bottom-right corner of each card
- No indication of who initiated the print (`requesting_user`), even though it has been captured into the API since PR #28

This is noise. The user wants to see, at a glance: **HH:MM:SS · user X · on Y printer · status · ago**.

## Scope

**In scope (UI only):**

- `crates/devbridge-ui/src/pages/dashboard.rs` — `JobCard` component (used for "Recent Jobs" on the server dashboard and the timeline on the client dashboard)
- `crates/devbridge-ui/src/pages/jobs.rs` — `/jobs` page table

**Explicitly out of scope (deferred to a new GitHub issue):**

- Capturing the real `document-name` IPP attribute. The `ippper-0.4.0` library strips IPP attributes into a `SimpleIppJobAttributes` struct that only exposes `originating_user_name` — `document-name` and `job-name` are consumed inside `ippper::print_job` before our handler ever sees them. Capturing the real document name would require either a fork of `ippper` (vendoring + maintenance burden) or implementing the lower-level `IppService` trait directly (~500 lines of IPP protocol handling). Both are significant scope expansion. A separate issue tracks this work.

**No backend changes are needed.** The dashboard API at `crates/devbridge-dashboard/src/api/jobs.rs` already returns `requesting_user`, `printer`, `created_at`, and `status` for every job (since PR #28). Only the frontend Leptos components need to be rewritten.

## Architecture

Both the Server Dashboard's "Recent Jobs" feed and the Client Dashboard's job timeline render through the same `JobCard` component in `dashboard.rs`. Updating that component fixes both views in one place. The `/jobs` page uses its own table layout in `jobs.rs` and is updated separately.

The two units have one clear responsibility each:

- `JobCard` — render a single job as a card with a header row, a relative-age line, and an expandable audit timeline. After this change, the header row contains: time-with-seconds · "user X" · "→ printer name" · status badge · reprint button. The middle "name" span and the bottom-right `short_id` are removed.
- Jobs table in `jobs.rs` — render a flat table for the `/jobs` page. After this change, columns are: Time · User · Printer · Status · Ago. The ID and Name columns are removed.

## Components

### `JobCard` (in `dashboard.rs`)

**Inputs:** unchanged — a `serde_json::Value` for the job (with fields `id`, `requesting_user`, `printer`, `status`, `created_at`), a `Vec<Value>` of audit events, an `ago_tick` signal, and an optional `show_reprint` callback.

**Layout (after change):**

```
┌────────────────────────────────────────────────────────────────────┐
│ 15:31:16   user p408   →  Canon MG3600   [completed]   [Reprint]  │
│ 8h ago                                                              │
│ ├─ 15:31:16  ✅  queued     received from IPP                      │
│ ├─ 15:31:17  ✅  rendering  PDF→JPEG via ghostscript               │
│ └─ 15:31:18  ✅  verified   Canon IPP success                      │
└────────────────────────────────────────────────────────────────────┘
```

**Header row contents:**

1. Large monospace `TimeWithSeconds` (existing component, color = status color)
2. `user X` — small dimmed text. If `requesting_user` is null/empty, render `user —` (em dash)
3. `→ {printer_name}` — small dimmed text with arrow prefix. If `printer` is null/empty, render `→ —`
4. `StatusBadge` (existing component)
5. Reprint button (existing, only visible when `show_reprint` is supplied and status ∈ {completed, failed})

**Removed from display:** the middle `name` (document_name) span, the `short_id` div at the bottom. The `name` and `id` local variables stay in scope because the reprint callback still passes them through to the feedback toast — only the rendered DOM elements are removed.

**Kept:** the `created_at_ago` line below the header, the audit timeline (the entire `if event_count > 0 { … }` block stays untouched).

### `JobsPage` table (in `jobs.rs`)

**Layout (after change):**

| Time | User | Printer | Status | Ago |
|------|------|---------|--------|-----|
| 15:31:16 | p408 | Canon MG3600 | completed | 8h ago |

- `Time` cell uses the existing `TimeWithSeconds` component
- `User` cell renders `requesting_user` or `—`
- `Printer` cell renders `printer` or `—`
- `Status` cell uses the existing `StatusBadge`
- `Ago` cell uses the existing `format_time_ago` helper string (no live updating needed in the table — the page re-fetches on navigation)
- Empty-state row colspan changes from `5` to `5` (still 5 columns, no change)
- Click handler stays — clicking a row toggles the expandable `JobEventTimeline` below

**Removed:** ID column, Name column.

## Data Flow

Unchanged. The API returns the same JSON shape:

```json
{
  "id": "...",
  "name": "job-{uuid}",          // present but not displayed
  "printer": "Canon MG3600",
  "status": "completed",
  "requesting_user": "p408",
  "created_at": "2026-04-10T15:31:16Z",
  "updated_at": "..."
}
```

The frontend simply reads different keys (`requesting_user` instead of `id`/`name`).

## Error Handling

- Missing `requesting_user` (null or empty string) → render `—`
- Missing `printer` → render `—`
- Missing `created_at` → render the time/ago cell empty (existing behavior)
- API fetch error → existing error rows stay as-is

No new error paths are introduced.

## Testing

### Playwright E2E (mandatory per CI gates)

Update existing files in `playwright/tests/`:

1. **`dashboard.spec.ts`** — add a test that asserts the "Recent Jobs" empty state still renders, plus when (later) jobs exist they show user/printer text. Initially the empty-state assertion is enough since CI dashboard starts fresh.
2. **`jobs.spec.ts`** — update the existing "shows table with correct headers" test:
   - Header count: still 5
   - Header texts: `Time`, `User`, `Printer`, `Status`, `Ago` (was `ID`, `Name`, `Printer`, `Status`, `Created`)

Both tests must keep their `assertCleanConsole(console)` call at the end (zero browser console errors/warnings is enforced).

### Rust unit tests

No changes. The API contract test in `crates/devbridge-dashboard/src/api/jobs.rs` (`test_jobs_response_matches_ui_contract`) already asserts the keys the UI consumes (`id`, `name`, `printer`, `status`, `created_at`). The new UI consumes `requesting_user` too, but PR #28 already added that field — the test does not need to assert its presence here because the existing `test_requesting_user_filter_returns_only_matching_jobs` already covers it.

### Mutation testing

Existing `cargo mutants --workspace` job runs as part of CI. No backend changes mean no new mutants to kill.

## Post-Deploy Verification

After CI deploys v0.8.8 to pz-server and pz-snv:

1. **Dashboard view** — open `http://10.88.1.100:9120` in Playwright (mcp tool). Read DOM. Assert no element contains the substring `job-` followed by a hyphen-separated UUID (regex check). Take a screenshot for visual confirmation.
2. **Submit a real print job** — from drlikzbynek's RDP session on pz-server, print a small PDF to a virtual printer pointing at pz-snv. Wait for completion.
3. **Verify the new row** — refresh `http://10.88.1.100:9120` in Playwright. Assert the most recent job card contains the substring `drlikzbynek` (the requesting user) and the target printer name. Browser console must be clean.
4. **Verify the table** — open `http://10.88.1.100:9120/jobs` in Playwright. Assert the column headers are exactly `Time`, `User`, `Printer`, `Status`, `Ago`. Assert at least one row contains `drlikzbynek`.
5. **Per-user filter still works** — navigate to `http://10.88.1.100:9120/jobs?requesting_user=drlikzbynek` and confirm only that user's jobs appear. (Smoke check that the existing API filter wasn't broken.)

If any step fails, the work is not done.

## Followup Issue (filed separately)

Filed as [#30](https://github.com/zbynekdrlik/devbridge/issues/30):

> **Capture real document name from IPP attributes**
>
> The `document_name` field in `JobMetadata` is currently hardcoded to `"job-{job_id}"` in `ipp_service.rs:248` because `ippper-0.4.0::SimpleIppJobAttributes` does not expose the IPP `document-name` or `job-name` attributes. Either fork `ippper` to expose these fields and submit upstream, or implement the lower-level `IppService` trait directly to parse raw attributes. Once captured, surface the real document name as secondary text in the dashboard JobCard.
