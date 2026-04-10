# Installer Hardening — Design Spec

**Issue:** [#21](https://github.com/zbynekdrlik/devbridge/issues/21) — "Harden installer: validate printer, port, session at deploy time"

**Closes also:** [#16](https://github.com/zbynekdrlik/devbridge/issues/16) (Direct IPP default port 631), [#17](https://github.com/zbynekdrlik/devbridge/issues/17) (Windows spooler validate target_printer)

**Date:** 2026-04-10

**Goal:** Stop "installed successfully" followed by hours of debugging. Validate the most common failure modes at install time **and** at service startup so configuration mistakes fail loudly with actionable messages instead of silently dropping print jobs.

---

## Background

Issue #21 originally listed five validations. Deep research narrowed the scope:

| # | Validation | Status | Decision |
|---|-----------|--------|----------|
| 1 | Printer name match | Still relevant — `check_printer_ready` is downgraded to warn at `backend_windows_spooler.rs:166`, no install-time check | **In scope** |
| 2 | direct_ipp port `:631` | Still relevant — `backend_direct_ipp.rs:29-44` does not append a default port | **In scope** (closes #16) |
| 3 | Session isolation warn | **Already mitigated** — `post-install.ps1:217-235` tries SYSTEM, falls back to S4U on runtime failure | Out of scope |
| 4 | Install-time print test | Risky and low-value — wastes paper, slow, and EventID 307 verification (PR #24) already proves real prints work | Out of scope |
| 5 | gRPC connectivity test | Still relevant — typo or VPN-down install completes "successfully" and silently never receives jobs | **In scope** |

This spec implements **1, 2, 5** at two layers each. Items 3 and 4 are not deferred to followup issues — research showed they don't need to be done.

## Architecture

Two redundant validation layers, both must pass:

1. **Install-time validation in `installer/post-install.ps1`** — runs after the config file is written, before declaring success. Hard-fails the install with a specific actionable message and a concrete next step.
2. **Runtime validation at service startup in Rust** — catches drift when someone edits `config.toml` after install. Service refuses to start and logs the alternatives.

The two layers are intentionally redundant: install-time catches deployment-day mistakes, runtime catches in-place config edits and post-install drift. Both check the same things: printer name (for `windows_spooler`) and reachability of `printer_address` (for `direct_ipp`). The port auto-append happens silently in Rust (forgiving) plus a one-line warning in PowerShell.

## Components

### Validation 1 — Printer name (`windows_spooler` only)

**Install-time (PowerShell, `installer/post-install.ps1`):**

**Before** config is written, if `Mode -eq "client"` and `print_backend` is `windows_spooler` (or empty/default), enumerate `Get-Printer | Select-Object -ExpandProperty Name` and check `$TargetPrinter` against the list.

- Exact match → OK, continue
- No match → fuzzy-match the top 3 candidates (case-insensitive substring + Levenshtein distance), print suggestions, **exit 1**

For `print_backend = direct_ipp`, this check is skipped — the printer is on the network, not local to this machine.

**Runtime (Rust, new module `crates/devbridge-client/src/startup_validation.rs`):**

Called from the client receiver entry point (`crates/devbridge-client/src/receiver.rs`) before the gRPC reconnect loop starts.

```rust
pub fn validate_client_config(config: &ClientConfig) -> Result<()> {
    match config.print_backend.as_deref().unwrap_or("windows_spooler") {
        "windows_spooler" | "" => validate_local_printer(&config.target_printer),
        "direct_ipp" => validate_ipp_address(config.printer_address.as_deref()),
        _ => Ok(()),
    }
}
```

`validate_local_printer` calls `printer::list_printers()` and checks the target. On failure, it returns an error with the full list of available printers. The receiver propagates the error and the service exits non-zero with a clear log line.

This **replaces** the silent warn-and-continue at `backend_windows_spooler.rs:166-172`. The runtime backend check is removed; startup validation is authoritative.

### Validation 2 — direct_ipp port `:631` auto-append

**Rust (`crates/devbridge-client/src/backend_direct_ipp.rs`):**

Add a private `normalized_address()` method called from `ipp_url()` and `printer_uri()`:

```rust
fn normalized_address(&self) -> String {
    // Already has explicit port (host:port) or path (host/path) — leave alone
    if self.address.contains(':') || self.address.contains('/') {
        self.address.clone()
    } else {
        format!("{}:631", self.address)
    }
}
```

The constructor `DirectIpp::new` emits a one-time `tracing::warn!` if normalization changes the address, so the operator sees it in logs:

```
WARN address="10.78.2.9" "printer_address has no port, defaulting to :631"
```

**PowerShell (`installer/post-install.ps1`):** When `print_backend = direct_ipp`, **before** config is written, inspect `$PrinterAddress`. If no port and no path, mutate `$PrinterAddress` to `"${PrinterAddress}:631"` and emit `Write-Warning "printer_address auto-corrected to ${addr} (default IPP port)"`. The corrected value is then written into the config file. Same forgiving behavior — fix it, don't fail.

This closes #16.

### Validation 5 — gRPC connectivity (client mode)

**PowerShell (`installer/post-install.ps1`):**

Client mode only. Before config is written, parse `${ServerHost}:${GrpcPort}`, then:

```powershell
$tcp = Test-NetConnection -ComputerName $host -Port $port -InformationLevel Quiet -WarningAction SilentlyContinue
if (-not $tcp) {
    Write-Error "gRPC server unreachable at ${ServerHost}:${GrpcPort}. Verify VPN connection and that the DevBridge server is running."
    exit 1
}
```

5-second timeout. Hard fail if unreachable.

**Rust:** No new code needed. The existing receiver loop already logs reconnect failures continuously, so post-install verification is enough at startup time.

### Error Message Format

All install-time failures use this consistent format:

```
ERROR: <one-line summary>
  <2-3 line explanation of what's wrong>
  Suggestion: <concrete next step>
```

Example for printer mismatch:
```
ERROR: target_printer "Brother DCP-1610W" not found on this machine.
  Available printers:
    - Brother DCP-1610W series
    - Microsoft Print to PDF
    - eholla printer
  Suggestion: Re-run installer with $env:DEVBRIDGE_TARGET_PRINTER = "Brother DCP-1610W series"
```

Example for gRPC unreachable:
```
ERROR: gRPC server unreachable at 10.88.1.100:50051.
  TCP connection to host:port timed out after 5s.
  Suggestion: Check VPN is connected (wg show) and DevBridge service is running on the server.
```

## Data Flow

Unchanged. No API or message-format changes. Validation runs at two pure-checking points:

```
install.ps1
  → NSIS installer
  → post-install.ps1
      → parse env vars
      → [NEW] auto-append :631 to PrinterAddress if no port (direct_ipp only)
      → [NEW] validate printer name (windows_spooler only) — exit 1 on miss
      → [NEW] validate gRPC connectivity (client mode only) — exit 1 on unreachable
      → write config.toml
      → start scheduled task
      → service boot
          → [NEW] startup_validation::validate_client_config (client mode)
          → receiver loop starts
```

## Error Handling

- **Install-time printer mismatch** → exit 1 with alternatives + suggestion
- **Install-time gRPC unreachable** → exit 1 with VPN/server hint
- **Runtime printer missing** → service exits non-zero, log lists alternatives, scheduled task auto-restarts (and fails again, with same clear log) — operator sees the problem in logs immediately
- **Runtime printer_address typo** → service exits non-zero, same pattern
- **Port auto-append** → not an error, warning only

No new error types are introduced in the public API.

## Testing

### Rust unit tests

**`crates/devbridge-client/src/backend_direct_ipp.rs::tests`** (additions):

```rust
#[test]
fn test_normalized_address_appends_default_port() {
    let backend = DirectIpp::new("10.78.2.9".into(), "jpeg".into(), 360, false);
    assert_eq!(backend.normalized_address(), "10.78.2.9:631");
    assert_eq!(backend.ipp_url(), "http://10.78.2.9:631/ipp/print");
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
```

**`crates/devbridge-client/src/startup_validation.rs::tests`** (new file):

```rust
#[test]
fn test_validate_local_printer_missing_lists_alternatives() {
    let err = validate_local_printer_against(&["Canon MG3600", "Microsoft Print to PDF"], "Brother")
        .expect_err("expected error");
    let msg = err.to_string();
    assert!(msg.contains("Brother"));
    assert!(msg.contains("Canon MG3600"));
    assert!(msg.contains("Microsoft Print to PDF"));
}

#[test]
fn test_validate_local_printer_exact_match_passes() {
    validate_local_printer_against(&["Canon MG3600 series Printer"], "Canon MG3600 series Printer")
        .expect("exact match should pass");
}

#[test]
fn test_validate_local_printer_case_insensitive_match_passes() {
    validate_local_printer_against(&["Canon MG3600"], "canon mg3600")
        .expect("case-insensitive match should pass");
}

#[test]
fn test_validate_ipp_address_missing_returns_error() {
    assert!(validate_ipp_address(None).is_err());
}

#[test]
fn test_validate_ipp_address_present_passes() {
    validate_ipp_address(Some("10.78.2.9:631")).expect("valid address should pass");
}
```

The dependency-injected `validate_local_printer_against(&[&str], &str)` makes the logic unit-testable on Linux CI without calling the real `Get-Printer`. The wrapper `validate_local_printer(target)` calls `printer::list_printers()` then delegates.

### Receiver integration

**`crates/devbridge-client/src/receiver.rs::tests`** (addition):

```rust
#[tokio::test]
async fn test_client_aborts_when_printer_invalid() {
    let mut config = test_client_config();
    config.target_printer = "DefinitelyNotARealPrinter_xyz123".into();
    let result = startup_validation::validate_client_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("DefinitelyNotARealPrinter_xyz123"));
}
```

This test runs on Linux CI — `printer::list_printers()` returns the empty `lpstat` list there, so any non-empty target fails. Good enough.

### PowerShell smoke checks

Manual via Windows MCP (`mcp__win-pz-snv__Shell`), not in CI:

1. **Bad printer name:** Re-run `post-install.ps1` with `$env:DEVBRIDGE_TARGET_PRINTER = "BogusPrinter"`, expect exit code 1, expect output to contain "Available printers:" and "Suggestion:".
2. **No port:** Set `$env:DEVBRIDGE_PRINTER_ADDRESS = "10.78.2.9"`, expect Write-Warning + auto-corrected config, exit 0.
3. **Bad gRPC host:** Set `$env:DEVBRIDGE_SERVER_HOST = "10.99.99.99"`, expect exit code 1.

These are smoke checks for the PowerShell layer; the Rust unit tests are the primary regression coverage.

### Playwright E2E

No UI changes — no new Playwright tests required.

### Mutation testing

Existing `cargo mutants --workspace` job in CI covers the new Rust functions automatically. Any surviving mutants must be killed before merge.

## Post-Deploy Verification

After CI deploys v0.8.9 to pz-server and pz-snv, both existing configs already have the correct printer name and port — they continue working with no regression. Then verify each new failure path:

1. **Printer name failure path (runtime):** Via `mcp__win-pz-snv__Shell`, edit `C:\ProgramData\DevBridge\config.toml` to set `target_printer = "Canon MG3600"` (missing the trailing word). Restart the scheduled task. Confirm the service exits non-zero, logs the available printer list. Restore the original config and restart.

2. **Port auto-append (runtime):** Via `mcp__win-pz-snv__Shell`, edit the config to `printer_address = "10.78.2.9"` (no port). Restart the scheduled task. Confirm: warning in logs, service starts, a print job still completes successfully (EventID 307 confirmed). Restore.

3. **gRPC unreachable (install-time):** On a throwaway test machine (or temporarily on pz-snv), re-run `post-install.ps1` with `$env:DEVBRIDGE_SERVER_HOST = "10.99.99.99"`. Confirm exit code 1 and the gRPC error message. Restore the real config and re-run with the correct host.

If any step fails, the work is not done.

## Followups

None. Items 3 and 4 from the original issue are explicitly **not** filed as followup issues — research showed they're either already mitigated or low-value. This PR closes #21, #16, and #17 fully.
