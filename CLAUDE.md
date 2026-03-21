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
- Commit messages: imperative mood, concise. No fixup commits - squash or amend locally.

## Test-Driven Development (MANDATORY)

- **Write tests first.** Every new feature or bug fix starts with a failing test.
- **No `#[ignore]`**: Every test must run. CI enforces this with grep.
- **No empty test bodies**: Tests must contain assertions. CI enforces this.
- **No `todo!()`/`unimplemented!()` in production code**: Use only in active test development.
- **No `continue-on-error: true`** in any CI workflow job.
- **Test pyramid**: Unit → Integration → E2E. All three tiers must pass for a PR to merge.

## CI/CD Pipeline

The CI workflow (`.github/workflows/ci.yml`) is the quality gate. It runs on every
push to `dev` and every PR to `main`. **All jobs must pass for a PR to be mergeable.**

### Tier 1 (ubuntu-latest) — Code Quality

1. **Format** - `cargo fmt --all -- --check` (zero tolerance)
2. **Lint** - `cargo clippy --workspace --all-targets -- -D warnings` (deny all warnings)
3. **Test** - `cargo test --workspace` (unit + integration tests must pass)
4. **Build** - `cargo build --workspace --release` (must compile cleanly)
5. **Audit** - `cargo deny check` (license + vulnerability audit)
6. **TDD Enforce** - grep for `#[ignore]`, empty tests, `todo!()`

### Tier 1.5 (windows-latest free runner) — Windows Build

7. **Windows Build** - compile service + E2E binary on free `windows-latest` runner, upload artifacts

### Tier 2 (self-hosted Windows) — Real Hardware E2E (no compilation)

8. **E2E Deploy** - download pre-built artifacts, deploy to both machines, start services
9. **E2E Test** - run pre-built E2E binary: IPP → gRPC → physical printer
10. **E2E Cleanup** - stop services, remove artifacts

Self-hosted runners have **zero dev tools** installed (no Rust, no cargo, no protoc).
They only download and run pre-built binaries.

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

## Certificates / TLS

mTLS certificates are generated with `installer/generate-certs.ps1`. The CA cert
is shared between server and client. See `config/default.toml` for path references.
