# Design Spec: macOS Client Support

**Date:** 2026-04-04
**Status:** Approved
**Approach:** Minimal — `#[cfg]` branches in existing files

## Context

DevBridge clients currently run only on Windows. A macOS client is needed — same architecture as any other client: a machine on WireGuard that receives print jobs from the server and prints to a local printer. The only difference is the operating system.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Architecture | Minimal `#[cfg]` branches | YAGNI — fill existing stubs, no new abstractions |
| Installer | `curl \| bash` one-liner | Mirrors Windows `irm \| iex` pattern |
| Tray app | Yes, menu bar icon (Tauri) | Parity with Windows from day one |
| CI runner | `macos-latest` (free) | Public repo, free minutes. Needed for Tauri .app/.dmg bundling |
| Code signing | Unsigned, `xattr -cr` workaround | Sign later when more Mac clients appear |
| Target | `aarch64-apple-darwin` only | All MacBooks since late 2020 are Apple Silicon |

## Scope

### No changes (already cross-platform)

- `devbridge-core` — config, proto, database, shared utils
- `devbridge-server` — IPP listener, gRPC server, queue, dispatch (stays Windows)
- `devbridge-dashboard` — Axum API + embedded UI
- `devbridge-ui` — Leptos WASM (runs in browser)
- `backend_direct_ipp.rs` — IPP printing via HTTP
- `backend_direct_raw.rs` — RAW TCP 9100 printing
- `receiver.rs` — gRPC job receiver loop

### Modified files (add macOS `#[cfg]` branches)

- `crates/devbridge-client/src/printer.rs` — 5 CUPS functions
- `crates/devbridge-client/src/ghostscript.rs` — Homebrew paths
- `crates/devbridge-app/src/ipc_client.rs` — Unix domain socket
- `crates/devbridge-app/src/tray.rs` — launchctl fallback
- `crates/devbridge-service/src/service.rs` — macOS daemon mode
- `crates/devbridge-app/tauri.conf.json` — dmg + app bundle targets
- `.github/workflows/ci.yml` — macos-build job, dev-release macOS artifacts
- `.github/workflows/release.yml` — macOS artifacts in releases

### New files

- `installer/install.sh` — macOS `curl | bash` one-liner installer
- `installer/post-install.sh` — macOS post-install configuration
- `installer/com.devbridge.service.plist` — launchd daemon definition

## Component Details

### 1. Printing (CUPS CLI)

All 5 functions in `printer.rs` get `#[cfg(target_os = "macos")]` implementations using CUPS command-line tools. No external dependencies — CUPS is built into macOS.

| Function | macOS Implementation |
|---|---|
| `list_printers()` | `lpstat -a` — parse printer names from output |
| `print_pdf(printer, path)` | `lp -d <printer> <path>` — CUPS prints PDFs natively, no SumatraPDF needed |
| `check_printer_ready(printer)` | `lpstat -p <printer>` — check for "idle" or "enabled" status |
| `verify_print_completion(printer, job_id)` | `lpstat -o <printer>` — poll until job leaves queue (2s interval, 60s timeout) |
| `get_print_queue(printer)` | `lpstat -o <printer>` — parse queued job list |

**`lp` returns a job ID** in the format `request id is <printer>-<id>`. Parse this for `verify_print_completion` tracking.

### 2. Ghostscript Paths

Add macOS search paths to `ghostscript.rs`:

```
/opt/homebrew/bin/gs          (Homebrew on Apple Silicon)
/usr/local/bin/gs             (Homebrew on Intel / manual install)
```

Also update the `which` command: already handled by `if cfg!(windows) { "where" } else { "which" }` — works on macOS.

Ghostscript is only needed for `direct_ipp` and `direct_raw` backends (PDF-to-raster rendering). The CUPS `lp` backend prints PDFs natively.

### 3. IPC (Unix Domain Socket)

Replace the Windows named pipe stub in `ipc_client.rs` with Unix domain socket:

- **Socket path:** `/tmp/devbridge.sock`
- **Transport:** `tokio::net::UnixStream`
- **Protocol:** Same JSON request/response as Windows named pipe
- **Code:** ~15 lines filling in the existing `#[cfg(not(target_os = "windows"))]` stub

The service creates and listens on the socket. The tray app connects as a client.

### 4. Service Management (launchd)

**Service daemon plist** (`/Library/LaunchDaemons/com.devbridge.service.plist`):
- Runs `/Applications/DevBridge.app/Contents/MacOS/devbridge-service`
- `KeepAlive: true` — restart on crash
- `RunAtLoad: true` — start at boot
- Stdout/stderr to `/Library/Logs/DevBridge/`
- Runs as root (needed for port binding)

**Tray app plist** (`~/Library/LaunchAgents/com.devbridge.tray.plist`):
- Runs `/Applications/DevBridge.app/Contents/MacOS/DevBridge` (Tauri app)
- `RunAtLoad: true` — start at login
- Runs in user session (needed for menu bar access)

**Control commands in `tray.rs`:**
- Start: `sudo launchctl load -w /Library/LaunchDaemons/com.devbridge.service.plist`
- Stop: `sudo launchctl unload /Library/LaunchDaemons/com.devbridge.service.plist`
- Status: `launchctl list | grep com.devbridge`

### 5. macOS File Paths

