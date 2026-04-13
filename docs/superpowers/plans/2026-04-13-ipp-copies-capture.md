# IPP Copies Attribute Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture the IPP `copies` attribute so multi-copy print jobs actually print the requested number of copies.

**Architecture:** Single-point capture fix mirroring PR #33 (issue #30 document_name). Extend the forked ippper.rs to expose `copies: Option<u32>` in `SimpleIppJobAttributes`, pin the new fork rev, replace the hardcoded `copies: 1` at `crates/devbridge-server/src/ipp_service.rs:284` with an `extract_copies` helper that clamps to ≥1 with default 1. Downstream plumbing (JobMetadata, storage, gRPC dispatch, receiver, backends) is already intact.

**Tech Stack:** Rust 2024, tonic, axum, forked ippper.rs, cargo-mutants, self-hosted E2E binary.

**Spec:** `docs/superpowers/specs/2026-04-13-ipp-copies-capture-design.md`

---

## File Structure

### New Files
None.

### Modified Files
| File | Changes |
|------|---------|
| (fork) `src/service/simple.rs` | Add `copies: Option<u32>` field + parser in `take_ipp_attributes` |
| `Cargo.toml` (workspace) | Bump `[workspace.package].version` 0.8.11 → 0.8.12; bump `[patch.crates-io].ippper.rev` to new fork SHA |
| `crates/devbridge-app/tauri.conf.json` | Bump `"version"` 0.8.11 → 0.8.12 |
| `crates/devbridge-server/src/ipp_service.rs` | Add `extract_copies` helper + tests; replace `copies: 1` at line 284 |
| `crates/devbridge-e2e/src/main.rs` | Add `copies=3` attribute to IPP Print-Job builder; assert `jobs[0].copies == 3` |

---

## Task 1: Fork — expose `copies` in `SimpleIppJobAttributes`

The fork lives in a separate git repo: `https://github.com/zbynekdrlik/ippper.rs`, branch `devbridge-document-name`. Work in `/tmp/ippper-fork`.

**Files:**
- Modify: `src/service/simple.rs` (in the fork)

- [ ] **Step 1: Clone the fork branch into a scratch dir**

```bash
rm -rf /tmp/ippper-fork
git clone --branch devbridge-document-name https://github.com/zbynekdrlik/ippper.rs /tmp/ippper-fork
cd /tmp/ippper-fork
git log --oneline -3
```

Expected: top commit is `914f0aa Expose document-name and job-name in SimpleIppJobAttributes`.

- [ ] **Step 2: Add `copies` field to `SimpleIppJobAttributes`**

Edit `/tmp/ippper-fork/src/service/simple.rs`. The struct is declared around line 42–52:

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
    pub copies: Option<u32>,
}
```

(Add the `pub copies: Option<u32>,` line after `printer_resolution`.)

- [ ] **Step 3: Add the parser in `take_ipp_attributes`**

Immediately after the `printer_resolution` parser (around line 113) and before the `Self { ... }` literal (around line 114), insert:

```rust
        let copies = take_ipp_attribute(attributes, DelimiterTag::JobAttributes, "copies")
            .and_then(|attr| match attr {
                IppValue::Integer(n) => u32::try_from(n).ok(),
                _ => None,
            });
```

Then add `copies,` to the struct literal at the bottom of the function (after `printer_resolution,`):

```rust
        Self {
            originating_user_name,
            document_name,
            job_name,
            media,
            orientation,
            sides,
            print_color_mode,
            printer_resolution,
            copies,
        }
