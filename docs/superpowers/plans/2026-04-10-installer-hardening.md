# Installer Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Validate `target_printer`, `printer_address`, and gRPC connectivity at install time AND service startup so config mistakes fail loudly with actionable messages instead of silently dropping print jobs. Closes #21, #16, #17.

**Architecture:** Two redundant validation layers. (1) `installer/post-install.ps1` runs before writing config — auto-appends `:631` for direct_ipp, fuzzy-checks the local printer name, TCP-tests the gRPC server, hard-fails with actionable messages. (2) A new `crates/devbridge-client/src/startup_validation.rs` module is called from `crates/devbridge-service/src/runtime.rs::run_client` before the receiver starts, so a hand-edited `config.toml` cannot bypass the install-time check.

**Tech Stack:** Rust 2024 (devbridge-client, devbridge-service), PowerShell (post-install.ps1), Tauri (version file)

**Spec:** `docs/superpowers/specs/2026-04-10-installer-hardening-design.md`

---

## File Structure

### New Files
| File | Purpose |
|------|---------|
| `crates/devbridge-client/src/startup_validation.rs` | Pure validation functions (DI-friendly) for printer name and direct_ipp address. Called once at service startup. |

### Modified Files
| File | Changes |
|------|---------|
| `Cargo.toml` | Bump workspace version 0.8.8 → 0.8.9 |
| `crates/devbridge-app/tauri.conf.json` | Bump version 0.8.8 → 0.8.9 |
| `crates/devbridge-client/src/lib.rs` | Add `pub mod startup_validation;` |
| `crates/devbridge-client/src/backend_direct_ipp.rs` | Add `normalized_address()` private method, call from `ipp_url`/`printer_uri`, warn at construction when port was missing |
| `crates/devbridge-client/src/backend_windows_spooler.rs:166-172` | Remove silent warn-and-continue on `check_printer_ready` failure (startup validation is now authoritative) |
| `crates/devbridge-service/src/runtime.rs::run_client` | Call `devbridge_client::startup_validation::validate_client_config(&config.client)?` near the top |
| `installer/post-install.ps1` | Insert validation block: port normalization, printer fuzzy-check, gRPC TCP probe — all before config is written |

---

## Task 1: Version Bump

This must be the first commit. Without it, the version-check CI job fails after 15 minutes.

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/devbridge-app/tauri.conf.json`

- [ ] **Step 1: Bump workspace version**

In `Cargo.toml`, find `[workspace.package]` and change:
```toml
version = "0.8.8"
```
to:
```toml
version = "0.8.9"
```

- [ ] **Step 2: Bump Tauri version**

In `crates/devbridge-app/tauri.conf.json`, change:
```json
"version": "0.8.8",
```
to:
```json
"version": "0.8.9",
```

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml crates/devbridge-app/tauri.conf.json
git commit -m "$(cat <<'EOF'
Bump version to 0.8.9

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: direct_ipp port auto-append

Closes #16. Pure logic change inside `DirectIpp` — `normalized_address()` is the single source of truth used by `ipp_url()` and `printer_uri()`. Constructor warns once when normalization mutates the address.

**Files:**
- Modify: `crates/devbridge-client/src/backend_direct_ipp.rs`

- [ ] **Step 1: Add the failing tests**

In `crates/devbridge-client/src/backend_direct_ipp.rs`, inside the existing `#[cfg(test)] mod tests { … }` block, add these tests **after** the existing `test_tls_url_uses_https`:

```rust
    #[test]
    fn test_normalized_address_appends_default_port() {
        let backend = DirectIpp::new("10.78.2.9".into(), "jpeg".into(), 360, false);
        assert_eq!(backend.normalized_address(), "10.78.2.9:631");
        assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
        assert_eq!(backend.printer_uri(), "ipp://10.78.2.9:631/ipp/print");
    }

    #[test]
    fn test_normalized_address_keeps_explicit_port() {
        let backend = DirectIpp::new("10.78.2.9:9100".into(), "jpeg".into(), 360, false);
        assert_eq!(backend.normalized_address(), "10.78.2.9:9100");
    }

    #[test]
    fn test_normalized_address_keeps_path() {
        let backend = DirectIpp::new("10.78.2.9/printers/foo".into(), "jpeg".into(), 360, false);
        assert_eq!(backend.normalized_address(), "10.78.2.9/printers/foo");
    }

    #[test]
    fn test_normalized_address_with_tls_uses_https() {
        let backend = DirectIpp::new("10.78.5.9".into(), "jpeg".into(), 360, true);
        assert_eq!(backend.ipp_url(), "https://10.78.5.9:631/ipp/print");
    }
```

