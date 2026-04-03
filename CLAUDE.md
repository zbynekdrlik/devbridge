<!-- Global rules inherited from ~/.claude/CLAUDE.md (managed by airuleset) -->
<!-- PR merge policy, CI monitoring, TDD, autonomous verification, git workflow, test strictness, deploy patterns -->

# DevBridge - Project Conventions

## Overview

DevBridge is a print bridge for retail stores. A server receives print jobs via
an IPP virtual printer and forwards them over gRPC (with mTLS) to remote client
machines that print to local hardware printers.

## Project-Specific Test Requirements

- **API schema tests must match the consumer.** If a frontend expects `{name, driver, status}` objects, the API test must assert that exact shape — not just that the endpoint returns 200 or a raw value.
- **E2E tests required for every new feature.** Every new feature, API endpoint, or UI feature MUST have corresponding E2E tests in `devbridge-e2e/src/main.rs` that run against the deployed server/client. A PR is NOT mergeable if new functionality lacks E2E test coverage. UI features must be verified via API calls against deployed dashboard URLs.
- **Every implementation plan must include:** (1) a testing section specifying unit tests, integration tests, and E2E tests to add or update, and (2) a post-deploy verification section describing how to confirm the change works on the actual server/client machines after CI deploys it.

## Windows MCP Tools — USE INSTEAD OF SSH

You have MCP servers configured for production Windows machines. **Always use these MCP tools for ALL Windows operations:**

- `mcp__win-pz-server__Shell` — pz-server (10.88.1.100) — DevBridge server
- `mcp__win-pz-snv__Shell` — pz-snv (10.78.2.10) — DevBridge client (Canon MG3600)
- `mcp__win-pz-holla__Shell` — pz-holla (10.88.1.105) — DevBridge client (Brother DCP-1610W)

Each also has `Snapshot`, `FileRead`, `FileWrite` variants.

**NEVER use SSH when MCP tools are available.**

## Post-Deploy Verification (Project-Specific Targets)

After CI deploys, verify both machines respond correctly before reporting success:

- **Server dashboard:** http://10.88.1.100:9120
- **Client dashboard:** http://10.78.2.10:9120

Use `mcp__win-pz-server__Shell` and `mcp__win-pz-snv__Shell` to verify services are running.

## CI/CD Pipeline

The CI workflow (`.github/workflows/ci.yml`) is the quality gate. It runs on every
push to `dev`, every PR to `main`, and every merge to `main`. **All jobs must pass for a PR to be mergeable.** After merge, the full pipeline re-runs on `main` to deploy and verify the production version on both server and client machines.

### Tier 1 (ubuntu-latest) — Code Quality

1. **Format** - `cargo fmt --all -- --check` (zero tolerance)
2. **Lint** - `cargo clippy --workspace --all-targets -- -D warnings` (deny all warnings)
3. **Test** - `cargo test --workspace` (unit + integration tests must pass)
4. **Build** - `cargo build --workspace --release` (must compile cleanly)
5. **Audit** - `cargo deny check` (license + vulnerability audit)
6. **TDD Enforce** - grep for `#[ignore]`, empty tests, `todo!()`

### Tier 1.5 (windows-latest free runner) — Windows Build + NSIS Installer

7. **Windows Build** - build service binary, WASM UI, and Tauri NSIS installer on free `windows-latest` runner. The NSIS installer bundles the service as a sidecar (`externalBin`) and installs to `C:\Program Files\DevBridge\`.

### Tier 2 (self-hosted Windows) — Real Hardware E2E (no compilation)

8. **E2E Deploy** - run NSIS installer silently on both machines, then `installer/post-install.ps1` configures service registration, config, certs, and tray app auto-start
9. **E2E Test** - run pre-built E2E binary: installation verification → service health → IPP → gRPC → physical printer (8 tests)

After CI passes, services **stay running** on both machines (no cleanup jobs). Each CI run upgrades in-place (stop → install → start).

Self-hosted runners have **zero dev tools** installed (no Rust, no cargo, no protoc).
They only download and run pre-built NSIS installers.

**All stages must pass.** The `All Pass` gate job is the required status check.

## Self-Hosted Runners

| Machine    | Hostname  | IP          | Labels                                 | Role              |
| ---------- | --------- | ----------- | -------------------------------------- | ----------------- |
| pz-server  | PZ-SERVER | 10.88.1.100 | self-hosted, windows, x64, pz-server   | IPP + gRPC server |
| pz-snv     | PZ-SNV    | 10.78.2.10  | self-hosted, windows, x64, pz-client   | E2E client        |

Available printers on pz-snv: Canon MG3600 (direct_ipp).
Default CI target: "Microsoft Print to PDF" (no paper waste).

## Rust Edition & Toolchain

- **Edition:** 2024
- **Toolchain:** stable
- **MSRV:** latest stable

## Workspace Structure

| Crate                 | Purpose                                                          |
| --------------------- | ---------------------------------------------------------------- |
| `devbridge-core`      | Shared types, config, proto codegen, database                    |
| `devbridge-server`    | IPP listener, gRPC server, spool manager                         |
| `devbridge-client`    | gRPC client, local print dispatcher                              |
| `devbridge-dashboard` | Axum web dashboard (serves embedded UI)                          |
| `devbridge-service`   | Windows service binary (entry point)                             |
| `devbridge-ui`        | Leptos WASM frontend (built with trunk, excluded from workspace) |
| `devbridge-app`       | Tauri desktop wrapper (excluded from workspace)                  |
| `xtask`               | Build orchestration (`cargo xtask build`, `cargo xtask dist`)    |

## Build Commands

```sh
# Check everything compiles
cargo build --workspace

