# DevBridge - Project Conventions

## Overview

DevBridge is a print bridge for retail stores. A server receives print jobs via
an IPP virtual printer and forwards them over gRPC (with mTLS) to remote client
machines that print to local hardware printers.

## Git Workflow (MANDATORY)

- **Two branches only**: `main` (protected) and `dev` (working branch).
- `main` accepts commits **only via PR merge** from `dev`. Never push directly.
- All development happens on `dev`. Every push to `dev` triggers the full CI pipeline.
- PRs from `dev` -> `main` must have **all CI checks green** before merge.
- Every prompt/task must end with a **PR URL** that is green and mergeable.
- **Monitor CI until fully green.** After pushing, watch the pipeline to completion. If any job fails, diagnose and fix immediately — do not leave a broken pipeline for the user.
- **Post-merge CI is mandatory.** Merging a PR to `main` triggers the full pipeline again (Tier 1 + Windows Build + E2E deploy + E2E test). This re-deploys the production version to both server and client machines. Monitor this pipeline to completion. If it fails, diagnose and fix on `dev`, then re-merge.
- After CI passes (both `dev` and post-merge `main`), provide links to verify:
  - **Server dashboard:** http://10.77.8.200:9120
  - **Client dashboard:** http://10.77.9.235:9120
- Commit messages: imperative mood, concise. No fixup commits - squash or amend locally.

## Test-Driven Development (MANDATORY)

- **Write tests first.** Every new feature or bug fix starts with a failing test.
- **No `#[ignore]`**: Every test must run. CI enforces this with grep.
- **No empty test bodies**: Tests must contain assertions. CI enforces this.
- **No `todo!()`/`unimplemented!()` in production code**: Use only in active test development.
- **No `continue-on-error: true`** in any CI workflow job.
- **Test pyramid**: Unit → Integration → E2E. All three tiers must pass for a PR to merge.
- **Every implementation plan must include:** (1) a testing section specifying unit tests, integration tests, and E2E tests to add or update, and (2) a post-deploy verification section describing how to confirm the change works on the actual server/client machines after CI deploys it.
- **API schema tests must match the consumer.** If a frontend expects `{name, driver, status}` objects, the API test must assert that exact shape — not just that the endpoint returns 200 or a raw value.
- **E2E tests required for every new feature.** Every new feature, API endpoint, or UI feature MUST have corresponding E2E tests in `devbridge-e2e/src/main.rs` that run against the deployed server/client. A PR is NOT mergeable if new functionality lacks E2E test coverage. UI features must be verified via API calls against deployed dashboard URLs.

## Post-Deploy Verification (MANDATORY)

- After CI deploys, verify both machines respond correctly before reporting success.
- Use `curl` against both server (10.77.8.200:9120) and client (10.77.9.235:9120) dashboards.
- When a tool fails (e.g. WebFetch returns ECONNREFUSED), try alternative tools (`curl` via Bash, MCP tools) before concluding the target is unreachable.
- NEVER claim verification passed without actually confirming via a working tool.

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

| Machine          | Hostname      | IP          | Labels                                  | Role              |
| ---------------- | ------------- | ----------- | --------------------------------------- | ----------------- |
| print-server.lan | stagebox1-snv | 10.77.8.200 | self-hosted, windows, x64, print-server | IPP + gRPC server |
| print-client.lan | moderatori    | 10.77.9.235 | self-hosted, windows, x64, print-client | Physical printer  |

Available printers on client: EPSON L3270 (WiFi), Canon MG3600 (USB).
Default CI target: "Microsoft Print to PDF" (no paper waste).
Nightly target: physical printer.

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

mTLS certificates are generated with `installer/generate-certs.ps1`. The CA cert
is shared between server and client. See `config/default.toml` for path references.