```

- [ ] **Step 4: Verify the fork compiles**

```bash
cd /tmp/ippper-fork
cargo check
```

Expected: compiles cleanly with zero errors, zero warnings related to the new field.

- [ ] **Step 5: Commit and push the fork change**

```bash
cd /tmp/ippper-fork
git add src/service/simple.rs
git commit -m "Expose IPP copies attribute in SimpleIppJobAttributes"
git push origin devbridge-document-name
NEW_SHA=$(git rev-parse HEAD)
echo "New ippper fork rev: $NEW_SHA"
```

Record the `$NEW_SHA` value — Task 3 Step 2 needs it.

---

## Task 2: Version bump to 0.8.12 (first commit on `dev`)

**Files:**
- Modify: `/home/newlevel/devel/devbridge/Cargo.toml` (line 16)
- Modify: `/home/newlevel/devel/devbridge/crates/devbridge-app/tauri.conf.json` (line 4)

- [ ] **Step 1: Sync `dev` with remote**

```bash
cd /home/newlevel/devel/devbridge
git fetch origin
git status
git merge origin/main || true
```

Expected: clean working tree on `dev`, up-to-date with `origin/main`.

- [ ] **Step 2: Bump workspace version**

In `/home/newlevel/devel/devbridge/Cargo.toml`, change:

```toml
[workspace.package]
version = "0.8.11"
```

to:

```toml
[workspace.package]
version = "0.8.12"
```

- [ ] **Step 3: Bump Tauri app version**

In `/home/newlevel/devel/devbridge/crates/devbridge-app/tauri.conf.json`, change:

```json
"version": "0.8.11",
```

to:

```json
"version": "0.8.12",
```

- [ ] **Step 4: Verify format cleanliness**

```bash
cd /home/newlevel/devel/devbridge
cargo fmt --all --check
```

Expected: exit 0 (no output).

- [ ] **Step 5: Commit**

```bash
cd /home/newlevel/devel/devbridge
git add Cargo.toml crates/devbridge-app/tauri.conf.json
git commit -m "Bump version to 0.8.12 for #37 IPP copies capture"
```

Do NOT push yet — continue to Task 3.

---

## Task 3: Pin new ippper rev + add `extract_copies` helper with TDD

**Files:**
- Modify: `/home/newlevel/devel/devbridge/Cargo.toml` (line 44, `[patch.crates-io]`)
- Modify: `/home/newlevel/devel/devbridge/crates/devbridge-server/src/ipp_service.rs` (add helper + tests)

- [ ] **Step 1: Write the failing unit tests first (TDD red)**

Open `/home/newlevel/devel/devbridge/crates/devbridge-server/src/ipp_service.rs`. The existing `#[cfg(test)] mod tests` block ends at line 384 with `test_extract_both_empty_returns_empty_string`. Just before the closing `}` of `mod tests` (line 384), insert the new tests AND update the `attrs` fixture to accept a `copies` parameter.

First, replace the existing `attrs` helper (currently lines 330–341) with this expanded version that adds a `copies` parameter:

```rust
    fn attrs(
        doc: Option<&str>,
        job: Option<&str>,
        copies: Option<u32>,
    ) -> SimpleIppJobAttributes {
        SimpleIppJobAttributes {
            originating_user_name: "test".into(),
            document_name: doc.map(String::from),
            job_name: job.map(String::from),
            media: "iso_a4_210x297mm".into(),
            orientation: None,
            sides: "one-sided".into(),
            print_color_mode: "monochrome".into(),
            printer_resolution: None,
            copies,
        }
    }
```

Then update every call site of `attrs(...)` in the existing tests to pass `None` as the third argument. The existing calls are:
- Line 345: `attrs(Some("invoice.pdf"), Some("other.pdf"))` → `attrs(Some("invoice.pdf"), Some("other.pdf"), None)`
- Line 351: `attrs(None, Some("receipt.pdf"))` → `attrs(None, Some("receipt.pdf"), None)`
- Line 357: `attrs(None, None)` → `attrs(None, None, None)`
- Line 363: `attrs(Some("  invoice.pdf  "), None)` → `attrs(Some("  invoice.pdf  "), None, None)`
- Line 369: `attrs(Some("   "), Some("receipt.pdf"))` → `attrs(Some("   "), Some("receipt.pdf"), None)`
- Line 375: `attrs(Some(""), Some("receipt.pdf"))` → `attrs(Some(""), Some("receipt.pdf"), None)`
- Line 381: `attrs(Some(""), Some(""))` → `attrs(Some(""), Some(""), None)`