# Run all tests
cargo test --workspace

# Lint (CI-strict mode)
cargo clippy --workspace --all-targets -- -D warnings

# Format check
cargo fmt --all -- --check

# Build the WASM UI (from crates/devbridge-ui/)
trunk build --release

# Full build via xtask
cargo xtask build

# Distribution build (includes Tauri installer)
cargo xtask dist
```

## Proto / gRPC

Proto files live in `proto/`. Code generation is handled by `devbridge-core/build.rs`
using `tonic-build`. Generated code should **not** be committed.

## Configuration

- Format: **TOML**
- Default config: `config/default.toml`
- Deploy templates: `deploy/config-templates/server.toml`, `deploy/config-templates/client.toml`
- The `mode` field in `[general]` determines whether the binary runs as server or client.

## Error Handling

- **Applications** (service, xtask): use `anyhow` for ergonomic error propagation.
- **Libraries** (core, server, client, dashboard): use `thiserror` for typed errors.

## Logging

Use the `tracing` crate throughout. Initialise the subscriber in `devbridge-service`.
Log level is controlled via config (`log_level`) and the `RUST_LOG` env var.

## Platform-Specific Code

Windows-only functionality (service control, printer APIs, etc.) is gated behind:

```rust
#[cfg(target_os = "windows")]
```

CI runs on Ubuntu for speed; platform-specific code compiles but is not exercised
in CI tests.

## Installation Paths (Windows)

| What                       | Path                                   |
| -------------------------- | -------------------------------------- |
| Binaries + tray app        | `C:\Program Files\DevBridge\`          |
| Config, certs, spool, logs | `C:\ProgramData\DevBridge\`            |
| Config file                | `C:\ProgramData\DevBridge\config.toml` |
| TLS certificates           | `C:\ProgramData\DevBridge\certs\`      |
| Spool directory            | `C:\ProgramData\DevBridge\spool\`      |

The NSIS installer (`cargo tauri build`) installs binaries to Program Files.
`installer/post-install.ps1` creates the ProgramData structure, writes config,
registers the Windows service, and sets up tray app auto-start.

## Certificates / TLS

gRPC runs plaintext (`http://`) over WireGuard VPN tunnels. The `TlsConfig`
struct exists for backward compatibility with old config files but is never
used. `printer_tls` in ClientConfig is unrelated — it controls HTTPS/IPPS
for direct printer connections (e.g., Epson with self-signed certs).

## Production Machines

| Machine | Hostname | WireGuard IP | MCP Server | Client ID | Printer | Backend |
|---------|----------|-------------|------------|-----------|---------|---------|
| pz-server | PZ-SERVER | 10.88.1.100 | win-pz-server | — | — | server |
| pz-snv | PZ-SNV | 10.78.2.10 | win-pz-snv | pjsnvs | Canon MG3600 | direct_ipp |
| pjpos | POKLADNA | 10.78.5.10 | — | pjpos-client | Epson L3260 | direct_ipp+TLS |
| pz-holla | EHOLLA-PC | 10.88.1.105 | win-pz-holla | holla-client | Brother DCP-1610W | windows_spooler |

## New Client Deployment

**NEVER manually write config.toml, copy certs, install SumatraPDF, or create scheduled tasks by hand.**
Always use `irm | iex` with environment variables. If the installer doesn't handle something, fix the installer.

```powershell
# Example: deploy new client
$env:DEVBRIDGE_MODE = "client"
$env:DEVBRIDGE_SERVER_HOST = "10.88.1.100"
$env:DEVBRIDGE_CLIENT_ID = "store-name"
$env:DEVBRIDGE_TARGET_PRINTER = "Printer Name"
$env:DEVBRIDGE_PRINT_BACKEND = "windows_spooler"
$env:DEVBRIDGE_VIRTUAL_PRINTER_NAME = "store printer"
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
# Then approve on server dashboard
```
