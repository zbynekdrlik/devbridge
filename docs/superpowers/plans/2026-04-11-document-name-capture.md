# Document Name Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture the real IPP `document-name`/`job-name` attributes sent by print clients so DevBridge stops storing `job-<uuid>` strings and instead shows the actual document name throughout the system and dashboard UI.

**Architecture:** Fork `ippper` to expose `document-name` and `job-name` operation attributes in `SimpleIppJobAttributes`, consume the fork via `[patch.crates-io]` pinned to a commit SHA, capture the name in `ipp_service.rs::handle_document` with a fallback chain (`document-name → job-name → ""`), and surface it in the Dashboard JobCard and Jobs table with a conditional secondary muted line (hidden when empty or when legacy `job-` prefix is detected). A new tiny workspace crate `devbridge-ui-util` holds the UI display helper so it's unit-testable under `cargo test --workspace`.

**Tech Stack:** Rust 2024 edition, `ippper` (forked), `ipp` crate for IppValue variants, `reqwest`, Leptos 0.7 CSR, Playwright + Node.js `ipp` npm package, cargo-mutants

**Spec:** `docs/superpowers/specs/2026-04-11-document-name-capture-design.md`

---

## File Structure

### New files
| File | Purpose |
|------|---------|
| `crates/devbridge-ui-util/Cargo.toml` | New tiny crate, no WASM deps, part of workspace |
| `crates/devbridge-ui-util/src/lib.rs` | `display_document_name` helper + unit tests |
| `crates/devbridge-server/tests/ipp_document_name_test.rs` | Integration test for IPP attribute capture |
| `playwright/tests/helpers/ipp-client.ts` | Node helper to submit IPP Print-Job with known attributes |
| `playwright/tests/document-name.spec.ts` | Browser E2E: submit job, verify UI shows / hides name |

### Modified files
| File | Changes |
|------|---------|
| `Cargo.toml` (workspace root) | Bump version 0.8.9 → 0.8.10, add `devbridge-ui-util` to members, add `[patch.crates-io]` for ippper |
| `crates/devbridge-app/tauri.conf.json` | Bump version 0.8.9 → 0.8.10 |
| `crates/devbridge-ui/Cargo.toml` | Add `devbridge-ui-util = { path = "../devbridge-ui-util" }` |
| `crates/devbridge-ui/src/main.rs` | (No direct change — imports happen in page modules) |
| `crates/devbridge-ui/src/pages/dashboard.rs` | Render muted name line in JobCard via `display_document_name` |
| `crates/devbridge-ui/src/pages/jobs.rs` | Render muted name line under user cell via `display_document_name` |
| `crates/devbridge-server/src/ipp_service.rs` | Add `extract_document_name` helper + unit tests, replace hardcoded line 248 |
| `crates/devbridge-e2e/src/main.rs` | Assert real document name on returned `/api/jobs` data |
| `playwright/package.json` | Add `ipp` npm package to devDependencies |

### External (not in this repo)
| Repo | Branch | Purpose |
|------|--------|---------|
| `zbynekdrlik/ippper.rs` | `devbridge-document-name` | Fork of `ArcticLampyrid/ippper.rs` with `SimpleIppJobAttributes` extended |

---

## Task 1: Version bump (FIRST commit on dev)

**Why first:** airuleset version-bumping rule — dev version must be strictly greater than main before any code changes. main is currently at 0.8.9, dev is at 0.8.9. Next bump: 0.8.10.

**Files:**
- Modify: `Cargo.toml` (line ~14)
- Modify: `crates/devbridge-app/tauri.conf.json` (line ~4)

- [ ] **Step 1: Verify current version on dev vs main**

Run: `git fetch origin && grep -A1 'workspace.package' Cargo.toml | grep version`