| What | Path |
|---|---|
| App bundle | `/Applications/DevBridge.app/` |
| Service binary | `/Applications/DevBridge.app/Contents/MacOS/devbridge-service` |
| Tray binary | `/Applications/DevBridge.app/Contents/MacOS/DevBridge` |
| Config, certs, spool | `/Library/Application Support/DevBridge/` |
| Config file | `/Library/Application Support/DevBridge/config.toml` |
| TLS certificates | `/Library/Application Support/DevBridge/certs/` |
| Spool directory | `/Library/Application Support/DevBridge/spool/` |
| Logs | `/Library/Logs/DevBridge/` |
| Service plist | `/Library/LaunchDaemons/com.devbridge.service.plist` |
| Tray plist | `~/Library/LaunchAgents/com.devbridge.tray.plist` |
| IPC socket | `/tmp/devbridge.sock` |

### 6. Installer

**install.sh** — macOS equivalent of install.ps1:

```bash
# Usage:
curl -fsSL https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.sh | bash

# Dev:
DEVBRIDGE_VERSION=dev curl -fsSL https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.sh | bash

# Specific:
DEVBRIDGE_VERSION=v0.5.2 curl -fsSL https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.sh | bash
```

Steps:
1. Detect `$DEVBRIDGE_VERSION` (default: `latest`)
2. Fetch release JSON from GitHub Releases API
3. Find `.dmg` asset
4. Download + verify SHA256 checksum
5. Mount DMG: `hdiutil attach -nobrowse`
6. Copy .app to `/Applications/`: `cp -R`
7. Unmount DMG: `hdiutil detach`
8. Remove quarantine: `xattr -cr /Applications/DevBridge.app`
9. Run post-install.sh (bundled inside .app)
10. Verify service running
11. Cleanup temp files

**post-install.sh** — macOS equivalent of post-install.ps1:

Environment variables (same names as Windows):
- `DEVBRIDGE_MODE` (server/client)
- `DEVBRIDGE_SERVER_HOST`
- `DEVBRIDGE_CLIENT_ID`
- `DEVBRIDGE_TARGET_PRINTER`
- `DEVBRIDGE_PRINT_BACKEND`
- `DEVBRIDGE_VIRTUAL_PRINTER_NAME`
- `DEVBRIDGE_DASHBOARD_PORT`
- `DEVBRIDGE_GHOSTSCRIPT_DEVICE`
- `DEVBRIDGE_GHOSTSCRIPT_RESOLUTION`

Steps:
1. Stop existing service if running (`launchctl unload`)
2. Create `/Library/Application Support/DevBridge/{certs,spool,logs}`
3. Write `config.toml` from environment variables
4. Install service plist to `/Library/LaunchDaemons/`
5. Install tray plist to `~/Library/LaunchAgents/`
6. Load and start service (`launchctl load`)
7. Check Ghostscript available (warn if not — only needed for direct_ipp)
8. Verify service responds on dashboard port

### 7. Tauri Configuration

Add macOS targets to `tauri.conf.json`:

```json
"bundle": {
    "targets": ["nsis", "app", "dmg"],
    "macOS": {
        "minimumSystemVersion": "12.0"
    }
}
```

Add macOS icon: `assets/icons/icon.icns`

The `externalBin` config already points to `devbridge-service` — Tauri resolves the platform-specific triple automatically (`devbridge-service-aarch64-apple-darwin`).

### 8. CI Pipeline

**New job: `macos-build`**

```
Triggers: same as windows-build (push to dev, PR to main, push to main)
Runner: macos-latest
Depends on: format, lint, test (Tier 1 gates)
```

Steps:
1. Install Rust stable + `wasm32-unknown-unknown` target
2. Install trunk
3. `trunk build --release` (WASM UI)
4. `cargo build --release -p devbridge-service`
5. Copy sidecar: `devbridge-service-aarch64-apple-darwin`
6. Install tauri-cli
7. `cargo tauri build` → `.app` + `.dmg`
8. Build E2E binary: `cargo build --release -p devbridge-e2e`
9. Upload artifacts: `devbridge-installer-macos` (.dmg), `devbridge-e2e-macos`

**Updated: `dev-release` job**
- Download both `devbridge-installer` and `devbridge-installer-macos`
- SHA256 checksums for all artifacts
- Both `.exe` and `.dmg` in `dev-latest` pre-release

**Updated: `release.yml`**
- Add `macos-build` job (same steps)
- Both `.exe` and `.dmg` in tagged releases

**No macOS E2E in CI** — no self-hosted Mac runner. Testing is manual via deploy + dashboard verification.

## Testing Strategy

### Unit tests (CI, ubuntu-latest)
- Existing tests continue to pass (non-Windows stubs unchanged for Linux)
- New macOS-specific code is behind `#[cfg(target_os = "macos")]` — only exercised on macOS runner

### macOS build verification (CI, macos-latest)
- `cargo build --workspace` compiles with macOS cfg paths active
- `cargo test --workspace` runs unit tests on macOS
- Tauri bundle produces `.dmg` artifact

### Manual verification (post-deploy)
- Install on CEO's MacBook via `curl | bash`
- Verify service starts and dashboard accessible
- Send test print from server, verify it reaches local printer
- Verify tray app appears in menu bar

## Version

This is part of the next version bump on `dev`. No separate versioning for macOS — same binary version as Windows.
