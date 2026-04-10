# Jobs Display Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace cluttered job rows (`job-{uuid}` noise, no requesting user, no clear "user X on Y printer" indication) with the three pieces of information the user actually wants — time, requesting user, target printer — in both the Server Dashboard's "Recent Jobs" cards and the `/jobs` page table.

**Architecture:** UI-only change. Two Leptos components are rewritten: `JobCard` in `crates/devbridge-ui/src/pages/dashboard.rs` (used by both server and client dashboards) and the `JobsPage` table in `crates/devbridge-ui/src/pages/jobs.rs`. The dashboard API at `crates/devbridge-dashboard/src/api/jobs.rs` already returns `requesting_user`, `printer`, `created_at`, and `status` for every job — no backend changes are required. Existing helper components (`TimeWithSeconds`, `StatusBadge`, `format_time_ago`) are reused.

**Tech Stack:** Leptos 0.7 (CSR/WASM), Rust 2024, Playwright TypeScript (for E2E), `serde_json::Value` for runtime field access.

**Spec:** `docs/superpowers/specs/2026-04-10-jobs-display-cleanup-design.md`

---

## File Structure

### Modified Files

| File | Changes |
|------|---------|
| `crates/devbridge-ui/src/pages/dashboard.rs` | Rewrite `JobCard` component (lines 227-366) — new header row layout with user/printer text, drop document name span and short_id div |
| `crates/devbridge-ui/src/pages/jobs.rs` | Rewrite the `JobsPage` table — drop ID/Name columns, add User/Ago columns |
| `playwright/tests/jobs.spec.ts` | Update `shows table with correct headers` test to assert new column names |
| `playwright/tests/dashboard.spec.ts` | Add a regression test asserting no `job-{uuid}` UUID strings appear in the rendered DOM on the empty dashboard |

### No New Files

The change is contained inside two existing components and two existing test files. No new helpers, no new modules.

---

## Task 1: Update Playwright test for `/jobs` page (RED)

**Files:**
- Modify: `playwright/tests/jobs.spec.ts:16-29`

**Goal:** Update the existing header-assertion test so it expects the new columns. This test will fail until Task 2 lands the UI rewrite — that is the failing-first part of TDD.

- [ ] **Step 1: Replace the `shows table with correct headers` test**

Open `playwright/tests/jobs.spec.ts` and replace lines 16-29 (the entire `shows table with correct headers` test block) with this exact code:

```typescript
  test('shows table with correct headers', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/jobs');

    const headers = page.locator('table thead th');
    await expect(headers).toHaveCount(5);
    await expect(headers.nth(0)).toHaveText('Time');
    await expect(headers.nth(1)).toHaveText('User');
    await expect(headers.nth(2)).toHaveText('Printer');
    await expect(headers.nth(3)).toHaveText('Status');
    await expect(headers.nth(4)).toHaveText('Ago');

    assertCleanConsole(cons);
  });
```

- [ ] **Step 2: Verify the test would fail today**

The existing UI in `crates/devbridge-ui/src/pages/jobs.rs` lines 19-25 renders headers `ID`, `Name`, `Printer`, `Status`, `Created`. The new test expects `Time`, `User`, `Printer`, `Status`, `Ago`. The first `expect(headers.nth(0)).toHaveText('Time')` would fail against the unmodified UI because it currently reads `ID`. (Do not actually run Playwright now — the WASM build runs only on CI. We rely on the spec assertion being demonstrably wrong against current code as proof of failing test.)

- [ ] **Step 3: Commit**

```bash
git add playwright/tests/jobs.spec.ts
git commit -m "Update jobs page Playwright test for new column headers (#27)

Tests now expect Time/User/Printer/Status/Ago instead of ID/Name/Printer/Status/Created.
Will fail until the UI rewrite in jobs.rs lands.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Rewrite `/jobs` page table (GREEN)

**Files:**
- Modify: `crates/devbridge-ui/src/pages/jobs.rs:1-111`

**Goal:** Make Task 1's test pass. Replace the table headers and row rendering to show Time/User/Printer/Status/Ago.

- [ ] **Step 1: Replace the entire `JobsPage` component**

Open `crates/devbridge-ui/src/pages/jobs.rs` and replace lines 1-111 (everything up to but not including the `JobEventTimeline` component) with this exact code:

```rust
use leptos::prelude::*;