Then append the new copies tests before the closing `}` of `mod tests`:

```rust
    #[test]
    fn test_extract_copies_none_defaults_to_1() {
        let a = attrs(None, None, None);
        assert_eq!(extract_copies(&a), 1);
    }

    #[test]
    fn test_extract_copies_zero_defaults_to_1() {
        let a = attrs(None, None, Some(0));
        assert_eq!(extract_copies(&a), 1);
    }

    #[test]
    fn test_extract_copies_one_returns_1() {
        let a = attrs(None, None, Some(1));
        assert_eq!(extract_copies(&a), 1);
    }

    #[test]
    fn test_extract_copies_seven_returns_7() {
        let a = attrs(None, None, Some(7));
        assert_eq!(extract_copies(&a), 7);
    }

    #[test]
    fn test_extract_copies_u32_max_returns_max() {
        let a = attrs(None, None, Some(u32::MAX));
        assert_eq!(extract_copies(&a), u32::MAX);
    }
```

- [ ] **Step 2: Pin the new ippper SHA in Cargo.toml**

Open `/home/newlevel/devel/devbridge/Cargo.toml`. Locate the `[patch.crates-io]` section (around line 43):

```toml
[patch.crates-io]
ippper = { git = "https://github.com/zbynekdrlik/ippper.rs", rev = "914f0aaf6089d76aa6231f837bb3208499457e02" }
```

Replace `914f0aaf6089d76aa6231f837bb3208499457e02` with the `$NEW_SHA` captured in Task 1 Step 5.

- [ ] **Step 3: Add the `extract_copies` helper (TDD green)**

In `/home/newlevel/devel/devbridge/crates/devbridge-server/src/ipp_service.rs`, right after `extract_document_name` (which ends at line 237) and before `struct JobHandler` (line 240), insert:

```rust
/// Extract requested copy count from IPP job attributes.
///
/// Returns `copies` when present and ≥ 1, otherwise defaults to 1. Zero and
/// negative values are treated as absent (IPP `copies` type is `integer(1:MAX)`
/// per RFC 8011 §5.2.5; a value < 1 is a client bug, not a valid request for
/// zero copies). See issue #37.
fn extract_copies(attrs: &SimpleIppJobAttributes) -> u32 {
    attrs.copies.filter(|&n| n >= 1).unwrap_or(1)
}
```

- [ ] **Step 4: Commit the helper + tests + pin (CI will verify)**

We cannot run `cargo test` locally (project rule: no local compile). CI will run the unit tests — we rely on it. Commit first, CI later.

```bash
cd /home/newlevel/devel/devbridge
cargo fmt --all --check
git add Cargo.toml crates/devbridge-server/src/ipp_service.rs
git commit -m "Add extract_copies helper + pin ippper fork (#37)"
```

Do NOT push yet — continue to Task 4.

---

## Task 4: Wire `extract_copies` into the capture point

**Files:**
- Modify: `/home/newlevel/devel/devbridge/crates/devbridge-server/src/ipp_service.rs:284`

- [ ] **Step 1: Replace the hardcoded literal**

In `/home/newlevel/devel/devbridge/crates/devbridge-server/src/ipp_service.rs`, find line 284 which currently reads:

```rust
            copies: 1,
```

Replace with:

```rust
            copies: extract_copies(&document.job_attributes),
```

- [ ] **Step 2: Verify format cleanliness**

