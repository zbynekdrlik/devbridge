# Issue #37: Capture IPP Copies Attribute — Design

## Problem

Multi-copy print jobs always print exactly 1 copy regardless of what the user requested. Reported by pjpos user (Epson L3260): printing with N copies set in the Windows print dialog yields 1 sheet; to get 7 copies the user had to press Print 7 times.

Root cause: `crates/devbridge-server/src/ipp_service.rs:284` hardcodes `copies: 1`. The IPP `copies` attribute (RFC 8011 §5.2.5, `integer(1:MAX)`) is never read from the incoming Print-Job request.

The downstream pipeline already supports multi-copy end-to-end:
- `JobMetadata.copies: u32` (`crates/devbridge-core/src/job.rs:38`)
- `storage.rs` persists it
- `dispatch.rs:474, 482` propagates it through gRPC
- `receiver.rs:251, 283, 337` passes it to the print backend and event log

Only the capture point is broken. This is the exact same shape as issue #30 (document_name capture).

## Architecture

Single-point fix. Mirrors #30 exactly:

1. Extend `zbynekdrlik/ippper.rs` fork (branch `devbridge-document-name`) to expose `copies: Option<u32>` in `SimpleIppJobAttributes`.
2. Pin the new fork SHA via `[patch.crates-io].ippper.rev` in workspace `Cargo.toml`.
3. Add `extract_copies` helper in `ipp_service.rs` with clamp-≥1, default-1 semantics.
4. Replace `copies: 1,` literal at `ipp_service.rs:284` with `copies: extract_copies(&document.job_attributes),`.

No schema changes. No UI changes. No config changes.

## Fork Change (`zbynekdrlik/ippper.rs`)

**Branch:** `devbridge-document-name` (reuse existing DevBridge-specific branch).

**File:** `src/service/simple.rs`

Add field to `SimpleIppJobAttributes`:

```rust
pub struct SimpleIppJobAttributes {
    // ... existing fields ...
    pub print_color_mode: String,
    pub printer_resolution: Option<Resolution>,
    pub copies: Option<u32>,
}
```

Add parser in `take_ipp_attributes`:

```rust
let copies = take_ipp_attribute(attributes, DelimiterTag::JobAttributes, "copies")
    .and_then(|attr| match attr {
        IppValue::Integer(n) => u32::try_from(n).ok(),
        _ => None,
    });
```

Include `copies` in the returned `Self { .. }` literal.

Add unit test asserting `copies=3` parses from a crafted `IppAttributes`.

## DevBridge Changes

### Version bump (first commit)

- `Cargo.toml`: `version = "0.8.11"` → `"0.8.12"` in `[workspace.package]`.
- `crates/devbridge-app/tauri.conf.json`: `"version": "0.8.11"` → `"0.8.12"`.

### Dependency pin

`Cargo.toml` (workspace root):
```toml
[patch.crates-io]
ippper = { git = "https://github.com/zbynekdrlik/ippper.rs", rev = "<new SHA>" }
```

Replace `rev = "914f0aaf6089d76aa6231f837bb3208499457e02"` with the new fork commit SHA.

### `extract_copies` helper

In `crates/devbridge-server/src/ipp_service.rs`, next to `extract_document_name`:

```rust
/// Extract requested copy count from IPP job attributes.
///
/// Returns `copies` when present and ≥ 1, otherwise defaults to 1. Zero and
/// negative values are treated as absent (IPP `copies` type is `integer(1:MAX)`
/// per RFC 8011 §5.2.5; a value < 1 is a client bug, not a valid request for
/// zero copies).
fn extract_copies(attrs: &SimpleIppJobAttributes) -> u32 {
    attrs.copies.filter(|&n| n >= 1).unwrap_or(1)
}
```

### Capture point

`ipp_service.rs:284`:

```diff
-            copies: 1,
+            copies: extract_copies(&document.job_attributes),
```

## Testing

### Unit tests (`ipp_service.rs` `#[cfg(test)] mod tests`)

- `extract_copies_none_defaults_to_1`
- `extract_copies_zero_defaults_to_1`
- `extract_copies_one_returns_1`
- `extract_copies_seven_returns_7`
- `extract_copies_u32_max_returns_max`

### Integration test (`ipp_service.rs`)

Build a raw IPP Print-Job request with a `copies` attribute set to 5, invoke the handler, assert the queued `JobMetadata` has `copies: 5`.

### Self-hosted E2E (`crates/devbridge-e2e/src/main.rs`)

Add a new test block after the existing document-name assertion:
- Send a synthetic IPP Print-Job with `copies=3` to the server.
- Poll `/api/jobs` until the job appears.
- Assert `jobs[0].copies == 3`.
- Assert physical output on the E2E client (spooler EventID 307 with page count for windows_spooler, or equivalent for direct_ipp).

### Playwright

Not applicable — copies is not a dashboard UI field. No Playwright test required.

## Error Handling

| Input | Output |
|-------|--------|
| Attribute absent | `1` |
| `IppValue::Integer(n)` where `n >= 1` | `n as u32` |
| `IppValue::Integer(n)` where `n < 1` | `1` |
| `IppValue::Integer(n)` where `n > u32::MAX` | `1` (via `try_from` failure) |
| Non-integer variant | `1` |

Fork-side: parse failures fall through to `None` (silent). DevBridge-side: `extract_copies` applies the clamp. No warn logs — absent/invalid is not worth polluting the log at info rate.

## Deployment

After PR merged to `main`:
- v0.8.12 NSIS installer (Windows) and DMG (macOS) auto-published via release workflow.
- Deploy to all 5 production instances via `irm install.ps1 | iex` or mac launchctl reload:
  - pz-server (10.88.1.100)
  - pz-snv (10.78.2.10) — verify copies=3 produces 3 sheets on Canon MG3600
  - pjpos (10.78.5.10) — original reporter; user verifies
  - pz-holla (10.88.1.105)
  - pz-david (10.88.1.104) — both `com.devbridge.wifi` and `com.devbridge.usb` instances

Post-deploy verification: print a test document with copies=3 from server, confirm `/api/jobs` reports `copies: 3` and the physical printer outputs 3 sheets.

## Out of Scope

- Capping copies at an upper bound (printers/spoolers handle their own limits).
- Supporting IPP Job-Template `number-of-documents` (not the same attribute).
- Per-backend copy-count overrides (e.g., "ignore copies for shared printers") — no user request for this.
- Dashboard column for "copies" — JobMetadata.copies is already persisted and available via `/api/jobs`; adding it to the UI is a separate enhancement.
