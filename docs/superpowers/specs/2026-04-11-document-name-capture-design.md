# Capture Real Document Name from IPP Attributes

**Issue:** [#30](https://github.com/zbynekdrlik/devbridge/issues/30)
**Date:** 2026-04-11
**Status:** Design

## Problem

`crates/devbridge-server/src/ipp_service.rs:248` hardcodes:

```rust
let document_name = format!("job-{job_id}");
```

The result is strings like `job-7d70b087-9021-45fd-bf90-676fd4ce83e8` wherever a document name is referenced — audit logs, reprint toasts, `/api/jobs` responses. Real information sent by the print client (`Invoice-2026-001.pdf`, `receipt.pdf`, `Untitled - Notepad`) is discarded.

### Root cause

The upstream `ippper-0.4.0` library strips IPP attributes inside `SimpleIppService::print_job` before invoking our `SimpleIppServiceHandler::handle_document` callback. The intermediate `SimpleIppJobAttributes` struct exposes only:

- `originating_user_name`
- `media`
- `orientation`
- `sides`
- `print_color_mode`
- `printer_resolution`

The IPP operation attributes `document-name` and `job-name` — sent by every Windows and macOS print client — are consumed and discarded. There is no public hook to reach them.

## Goals

1. Capture the real `document-name` / `job-name` sent by the IPP client.
2. Propagate it unchanged through JobMetadata → SQLite → gRPC → dashboard.
3. Surface it as secondary muted text in the Dashboard JobCard and the Jobs page table row when present.
4. Hide the field entirely when no real name is available (no `Untitled` placeholder, no `job-<uuid>` leakage).

## Non-goals

- SQLite schema migration. The `document_name` column stays `TEXT NOT NULL`; absence is represented by the empty string.
- Backfilling old `job-<uuid>` rows. Legacy jobs remain as-is; the UI detects the `job-` prefix and treats them as absent.
- Publishing the ippper fork to crates.io. We consume the fork via `[patch.crates-io]` pinned to a commit SHA.
- Upstream PR to `ArcticLampyrid/ippper.rs`. Nice-to-have, not blocking.

## Architecture

Two components:

1. **Upstream library fork** (`zbynekdrlik/ippper.rs`): minimal patch that extends `SimpleIppJobAttributes` with `document_name: Option<String>` and `job_name: Option<String>`, extracted from `DelimiterTag::OperationAttributes` inside `take_ipp_attributes`. Consumed from DevBridge via `[patch.crates-io]` pinned to a commit SHA for reproducible builds.

2. **DevBridge capture + surfacing**: `ipp_service.rs:248` consults the new fields with the fallback chain `document-name → job-name → ""`. The empty-string sentinel flows unchanged through the existing plumbing. UI components on the Dashboard and Jobs pages render a conditional secondary muted line, hidden when empty or when the name begins with `job-`.

## ippper fork — exact changes

Target file: `ippper/src/service/simple.rs` on branch `devbridge-document-name` of `zbynekdrlik/ippper.rs`.

**Extend `SimpleIppJobAttributes` struct (line 43):**

```rust
#[derive(fmt_derive::Debug, Clone)]
pub struct SimpleIppJobAttributes {
    pub originating_user_name: String,
    pub document_name: Option<String>,  // NEW
    pub job_name: Option<String>,       // NEW
    pub media: String,
    pub orientation: Option<PageOrientation>,
    pub sides: String,
    pub print_color_mode: String,
    pub printer_resolution: Option<Resolution>,
}
```

**Extend `take_ipp_attributes` (around line 53):**

```rust
impl SimpleIppJobAttributes {
    pub(crate) fn take_ipp_attributes(
        info: &PrinterInfo,
        originating_user_name: String,
        attributes: &mut IppAttributes,
    ) -> Self {
        // document-name and job-name are operation attributes, not job attributes
        let document_name = take_ipp_attribute(
            attributes,
            DelimiterTag::OperationAttributes,
            "document-name",
        )
        .and_then(|attr| attr.into_name_without_language().ok());

        let job_name = take_ipp_attribute(
            attributes,
            DelimiterTag::OperationAttributes,
            "job-name",
        )
        .and_then(|attr| attr.into_name_without_language().ok());

        let media = take_ipp_attribute(attributes, DelimiterTag::JobAttributes, "media")
            .and_then(|attr| attr.into_keyword().ok())
            .unwrap_or_else(|| info.media_default.clone());
        // ... rest unchanged ...

        Self {
            originating_user_name,
            document_name,
            job_name,
            media,
            orientation,
            sides,
            print_color_mode,
            printer_resolution,
        }
    }
}
```

**Update any `SimpleIppJobAttributes { ... }` constructions in the library's own tests** to include the two new fields as `None`.

## DevBridge — exact changes

### `Cargo.toml` (workspace root)

Add at the bottom:

```toml
[patch.crates-io]
ippper = { git = "https://github.com/zbynekdrlik/ippper.rs", rev = "<commit-sha-after-fork-push>" }
```

Workspace version bump: `0.8.9 → 0.8.10` in both `Cargo.toml` and `crates/devbridge-app/tauri.conf.json`.

### `crates/devbridge-server/src/ipp_service.rs`

Extract the extraction logic into a pure helper for unit testability:

```rust
/// Extract a display-friendly document name from IPP job attributes.
///
/// Prefers `document-name` (set by Windows/macOS print spoolers), falls back
/// to `job-name` (set by LPR/CUPS clients), returns empty string when neither
/// is present. Whitespace-only names are treated as absent. The empty string
/// is the sentinel value meaning "no real name available" — UI components
/// hide the field when they see it.
fn extract_document_name(attrs: &SimpleIppJobAttributes) -> String {
    attrs
        .document_name
        .as_deref()
        .or(attrs.job_name.as_deref())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_default()
}
```

Replace `ipp_service.rs:248`:

```rust
// Before:
let document_name = format!("job-{job_id}");

// After:
let document_name = extract_document_name(&document.job_attributes);
```

The rest of `handle_document` (hash, size, spool write, queue push) is unchanged.

### `crates/devbridge-ui/src/pages/dashboard.rs` (JobCard component)

Add a conditional secondary muted line below the user/printer header row. The existing `name` extraction at line 241 stays; we add a render check.

Helper to decide whether to display the name:

```rust
fn display_document_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.starts_with("job-") {
        None
    } else {
        // Truncate overly long names for layout
        if trimmed.len() > 80 {
            Some(format!("{}…", &trimmed[..79]))
        } else {
            Some(trimmed.to_string())
        }
    }
}
```

Render block added after the user/printer span (around line 311):

```rust
{display_document_name(&name).map(|display| view! {
    <div style="font-size: 0.8em; color: var(--text-muted); font-family: monospace; margin-top: 0.15rem; padding-left: 0.25rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">
        {display}
    </div>
})}
```

### `crates/devbridge-ui/src/pages/jobs.rs` (JobsPage table)

Same helper (`display_document_name`) placed in a shared module (or duplicated — YAGNI). In each row's User cell, append the document name as a second muted line:

```rust
<td>
    {user}
    {display_document_name(&name).map(|display| view! {
        <div style="font-size: 0.8em; color: var(--text-muted); font-family: monospace; margin-top: 0.15rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 20rem">
            {display}
        </div>
    })}
</td>
```

The existing 5-column table layout is preserved. `name` is fetched from the API response alongside `user` in the same `.map` closure.

## Data flow

```
Print client (Windows/macOS/CUPS)
  │
  │  IPP Print-Job request
  │    operation-attributes:
  │      document-name        = "Invoice-2026-001.pdf"   [NEW: captured]
  │      job-name             = "Invoice-2026-001.pdf"   [NEW: fallback]
  │      requesting-user-name = "cashier1"               [existing]
  │    job-attributes:
  │      media, sides, ...                               [existing]
  ▼
ippper::SimpleIppService::print_job
  - take_ipp_attributes() pulls all fields into SimpleIppJobAttributes
  - calls handler.handle_document(SimpleIppDocument)
  │
  ▼
devbridge-server::JobHandler::handle_document (ipp_service.rs)
  - extract_document_name(&document.job_attributes)
  - JobMetadata { document_name: "Invoice-2026-001.pdf", ... }
  │
  ▼
queue.push → SQLite (TEXT NOT NULL, empty "" for absent)
  ▼
dispatch → gRPC → client receiver → print backend  [unchanged]
  ▼
dashboard GET /api/jobs returns { "name": "Invoice-2026-001.pdf", ... }
  ▼
UI Dashboard JobCard + Jobs table row
  - display_document_name() filters out "" and "job-*" legacy names
  - renders secondary muted line when Some, hides when None
```

## Error handling

| Situation | Behavior |
|-----------|----------|
| IPP request omits both `document-name` and `job-name` | `extract_document_name` returns `""`, UI hides the line. No placeholder. |
| `into_name_without_language()` fails (unexpected attribute type) | Treated as `None`, falls through to next attribute or empty. |
| Whitespace-only name (`"   "`) | `.trim().is_empty()` filters out, treated as absent. |
| Legacy `job-<uuid>` rows in SQLite | `display_document_name()` sees `job-` prefix, returns `None`, UI hides. |
| Name longer than 80 chars | Truncated with `…` suffix at 79 chars in UI helper. |
| ippper fork commit hash stale / GitHub unreachable during `cargo build` | CI build fails loudly. Pinned-rev approach means no silent drift. |

## Testing plan

### Unit tests — `ipp_service.rs` (`#[cfg(test)] mod tests`)

Pure tests of `extract_document_name`:

- `test_prefers_document_name_over_job_name` — both set, returns `document-name`.
- `test_falls_back_to_job_name` — only `job-name` set, returns `job-name`.
- `test_empty_when_neither_present` — both `None`, returns `""`.
- `test_trims_whitespace` — `"  invoice.pdf  "` → `"invoice.pdf"`.
- `test_whitespace_only_treated_as_absent` — `"   "` → `""`.
- `test_empty_string_treated_as_absent` — `Some("")` → `""`.

### Unit tests — UI helper `display_document_name`

- `test_hides_empty` — `""` → `None`.
- `test_hides_legacy_uuid_name` — `"job-abc-123"` → `None`.
- `test_shows_real_name` — `"invoice.pdf"` → `Some("invoice.pdf")`.
- `test_truncates_long_name` — 200-char input → 80-char output ending with `…`.
- `test_trims_whitespace` — `"  x  "` → `Some("x")`.

### Integration test — `crates/devbridge-server/tests/ipp_document_name_test.rs`

Build a raw IPP Print-Job request with known operation attributes using the `ipp` crate, send it to a test-harness `SimpleIppService` backed by a capturing handler, assert the handler observed the expected `document_name`/`job_name` values through `SimpleIppJobAttributes`. Two cases:

1. Both `document-name` and `job-name` present → captured correctly, fallback chain produces `document-name`.
2. Neither attribute present → `SimpleIppJobAttributes.document_name` and `.job_name` both `None`, `extract_document_name` returns `""`.

### Playwright E2E — `playwright/tests/document-name.spec.ts`

Runs against the CI devbridge-service (started by existing `playwright-e2e` job).

1. Submit an IPP Print-Job with `document-name = "Playwright-Invoice-2026.pdf"` via `curl` with pre-built raw IPP bytes (or a small Node helper using the `ipp` npm package).
2. Navigate to Dashboard page, wait for the job card to appear, assert the muted name line contains `Playwright-Invoice-2026.pdf`.
3. Navigate to Jobs page, assert the second line under the user cell contains the same name.
4. Submit a second job without `document-name`/`job-name` attributes.
5. Assert that job's rendering does NOT contain a muted name line (query returns zero elements matching the name-line selector for that row).
6. Console zero-errors assertion on both pages.

### E2E test on self-hosted runner — `crates/devbridge-e2e/src/main.rs`

Add a step after the existing IPP print test:
- Fetch `/api/jobs?limit=5`
- Assert the most recent job's `name` field does NOT start with `job-` (i.e., the real IPP name was captured).
- Assert it equals the expected filename the test sent.

### Mutation testing

`cargo mutants` runs against the new `extract_document_name` and `display_document_name` helpers. All mutants must be killed by the unit tests above. Zero surviving mutants in new code is a hard requirement.

## Post-deploy verification

After CI deploys v0.8.10 to pz-server and pz-snv:

1. **Server (pz-server)**:
   - `curl http://10.88.1.100:9120/api/status | jq .version` → `"0.8.10"`
   - Print a real PDF from a Windows host to the virtual printer.
   - `curl http://10.88.1.100:9120/api/jobs?limit=5 | jq '.[0].name'` → real filename, not `job-<uuid>`.
   - Open dashboard in Playwright at `http://10.88.1.100:9120`, screenshot newest JobCard, verify muted name line visible with the real filename.

2. **Client (pz-snv)**:
   - `curl http://10.78.2.10:9120/api/status | jq .version` → `"0.8.10"`
   - Submit a test print via the existing E2E flow, verify the client-side dashboard also shows the real name.

3. Report evidence in completion report with actual observed filenames, not a generic "working" claim.

## Version bump

- Workspace: `Cargo.toml` `[workspace.package] version = "0.8.9" → "0.8.10"`.
- Tauri: `crates/devbridge-app/tauri.conf.json` `"version": "0.8.9" → "0.8.10"`.
- Must be the first commit on `dev` before any other change.

## Out of scope

- Upstream PR to `ArcticLampyrid/ippper.rs`. Track as a follow-up GitHub issue but do not block this work on it.
- Publishing `zbynekdrlik/ippper.rs` to crates.io.
- SQLite migration to nullable `document_name` column.
- Backfill of legacy `job-<uuid>` rows.
- Changes to reprint toast text (it already uses `name`; it will automatically benefit once the real name is captured).