- [ ] **Step 2: Run the tests and verify they fail to compile**

```bash
cargo test -p devbridge-client backend_direct_ipp::tests::test_normalized_address 2>&1 | tail -20
```

Expected: compile error `no method named normalized_address found for struct DirectIpp`.

- [ ] **Step 3: Add `normalized_address()` and rewire `ipp_url`/`printer_uri`**

Replace the existing `ipp_url` and `printer_uri` methods (`backend_direct_ipp.rs:29-45`) with:

```rust
    /// Returns `address` unchanged if it already contains a port (`host:port`)
    /// or a path (`host/path`); otherwise appends `:631` (the default IPP port).
    fn normalized_address(&self) -> String {
        if self.address.contains(':') || self.address.contains('/') {
            self.address.clone()
        } else {
            format!("{}:631", self.address)
        }
    }

    fn ipp_url(&self) -> String {
        let scheme = if self.use_tls { "https" } else { "http" };
        let addr = self.normalized_address();
        if addr.contains('/') {
            format!("{}://{}", scheme, addr)
        } else {
            format!("{}://{}/ipp/print", scheme, addr)
        }
    }

    fn printer_uri(&self) -> String {
        let scheme = if self.use_tls { "ipps" } else { "ipp" };
        let addr = self.normalized_address();
        if addr.contains('/') {
            format!("{}://{}", scheme, addr)
        } else {
            format!("{}://{}/ipp/print", scheme, addr)
        }
    }
```

- [ ] **Step 4: Warn at construction when port was missing**

Replace the existing `pub fn new(...)` (`backend_direct_ipp.rs:20-27`) with:

```rust
    pub fn new(address: String, gs_device: String, gs_resolution: u32, use_tls: bool) -> Self {
        if !address.contains(':') && !address.contains('/') {
            warn!(
                address = %address,
                "printer_address has no port, defaulting to :631 (IPP default)"
            );
        }
        Self {
            address,
            gs_device,
            gs_resolution,
            use_tls,
        }
    }
```

`warn` is already imported via `use tracing::{debug, info, warn};` at the top of the file — no new import needed.

- [ ] **Step 5: Run the tests and verify they pass**

```bash
cargo test -p devbridge-client backend_direct_ipp 2>&1 | tail -30
```