```bash
cd /home/newlevel/devel/devbridge
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 3: Commit**

```bash
cd /home/newlevel/devel/devbridge
git add crates/devbridge-server/src/ipp_service.rs
git commit -m "Capture IPP copies from job attributes (#37)"
```

Do NOT push yet — continue to Task 5.

---

## Task 5: Extend self-hosted E2E to assert `copies=3`

The E2E binary in `crates/devbridge-e2e/src/main.rs` already sends `document-name` in its Print-Job and asserts the server captured it. Add the same shape for `copies`.

The spec calls for both an integration test and an E2E test. The E2E binary already exercises the exact same path — raw IPP bytes → HTTP POST → `JobHandler::handle_document` → `/api/jobs` — against a live server. A separate in-process integration test would duplicate the coverage without adding signal, so this plan relies solely on the E2E test for end-to-end verification of the capture point.

**Files:**
- Modify: `/home/newlevel/devel/devbridge/crates/devbridge-e2e/src/main.rs`

- [ ] **Step 1: Add an E2E constant for the expected copy count**

Open `/home/newlevel/devel/devbridge/crates/devbridge-e2e/src/main.rs`. After the existing `E2E_DOCUMENT_NAME` constant (lines 4–8), add:

```rust
/// Expected copies value sent in the E2E Print-Job request. Used by
/// `build_ipp_print_job` to populate the `copies` job attribute and by
/// `test_job_metadata_correct` to assert the server captured it (issue #37).
const E2E_COPIES: u32 = 3;
```

- [ ] **Step 2: Extend `build_ipp_print_job` to emit a Job Attributes group with `copies`**

Open `/home/newlevel/devel/devbridge/crates/devbridge-e2e/src/main.rs`. The function `build_ipp_print_job` starts at line 975. Currently the operation attributes block ends with the `document-name` attribute (lines 1031–1038), followed by `End of attributes (0x03)` at line 1041, then the document data.

IPP `copies` is a Job Template attribute — it must live in the Job Attributes group (delimiter tag `0x02`), not Operation Attributes (`0x01`). IPP integer type tag is `0x21`, values are 4-byte signed big-endian.

Replace lines 1040–1041 (the `// End of attributes` comment and `buf.push(0x03);`) with the following block:

```rust
    // Job Attributes group (issue #37) — delimiter tag 0x02
    buf.push(0x02);

    // copies — integer type 0x21, value is 4-byte signed big-endian
    buf.push(0x21);
    let name = b"copies";
    buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
    buf.extend_from_slice(name);
    let val: i32 = E2E_COPIES as i32;
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&val.to_be_bytes());

    // End of attributes
    buf.push(0x03);
```

- [ ] **Step 3: Extend `test_job_metadata_correct` to assert captured `copies`**

In the same file, the function `test_job_metadata_correct` starts at line 395. After the existing `document_name` assertion (lines 410–422, which ends with `println!("  ✓ Document name captured: {}", name);`) and before the final `Ok(())`, insert:

```rust
    // Assert the real IPP copies value was captured (issue #37). The
    // Print-Job step sent `copies = E2E_COPIES` as a Job attribute, so the
    // stored job must echo that back. Regression = hardcoded `copies: 1`
    // returned.
    let copies = job["copies"].as_u64().unwrap_or(0) as u32;
    anyhow::ensure!(
        copies == E2E_COPIES,
        "Expected copies = {}, got {} (#37 regression: hardcoded copies=1 \
         behavior returned)",
        E2E_COPIES,
        copies
    );
    println!("  ✓ Copies captured: {}", copies);
```

- [ ] **Step 4: Verify format cleanliness**