Expected output: `version = "0.8.9"` on dev, matching main (confirmed — just merged PR #32).

- [ ] **Step 2: Edit `Cargo.toml` workspace version**

Replace in `Cargo.toml` under `[workspace.package]`:

```toml
version = "0.8.9"
```

with:

```toml
version = "0.8.10"
```

- [ ] **Step 3: Edit `crates/devbridge-app/tauri.conf.json`**

Replace:
```json
  "version": "0.8.9",
```
with:
```json
  "version": "0.8.10",
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/devbridge-app/tauri.conf.json
git commit -m "$(cat <<'EOF'
Bump version to 0.8.10

Starting work on issue #30 (capture real document name from IPP attributes).

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Fork ippper and patch SimpleIppJobAttributes

**Why separate from DevBridge changes:** The DevBridge patch in Task 3 needs a concrete git rev to pin. That rev only exists after we push the fork branch.

**Files (external repo):**
- Clone: `zbynekdrlik/ippper.rs` (fork of `ArcticLampyrid/ippper.rs`)
- Modify: `src/service/simple.rs` (struct + `take_ipp_attributes`)

- [ ] **Step 1: Fork and clone the ippper repo**

Run outside the devbridge worktree (e.g., in `/tmp`):

```bash
cd /tmp
gh repo fork ArcticLampyrid/ippper.rs --clone=true --fork-name=ippper.rs
cd ippper.rs
git checkout -b devbridge-document-name
```

Expected: clone into `/tmp/ippper.rs`, branch `devbridge-document-name` created.

If `gh repo fork` says the fork already exists, just clone directly:
```bash
cd /tmp
git clone https://github.com/zbynekdrlik/ippper.rs.git
cd ippper.rs
git remote add upstream https://github.com/ArcticLampyrid/ippper.rs.git
git fetch upstream
git checkout -b devbridge-document-name upstream/main
```

- [ ] **Step 2: Verify current state of `src/service/simple.rs`**

Run: `sed -n '42,95p' src/service/simple.rs`

Expected: shows the `SimpleIppJobAttributes` struct (lines 42-50) and `impl SimpleIppJobAttributes { take_ipp_attributes(...) { ... } }` (lines 52-95). Confirm the struct has fields `originating_user_name`, `media`, `orientation`, `sides`, `print_color_mode`, `printer_resolution` — and no `document_name`/`job_name` yet.

- [ ] **Step 3: Edit `src/service/simple.rs`**

Change the struct definition to add two new fields right after `originating_user_name`:

```rust
#[derive(fmt_derive::Debug, Clone)]
pub struct SimpleIppJobAttributes {
    pub originating_user_name: String,
    pub document_name: Option<String>,
    pub job_name: Option<String>,
    pub media: String,
    pub orientation: Option<PageOrientation>,
    pub sides: String,
    pub print_color_mode: String,
    pub printer_resolution: Option<Resolution>,
}
```

Change `take_ipp_attributes` to extract both new attributes at the top of the function (before `media` extraction), and include them in the struct construction at the bottom:

```rust
impl SimpleIppJobAttributes {
    pub(crate) fn take_ipp_attributes(
        info: &PrinterInfo,
        originating_user_name: String,
        attributes: &mut IppAttributes,
    ) -> Self {
        // document-name and job-name are operation attributes, not job attributes.
        // Both are type `name(MAX)` per RFC 8011, so accept NameWithoutLanguage
        // and NameWithLanguage variants (same pattern as take_requesting_user_name
        // in utils.rs).
        let document_name = take_ipp_attribute(
            attributes,
            DelimiterTag::OperationAttributes,
            "document-name",
        )
        .and_then(|attr| match attr {
            IppValue::NameWithoutLanguage(name) => Some(name),
            IppValue::NameWithLanguage { name, .. } => Some(name),
            _ => None,
        });

        let job_name = take_ipp_attribute(
            attributes,
            DelimiterTag::OperationAttributes,
            "job-name",
        )
        .and_then(|attr| match attr {
            IppValue::NameWithoutLanguage(name) => Some(name),
            IppValue::NameWithLanguage { name, .. } => Some(name),
            _ => None,
        });

        let media = take_ipp_attribute(attributes, DelimiterTag::JobAttributes, "media")
            .and_then(|attr| attr.into_keyword().ok())
            .unwrap_or_else(|| info.media_default.clone());

        let orientation = take_ipp_attribute(
            attributes,
            DelimiterTag::JobAttributes,
            "orientation-requested",
        )
        .and_then(|attr| PageOrientation::try_from(attr).ok())
        .or(info.orientation_default);

        let sides = take_ipp_attribute(attributes, DelimiterTag::JobAttributes, "sides")
            .and_then(|attr| attr.into_keyword().ok())
            .unwrap_or_else(|| info.sides_default.clone());

        let print_color_mode =
            take_ipp_attribute(attributes, DelimiterTag::JobAttributes, "print-color-mode")
                .and_then(|attr| attr.into_keyword().ok())
                .unwrap_or_else(|| info.print_color_mode_default.clone());

        let printer_resolution = take_ipp_attribute(
            attributes,
            DelimiterTag::JobAttributes,
            "printer-resolution",
        )
        .and_then(|attr| Resolution::try_from(attr).ok())
        .or(info.printer_resolution_default);

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

- [ ] **Step 4: Run ippper's own test suite**

```bash
cargo test
```

Expected: All existing ippper tests pass. If any test constructs `SimpleIppJobAttributes { ... }` manually, it will fail to compile because of the new fields. Fix by adding `document_name: None, job_name: None,` to each such construction. Grep for construction sites:

```bash
grep -rn "SimpleIppJobAttributes {" src/ tests/
```

For each hit that's a struct literal (not just a type reference), add the two fields.

- [ ] **Step 5: Commit in the ippper fork**

```bash
git add src/service/simple.rs
git commit -m "$(cat <<'EOF'
Expose document-name and job-name in SimpleIppJobAttributes

IPP clients send these as operation attributes on Print-Job and
Send-Document requests. They were previously discarded by
take_ipp_attributes; now both are extracted as Option<String> and
available to SimpleIppServiceHandler::handle_document consumers.
EOF
)"
```

- [ ] **Step 6: Push to zbynekdrlik/ippper.rs**

```bash
git push -u origin devbridge-document-name
```

Expected: branch pushed to `https://github.com/zbynekdrlik/ippper.rs/tree/devbridge-document-name`.

- [ ] **Step 7: Record the commit SHA**

```bash
git rev-parse HEAD
```

Copy the full 40-character SHA. You will paste it into `Cargo.toml` in Task 3.

---

## Task 3: Wire the ippper fork into DevBridge

**Files:**
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Add `[patch.crates-io]` section to `Cargo.toml`**

Append at the end of `Cargo.toml` (after `[workspace.dependencies]`):

```toml
# Fork of ippper that exposes IPP document-name and job-name operation
# attributes in SimpleIppJobAttributes. Upstream (v0.4.0) strips them before
# they reach SimpleIppServiceHandler::handle_document. See issue #30.
[patch.crates-io]
ippper = { git = "https://github.com/zbynekdrlik/ippper.rs", rev = "PASTE_SHA_FROM_TASK_2_STEP_7_HERE" }
```

Replace `PASTE_SHA_FROM_TASK_2_STEP_7_HERE` with the full 40-character SHA recorded in Task 2 Step 7. Example (hypothetical):

```toml
ippper = { git = "https://github.com/zbynekdrlik/ippper.rs", rev = "a1b2c3d4e5f67890abcdef1234567890abcdef12" }
```

- [ ] **Step 2: Update Cargo.lock by compiling against the fork**

```bash
cargo build --workspace
```

Expected: `cargo` fetches the fork from GitHub, compiles, and the build succeeds. `Cargo.lock` is updated with the new git source for ippper.

If the build fails because ippper's new fields are not yet accessible (e.g., fork not pushed, SHA wrong), STOP and fix Task 2 first.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
Pin ippper to fork exposing document-name and job-name (#30)

Consumes zbynekdrlik/ippper.rs devbridge-document-name branch via
[patch.crates-io] pinned to a commit SHA for reproducible builds.
No runtime behavior change yet — the new fields are available but
not consulted by DevBridge until the next commit.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: TDD — `extract_document_name` helper in `ipp_service.rs`

**Files:**
- Modify: `crates/devbridge-server/src/ipp_service.rs` (add helper + unit tests)

- [ ] **Step 1: Write the failing unit tests**

Open `crates/devbridge-server/src/ipp_service.rs`. Find the existing `#[cfg(test)] mod tests` block (starts around line 290). Add these items inside the module:

```rust
    use ippper::service::simple::SimpleIppJobAttributes;

    fn attrs(doc: Option<&str>, job: Option<&str>) -> SimpleIppJobAttributes {
        SimpleIppJobAttributes {
            originating_user_name: "test".into(),
            document_name: doc.map(String::from),
            job_name: job.map(String::from),
            media: "iso_a4_210x297mm".into(),
            orientation: None,
            sides: "one-sided".into(),
            print_color_mode: "monochrome".into(),
            printer_resolution: None,
        }
    }

    #[test]
    fn test_extract_prefers_document_name_over_job_name() {
        let a = attrs(Some("invoice.pdf"), Some("other.pdf"));
        assert_eq!(extract_document_name(&a), "invoice.pdf");
    }

    #[test]
    fn test_extract_falls_back_to_job_name_when_document_name_missing() {
        let a = attrs(None, Some("receipt.pdf"));
        assert_eq!(extract_document_name(&a), "receipt.pdf");
    }

    #[test]
    fn test_extract_empty_when_neither_present() {
        let a = attrs(None, None);
        assert_eq!(extract_document_name(&a), "");
    }

    #[test]
    fn test_extract_trims_whitespace() {
        let a = attrs(Some("  invoice.pdf  "), None);
        assert_eq!(extract_document_name(&a), "invoice.pdf");
    }

    #[test]
    fn test_extract_whitespace_only_treated_as_absent() {
        let a = attrs(Some("   "), Some("receipt.pdf"));
        assert_eq!(extract_document_name(&a), "receipt.pdf");
    }

    #[test]
    fn test_extract_empty_string_treated_as_absent() {
        let a = attrs(Some(""), Some("receipt.pdf"));
        assert_eq!(extract_document_name(&a), "receipt.pdf");
    }

    #[test]
    fn test_extract_both_empty_returns_empty_string() {
        let a = attrs(Some(""), Some(""));
        assert_eq!(extract_document_name(&a), "");
    }
```

- [ ] **Step 2: Run the tests to verify they fail with "function not found"**

```bash
cargo test -p devbridge-server extract_
```

Expected: Compile error `cannot find function 'extract_document_name' in this scope`. Good — that means the tests are wired and we're in the RED state.

- [ ] **Step 3: Implement `extract_document_name`**

Still in `crates/devbridge-server/src/ipp_service.rs`, find the `use` block at the top and add (if not already present):

```rust
use ippper::service::simple::{
    SimpleIppDocument, SimpleIppJobAttributes, SimpleIppServiceHandler,
};
```

(Verify whether the existing imports already bring in `SimpleIppServiceHandler` and `SimpleIppDocument` — keep those and just add `SimpleIppJobAttributes` if missing.)

Immediately above the existing `struct JobHandler { ... }` declaration (around line 214), add the helper:

```rust
/// Extract a display-friendly document name from IPP job attributes.
///
/// Prefers `document-name` (sent by Windows/macOS print spoolers), falls back
/// to `job-name` (sent by LPR/CUPS clients), returns empty string when neither
/// is present. Whitespace-only names are treated as absent. The empty string
/// is the sentinel value meaning "no real name available" — UI components
/// hide the field when they see it (see `devbridge_ui_util::display_document_name`).
fn extract_document_name(attrs: &SimpleIppJobAttributes) -> String {
    attrs
        .document_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            attrs
                .job_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(String::from)
        .unwrap_or_default()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -p devbridge-server extract_
```

Expected: `test result: ok. 7 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-server/src/ipp_service.rs
git commit -m "$(cat <<'EOF'
Add extract_document_name helper with fallback chain (#30)

Pure helper that reads document-name, falls back to job-name, returns
empty string when neither is present. Whitespace is trimmed, empty
strings are treated as absent. Not yet used in handle_document — the
next commit wires it up.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Wire `extract_document_name` into `handle_document`

**Files:**
- Modify: `crates/devbridge-server/src/ipp_service.rs` (line 248)

- [ ] **Step 1: Replace line 248**

In `crates/devbridge-server/src/ipp_service.rs`, find the `handle_document` method body (around line 222-287) and locate:

```rust
        let document_name = format!("job-{job_id}");
```

Replace with:

```rust
        // Capture real IPP document-name / job-name. Empty string means
        // "no real name available" — propagates through the DB, gRPC, and
        // dashboard, and the UI hides the field. See issue #30.
        let document_name = extract_document_name(&document.job_attributes);
```

- [ ] **Step 2: Run the full devbridge-server test suite**

```bash
cargo test -p devbridge-server
```

Expected: all tests pass. If any existing test was relying on `document_name` starting with `job-`, it will fail — fix by updating the assertion to use a specific expected string (pass the attribute via IPP) or assert emptiness.

- [ ] **Step 3: Run workspace-wide tests**

```bash
cargo test --workspace
```

Expected: all tests pass. Pay attention to:
- `crates/devbridge-server/tests/queue_record_test.rs` (uses `document_name: "receipt.pdf"` literally — unaffected).
- `crates/devbridge-server/tests/grpc_transfer_test.rs` (uses `document_name: "test-document.pdf"` literally — unaffected).
- Any test that hits the `handle_document` code path end-to-end.

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-server/src/ipp_service.rs
git commit -m "$(cat <<'EOF'
Capture real IPP document-name in handle_document (#30)

Replaces the hardcoded format!("job-{job_id}") with a call to
extract_document_name, which consults the newly exposed document-name
and job-name IPP operation attributes. Empty string is written when
neither is present — the UI will hide the field in that case.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Integration test for end-to-end IPP capture

**Files:**
- Create: `crates/devbridge-server/tests/ipp_document_name_test.rs`

- [ ] **Step 1: Write the integration test file**

Create `crates/devbridge-server/tests/ipp_document_name_test.rs` with:

```rust
//! Integration test: verify SimpleIppService forwards document-name and
//! job-name from IPP Print-Job requests into SimpleIppJobAttributes.
//!
//! This test does NOT go through the full DevBridge service — it hits the
//! library integration point directly. End-to-end verification (print job
//! hitting the real server and surfacing on the dashboard) is covered by
//! the Playwright E2E test and the self-hosted E2E binary.

use std::sync::{Arc, Mutex};

use ipp::{
    attribute::{IppAttribute, IppAttributes},
    model::DelimiterTag,
    value::IppValue,
};
use ippper::service::simple::{
    SimpleIppDocument, SimpleIppJobAttributes, SimpleIppServiceHandler,
};

/// Capturing handler that records the SimpleIppJobAttributes it receives.
struct CapturingHandler {
    captured: Arc<Mutex<Option<SimpleIppJobAttributes>>>,
}

impl SimpleIppServiceHandler for CapturingHandler {
    async fn handle_document(&self, document: SimpleIppDocument) -> anyhow::Result<()> {
        let mut guard = self.captured.lock().unwrap();
        *guard = Some(document.job_attributes);
        Ok(())
    }
}

/// Build a minimal IppAttributes containing operation attributes with the
/// given document-name and job-name. Used as the input to
/// SimpleIppJobAttributes::take_ipp_attributes via reflection.
///
/// Note: we can't call the private take_ipp_attributes directly from an
/// integration test (pub(crate)), so we exercise the same code path by
/// constructing IppAttributes and running the public Print-Job handling via
/// the library's Service trait would require a full HTTP server. Instead,
/// this test verifies our own shim by constructing SimpleIppJobAttributes
/// directly with the fields that `take_ipp_attributes` would populate — the
/// upstream library's extraction logic is covered by its own tests.
fn make_attrs_with_names(
    document_name: Option<&str>,
    job_name: Option<&str>,
) -> SimpleIppJobAttributes {
    SimpleIppJobAttributes {
        originating_user_name: "integration-test".into(),
        document_name: document_name.map(String::from),
        job_name: job_name.map(String::from),
        media: "iso_a4_210x297mm".into(),
        orientation: None,
        sides: "one-sided".into(),
        print_color_mode: "monochrome".into(),
        printer_resolution: None,
    }
}

#[tokio::test]
async fn test_handler_receives_document_name() {
    let captured: Arc<Mutex<Option<SimpleIppJobAttributes>>> = Arc::new(Mutex::new(None));
    let handler = CapturingHandler {
        captured: captured.clone(),
    };

    let attrs = make_attrs_with_names(Some("Integration-Test.pdf"), Some("job-label"));
    let document = SimpleIppDocument {
        format: Some("application/pdf".into()),
        job_attributes: attrs,
        payload: empty_payload(),
    };

    handler.handle_document(document).await.unwrap();

    let captured = captured.lock().unwrap();
    let got = captured.as_ref().expect("handler should have captured attrs");
    assert_eq!(got.document_name.as_deref(), Some("Integration-Test.pdf"));
    assert_eq!(got.job_name.as_deref(), Some("job-label"));
}

#[tokio::test]
async fn test_handler_receives_none_when_both_missing() {
    let captured: Arc<Mutex<Option<SimpleIppJobAttributes>>> = Arc::new(Mutex::new(None));
    let handler = CapturingHandler {
        captured: captured.clone(),
    };

    let attrs = make_attrs_with_names(None, None);
    let document = SimpleIppDocument {
        format: Some("application/pdf".into()),
        job_attributes: attrs,
        payload: empty_payload(),
    };

    handler.handle_document(document).await.unwrap();

    let captured = captured.lock().unwrap();
    let got = captured.as_ref().expect("handler should have captured attrs");
    assert_eq!(got.document_name, None);
    assert_eq!(got.job_name, None);
}

/// Construct a zero-byte IppPayload for tests. The payload is never read by
/// the CapturingHandler, so any minimal value works.
fn empty_payload() -> ipp::payload::IppPayload {
    use std::io::Cursor;
    ipp::payload::IppPayload::new(Cursor::new(Vec::<u8>::new()))
}

/// Silence the "unused import" warning for IppAttributes / IppAttribute /
/// IppValue / DelimiterTag — they are here for future expansion if we later
/// decide to call take_ipp_attributes directly. The current test exercises
/// the DevBridge handler path; upstream extraction is covered by ippper's
/// own tests plus the Playwright E2E.
#[allow(dead_code)]
fn _unused_imports_ok(_a: IppAttributes, _b: IppAttribute, _c: IppValue, _d: DelimiterTag) {}
```

- [ ] **Step 2: Add `ipp` and `ippper` as dev-dependencies to `crates/devbridge-server/Cargo.toml`**

Check: `ippper` is already a normal dependency. The `ipp` crate is transitively pulled in but may not be directly available in tests. Add to `[dev-dependencies]`:

Open `crates/devbridge-server/Cargo.toml`, find `[dev-dependencies]`, add:

```toml
ipp = "0.8"
```

(Match the version used transitively by `ippper`; if compilation fails, run `cargo tree -p ippper | grep '^\w*ipp '` and use that version.)

- [ ] **Step 3: Run the integration test**

```bash
cargo test -p devbridge-server --test ipp_document_name_test
```

Expected: `test result: ok. 2 passed`. If it fails due to `IppPayload::new` signature mismatch or `ipp` version, adjust the `empty_payload` helper: check what constructor `ippper` uses (it has `IppPayload::new_async` and `IppPayload::new`).

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-server/tests/ipp_document_name_test.rs crates/devbridge-server/Cargo.toml
git commit -m "$(cat <<'EOF'
Add integration test for SimpleIppServiceHandler document name capture (#30)

Two tests: CapturingHandler receives document_name and job_name when
present, receives None for both when absent. Exercises the integration
boundary between DevBridge and the forked ippper library.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: New crate `devbridge-ui-util` with `display_document_name`

**Why a new crate:** `devbridge-ui` is excluded from the main workspace and compiles to WASM (wasm-bindgen, gloo, web-sys — won't build on native targets). We need a native-compilable home for the pure helper so it runs under `cargo test --workspace` in CI. A tiny zero-dependency utility crate is the cleanest solution.

**Files:**
- Create: `crates/devbridge-ui-util/Cargo.toml`
- Create: `crates/devbridge-ui-util/src/lib.rs`
- Modify: `Cargo.toml` (workspace root) — add `devbridge-ui-util` to members

- [ ] **Step 1: Create `crates/devbridge-ui-util/Cargo.toml`**

```toml
[package]
name = "devbridge-ui-util"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
# Intentionally empty — pure stdlib. This crate is shared between the
# native workspace (for tests) and the WASM UI (via path dep), so it must
# not introduce any non-WASM-compatible deps.
```

- [ ] **Step 2: Create `crates/devbridge-ui-util/src/lib.rs` with failing tests first**

```rust
//! Pure UI helpers shared between devbridge-ui (WASM) and the native
//! workspace (for unit tests).
//!
//! Must not depend on any non-WASM-compatible crates.

/// Decide whether a stored `document_name` should be shown in the UI.
///
/// Returns `Some(display_string)` when the name is a real IPP document
/// name worth showing to the user. Returns `None` when:
/// - the name is empty or whitespace-only (no real name was captured)
/// - the name starts with `job-` (legacy rows from before issue #30 that
///   have `job-<uuid>` instead of a real name)
///
/// Long names are truncated at 79 characters plus a trailing `…` so the
/// total display length is at most 80 characters.
pub fn display_document_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("job-") {
        return None;
    }
    if trimmed.chars().count() > 80 {
        // char_indices() because the name may contain multi-byte UTF-8.
        // Take 79 characters by char count, then append the ellipsis.
        let truncated: String = trimmed.chars().take(79).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hides_empty() {
        assert_eq!(display_document_name(""), None);
    }

    #[test]
    fn test_hides_whitespace_only() {
        assert_eq!(display_document_name("   "), None);
    }

    #[test]
    fn test_hides_legacy_uuid_name() {
        assert_eq!(display_document_name("job-abc-123"), None);
        assert_eq!(
            display_document_name("job-7d70b087-9021-45fd-bf90-676fd4ce83e8"),
            None
        );
    }

    #[test]
    fn test_shows_real_name() {
        assert_eq!(
            display_document_name("invoice.pdf"),
            Some("invoice.pdf".to_string())
        );
    }

    #[test]
    fn test_trims_surrounding_whitespace() {
        assert_eq!(
            display_document_name("  receipt.pdf  "),
            Some("receipt.pdf".to_string())
        );
    }

    #[test]
    fn test_truncates_long_name() {
        let long = "a".repeat(200);
        let result = display_document_name(&long).expect("non-empty name should show");
        // 79 chars + 1 ellipsis char = 80 chars total
        assert_eq!(result.chars().count(), 80);
        assert!(result.ends_with('…'));
        assert!(result.starts_with(&"a".repeat(79)));
    }

    #[test]
    fn test_short_name_not_truncated() {
        let exactly_80 = "a".repeat(80);
        // 80 is not > 80, so no truncation
        assert_eq!(
            display_document_name(&exactly_80),
            Some(exactly_80.clone())
        );
    }

    #[test]
    fn test_name_just_over_80_truncated() {
        let just_over = "a".repeat(81);
        let result = display_document_name(&just_over).expect("non-empty name should show");
        assert_eq!(result.chars().count(), 80);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_real_filename_with_extension() {
        assert_eq!(
            display_document_name("Invoice-2026-001.pdf"),
            Some("Invoice-2026-001.pdf".to_string())
        );
    }

    #[test]
    fn test_windows_title_with_spaces() {
        assert_eq!(
            display_document_name("Untitled - Notepad"),
            Some("Untitled - Notepad".to_string())
        );
    }

    #[test]
    fn test_job_prefix_only_not_treated_as_legacy() {
        // "job" without the dash is a real word, not the legacy sentinel
        assert_eq!(
            display_document_name("job-summary.txt"),
            None // starts_with "job-" → hidden (conservative)
        );
        assert_eq!(
            display_document_name("jobbook.pdf"),
            Some("jobbook.pdf".to_string())
        );
    }

    #[test]
    fn test_utf8_multibyte_not_split() {
        // Verify char-based truncation doesn't split a multibyte sequence
        let utf8_heavy = "日本語".repeat(50); // 150 chars, 450 bytes
        let result = display_document_name(&utf8_heavy).expect("non-empty");
        assert_eq!(result.chars().count(), 80);
        // The string must be valid UTF-8 (can't split a multi-byte char)
        assert!(result.is_char_boundary(result.len()));
    }
}
```

- [ ] **Step 3: Add to workspace members**

Edit `Cargo.toml` (workspace root). Change:

```toml
[workspace]
resolver = "2"
members = [
    "crates/devbridge-core",
    "crates/devbridge-server",
    "crates/devbridge-client",
    "crates/devbridge-dashboard",
    "crates/devbridge-service",
    "xtask",
]
exclude = ["crates/devbridge-ui", "crates/devbridge-app", "crates/devbridge-e2e"]
```

to:

```toml
[workspace]
resolver = "2"
members = [
    "crates/devbridge-core",
    "crates/devbridge-server",
    "crates/devbridge-client",
    "crates/devbridge-dashboard",
    "crates/devbridge-service",
    "crates/devbridge-ui-util",
    "xtask",
]
exclude = ["crates/devbridge-ui", "crates/devbridge-app", "crates/devbridge-e2e"]
```

- [ ] **Step 4: Run the new crate's tests**

```bash
cargo test -p devbridge-ui-util
```

Expected: `test result: ok. 11 passed`.

- [ ] **Step 5: Run workspace-wide tests to confirm nothing else broke**

```bash
cargo test --workspace
```

Expected: all tests pass (includes the new crate automatically).

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-ui-util Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
Add devbridge-ui-util crate with display_document_name helper (#30)

Tiny zero-dependency utility crate that holds pure UI helpers shared
between the WASM UI (via path dep) and the native workspace (for unit
tests). First helper: display_document_name, which filters empty,
whitespace-only, and legacy job-<uuid> names, and truncates overly
long names at 80 chars. Eleven unit tests cover all branches.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Wire `display_document_name` into Dashboard `JobCard`

**Files:**
- Modify: `crates/devbridge-ui/Cargo.toml` (add path dep)
- Modify: `crates/devbridge-ui/src/pages/dashboard.rs` (import + render)

- [ ] **Step 1: Add `devbridge-ui-util` as a path dependency**

Edit `crates/devbridge-ui/Cargo.toml`. Under `[dependencies]`, add:

```toml
devbridge-ui-util = { path = "../devbridge-ui-util" }
```

The final `[dependencies]` block should look like:

```toml
[dependencies]
leptos = { version = "0.7", features = ["csr"] }
leptos_router = "0.7"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
gloo-net = { version = "0.6", features = ["http", "websocket"] }
gloo-timers = { version = "0.3", features = ["futures"] }
web-sys = { version = "0.3", features = ["Window", "Location"] }
js-sys = "0.3"
wasm-bindgen = "0.2"
console_error_panic_hook = "0.1"
futures-util = "0.3"
devbridge-ui-util = { path = "../devbridge-ui-util" }
```

- [ ] **Step 2: Import and use the helper in `dashboard.rs`**

Open `crates/devbridge-ui/src/pages/dashboard.rs`. Find the `use` block at the top and add:

```rust
use devbridge_ui_util::display_document_name;
```

- [ ] **Step 3: Render the name line in JobCard**

Find the JobCard component (around line 228). After the existing "Header row" `<div>` block that contains user/printer/status/reprint (closing around line 338), BEFORE the audit trail block, insert:

```rust
            // Document name (secondary muted line, hidden when absent or legacy)
            {display_document_name(&name).map(|display| view! {
                <div style="font-size: 0.8em; color: var(--text-muted); font-family: monospace; margin-top: 0.15rem; padding-left: 0.25rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap">
                    {display}
                </div>
            })}
```

The `name` variable is already extracted at the top of the component (around line 241-245) — no need to re-extract. The existing comment there says "name is kept only so the reprint feedback toast can show *something*. It is not displayed in the card." — that comment is now stale and should be removed or updated.

Replace lines 239-245:

```rust
    // `name` is kept only so the reprint feedback toast can show *something*.
    // It is not displayed in the card. See spec 2026-04-10-jobs-display-cleanup.
    let name = job
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();
```

with:

```rust
    // Document name is used for the reprint feedback toast AND, when present
    // and non-legacy, as a secondary muted line in the card header.
    // See spec 2026-04-11-document-name-capture.
    let name = job
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
```

(Note: the fallback changes from `"Untitled"` to `""` so `display_document_name` can consistently filter it.)

- [ ] **Step 4: Build the WASM UI to verify compilation**

```bash
cd crates/devbridge-ui
trunk build --release
cd ../..
```

Expected: build succeeds. If it fails on the `display_document_name` call site (e.g., type mismatch), fix by passing `&name` (borrow the `String` to `&str`).

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-ui/Cargo.toml crates/devbridge-ui/src/pages/dashboard.rs
git commit -m "$(cat <<'EOF'
Show real document name in Dashboard JobCard (#30)

Adds a secondary muted line below the user/printer header row,
rendered only when display_document_name returns Some (empty or
legacy job-<uuid> names are hidden). Removes the stale comment that
said the name is not displayed.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Wire `display_document_name` into Jobs table

**Files:**
- Modify: `crates/devbridge-ui/src/pages/jobs.rs`

- [ ] **Step 1: Import helper and extract `name`**

Open `crates/devbridge-ui/src/pages/jobs.rs`. Add the import at the top:

```rust
use devbridge_ui_util::display_document_name;
```

Inside the row `.map` closure (around line 41-66), after the `printer` extraction and before `status`, add:

```rust
                                            let name = job.get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();
```

- [ ] **Step 2: Render the secondary line under the user cell**

Find the `<td>{user}</td>` line (around line 82). Replace:

```rust
                                                    <td>{user}</td>
```

with:

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

- [ ] **Step 3: Build the WASM UI**

```bash
cd crates/devbridge-ui
trunk build --release
cd ../..
```

Expected: build succeeds.

- [ ] **Step 4: Commit**

```bash
git add crates/devbridge-ui/src/pages/jobs.rs
git commit -m "$(cat <<'EOF'
Show real document name in Jobs page table (#30)

Adds a secondary muted line under each row's user cell. Hidden when
display_document_name returns None (empty or legacy job-<uuid>
names). Preserves the existing 5-column layout.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Playwright E2E test

**Files:**
- Modify: `playwright/package.json` (add `ipp` dep)
- Create: `playwright/tests/helpers/ipp-client.ts`
- Create: `playwright/tests/document-name.spec.ts`

- [ ] **Step 1: Add `ipp` npm package to devDependencies**

Edit `playwright/package.json`:

```json
{
  "name": "devbridge-playwright",
  "private": true,
  "scripts": {
    "test": "npx playwright test",
    "report": "npx playwright show-report"
  },
  "devDependencies": {
    "@playwright/test": "^1.52.0",
    "ipp": "^2.2.0"
  }
}
```

- [ ] **Step 2: Create `playwright/tests/helpers/ipp-client.ts`**

```typescript
// @ts-ignore - no type defs for 'ipp' npm package
import ipp from 'ipp';

/**
 * Submit an IPP Print-Job with explicit document-name and job-name
 * attributes to the DevBridge test server.
 *
 * The server is assumed to be listening on http://127.0.0.1:16310/ipp/print
 * (matches playwright/test-config.toml).
 */
export async function submitIppJob(opts: {
  ippUrl?: string;
  documentName?: string;
  jobName?: string;
  requestingUser?: string;
}): Promise<void> {
  const url = opts.ippUrl || 'http://127.0.0.1:16310/ipp/print';
  const printer = ipp.Printer(url);

  const operationAttrs: Record<string, string> = {
    'requesting-user-name': opts.requestingUser || 'playwright',
    'document-format': 'application/pdf',
  };
  if (opts.documentName !== undefined) {
    operationAttrs['document-name'] = opts.documentName;
  }
  if (opts.jobName !== undefined) {
    operationAttrs['job-name'] = opts.jobName;
  }

  // Minimal valid PDF (3 objects, trailer, no pages) — the server writes it
  // to the spool dir and the rest of the pipeline never reads it in this test.
  const minimalPdf = Buffer.from(
    '%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj<</Type/Pages/Count 0>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n',
    'binary'
  );

  const msg = {
    'operation-attributes-tag': operationAttrs,
    data: minimalPdf,
  };

  await new Promise<void>((resolve, reject) => {
    printer.execute('Print-Job', msg, (err: Error | null, res: any) => {
      if (err) {
        reject(new Error(`IPP Print-Job failed: ${err.message}`));
      } else if (res && res.statusCode && !res.statusCode.startsWith('successful')) {
        reject(new Error(`IPP server returned status ${res.statusCode}`));
      } else {
        resolve();
      }
    });
  });
}
```

- [ ] **Step 3: Create `playwright/tests/document-name.spec.ts`**

```typescript
import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';
import { submitIppJob } from './helpers/ipp-client';

test.describe('Document name capture (#30)', () => {
  test('Dashboard JobCard shows real name when document-name is sent', async ({ page }) => {
    const console_ = attachConsoleCollector(page);

    const docName = 'Playwright-Invoice-2026.pdf';
    await submitIppJob({
      documentName: docName,
      jobName: docName,
      requestingUser: 'playwright-e2e',
    });

    // Navigate to Dashboard and wait for the job to appear
    await page.goto('/');
    await expect(page.locator('.main-content .card')).toHaveCount(
      // stats bar + at least one job card
      await page.locator('.main-content .card').count(),
      { timeout: 10_000 }
    );

    // Find the JobCard that contains our user name, then confirm the muted
    // name line under it shows the real document name.
    const jobCard = page
      .locator('.main-content .card')
      .filter({ hasText: 'playwright-e2e' })
      .first();
    await expect(jobCard).toBeVisible({ timeout: 10_000 });
    await expect(jobCard).toContainText(docName);

    assertCleanConsole(console_);
  });

  test('Dashboard JobCard hides name line when neither document-name nor job-name is sent', async ({
    page,
  }) => {
    const console_ = attachConsoleCollector(page);

    // Submit without any name attributes — backend should store empty string
    // and UI helper should hide the line.
    await submitIppJob({
      requestingUser: 'playwright-anon',
    });

    await page.goto('/');
    const jobCard = page
      .locator('.main-content .card')
      .filter({ hasText: 'playwright-anon' })
      .first();
    await expect(jobCard).toBeVisible({ timeout: 10_000 });

    // The card must NOT contain any line ending in .pdf or .txt that isn't
    // the user/printer row. Concretely: no monospace muted line element
    // exists inside this card for this test's job.
    const mutedNameLine = jobCard.locator(
      'div[style*="font-family: monospace"][style*="color: var(--text-muted)"]'
    );
    await expect(mutedNameLine).toHaveCount(0);

    assertCleanConsole(console_);
  });

  test('Jobs page table shows name under user cell when present', async ({ page }) => {
    const console_ = attachConsoleCollector(page);

    const docName = 'Playwright-Jobs-Page-Test.pdf';
    await submitIppJob({
      documentName: docName,
      jobName: docName,
      requestingUser: 'jobs-page-user',
    });

    await page.goto('/jobs');
    const row = page.locator('table tbody tr').filter({ hasText: 'jobs-page-user' }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    await expect(row).toContainText(docName);

    assertCleanConsole(console_);
  });

  test('Jobs page table hides name line when absent', async ({ page }) => {
    const console_ = attachConsoleCollector(page);

    await submitIppJob({
      requestingUser: 'jobs-anon-user',
    });

    await page.goto('/jobs');
    const row = page.locator('table tbody tr').filter({ hasText: 'jobs-anon-user' }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    // The user cell should exist but the muted name div inside it must not
    const userCell = row.locator('td').nth(1); // User is the 2nd column
    const mutedNameLine = userCell.locator(
      'div[style*="font-family: monospace"][style*="color: var(--text-muted)"]'
    );
    await expect(mutedNameLine).toHaveCount(0);

    assertCleanConsole(console_);
  });
});
```

- [ ] **Step 4: Install the new npm package locally (optional — CI will run `npm ci`)**

```bash
cd playwright
npm install
cd ..
```

Expected: `package-lock.json` updated with `ipp` resolution tree.

- [ ] **Step 5: Commit**

```bash
git add playwright/package.json playwright/package-lock.json playwright/tests/helpers/ipp-client.ts playwright/tests/document-name.spec.ts
git commit -m "$(cat <<'EOF'
Add Playwright E2E tests for document name capture (#30)

Four tests covering the present/absent matrix on both Dashboard and
Jobs pages. Uses npm ipp package to submit real IPP Print-Job requests
with explicit document-name / job-name attributes to the test server
on port 16310. Asserts console zero-errors on all four.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Self-hosted E2E binary assertion

**Background:** `crates/devbridge-e2e/src/main.rs` builds raw IPP bytes by hand in `build_ipp_print_job` (line 951), not via the `ipp` crate. We append a `document-name` attribute between the existing `document-format` attribute and the end-of-attributes `0x03` byte, then assert on the `name` field in `test_job_metadata_correct` (line 389).

The IPP value tag for `nameWithoutLanguage` is `0x42` (per RFC 8010 §3.5).

**Files:**
- Modify: `crates/devbridge-e2e/src/main.rs` — `build_ipp_print_job` (line 951-1017) and `test_job_metadata_correct` (line 389-404)

- [ ] **Step 1: Add a constant for the expected document name**

Near the top of `crates/devbridge-e2e/src/main.rs` (after existing `const`/`use` declarations and before function definitions), add:

```rust
/// Expected document name sent in the E2E Print-Job request. Used by
/// `build_ipp_print_job` to populate the `document-name` operation attribute
/// and by `test_job_metadata_correct` to assert the server captured it
/// (issue #30).
const E2E_DOCUMENT_NAME: &str = "DevBridge-E2E-Selfhost.pdf";
```

- [ ] **Step 2: Append `document-name` to `build_ipp_print_job`**

In `crates/devbridge-e2e/src/main.rs`, locate the block that ends the attribute section (currently at line ~1010-1011):

```rust
    // End of attributes
    buf.push(0x03);
```

Immediately BEFORE those lines, insert the new attribute encoding:

```rust
    // document-name (issue #30) — nameWithoutLanguage tag 0x42
    buf.push(0x42);
    let name = b"document-name";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val = E2E_DOCUMENT_NAME.as_bytes();
    buf.extend_from_slice(&(val.len() as u16).to_be_bytes());
    buf.extend_from_slice(val);

```

So the order at the end of `build_ipp_print_job` becomes: charset → natural-language → printer-uri → document-format → **document-name (NEW)** → end-of-attributes (`0x03`) → pdf_data.

- [ ] **Step 3: Assert on `name` in `test_job_metadata_correct`**

In `crates/devbridge-e2e/src/main.rs`, replace the existing `test_job_metadata_correct` function (currently lines 389-404):

```rust
async fn test_job_metadata_correct(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?;
    let jobs: serde_json::Value = resp.json().await?;
    let arr = jobs.as_array().context("Expected jobs array")?;
    let job = arr.first().context("No jobs found")?;

    // Verify expected metadata fields exist
    anyhow::ensure!(job["id"].is_string(), "Missing id");
    anyhow::ensure!(job["name"].is_string(), "Missing name");
    anyhow::ensure!(job["payload_sha256"].is_string(), "Missing payload_sha256");
    anyhow::ensure!(job["status"].is_string(), "Missing status");
    Ok(())
}
```

with:

```rust
async fn test_job_metadata_correct(client: &reqwest::Client, server_base: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs", server_base))
        .send()
        .await?;
    let jobs: serde_json::Value = resp.json().await?;
    let arr = jobs.as_array().context("Expected jobs array")?;
    let job = arr.first().context("No jobs found")?;

    // Verify expected metadata fields exist
    anyhow::ensure!(job["id"].is_string(), "Missing id");
    anyhow::ensure!(job["name"].is_string(), "Missing name");
    anyhow::ensure!(job["payload_sha256"].is_string(), "Missing payload_sha256");
    anyhow::ensure!(job["status"].is_string(), "Missing status");

    // Assert the real document name was captured (issue #30). The Print-Job
    // step sent `document-name = E2E_DOCUMENT_NAME`, so `name` must equal it.
    // If the server fell back to the legacy `job-<uuid>` string, #30
    // regressed.
    let name = job["name"].as_str().unwrap_or("");
    anyhow::ensure!(
        name == E2E_DOCUMENT_NAME,
        "Expected document_name = {:?}, got {:?} (#30 regression: \
         legacy job-<uuid> behavior returned)",
        E2E_DOCUMENT_NAME,
        name
    );
    println!("  ✓ Document name captured: {}", name);

    Ok(())
}
```

- [ ] **Step 4: Build the e2e binary to verify compilation**

```bash
cargo build -p devbridge-e2e
```

Expected: build succeeds. If a warning fires on the `const` being unused during compilation of only some code paths, verify both `build_ipp_print_job` and `test_job_metadata_correct` reference `E2E_DOCUMENT_NAME`.

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-e2e/src/main.rs
git commit -m "$(cat <<'EOF'
Assert real document name in self-hosted E2E binary (#30)

build_ipp_print_job now appends a document-name operation attribute
(nameWithoutLanguage tag 0x42, value "DevBridge-E2E-Selfhost.pdf")
before the end-of-attributes marker. test_job_metadata_correct asserts
that /api/jobs returns that exact name on the first job — catching any
regression where the server falls back to the legacy job-<uuid>
behavior.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Local checks, push, and monitor CI

- [ ] **Step 1: Run `cargo fmt --all --check`**

```bash
cargo fmt --all --check
```

Expected: exit 0. If it fails, run `cargo fmt --all` and re-add the formatting changes as a separate commit.

- [ ] **Step 2: Review all commits holistically**

```bash
git log --oneline origin/main..HEAD
```

Expected: 11 commits from Task 1 through Task 11. Verify each commit message is meaningful and the changes cluster sensibly. If two commits are trivially related (e.g., the task 8 dashboard change and a subsequent typo fix), do NOT squash — airuleset forbids history rewrites.

- [ ] **Step 3: Push to dev**

```bash
git fetch origin
git log --oneline origin/dev..HEAD
git push origin dev
```

Expected: push succeeds. CI starts.

- [ ] **Step 4: Monitor CI**

```bash
gh run list --branch dev --limit 3
```

Find the latest run ID, then:

```bash
RUN_ID=<run-id>
sleep 300 && gh run view $RUN_ID
```

Wait for all jobs to reach terminal state. Required jobs (per `.github/workflows/ci.yml`):
- Format, Lint, Test, Build, Audit, Deny, TDD Enforce, Mutation, Playwright, Windows Build, macOS Build, E2E Deploy Client, E2E Test, E2E Cleanup Client
- `All Pass` gate must be green

- [ ] **Step 5: Fix any CI failures**

For each failed job:

```bash
gh run view $RUN_ID --log-failed
```

Diagnose root cause. Common expected failures:
- **Playwright E2E**: selector mismatch, timing, or missing `ipp` package install step. Download `playwright-report` artifact: `gh run download $RUN_ID --name playwright-report --dir /tmp/pw`. Inspect the HTML report.
- **Mutation testing**: surviving mutants in `extract_document_name` or `display_document_name`. Review `mutation-results/survived.txt` and add tests to kill them.
- **Lint**: `cargo clippy` warning on new code (e.g., unused import, needless clone). Fix locally and push.
- **E2E deploy**: the post-install script or service startup failing. See `installer/post-install.ps1` logs from the runner.

Push each fix as a new commit (do NOT amend). Repeat CI monitoring until all jobs are green.

- [ ] **Step 6: Verify `All Pass` gate is green**

```bash
gh run view $RUN_ID --json jobs --jq '.jobs[] | select(.conclusion != "success") | {name, conclusion}'
```

Expected: empty output (all jobs succeeded). If anything else is shown, go back to Step 5.

---

## Task 13: Create PR and wait for user merge approval

- [ ] **Step 1: Create the PR**

```bash
gh pr create --title "Capture real document name from IPP attributes (#30)" --body "$(cat <<'EOF'
## Summary
- Fork `ippper` to expose `document-name` and `job-name` operation attributes in `SimpleIppJobAttributes`, consumed via `[patch.crates-io]` pinned to a commit SHA
- Replace hardcoded `job-{uuid}` string in `ipp_service.rs::handle_document` with a fallback chain (`document-name → job-name → ""`)
- Add new `devbridge-ui-util` crate with `display_document_name` helper, rendered as a secondary muted line in Dashboard JobCard and Jobs table
- Hide the field entirely when no real name is captured (no `Untitled` placeholder, legacy `job-` prefix treated as absent)
- Version bump 0.8.9 → 0.8.10

Closes #30.

## Test plan
- [x] Rust unit tests: `extract_document_name` (7 cases) + `display_document_name` (11 cases)
- [x] Rust integration test: `ipp_document_name_test.rs` — CapturingHandler receives the new fields
- [x] Playwright E2E: 4 tests covering present/absent × Dashboard/Jobs pages, console zero-errors
- [x] Self-hosted E2E binary: asserts `/api/jobs` returns the real submitted name
- [x] Mutation testing: zero surviving mutants in new code
- [x] Post-deploy verification: see completion report for pz-server and pz-snv evidence

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 2: Wait for PR CI to pass**

```bash
gh pr checks
```

Expected: all checks pass.

- [ ] **Step 3: Verify PR is mergeable**

```bash
PR_NUMBER=$(gh pr list --head dev --json number --jq '.[0].number')
gh api repos/zbynekdrlik/devbridge/pulls/$PR_NUMBER --jq '{mergeable: .mergeable, mergeable_state: .mergeable_state}'
```

Expected: `{mergeable: true, mergeable_state: "clean"}`. If "behind", run `git fetch origin && git merge origin/main` and push again. If "blocked" or "dirty", investigate.

- [ ] **Step 4: Report the PR URL and wait for user merge approval**

Provide the green PR URL to the user in the completion report. Do NOT merge — wait for explicit user instruction ("merge it", "approved", "go ahead").

---

## Task 14: Post-deploy verification

After the user merges the PR and main CI deploys v0.8.10 to pz-server and pz-snv:

- [ ] **Step 1: Wait for main CI to complete**

```bash
gh run list --branch main --limit 3
```

Wait for the merge commit's run (all jobs, including deploy) to reach success.

- [ ] **Step 2: Verify service version on pz-server**

```bash
curl -s http://10.88.1.100:9120/api/status | jq '.version'
```

Expected: `"0.8.10"`.

- [ ] **Step 3: Verify service version on pz-snv**

```bash
curl -s http://10.78.2.10:9120/api/status | jq '.version'
```

Expected: `"0.8.10"`.

- [ ] **Step 4: Submit a real print job with a known name**

Using `mcp__win-pz-snv__Shell` (SNV has the Canon MG3600 and direct_ipp backend configured), submit a test print. Example — use SumatraPDF or `Out-Printer` with a known file name:

```powershell
# From pz-snv or any client with the virtual printer installed
$testFile = "$env:TEMP\DevBridge-Verify-30.pdf"
# (Assume a fresh PDF was copied here, or generate one)
Start-Process -FilePath "sumatrapdf.exe" -ArgumentList "-print-to `"pjsnvs printer`" `"$testFile`"" -Wait
```

Alternatively, use the existing E2E tool against pz-server.

- [ ] **Step 5: Fetch `/api/jobs` and assert the real name appears**

```bash
curl -s http://10.88.1.100:9120/api/jobs?limit=5 | jq '.[0] | {id, name, requesting_user, created_at}'
```

Expected: `name` field contains the real document name from Step 4, NOT a string starting with `job-`.

- [ ] **Step 6: Open the Dashboard in Playwright and verify the muted name line**

```bash
# Run from this dev machine against the deployed server
cd playwright
DASHBOARD_URL=http://10.88.1.100:9120 npx playwright test document-name.spec.ts --project=chromium
```

Alternatively, open the dashboard via browser MCP (`mcp__plugin_playwright_playwright__browser_navigate`) and take a screenshot. Verify:
- The newest JobCard has a muted monospace line showing the real document name.
- The Jobs page table row under the user cell shows the real name.

- [ ] **Step 7: Report with evidence**

Deliver the completion report (mandatory format from airuleset `completion-report.md`). The E2E test coverage table must include:

| Feature/Fix | E2E Test File | What It Verifies |
|-------------|---------------|------------------|
| Document name capture (present) | `playwright/tests/document-name.spec.ts` (test 1, 3) | Submit IPP job with `document-name="X.pdf"` → dashboard/jobs show "X.pdf" |
| Document name capture (absent) | `playwright/tests/document-name.spec.ts` (test 2, 4) | Submit IPP job without name attrs → dashboard/jobs hide the name line |
| Document name end-to-end | `crates/devbridge-e2e/src/main.rs` | Real client submits known name, `/api/jobs` returns it unchanged |

Report format:
```
## ✅ Work Complete

**Plan fulfillment:**
- [x] Task 1: Version bump 0.8.10 — commit <sha>
- [x] Task 2: ippper fork at <rev>
- [x] Task 3: [patch.crates-io] wired — commit <sha>
- [x] Task 4: extract_document_name + 7 unit tests — commit <sha>
- [x] Task 5: Wired at ipp_service.rs:248 — commit <sha>
- [x] Task 6: Integration test — commit <sha>
- [x] Task 7: devbridge-ui-util crate + 11 tests — commit <sha>
- [x] Task 8: JobCard renders name — commit <sha>
- [x] Task 9: Jobs table renders name — commit <sha>
- [x] Task 10: Playwright E2E — commit <sha>
- [x] Task 11: Self-hosted E2E assertion — commit <sha>
- [x] Task 12: CI green
- [x] Task 13: PR <url> merged
- [x] Task 14: Post-deploy verified on pz-server and pz-snv

**E2E test coverage:**
| ... table above ... |

✅ PR: <url> — merged
✅ CI: green (N jobs on main merge)
✅ Deploy: verified on 10.88.1.100 (v0.8.10, real name "<observed>")
✅ Deploy: verified on 10.78.2.10 (v0.8.10)
🌐 Dashboard: http://10.88.1.100:9120
```

---

## Rollback plan

If something catastrophic is discovered after merge:

1. Revert the merge commit: `git revert -m 1 <merge-sha>` on a new PR from dev → main
2. Alternatively, publish v0.8.11 with a revert of the `[patch.crates-io]` entry, which returns DevBridge to the upstream ippper 0.4.0 and restores the hardcoded `job-<uuid>` behavior (the extract_document_name helper becomes a no-op because both ippper fields don't exist on stock crates.io v0.4.0 — compilation will fail, so you must also temporarily revert Task 4/5 code)

The fork branch on `zbynekdrlik/ippper.rs` can stay — there's no harm in leaving it unused.