use crate::api;
use crate::components::header::PageHeader;
use crate::components::status_badge::StatusBadge;
use crate::components::time_display::{TimeWithSeconds, format_time_ago};

#[component]
pub fn JobsPage() -> impl IntoView {
    let jobs = LocalResource::new(|| api::fetch_jobs());
    let (selected_job, set_selected_job) = signal(None::<String>);

    view! {
        <PageHeader title="Jobs" />

        <div class="card">
            <table>
                <thead>
                    <tr>
                        <th>"Time"</th>
                        <th>"User"</th>
                        <th>"Printer"</th>
                        <th>"Status"</th>
                        <th>"Ago"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        jobs.read().as_ref().map(|res| {
                            match &**res {
                                Ok(job_list) => {
                                    if job_list.is_empty() {
                                        view! {
                                            <tr>
                                                <td colspan="5" style="text-align:center; color: var(--text-muted)">
                                                    "No jobs found."
                                                </td>
                                            </tr>
                                        }.into_any()
                                    } else {
                                        job_list.iter().cloned().map(|job| {
                                            let id = job.get("id")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("-")
                                                .to_string();
                                            let user = job.get("requesting_user")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .unwrap_or("\u{2014}")
                                                .to_string();
                                            let printer = job.get("printer")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .unwrap_or("\u{2014}")
                                                .to_string();
                                            let status = job.get("status")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let created = job.get("created_at")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                            let ago = format_time_ago(&created);

                                            let row_id = id.clone();
                                            let timeline_id = id.clone();

                                            view! {
                                                <tr
                                                    style="cursor: pointer;"
                                                    on:click=move |_| {
                                                        let clicked = row_id.clone();
                                                        set_selected_job.update(|sel| {
                                                            if sel.as_deref() == Some(&clicked) {
                                                                *sel = None;
                                                            } else {
                                                                *sel = Some(clicked);
                                                            }
                                                        });
                                                    }
                                                >
                                                    <td><TimeWithSeconds datetime=created /></td>
                                                    <td>{user}</td>
                                                    <td>{printer}</td>
                                                    <td><StatusBadge status=status /></td>
                                                    <td>{ago}</td>
                                                </tr>
                                                {move || {
                                                    if selected_job.get().as_deref() == Some(&timeline_id) {
                                                        Some(view! { <JobEventTimeline job_id=timeline_id.clone() /> })
                                                    } else {
                                                        None
                                                    }
                                                }}
                                            }
                                        }).collect_view().into_any()
                                    }
                                }
                                Err(e) => view! {
                                    <tr>
                                        <td colspan="5" style="text-align:center; color: var(--danger)">
                                            {format!("Error loading jobs: {e}")}
                                        </td>
                                    </tr>
                                }.into_any(),
                            }
                        })
                    }}
                </tbody>
            </table>
        </div>
    }
}
```

- [ ] **Step 2: Verify the `JobEventTimeline` component below is unchanged**

Lines 113-157 of `jobs.rs` define `JobEventTimeline` (used when a row is clicked). Do NOT touch it. After your edit, the file should still have that component intact at the bottom.

- [ ] **Step 3: Run cargo fmt**

```bash
cargo fmt --all
```

Expected: no output (file already formatted, or formatting differences fixed silently).

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-ui/src/pages/jobs.rs
git commit -m "Rewrite jobs page table: Time/User/Printer/Status/Ago (#27)

Drop ID and Name columns. Add User (requesting_user) and Ago columns.
Time uses TimeWithSeconds, Ago uses format_time_ago. Empty user/printer
fall back to em dash. Existing row-click expand-timeline behavior preserved.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Add Playwright regression test for dashboard `job-{uuid}` noise (RED)

**Files:**
- Modify: `playwright/tests/dashboard.spec.ts` — add one new test inside the existing `test.describe('Dashboard Page', ...)` block

**Goal:** Catch any future regression where a `job-{uuid}` string leaks into the rendered dashboard DOM. The test asserts the empty dashboard's HTML body does not contain the regex `/job-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i`. This passes today on the empty dashboard (no jobs → no UUIDs rendered), but it locks in the rule.

- [ ] **Step 1: Append a new test inside the existing describe block**

Open `playwright/tests/dashboard.spec.ts`. Just before the closing `});` of `test.describe('Dashboard Page', () => {` (which is on line 55), insert this exact test:

```typescript

  test('does not render job-{uuid} noise on empty dashboard', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    // Wait for the empty state to render so we know the page is hydrated
    await expect(page.locator('text=No jobs yet')).toBeVisible();

    // The hardcoded "job-{uuid}" document_name pattern must never appear
    // in the rendered DOM. This is a regression guard for issue #27.
    const bodyText = await page.locator('body').innerText();
    expect(bodyText).not.toMatch(
      /job-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i
    );

    assertCleanConsole(cons);
  });
```

- [ ] **Step 2: Commit**

```bash
git add playwright/tests/dashboard.spec.ts
git commit -m "Add Playwright regression test for job-{uuid} noise (#27)

Asserts the rendered dashboard DOM never contains a 'job-{uuid}' string.
Locks in the fix from #27 against future regressions.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Rewrite `JobCard` component (GREEN for both visual change and Task 3 regression guard)

**Files:**
- Modify: `crates/devbridge-ui/src/pages/dashboard.rs:227-366`

**Goal:** Drop the document name span and the short_id UUID div from `JobCard`. Add a "user X" line and an arrow-prefixed "→ printer name" line in the header row. The reprint button keeps working (the `name` variable stays in scope but is not rendered).

- [ ] **Step 1: Replace the entire `JobCard` component**

Open `crates/devbridge-ui/src/pages/dashboard.rs` and replace lines 227-366 (the `#[component] fn JobCard(...)` block, from the `#[component]` attribute through the closing `}` of the function, inclusive) with this exact code:

```rust
#[component]
fn JobCard(
    job: Value,
    events: Vec<Value>,
    ago_tick: ReadSignal<u32>,
    #[prop(optional)] show_reprint: Option<Box<dyn Fn(String, String) + 'static>>,
) -> impl IntoView {
    let id = job
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // `name` is kept only so the reprint feedback toast can show *something*.
    // It is not displayed in the card. See spec 2026-04-10-jobs-display-cleanup.
    let name = job
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();
    let user = job
        .get("requesting_user")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("\u{2014}")
        .to_string();
    let printer = job
        .get("printer")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("\u{2014}")
        .to_string();
    let status = job
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let created_at = job
        .get("created_at")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let color = status_color(&status);
    let can_reprint = status == "completed" || status == "failed";

    let event_count = events.len();

    // For "ago" display that updates with tick
    let created_at_ago = created_at.clone();

    view! {
        <div
            class="card"
            style:margin-bottom="0.75rem"
            style:border-left=format!("4px solid {color}")
            style:padding="0.75rem 1rem"
        >
            // Header row: timestamp + user + printer + status + reprint
            <div style="display: flex; align-items: center; gap: 0.75rem; margin-bottom: 0.25rem">
                // Large timestamp
                <div style="min-width: 6rem">
                    {if !created_at.is_empty() {
                        let ts = created_at.clone();
                        Some(view! {
                            <div style:font-family="monospace" style:font-size="1.4em" style:font-weight="bold" style:color=color>
                                <TimeWithSeconds datetime=ts />
                            </div>
                            <div style="font-size: 0.8em; color: var(--text-muted)">
                                {move || {
                                    let _ = ago_tick.get();
                                    format_time_ago(&created_at_ago)
                                }}
                            </div>
                        })
                    } else {
                        None
                    }}
                </div>

                // User + printer (the 3 base infos requested in #27)
                <span style="flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text-muted); font-size: 0.95em">
                    "user " <strong style="color: var(--text)">{user}</strong>
                    " \u{2192} " // arrow
                    <strong style="color: var(--text)">{printer}</strong>
                </span>

                // Status badge
                <StatusBadge status=status.clone() />

                // Reprint button
                {if can_reprint {
                    if let Some(reprint_fn) = show_reprint {
                        let reprint_id = id.clone();
                        let reprint_name = name.clone();
                        Some(view! {
                            <button
                                class="btn btn-sm"
                                style="font-size: 0.8em; padding: 0.2rem 0.6rem"
                                on:click=move |_| {
                                    reprint_fn(reprint_id.clone(), reprint_name.clone());
                                }
                            >
                                "Reprint"
                            </button>
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }}
            </div>

            // Audit trail — always visible
            {if event_count > 0 {
                let job_status = status.clone();
                Some(view! {
                    <div style:margin-top="0.5rem" style:padding-left="0.5rem" style:border-left=format!("2px solid {color}") style:font-size="0.85em">
                        {events.iter().enumerate().map(|(i, evt)| {
                            let stage = evt["stage"].as_str().unwrap_or("unknown").to_string();
                            let success = evt["success"].as_bool().unwrap_or(false);
                            let detail = evt["detail"].as_str().unwrap_or("").to_string();
                            let timestamp = evt["timestamp"].as_str().unwrap_or("").to_string();
                            let is_last = i == event_count - 1;
                            let icon = status_icon(success, is_last, &job_status);
                            let detail_color = if !success { "var(--danger, #ef4444)" } else { "var(--text)" };

                            view! {
                                <div style="display: flex; gap: 0.5rem; padding: 0.15rem 0; font-family: monospace; font-size: 0.95em; color: var(--text-muted)">
                                    <span style="min-width: 5.5rem; text-align: right">
                                        <TimeWithSeconds datetime=timestamp />
                                    </span>
                                    <span>{icon}</span>
                                    <span style="min-width: 6rem; font-weight: 600">{stage}</span>
                                    <span style:color=detail_color>{detail}</span>
                                </div>
                            }
                        }).collect_view()}
                    </div>
                })
            } else {
                None
            }}
        </div>
    }
}
```

Key removals from the original:

1. The `let short_id = ...` block (was lines 239-243) is gone.
2. The "Document name" span (was lines 300-303, the span containing `{name.clone()}`) is replaced by the new "user / printer" span.
3. The "Job ID at bottom right" div (was lines 360-363, the bottom-right `short_id` display) is gone.

Key keeps from the original:

1. The `name` local variable stays — the reprint button still passes it to `reprint_fn` for the feedback toast.
2. The `id` local variable stays — the reprint button still passes it.
3. The audit trail block (everything inside `if event_count > 0`) is unchanged.
4. The `created_at` / `created_at_ago` / `TimeWithSeconds` / `format_time_ago` usage is unchanged.

- [ ] **Step 2: Run cargo fmt**

```bash
cargo fmt --all
```

Expected: no output.

- [ ] **Step 3: Commit**

```bash
git add crates/devbridge-ui/src/pages/dashboard.rs
git commit -m "Rewrite JobCard: show user/printer instead of doc name + UUID (#27)

Drop the 'job-{uuid}' document_name span and the bottom-right short_id
display. Replace with 'user X → printer Y' (the 3 base infos asked for
in the issue). Reprint button still receives id+name internally so the
feedback toast keeps working. Audit timeline and ago display unchanged.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Local pre-push checks

**Files:** none — verification only.

- [ ] **Step 1: Run format check**

```bash
cargo fmt --all --check
```

Expected: no output, exit code 0. If anything fails, run `cargo fmt --all` and re-check.

- [ ] **Step 2: Verify all four commits are on the branch**

```bash
git log --oneline origin/dev..HEAD
```

Expected: at minimum these four commits in order (oldest first):

1. `Update jobs page Playwright test for new column headers (#27)`
2. `Rewrite jobs page table: Time/User/Printer/Status/Ago (#27)`
3. `Add Playwright regression test for job-{uuid} noise (#27)`
4. `Rewrite JobCard: show user/printer instead of doc name + UUID (#27)`

(The version bump commit `09181af` from earlier in the session is also there, before these four.)

- [ ] **Step 3: Verify no other files were touched**

```bash
git diff --stat origin/dev..HEAD
```

Expected: only these files appear in the stat:

- `Cargo.toml` (1 line, version bump from earlier)
- `crates/devbridge-app/tauri.conf.json` (1 line, version bump from earlier)
- `crates/devbridge-ui/src/pages/dashboard.rs`
- `crates/devbridge-ui/src/pages/jobs.rs`
- `docs/superpowers/specs/2026-04-10-jobs-display-cleanup-design.md`
- `docs/superpowers/plans/2026-04-10-jobs-display-cleanup.md`
- `playwright/tests/dashboard.spec.ts`
- `playwright/tests/jobs.spec.ts`

No other files. If anything else appears, investigate before pushing.

---

## Task 6: Push and monitor CI

**Files:** none.

- [ ] **Step 1: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 2: Identify the new run**

```bash
gh run list --branch dev --limit 3
```

Note the latest run ID (the topmost one with status `queued` or `in_progress`).

- [ ] **Step 3: Monitor run to completion**

Use a single non-polling check after a reasonable wait:

```bash
sleep 600 && gh run view <run-id>
```

If the run is still in progress, wait another 300 seconds and check again. Do NOT use `gh run watch` (rate-limit risk).

- [ ] **Step 4: If any job fails, fetch the failure log**

```bash
gh run view <run-id> --log-failed
```

Common expected failure modes:

- **Playwright `dashboard.spec.ts` `does not render job-{uuid} noise` fails** → means the rewrite missed a place that still emits a `job-` UUID. Read the diff carefully and remove the leftover.
- **Playwright `jobs.spec.ts` headers fail** → means `jobs.rs` still has old column names. Re-check Task 2.
- **Trunk WASM build fails with type/borrow errors** → fix the Rust compile errors (read the message, the issue is almost always a missing `.clone()` on a String moved into a closure or a mismatched return type).
- **`cargo fmt --all -- --check` fails** → run `cargo fmt --all` locally, commit, push.
- **Mutation testing finds a survivor** → only if backend logic was accidentally touched. Should not happen for this UI-only change. If it does, investigate which file changed and revert any unintended backend edits.

Fix all failures in **one** consolidated commit, then push and monitor again. Do NOT push partial fixes one at a time. Read every failed job's log first, batch all fixes, then commit once with a message that names each thing fixed (e.g., "Fix jobs.rs UnusedImport, dashboard.rs missing clone, playwright header text typo"). Then:

```bash
git add -A
git push origin dev
```

- [ ] **Step 5: Confirm all jobs green**

When `gh run view <run-id>` shows `Status: completed Conclusion: success` and every job is `success` (no skipped, no failed), proceed to Task 7.

---

## Task 7: Post-deploy verification on pz-server

**Files:** none — verification only.

The CI pipeline auto-deploys to pz-server (10.88.1.100) and pz-snv (10.78.2.10). After CI is green, verify the actual deployed UI works.

- [ ] **Step 1: Confirm v0.8.8 is running on pz-server**

```bash
gh run list --branch dev --limit 1 --json conclusion,status
```

Then via MCP:

```
mcp__win-pz-server__Shell: powershell -Command "Get-Process | Where-Object { $_.Name -like '*devbridge*' } | Select-Object Name,Id,StartTime"
```

Expected: at least one `devbridge-service` process started after the CI deploy completion time.

- [ ] **Step 2: Check the version endpoint**

```bash
curl -sf http://10.88.1.100:9120/api/status | python3 -m json.tool
```

Expected: a JSON object that includes `"mode": "server"` and the version reflecting 0.8.8 (or whatever the API exposes for version).

- [ ] **Step 3: Open the dashboard in Playwright via MCP and screenshot the empty state**

Use the `mcp__plugin_playwright_playwright__browser_navigate` and `mcp__plugin_playwright_playwright__browser_snapshot` tools:

```
browser_navigate: http://10.88.1.100:9120
browser_snapshot
```

Read the snapshot. Confirm:

- The page header reads `Dashboard`
- The "Recent Jobs" h3 is visible
- No element contains a string matching `job-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}` (use the snapshot text to grep mentally — there should be no hex UUID strings)

If a `job-{uuid}` string appears in the snapshot, the deploy is broken — investigate immediately.

- [ ] **Step 4: Open the /jobs page and verify the new headers**

```
browser_navigate: http://10.88.1.100:9120/jobs
browser_snapshot
```

Read the snapshot. Confirm the table headers are exactly: `Time`, `User`, `Printer`, `Status`, `Ago` (in that order). Check the browser console with:

```
browser_console_messages
```

Expected: no `[error]` or `[warning]` entries (filter out the WebSocket reconnect noise per the existing helper rules).

- [ ] **Step 5: Submit a real print job from drlikzbynek's RDP session**

If drlikzbynek is currently logged in to pz-server (check via `query user` over MCP), submit a small PDF print to one of the virtual printers. If not logged in, skip this step and rely on whatever recent jobs exist.

```
mcp__win-pz-server__Shell: powershell -Command "Get-Printer | Where-Object { $_.PortName -like '*ipp*' -or $_.PortName -like '*devbridge*' } | Select Name,PortName"
```

Pick one of the listed virtual printers. Then trigger a test print as the current desktop user (the MCP runs in the active console session by default, but if the active console user is not drlikzbynek, the requesting_user in the new row will be whoever is interactive — that's still valid evidence the field flows through).

If no virtual printer is listed or the test page command fails, skip the live job submission and verify with whatever job already exists in the API:

```bash
curl -sf "http://10.88.1.100:9120/api/jobs?limit=5" | python3 -m json.tool
```

Pick the most recent entry and confirm it has a non-null `requesting_user` and `printer`.

- [ ] **Step 6: Refresh the dashboard and verify the new row format**

```
browser_navigate: http://10.88.1.100:9120
browser_snapshot
```

Read the snapshot. For the most recent job card, confirm:

- It contains the substring `user ` followed by a non-empty username
- It contains the arrow `→` followed by the target printer name
- It contains a status badge text (`completed`, `failed`, `printing`, etc.)
- It does NOT contain any `job-` UUID substring

Take a final screenshot for the completion report:

```
browser_take_screenshot
```

- [ ] **Step 7: Verify per-user filter still works**

```bash
curl -sf "http://10.88.1.100:9120/api/jobs?requesting_user=drlikzbynek" | python3 -m json.tool
```

Expected: a JSON array. If drlikzbynek has any jobs, only those appear. If they have none, an empty array `[]`. Compare against the unfiltered call to confirm the filter narrows the results.

---

## Task 8: Create PR and wait for merge approval

**Files:** none.

- [ ] **Step 1: Create the PR**

```bash
gh pr create --title "Jobs display cleanup: time/user/printer (#27)" --body "$(cat <<'EOF'
## Summary
- Server Dashboard "Recent Jobs" cards now show **time · user X · → printer Y · status · ago** instead of cluttered `job-{uuid}` document name + truncated UUID at the bottom right
- `/jobs` page table columns are now **Time · User · Printer · Status · Ago** (was ID · Name · Printer · Status · Created)
- UI-only change — API already returns `requesting_user`, `printer`, `created_at`, `status`
- Empty `requesting_user` / `printer` render as em dash `—`
- Audit timeline and reprint button behavior unchanged
- New Playwright regression test asserts no `job-{uuid}` strings ever leak into the rendered DOM

Closes #27. Followup #30 tracks capturing the real IPP `document-name` (deferred — requires ippper fork).

## Test plan
- [x] Updated `playwright/tests/jobs.spec.ts` header assertions
- [x] New `playwright/tests/dashboard.spec.ts` regression test for job-{uuid} noise
- [x] Existing API contract tests untouched (no backend changes)
- [x] Mutation testing untouched (no backend changes)
- [x] All CI jobs green
- [x] Post-deploy verification on pz-server: dashboard + /jobs both show new layout, console clean

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 2: Verify the PR is mergeable and clean**

Find the PR number, then:

```bash
gh api repos/zbynekdrlik/devbridge/pulls/<PR_NUMBER> --jq '{mergeable: .mergeable, mergeable_state: .mergeable_state}'
```

Expected: `{"mergeable": true, "mergeable_state": "clean"}`

If `mergeable_state` is `behind`, sync first:
```bash
git fetch origin && git merge origin/main --no-edit && git push origin dev
```

- [ ] **Step 3: Provide the green PR URL**

Output the PR URL to the user with the completion report. **Do not merge.** Wait for the user's explicit "merge it" approval.

---

## Verification Checklist (use before sending the completion report)

- [ ] All 8 tasks above are checked off
- [ ] CI run is green (every job = success, no skipped)
- [ ] PR is mergeable: true, mergeable_state: clean
- [ ] Dashboard at http://10.88.1.100:9120 shows new layout (verified via Playwright snapshot)
- [ ] /jobs at http://10.88.1.100:9120/jobs shows new headers (verified via Playwright snapshot)
- [ ] Browser console clean on both pages
- [ ] No `job-{uuid}` strings appear anywhere in the rendered DOM
- [ ] Issue #30 exists for the deferred document_name capture work