```bash
cd /home/newlevel/devel/devbridge
cargo fmt --all --check
```

Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
cd /home/newlevel/devel/devbridge
git add crates/devbridge-e2e/src/main.rs
git commit -m "E2E: send and assert IPP copies=3 (#37)"
```

---

## Task 6: Push, monitor CI, kill mutation survivors

- [ ] **Step 1: Confirm commit sequence**

```bash
cd /home/newlevel/devel/devbridge
git log --oneline origin/main..HEAD
```

Expected output (4 new commits, top-down):
```
<sha> E2E: send and assert IPP copies=3 (#37)
<sha> Capture IPP copies from job attributes (#37)
<sha> Add extract_copies helper + pin ippper fork (#37)
<sha> Bump version to 0.8.12 for #37 IPP copies capture
<sha> Design spec for #37: IPP copies attribute capture
```

(The design spec commit 56c8bfb was already made before this plan.)

- [ ] **Step 2: Push to dev**

```bash
cd /home/newlevel/devel/devbridge
git push origin dev
```

- [ ] **Step 3: Watch the latest CI run**

```bash
cd /home/newlevel/devel/devbridge
gh run list --branch dev --limit 3
# identify the newest run id, then:
gh run view <run-id>
```

Do not poll in a tight loop. Use `sleep 300 && gh run view <run-id>` in the background.

- [ ] **Step 4: If any job fails — investigate and fix**

```bash
gh run view <run-id> --log-failed
```

Common expected failures:
- `test`: unit tests fail because `attrs(...)` signature mismatch — fix the call sites that were missed in Task 3 Step 1.
- `test`: `extract_copies` not in scope inside `mod tests` — add `use super::extract_copies;` or rely on `use super::*;` which is already present (line 320).
- `mutation`: one or more mutants survive in `extract_copies` or the new capture line. Go to Step 5.
- `build`: ippper rev not found — the fork push failed in Task 1 Step 5 or SHA was mistyped in Task 3 Step 2. Re-push the fork, update Cargo.toml with correct SHA, commit, push.

Fix all failures in one combined commit, push once, monitor again.

- [ ] **Step 5: If mutation job flags survivors — add tests**

Download the mutation artifact:

```bash
gh run download <run-id> --name mutation-results --dir /tmp/mutation-results
cat /tmp/mutation-results/survived.txt 2>/dev/null || echo "No survivors file"
```

For each survivor in `extract_copies` or `ipp_service.rs` handler: add a unit test that catches the mutation. For example, a surviving mutant that changes `>= 1` to `> 1` would require a test that distinguishes `Some(1) → 1`. Our existing `test_extract_copies_one_returns_1` already catches that. A mutant that changes `.unwrap_or(1)` to `.unwrap_or(0)` is caught by `test_extract_copies_none_defaults_to_1`. If a survivor persists, the test for that specific behavior is missing — add it.

Commit fixes:

```bash
cargo fmt --all --check
git add crates/devbridge-server/src/ipp_service.rs
git commit -m "Kill mutation survivors for extract_copies (#37)"
git push origin dev
```

Repeat Steps 3–5 until CI is green with zero survivors.

---

## Task 7: Create PR and wait for explicit merge approval

- [ ] **Step 1: Confirm all CI jobs are green on the latest dev push**

```bash
cd /home/newlevel/devel/devbridge
gh run list --branch dev --limit 3
```

Expected: latest run status `completed`, conclusion `success`, all jobs success.

- [ ] **Step 2: Open the PR**

```bash
cd /home/newlevel/devel/devbridge
gh pr create --base main --head dev \
  --title "Capture IPP copies attribute (#37)" \
  --body "$(cat <<'EOF'
## Summary
- Fixes #37: IPP copies attribute was ignored; multi-copy print jobs always printed 1 sheet
- Extends zbynekdrlik/ippper.rs fork to expose `copies: Option<u32>` in `SimpleIppJobAttributes`
- Adds `extract_copies` helper in `ipp_service.rs` with clamp-≥1, default-1 semantics
- Replaces hardcoded `copies: 1` at `ipp_service.rs:284` with captured value
- Self-hosted E2E sends `copies=3` and asserts `/api/jobs[0].copies == 3`
- Version bump 0.8.11 → 0.8.12

## Test plan
- [ ] Unit tests for `extract_copies` (None→1, 0→1, 1→1, 7→7, u32::MAX→MAX)
- [ ] `cargo fmt --all --check` passes locally before push
- [ ] CI: Tier 1 (format, clippy, test, mutation, playwright, audit, deny) all green
- [ ] CI: Tier 1.5 Windows NSIS build green
- [ ] CI: Tier 2 self-hosted E2E asserts captured copies=3
- [ ] Post-deploy on pjpos: original reporter prints multi-copy job, confirms sheet count matches

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Confirm mergeability**

```bash
# replace <N> with the new PR number
gh api repos/zbynekdrlik/devbridge/pulls/<N> --jq '{mergeable: .mergeable, mergeable_state: .mergeable_state}'
```

Expected: `{"mergeable": true, "mergeable_state": "clean"}`.

If state is `behind`, sync:

```bash
git fetch origin
git merge origin/main
git push origin dev
```

If state is `dirty` or `blocked`, investigate and fix — do not force-merge.

- [ ] **Step 4: Report PR URL and WAIT for user merge approval**

Provide the PR URL to the user. Do NOT merge until the user explicitly says "merge it" (or equivalent). Green CI is not merge authorization.

---

## Task 8: Post-merge deployment to production instances

Only start this task after the user has explicitly approved merging AND the main-branch CI has finished publishing release artifacts.

- [ ] **Step 1: Merge the PR (after explicit user approval)**

```bash
gh pr merge <N> --merge
```

No `--squash`, no `--rebase` (project rule: merge commits only).

- [ ] **Step 2: Watch the main-branch CI and release pipeline**

```bash
gh run list --branch main --limit 3
```

Wait for all jobs to reach `success` — including the release publish job that attaches v0.8.12 artifacts to the GitHub release.

- [ ] **Step 3: Confirm v0.8.12 artifacts are published**

```bash
gh release view v0.8.12 --json assets --jq '.assets[].name'
```

Expected assets include:
- `DevBridge_0.8.12_x64-setup.exe`
- `DevBridge_0.8.12_aarch64.dmg`
- `SHA256SUMS` (or similar)

If v0.8.12 is not yet tagged, the release workflow may be tag-based and require a manual tag push. Check `.github/workflows/release.yml` and trigger accordingly.

- [ ] **Step 4: Deploy to pz-server (10.88.1.100)**

```
mcp__win-pz-server__Shell command:
$env:DEVBRIDGE_VERSION = "v0.8.12"; irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

Wait for completion. Then verify:

```
mcp__win-pz-server__Shell command:
Get-Process devbridge-service
```

Expected: process running.

- [ ] **Step 5: Deploy to pz-snv (10.78.2.10)**

```
mcp__win-pz-snv__Shell command:
$env:DEVBRIDGE_VERSION = "v0.8.12"; irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

- [ ] **Step 6: Deploy to pjpos (10.78.5.10)**

pjpos requires `DEVBRIDGE_PRINTER_TLS=true` and `DEVBRIDGE_PRINTER_DISPLAY_NAME="Epson L3260"` to preserve config:

```
mcp__win-pjpos__Shell command:
$env:DEVBRIDGE_VERSION = "v0.8.12"
$env:DEVBRIDGE_MODE = "client"
$env:DEVBRIDGE_SERVER_HOST = "10.88.1.100"
$env:DEVBRIDGE_CLIENT_ID = "pjpos-client"
$env:DEVBRIDGE_TARGET_PRINTER = "EPSON L3260"
$env:DEVBRIDGE_PRINT_BACKEND = "direct_ipp"
$env:DEVBRIDGE_PRINTER_ADDRESS = "10.78.5.9:631"
$env:DEVBRIDGE_PRINTER_TLS = "true"
$env:DEVBRIDGE_PRINTER_DISPLAY_NAME = "Epson L3260"
$env:DEVBRIDGE_GHOSTSCRIPT_DEVICE = "jpeg"
$env:DEVBRIDGE_GHOSTSCRIPT_RESOLUTION = "360"
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

- [ ] **Step 7: Deploy to pz-holla (10.88.1.105)**

pz-holla uses `windows_spooler` and the "eholla printer" class driver:

```
mcp__win-pz-holla__Shell command:
$env:DEVBRIDGE_VERSION = "v0.8.12"
$env:DEVBRIDGE_MODE = "client"
$env:DEVBRIDGE_SERVER_HOST = "10.88.1.100"
$env:DEVBRIDGE_CLIENT_ID = "holla-client"
$env:DEVBRIDGE_TARGET_PRINTER = "eholla printer"
$env:DEVBRIDGE_PRINT_BACKEND = "windows_spooler"
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

- [ ] **Step 8: Deploy BOTH instances on pz-david (10.88.1.104, macOS arm64)**

pz-david runs two DevBridge instances sharing `/Applications/DevBridge.app`. Upgrade replaces the binary; configs in `~/Library/Application Support/DevBridge-*/config.toml` are preserved.

```
mcp__mac-pz-david__Shell command:
launchctl unload ~/Library/LaunchAgents/com.devbridge.wifi.plist 2>/dev/null || true
launchctl unload ~/Library/LaunchAgents/com.devbridge.usb.plist 2>/dev/null || true
curl -L -o /tmp/DevBridge.dmg https://github.com/zbynekdrlik/devbridge/releases/download/v0.8.12/DevBridge_0.8.12_aarch64.dmg
hdiutil attach /tmp/DevBridge.dmg
rm -rf /Applications/DevBridge.app
cp -R /Volumes/DevBridge/DevBridge.app /Applications/DevBridge.app
hdiutil detach /Volumes/DevBridge
launchctl load ~/Library/LaunchAgents/com.devbridge.wifi.plist
launchctl load ~/Library/LaunchAgents/com.devbridge.usb.plist
sleep 3
ps aux | grep devbridge-service | grep -v grep
```

Expected: two `devbridge-service` processes (one per config directory).

- [ ] **Step 9: Post-deploy functional verification on pz-snv**

Print a multi-copy test job via Playwright-driven submission or an IPP helper that sets `copies=3`, then check the Canon MG3600 physical output and the client dashboard.

From the dev machine:

```bash
# craft an IPP Print-Job with copies=3 against pz-server's virtual printer paired to pz-snv
# use the same build_ipp_print_job shape as crates/devbridge-e2e/src/main.rs with E2E_COPIES=3
curl -X POST http://10.88.1.100:631/ipp/print \
  -H "Content-Type: application/ipp" \
  --data-binary @/tmp/ipp-copies3-payload.bin
```

(The payload is the IPP request built by the E2E crate; easiest path is to run the E2E binary once against the deployed server.)

Then confirm:

```bash
curl -s http://10.88.1.100:9120/api/jobs | jq '.[0] | {id, name, copies, status}'
```

Expected: `{"id": "...", "name": "...", "copies": 3, "status": "completed"}`.

And via MCP, verify the physical printer spooled 3 pages:

```
mcp__win-pz-snv__Shell command:
Get-WinEvent -LogName "Microsoft-Windows-PrintService/Operational" -MaxEvents 5 | Where-Object { $_.Id -eq 307 }
```

Expected: a recent EventID 307 with `Pages printed: 3`.

- [ ] **Step 10: Ask pjpos user to validate original bug report**

Send the user a brief note: "v0.8.12 deployed to pjpos with copies capture fix. Please print a multi-copy job from the server and confirm the correct number of sheets appears." Wait for confirmation before closing #37.

- [ ] **Step 11: Close issue #37**

Only after pjpos user confirms:

```bash
gh issue close 37 --comment "Fixed in PR <N>, deployed as v0.8.12, verified on pjpos + pz-snv (physical sheet count matches IPP copies value)."
```

---

## Verification checklist

After deployment completes:

1. **CI green** on `dev` and `main` for v0.8.12.
2. **Release published**: `gh release view v0.8.12 --json assets`.
3. **Processes running** on all 5 machines (6 processes total — pz-david has two).
4. **Captured copies** correct: `/api/jobs[0].copies` matches value sent in IPP request.
5. **Physical output** correct: printer produced N sheets for N copies (minimum pz-snv verification).
6. **User confirmation** from pjpos on original bug report.
7. **Issue #37 closed** with deployment evidence in the closing comment.