Expected: all tests in `backend_direct_ipp::tests` pass, including the four new ones.

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-client/src/backend_direct_ipp.rs
git commit -m "$(cat <<'EOF'
Auto-append :631 to direct_ipp printer_address (#16)

DirectIpp now normalizes printer_address through a single
normalized_address() method used by both ipp_url() and printer_uri().
Constructor logs a warning when normalization fires so operators
notice the missing port.

Closes #16

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: startup_validation module

Pure check functions, dependency-injected so they unit-test on Linux CI without touching `Get-Printer` or `lpstat`.

**Files:**
- Create: `crates/devbridge-client/src/startup_validation.rs`
- Modify: `crates/devbridge-client/src/lib.rs`

- [ ] **Step 1: Create the new file with failing tests first**

Create `crates/devbridge-client/src/startup_validation.rs` with **only** the test module (so the tests fail to compile because the functions don't exist yet):

```rust
//! Startup validation for client-mode configuration.
//!
//! Called once from `devbridge-service::runtime::run_client` before the
//! receiver starts. Refuses to start the service when target_printer or
//! printer_address would silently drop jobs at runtime.

use anyhow::{Result, bail};

use devbridge_core::config::ClientConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_local_printer_exact_match_passes() {
        validate_local_printer_against(
            &["Canon MG3600 series Printer".to_string()],
            "Canon MG3600 series Printer",
        )
        .expect("exact match should pass");
    }

    #[test]
    fn test_validate_local_printer_case_insensitive_match_passes() {
        validate_local_printer_against(&["Canon MG3600".to_string()], "canon mg3600")
            .expect("case-insensitive match should pass");
    }

    #[test]
    fn test_validate_local_printer_missing_lists_alternatives() {
        let err = validate_local_printer_against(
            &[
                "Canon MG3600 series Printer".to_string(),
                "Microsoft Print to PDF".to_string(),
                "eholla printer".to_string(),
            ],
            "Brother DCP-1610W",
        )
        .expect_err("missing printer should fail");
        let msg = err.to_string();
        assert!(msg.contains("Brother DCP-1610W"), "msg: {}", msg);
        assert!(msg.contains("Canon MG3600 series Printer"), "msg: {}", msg);
        assert!(msg.contains("Microsoft Print to PDF"), "msg: {}", msg);
        assert!(msg.contains("eholla printer"), "msg: {}", msg);
    }

    #[test]
    fn test_validate_local_printer_empty_list_fails_with_hint() {
        let err = validate_local_printer_against(&[], "Anything")
            .expect_err("empty list should fail");
        assert!(
            err.to_string().contains("No printers installed"),
            "msg: {}",
            err
        );
    }

    #[test]
    fn test_validate_ipp_address_missing_returns_error() {
        let err = validate_ipp_address(None).expect_err("None should fail");
        assert!(err.to_string().contains("printer_address"));
    }

    #[test]
    fn test_validate_ipp_address_present_passes() {
        validate_ipp_address(Some("10.78.2.9:631")).expect("valid address should pass");
    }

    #[test]
    fn test_validate_ipp_address_empty_string_fails() {
        let err = validate_ipp_address(Some("")).expect_err("empty string should fail");
        assert!(err.to_string().contains("printer_address"));
    }

    fn make_config(backend: &str, target: &str, addr: Option<&str>) -> ClientConfig {
        ClientConfig {
            server_address: "127.0.0.1:50051".into(),
            target_printer: target.into(),
            dashboard_port: 9120,
            reconnect_interval_secs: 5,
            max_reconnect_interval_secs: 60,
            client_id: None,
            print_backend: backend.into(),
            printer_address: addr.map(String::from),
            ghostscript_device: "jpeg".into(),
            ghostscript_resolution: 360,
            printer_tls: false,
            printer_display_name: None,
            print_proxy_url: None,
            virtual_printer_name: None,
            tls: Default::default(),
        }
    }

    #[test]
    fn test_validate_client_config_direct_ipp_with_address_passes() {
        let cfg = make_config("direct_ipp", "ignored", Some("10.78.2.9:631"));
        validate_client_config_against(&cfg, &[]).expect("direct_ipp with address should pass");
    }

    #[test]
    fn test_validate_client_config_direct_ipp_without_address_fails() {
        let cfg = make_config("direct_ipp", "ignored", None);
        validate_client_config_against(&cfg, &[])
            .expect_err("direct_ipp without printer_address should fail");
    }

    #[test]
    fn test_validate_client_config_windows_spooler_uses_printer_list() {
        let cfg = make_config("windows_spooler", "Canon MG3600", None);
        validate_client_config_against(&cfg, &["Canon MG3600".to_string()])
            .expect("matching printer should pass");

        let cfg_bad = make_config("windows_spooler", "NonExistent", None);
        validate_client_config_against(&cfg_bad, &["Canon MG3600".to_string()])
            .expect_err("non-matching printer should fail");
    }

    #[test]
    fn test_validate_client_config_unknown_backend_passes() {
        // Unknown backends (cups, print_proxy, etc.) are not validated here.
        let cfg = make_config("print_proxy", "ignored", None);
        validate_client_config_against(&cfg, &[]).expect("unknown backend should be skipped");
    }
}
```

- [ ] **Step 2: Register the module**

Modify `crates/devbridge-client/src/lib.rs`. After the existing `pub mod printer;` line, add:

```rust
pub mod startup_validation;
```

- [ ] **Step 3: Run the tests and verify they fail to compile**

```bash
cargo test -p devbridge-client startup_validation 2>&1 | tail -30
```

Expected: compile errors `cannot find function validate_local_printer_against in this scope` (and the others).

- [ ] **Step 4: Implement the validation functions**

In `crates/devbridge-client/src/startup_validation.rs`, **above** the `#[cfg(test)] mod tests` block, add:

```rust
/// Entry point: validate a `ClientConfig` against the live system.
///
/// Calls `crate::printer::list_printers()` for the windows_spooler branch.
/// On Linux CI this returns an empty list, so non-empty target names fail
/// fast — which is the desired behavior for tests.
pub fn validate_client_config(config: &ClientConfig) -> Result<()> {
    let printers = crate::printer::list_printers()
        .map(|list| list.into_iter().map(|p| p.name).collect::<Vec<_>>())
        .unwrap_or_default();
    validate_client_config_against(config, &printers)
}

/// DI variant for unit tests — pass the printer list explicitly instead of
/// shelling out to `Get-Printer` / `lpstat`.
fn validate_client_config_against(config: &ClientConfig, printers: &[String]) -> Result<()> {
    match config.print_backend.as_str() {
        "windows_spooler" | "" => validate_local_printer_against(printers, &config.target_printer),
        "direct_ipp" => validate_ipp_address(config.printer_address.as_deref()),
        // Other backends (cups, direct_raw, print_proxy) are not validated here.
        _ => Ok(()),
    }
}

/// Validate `target` is exactly one of the entries in `available` (case-insensitive).
fn validate_local_printer_against(available: &[String], target: &str) -> Result<()> {
    if available.is_empty() {
        bail!(
            "No printers installed on this machine. \
             Install the printer driver before configuring DevBridge \
             (target_printer = \"{}\")",
            target
        );
    }
    let target_lower = target.to_lowercase();
    if available.iter().any(|p| p.to_lowercase() == target_lower) {
        return Ok(());
    }
    let alternatives = available
        .iter()
        .map(|p| format!("    - {}", p))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "target_printer \"{}\" not found on this machine.\n  \
         Available printers:\n{}\n  \
         Suggestion: edit C:\\ProgramData\\DevBridge\\config.toml \
         and set target_printer to one of the names above, then restart \
         the DevBridge scheduled task.",
        target,
        alternatives
    );
}

/// Validate that direct_ipp has a `printer_address` set.
fn validate_ipp_address(address: Option<&str>) -> Result<()> {
    match address {
        Some(s) if !s.is_empty() => Ok(()),
        _ => bail!(
            "direct_ipp backend requires printer_address in config. \
             Suggestion: set [client] printer_address = \"<host>:631\" \
             in C:\\ProgramData\\DevBridge\\config.toml."
        ),
    }
}
```

- [ ] **Step 5: Run the tests and verify they pass**

```bash
cargo test -p devbridge-client startup_validation 2>&1 | tail -30
```

Expected: all 11 tests in `startup_validation::tests` pass.

- [ ] **Step 6: Commit**

```bash
git add crates/devbridge-client/src/startup_validation.rs crates/devbridge-client/src/lib.rs
git commit -m "$(cat <<'EOF'
Add startup_validation module for client config (#21, #17)

Refuses to start the service when target_printer is missing
(windows_spooler) or printer_address is unset (direct_ipp).
Errors include the list of available printers and a suggested fix.

DI variant validate_client_config_against takes the printer list
explicitly so unit tests run on Linux CI without Get-Printer.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire validation into the service runtime

Hook the new module into `run_client` so the service refuses to start with a bad config. Tests at this layer cover the integration glue.

**Files:**
- Modify: `crates/devbridge-service/src/runtime.rs`

- [ ] **Step 1: Add validation call**

In `crates/devbridge-service/src/runtime.rs`, find `async fn run_client` (around line 165). Immediately **after** the line that creates `spool_dir` and **before** the `tokio::fs::create_dir_all(&spool_dir).await?;` line, insert:

```rust
    // Refuse to start when target_printer / printer_address is invalid.
    // See crates/devbridge-client/src/startup_validation.rs and
    // docs/superpowers/specs/2026-04-10-installer-hardening-design.md
    devbridge_client::startup_validation::validate_client_config(&config.client)
        .context("client config validation failed")?;
```

`Context` is already in scope via `use anyhow::{Context, Result};` at the top of the file — verify with:

```bash
head -20 crates/devbridge-service/src/runtime.rs
```

If `Context` is **not** imported, add it to the existing `use anyhow::...` line.

- [ ] **Step 2: Build to verify it compiles**

```bash
cargo build -p devbridge-service 2>&1 | tail -20
```

Expected: clean build, no warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/devbridge-service/src/runtime.rs
git commit -m "$(cat <<'EOF'
Refuse to start client when config is invalid (#21)

run_client now calls startup_validation::validate_client_config
before initializing the spool dir or receiver. A typo in
target_printer or a missing printer_address fails fast with the
error message from startup_validation.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Remove silent warn-and-continue in windows_spooler backend

Startup validation is now authoritative; the runtime soft-warn at print time is dead code that masked the bug.

**Files:**
- Modify: `crates/devbridge-client/src/backend_windows_spooler.rs:166-172`

- [ ] **Step 1: Delete the soft warn**

In `crates/devbridge-client/src/backend_windows_spooler.rs`, find the `print` method (around line 156) and remove these lines (currently 166-172):

```rust
        if let Err(e) = crate::printer::check_printer_ready(printer) {
            warn!(
                printer,
                error = %e,
                "printer readiness check failed, attempting print anyway"
            );
        }
```

The result should look like:

```rust
        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("Windows spooler → {}", display),
        );

        crate::printer::print_pdf(printer, pdf_path)?;
```

(The `warn` import on line 5 — `use tracing::warn;` — becomes unused and must be removed too.)

- [ ] **Step 2: Remove the now-unused `warn` import**

On line 5 of `backend_windows_spooler.rs`:

```rust
use tracing::warn;
```

Delete this entire line.

- [ ] **Step 3: Build to verify clippy is happy**

```bash
cargo build -p devbridge-client 2>&1 | tail -20
```

Expected: clean build, no `unused_imports` warning.

- [ ] **Step 4: Run existing tests for the file**

```bash
cargo test -p devbridge-client backend_windows_spooler 2>&1 | tail -10
```

Expected: existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add crates/devbridge-client/src/backend_windows_spooler.rs
git commit -m "$(cat <<'EOF'
Remove soft warn-and-continue when printer is missing (#17)

Startup validation now refuses to start the service when
target_printer doesn't match an installed printer, so the
runtime check_printer_ready fallthrough was dead code that
masked the original failure mode.

Closes #17

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Harden post-install.ps1

Three install-time checks: port auto-append, printer fuzzy-check, gRPC TCP probe. All happen **before** the config file is written so the values that land in `config.toml` are already correct.

**Files:**
- Modify: `installer/post-install.ps1`

- [ ] **Step 1: Insert validation block before the config write**

In `installer/post-install.ps1`, find the line that begins the config write section (currently around line 125):

```powershell
# ── Write configuration ────────────────────────────────────────────────────
$configPath = Join-Path $DataDir "config.toml"
```

**Immediately before** that comment, insert this block:

```powershell
# ── Validate configuration before writing config.toml ─────────────────────
# These checks fail loudly with actionable messages instead of letting the
# service start with a config that will silently drop print jobs.
# See docs/superpowers/specs/2026-04-10-installer-hardening-design.md

if ($Mode -eq "client") {
    $effectiveBackend = if ($PrintBackend) { $PrintBackend } else { "windows_spooler" }

    # 1. direct_ipp port auto-append (closes #16)
    if ($effectiveBackend -eq "direct_ipp" -and $PrinterAddress -and
        ($PrinterAddress -notmatch ':') -and ($PrinterAddress -notmatch '/')) {
        $corrected = "${PrinterAddress}:631"
        Write-Warning "printer_address auto-corrected to $corrected (default IPP port)"
        $PrinterAddress = $corrected
    }

    # 2. windows_spooler printer name validation (closes #17)
    if ($effectiveBackend -eq "windows_spooler" -or $effectiveBackend -eq "") {
        $installedPrinters = @(Get-Printer -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name)
        if ($installedPrinters.Count -eq 0) {
            Write-Host ""
            Write-Host "ERROR: No printers installed on this machine." -ForegroundColor Red
            Write-Host "  Install the printer driver before configuring DevBridge." -ForegroundColor Red
            Write-Host "  Suggestion: open Settings → Bluetooth & devices → Printers & scanners," -ForegroundColor Yellow
            Write-Host "              add the printer, then re-run the installer." -ForegroundColor Yellow
            exit 1
        }
        $exactMatch = $installedPrinters | Where-Object { $_ -ieq $TargetPrinter }
        if (-not $exactMatch) {
            Write-Host ""
            Write-Host "ERROR: target_printer `"$TargetPrinter`" not found on this machine." -ForegroundColor Red
            Write-Host "  Available printers:" -ForegroundColor Red
            foreach ($p in $installedPrinters) {
                Write-Host "    - $p" -ForegroundColor Red
            }
            # Closest substring match for the suggestion line
            $suggestion = $installedPrinters | Where-Object { $_ -like "*$TargetPrinter*" -or $TargetPrinter -like "*$_*" } | Select-Object -First 1
            if (-not $suggestion) { $suggestion = $installedPrinters[0] }
            Write-Host "  Suggestion: re-run installer with " -NoNewline -ForegroundColor Yellow
            Write-Host "`$env:DEVBRIDGE_TARGET_PRINTER = `"$suggestion`"" -ForegroundColor Yellow
            exit 1
        }
        Write-Host "  Validated target_printer: $TargetPrinter" -ForegroundColor Green
    }

    # 3. gRPC connectivity test (closes #21 main scope)
    Write-Host "  Probing gRPC server at ${ServerHost}:${GrpcPort}..."
    $tcp = Test-NetConnection -ComputerName $ServerHost -Port $GrpcPort `
        -InformationLevel Quiet -WarningAction SilentlyContinue
    if (-not $tcp) {
        Write-Host ""
        Write-Host "ERROR: gRPC server unreachable at ${ServerHost}:${GrpcPort}." -ForegroundColor Red
        Write-Host "  TCP connection timed out." -ForegroundColor Red
        Write-Host "  Suggestion: verify VPN is connected (e.g. wg show), and that the" -ForegroundColor Yellow
        Write-Host "              DevBridge service is running on the server." -ForegroundColor Yellow
        exit 1
    }
    Write-Host "  gRPC server reachable" -ForegroundColor Green
}
```

- [ ] **Step 2: Verify nothing else in post-install.ps1 needs to change**

Run a quick grep to confirm `$PrinterAddress` is only consumed by the config-write block downstream (not by anything earlier):

```bash
grep -n PrinterAddress installer/post-install.ps1
```

Expected: occurrences are only in the param block, the new validation block, and the config-write block. The validation block runs before the config write, so its mutation of `$PrinterAddress` correctly flows into the written file.

- [ ] **Step 3: Commit**

```bash
git add installer/post-install.ps1
git commit -m "$(cat <<'EOF'
Validate printer, port, and gRPC at install time (#21, #16, #17)

post-install.ps1 now refuses to write config.toml when
target_printer doesn't match an installed printer or the
gRPC server is unreachable. Auto-appends :631 to direct_ipp
printer_address when no port is specified.

All errors include the list of available printers / a concrete
next step the operator can copy-paste.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Local checks and push

**Files:** none

- [ ] **Step 1: Format check**

```bash
cargo fmt --all --check
```

Expected: clean exit. If anything needs formatting, run `cargo fmt --all` and amend the relevant commit (not via `git commit --amend` — make a new fix-up commit).

- [ ] **Step 2: Push**

```bash
git fetch origin
git push origin dev
```

- [ ] **Step 3: Monitor CI**

```bash
gh run list --branch dev --limit 3
```

Then watch the latest run:

```bash
sleep 300 && gh run view <run-id>
```

Wait until **all** jobs reach a terminal state. If any job fails, run `gh run view <run-id> --log-failed`, fix the root cause, and push a single follow-up commit.

Pay special attention to the **mutation** job — any surviving mutants in the new code (`startup_validation.rs`, `normalized_address`, the warn-removal in `backend_windows_spooler.rs`) must be killed by adding more assertions to existing tests, not by excluding code.

---

## Task 8: Post-Deploy Verification on pz-snv

After CI deploys v0.8.9 to pz-snv, exercise each new failure path on the live machine.

**Files:** none — verification only.

- [ ] **Step 1: Confirm v0.8.9 is live on pz-snv**

```
mcp__win-pz-snv__Shell command:
curl -s http://10.78.2.10:9120/api/status
```

Expected JSON contains `"version": "0.8.9"`.

- [ ] **Step 2: Verify Validation 1 (printer-name mismatch refuses to start)**

Read the current config:
```
mcp__win-pz-snv__FileRead path: C:\ProgramData\DevBridge\config.toml
```

Save the original `target_printer` value. Then write a corrupted copy:
```
mcp__win-pz-snv__FileWrite path: C:\ProgramData\DevBridge\config.toml
content: <same config but target_printer = "Canon MG3600">  # missing trailing words
```

Restart the scheduled task:
```
mcp__win-pz-snv__Shell command:
Stop-ScheduledTask -TaskName DevBridgeService; Start-Sleep 2; Start-ScheduledTask -TaskName DevBridgeService; Start-Sleep 5
Get-Process devbridge-service -ErrorAction SilentlyContinue
```

Expected: `Get-Process` returns nothing (service refused to start).

Read the latest log line that shows the validation error:
```
mcp__win-pz-snv__Shell command:
Get-Content C:\ProgramData\DevBridge\logs\*.log | Select-String 'target_printer' | Select-Object -Last 5
```

Expected: log line containing "Canon MG3600" not found, "Available printers", and the original printer name in the alternatives list.

**Restore the config:**
```
mcp__win-pz-snv__FileWrite path: C:\ProgramData\DevBridge\config.toml content: <original>
mcp__win-pz-snv__Shell command:
Stop-ScheduledTask -TaskName DevBridgeService; Start-Sleep 2; Start-ScheduledTask -TaskName DevBridgeService; Start-Sleep 5
Get-Process devbridge-service
```

Expected: service is back up.

- [ ] **Step 3: Verify Validation 2 (port auto-append warning, service still works)**

Edit the config to remove `:631` from `printer_address`:
```
mcp__win-pz-snv__FileWrite path: C:\ProgramData\DevBridge\config.toml
content: <same config but printer_address = "10.78.2.9">
```

Restart the task and check logs:
```
mcp__win-pz-snv__Shell command:
Stop-ScheduledTask -TaskName DevBridgeService; Start-Sleep 2; Start-ScheduledTask -TaskName DevBridgeService; Start-Sleep 8
Get-Content C:\ProgramData\DevBridge\logs\*.log | Select-String 'defaulting to :631' | Select-Object -Last 3
```

Expected: log line `printer_address has no port, defaulting to :631 (IPP default)`.

Submit a test print from the dashboard:
```
mcp__win-pz-snv__Shell command:
Invoke-WebRequest -UseBasicParsing -Uri http://10.78.2.10:9120/api/print-test -Method Post
```

(Or use whatever existing API endpoint triggers a test print on this machine. If none exists, send via `mcp__win-pz-server__Shell` to the server's IPP printer. The point is to drive a real job through pz-snv.)

Wait 30 seconds and read the printer events:
```
mcp__win-pz-snv__Shell command:
Get-WinEvent -LogName 'Microsoft-Windows-PrintService/Operational' -MaxEvents 10 | Where-Object { $_.Id -eq 307 -and $_.TimeCreated -gt (Get-Date).AddMinutes(-2) }
```

Expected: at least one EventID 307 (physical delivery confirmed).

**Restore the config** with the original `printer_address` including `:631` and restart the task again.

- [ ] **Step 4: Verify Validation 5 (gRPC unreachable refuses install)**

This step uses post-install.ps1 directly — do **not** rerun the full installer.

```
mcp__win-pz-snv__Shell command:
$env:DEVBRIDGE_SERVER_HOST = "10.99.99.99"
$env:DEVBRIDGE_MODE = "client"
$env:DEVBRIDGE_TARGET_PRINTER = "Canon MG3600 series Printer"
$env:DEVBRIDGE_CLIENT_ID = "pjsnvs"
$env:DEVBRIDGE_PRINT_BACKEND = "direct_ipp"
$env:DEVBRIDGE_PRINTER_ADDRESS = "10.78.2.9:631"
$env:DEVBRIDGE_GHOSTSCRIPT_DEVICE = "jpeg"
& 'C:\Program Files\DevBridge\post-install.ps1' -Mode client -ServerHost 10.99.99.99 -TargetPrinter "Canon MG3600 series Printer" -ClientId pjsnvs -PrintBackend direct_ipp -PrinterAddress "10.78.2.9:631" -GhostscriptDevice jpeg
echo "Exit: $LASTEXITCODE"
```

Expected: `gRPC server unreachable at 10.99.99.99:50051` error and `Exit: 1`.

**The original config.toml on disk is unchanged** because the script exited before the config-write step. Confirm:
```
mcp__win-pz-snv__FileRead path: C:\ProgramData\DevBridge\config.toml
```

Expected: still has `server_address = "10.88.1.100:50051"`.

Restart the scheduled task to pick up any state, then confirm the service is healthy:
```
mcp__win-pz-snv__Shell command:
curl -s http://10.78.2.10:9120/api/status
```

Expected: still `"version": "0.8.9"`, status `running`.

If any verification step fails, the work is **not** done — go back, diagnose, and fix.

---

## Task 9: Open the PR and close the duplicate issues

- [ ] **Step 1: Verify branch state**

```bash
git fetch origin
git status
git log --oneline origin/main..dev
```

Expected: clean working tree, dev is several commits ahead of main.

- [ ] **Step 2: Create PR**

```bash
gh pr create --title "Harden installer: validate printer, port, gRPC at install + startup (#21)" --body "$(cat <<'EOF'
## Summary
- Validates target_printer (windows_spooler) and printer_address (direct_ipp) at install time AND service startup
- Auto-appends :631 to direct_ipp printer_address when no port is set
- TCP-tests gRPC connectivity from client mode installer before declaring success
- Removes the silent warn-and-continue path in backend_windows_spooler — startup validation is authoritative
- All failure messages include the list of available printers / a concrete next-step suggestion

Closes #21
Closes #16
Closes #17

## Test plan
- [ ] cargo test -p devbridge-client startup_validation passes (11 new unit tests)
- [ ] cargo test -p devbridge-client backend_direct_ipp passes (4 new tests for normalized_address)
- [ ] cargo mutants survives — no mutants left alive in the new code
- [ ] CI green on dev (all Tier 1 + Tier 1.5 + Tier 2 jobs)
- [ ] post-deploy: pz-snv refuses to start with bad target_printer (verified via MCP)
- [ ] post-deploy: pz-snv warns + auto-appends :631 and prints successfully (EventID 307 confirmed)
- [ ] post-deploy: post-install.ps1 exits 1 with gRPC error when server is unreachable (config unchanged)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for PR CI green**

```bash
gh pr view --json url,statusCheckRollup --jq '.statusCheckRollup'
```

Wait until every check is `SUCCESS`. If any check fails, fix the root cause and push a single follow-up commit.

- [ ] **Step 4: Verify mergeable + report PR URL**

```bash
gh pr view --json number,url,mergeable,mergeStateStatus
```

Expected: `mergeable: MERGEABLE`, `mergeStateStatus: CLEAN`.

Report the green PR URL to the user and wait for explicit `merge it` approval before merging.

---

## Verification

After the PR is merged and main CI runs the deploy stage:

1. **Mutation testing:** survived.txt empty for the new files
2. **Unit tests:** 11 new `startup_validation` tests + 4 new `normalized_address` tests pass on Linux CI
3. **Post-deploy on pz-snv:** all three failure-path checks from Task 8 pass
4. **No regression:** all existing CI jobs (format, lint, test, build, audit, deny, tdd-enforce, mutation, playwright, windows-build, e2e-deploy, e2e-test) still pass
5. **Issues closed:** #21, #16, #17 all show `Closed` on GitHub
