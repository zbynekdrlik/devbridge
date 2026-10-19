<!-- Global rules inherited from ~/.claude/CLAUDE.md (managed by airuleset) -->
<!-- PR merge policy, CI monitoring, TDD, autonomous verification, git workflow, test strictness, deploy patterns -->

# DevBridge — Project Conventions

## Playbook Router

| Topic | Load skill |
|---|---|
| Deploying / upgrading machines, irm\|iex, fleet, VC++ redist | `.claude/skills/deploy/SKILL.md` |
| Ghostscript device selection, print verification (EventID 307), E2E print tests | `.claude/skills/print-config/SKILL.md` |
| Serial bridge, barcode scanner, COM ports, Codex ERP config | `.claude/skills/serial-bridge/SKILL.md` |
| PowerShell-via-MCP testing, CI runner quirks, no-local-build discipline | `.claude/skills/dev-workflow/SKILL.md` |
| Installer PowerShell gotchas (PS 5.1, UTF-8, post-install lib + packaging gate, install.ps1 inline helpers) | `.claude/rules/installer-powershell.md` (auto-loads on `installer/**`, `deploy/**/*.ps1`) |
| E2E suite (isolated instance, new steps as modules, installer functions via AST, retry test last) | `.claude/rules/e2e.md` (auto-loads on `crates/devbridge-e2e/**`, `deploy/e2e-*.ps1`) |
| CI pipeline (push-only triggers, mutation job "unviable" trap, per-job logs) | `.claude/rules/ci.md` (auto-loads on `.github/workflows/**`, `crates/devbridge-ui-util/**`) |
| Odoo label source (contract, print-once ledger invariant, TSPL format, ack failures) | `.claude/rules/odoo-source.md` (auto-loads on `crates/devbridge-client/src/odoo_source/**` + its tests) |
| Odoo device jobs / pokladňa (exactly-once journal, pending/next loop, executors, Portos eKasa executor + real-Portos CI, test executor, E2E restart) | `.claude/rules/device-jobs.md` (auto-loads on `crates/devbridge-client/src/device_jobs/**`, `odoo_rpc.rs`, its tests, the E2E step, `deploy/ci-portos-inmemory.ps1`); fiscal contract `docs/device-jobs-fiscal.md` |
| LAN API of the tills (HTTPS listener, per-till keys, `lan_jobs` + negative ids, report_offline reporter, certificate from pz-server via ACME DNS-01 / Cloudflare, Pebble CI, E2E test certificate) | `.claude/rules/lan-api.md` (auto-loads on `lan_api/**`, the LAN device_jobs files, `devbridge-server/src/cert/**`, their tests, the E2E step); contract `docs/lan-api.md` |

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
- `mcp__win-pjsnvs-pokladna__Shell` — pz-snv (10.78.2.10) — DevBridge client pjsnvs (HP LaserJet M110w)
- `mcp__win-pz-holla__Shell` — pz-holla (10.88.1.105) — DevBridge client (Brother DCP-1610W)

Each also has `Snapshot`, `FileRead`, `FileWrite` variants.

**NEVER use SSH when MCP tools are available.**

## Post-Deploy Verification (Project-Specific Targets)

After CI deploys, verify both machines respond correctly before reporting success:

- **Server dashboard:** http://10.88.1.100:9120
- **Client dashboard:** http://10.78.2.10:9120

Use `mcp__win-pz-server__Shell` and `mcp__win-pjsnvs-pokladna__Shell` to verify services are running.

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
7. **Mutation** - `cargo mutants --workspace --timeout 120` (zero surviving mutants)
8. **Playwright** - Browser E2E tests against dashboard UI (all 5 pages, console zero-errors)

### Tier 1.5 (windows-latest free runner) — Windows Build + NSIS Installer

7. **Windows Build** - build service binary, WASM UI, and Tauri NSIS installer on free `windows-latest` runner. The NSIS installer bundles the service as a sidecar (`externalBin`) and installs to `C:\Program Files\DevBridge\`.

**Portos eKasa (InMemory/Playground)** (Tier 1, own `windows-latest` runner, #100) - installs the pinned Portos eKasa API 7.0.4 MSI headless in InMemory storage + Playground environment and runs the `portos_live` device-job tests (`--features portos-live`) against it. Never a store's Portos.

**ACME (Pebble)** (Tier 1, ubuntu, #101) - runs pz-server's real ACME DNS-01 order (`--features acme-pebble`) against pinned Pebble + pebble-challtestsrv. Never Let's Encrypt.

### Tier 2 (self-hosted Windows) — Real Hardware E2E (no compilation)

8. **E2E Deploy** - on pz-snv FIRST a **real `install.ps1` upgrade** of the production install with a tray app running (`DEVBRIDGE_INSTALLER_PATH` = the CI build; exit 0, tray stopped before NSIS, service running, version == build, config hash unchanged, tray relaunched — #93); then run the NSIS installer silently on both machines and write an ISOLATED E2E config; on pz-snv the **installer packaging gate** then runs the INSTALLED `post-install.ps1 -ValidateOnly` under PS 5.1 (lib next to it, missing lib → exit 1, production config hash unchanged — #80)
9. **E2E Test** - run pre-built E2E binary: installation verification → service health → IPP → gRPC → serial bridge → RAW label passthrough (2nd isolated client `e2e-raw-client`, #88) → Odoo label source on that RAW client against a fake Odoo hosted by the E2E binary (#90; a 2nd batch on the 50 × 30 mm crate roll, #108) → Odoo device jobs on the same RAW client with the E2E test executor, incl. a forced restart mid-job (#99) → the tills' LAN API on that client over HTTPS with a test certificate, Odoo down then one `report_offline` (#101) → server-driven retry (38 steps)

After CI passes, services **stay running** on both machines (no cleanup jobs). Each CI run upgrades in-place (stop → install → start).

Self-hosted runners have **zero dev tools** installed (no Rust, no cargo, no protoc).
They only download and run pre-built NSIS installers.

**All stages must pass.** The `All Pass` gate job is the required status check.

## Self-Hosted Runners

| Machine    | Hostname  | IP          | Labels                                 | Role              |
| ---------- | --------- | ----------- | -------------------------------------- | ----------------- |
| pz-server  | PZ-SERVER | 10.88.1.100 | self-hosted, windows, x64, pz-server   | IPP + gRPC server |
| pz-snv     | PZ-SNV    | 10.78.2.10  | self-hosted, windows, x64, pz-client   | E2E client        |

Available printers on pz-snv: HP LaserJet M110w (direct_ipp, urfgray) since 2026-09-23.
Default CI target: "Microsoft Print to PDF" (no paper waste).

## Rust Edition & Toolchain

- **Edition:** 2024
- **Toolchain:** stable
- **MSRV:** latest stable

## Workspace Structure

| Crate                 | Purpose                                                          |
| --------------------- | ---------------------------------------------------------------- |
| `devbridge-core`      | Shared types, config, proto codegen, database                    |
| `devbridge-server`    | IPP listener, gRPC server, spool manager, serial bridge writer   |
| `devbridge-client`    | gRPC client, local print dispatcher, serial bridge reader        |
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

## Serial Bridge (COM Port Forwarding)

DevBridge can forward serial port data (barcode scanners) from client machines
to the server over gRPC. The server writes to a com0com virtual COM port that
ERP applications (Codex) can read.

### Client config
```toml
[client.serial_bridge]
enabled = true
port = "COM5"
baud_rate = 9600
```

### Server config
```toml
[[server.serial_bridges]]
client_id = "pjkeb-client"
virtual_port = "COM20"
baud_rate = 9600
```

Requires com0com driver on the server to create virtual COM port pairs.
Written by the installer from `DEVBRIDGE_SERIAL_BRIDGES="client_id=COMn[:baud],..."` (server mode, #69) — never by hand.

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
registers the Windows service, and sets up tray app auto-start. Its helper
functions live in the siblings `installer/DevBridgeInstallerLib.ps1` (#80) and
`installer/DevBridgeLanApiLib.ps1` (the LAN API / `[server.acme]` helpers, #101)
— both bundled next to it, dot-sourced at start, each kept under 1000 lines.

## Certificates / TLS

gRPC runs plaintext (`http://`) over WireGuard VPN tunnels. The `TlsConfig`
struct exists for backward compatibility with old config files but is never
used. `printer_tls` in ClientConfig is unrelated — it controls HTTPS/IPPS
for direct printer connections (e.g., Epson with self-signed certs).

## Production Machines

| Machine | Hostname | WireGuard IP | MCP Server | Client ID | Printer | Backend |
|---------|----------|-------------|------------|-----------|---------|---------|
| pz-server | PZ-SERVER | 10.88.1.100 | win-pz-server | — | — | server |
| pz-snv | PZ-SNV | 10.78.2.10 | win-pjsnvs-pokladna | pjsnvs | HP LaserJet M110w (10.78.2.9) | direct_ipp urfgray |
| pjpos | POKLADNA | 10.78.5.10 | — | pjpos-client | Epson L3260 | direct_ipp+TLS |
| pz-holla | EHOLLA-PC | 10.88.1.105 | win-pz-holla | holla-client | Brother DCP-1610W | windows_spooler |
| pjzav | PJSLN | 10.78.9.10 | win-pjzav-pokladna | pjsln-client | HP LaserJet M110w (10.78.9.9) | direct_ipp urfgray |
| pjpop | POKLADNA | 10.78.3.10 | win-pjpop-pokladna | pjpop-client | HP LaserJet M110w (10.78.3.9) | direct_ipp urfgray |
| pjkes | DESKTOP-1HA36KG | 10.78.10.10 | win-pjkes-pokladna | pjkes-client | HP LaserJet M110w (10.78.10.9) | direct_ipp urfgray |
| pz-spisska | SPISSKA-PC | 192.168.1.60 (LAN pz-servera, server_host 192.168.1.10) | win-spisska-pc | spisska-client | TSC ML241P USB (label, VP `TSC ML241P` s TSC driverom) | windows_spooler_raw |

## New Client Deployment

See `.claude/skills/deploy/SKILL.md` for the full procedure. Short form:

```powershell
$env:DEVBRIDGE_MODE = "client"; $env:DEVBRIDGE_SERVER_HOST = "10.88.1.100"
$env:DEVBRIDGE_CLIENT_ID = "store-name"; $env:DEVBRIDGE_TARGET_PRINTER = "Printer Name"
$env:DEVBRIDGE_PRINT_BACKEND = "windows_spooler"
$env:DEVBRIDGE_SERIAL_PORT = "COM4"   # optional: only for a client with a serial barcode scanner
irm https://raw.githubusercontent.com/zbynekdrlik/devbridge/main/installer/install.ps1 | iex
```

`DEVBRIDGE_SERIAL_PORT` writes a `[client.serial_bridge]` block on a fresh install. On an **existing** install (config.toml preserved on upgrade) it **ADDS** the block if the preserved config doesn't have one yet — it is **KEPT** untouched (values not overwritten) if a `[client.serial_bridge]` section is already present; set `$env:DEVBRIDGE_FORCE_CONFIG_REWRITE = "true"` to regenerate the whole file instead.

**Label printer (RAW passthrough, #88)** — the server-side Windows printer uses the vendor driver and the client spools its bytes unchanged:

```powershell
$env:DEVBRIDGE_PRINT_BACKEND = "windows_spooler_raw"          # winspool RAW, no PDF rendering
$env:DEVBRIDGE_TARGET_PRINTER = "TSC ML241P"                  # local Windows printer on the client
$env:DEVBRIDGE_VIRTUAL_PRINTER_NAME = "spisska stitky"        # printer name users see on pz-server
$env:DEVBRIDGE_VIRTUAL_PRINTER_DRIVER = "TSC ML241P"          # driver ALREADY installed on pz-server
```

→ `[client] print_backend / virtual_printer_name / virtual_printer_driver`. On approval the server creates the VP with that driver; the reconciler only USES an installed driver (missing → ERROR in `register-virtual-printers.log`, no printer). Verification = client EventID 307 byte count == payload × copies (an EventID 842 print-processor error fails the job at once — the client printer needs a v3/winprint driver). A RAW client never receives default-queue (unpaired) jobs. On an upgrade that keeps config.toml these env vars are ignored (warning) — use `DEVBRIDGE_FORCE_CONFIG_REWRITE=true`. Driver of an existing VP: `PUT /api/virtual-printers/{id} {"driver": "..."}` (`null` = back to IPP Class Driver).

**Odoo label source (#90)** — a `windows_spooler_raw` client can ALSO pull label batches straight from Odoo (`POST /food/print/next|ack|heartbeat`, JSON-RPC 2.0, Bearer key), no pz-server in that path:

```powershell
$env:DEVBRIDGE_ODOO_URL = "https://erp.slovnormal.sk"          # Odoo base URL
$env:DEVBRIDGE_ODOO_API_KEY = "<key of the Odoo 'tlačiareň' user>" # read from env only, never on a command line, never logged
$env:DEVBRIDGE_ODOO_PRINTER_NAME = "TSC ML241P Spišská"          # food.printer name in the heartbeat
# optional: DEVBRIDGE_ODOO_LABEL_WIDTH_MM (72.7) / DEVBRIDGE_ODOO_LABEL_HEIGHT_MM (110.1) / DEVBRIDGE_ODOO_DPI (203)
# optional layout (#95): DEVBRIDGE_ODOO_ROTATE_180 (true) / DEVBRIDGE_ODOO_X_OFFSET_DOTS (0) / DEVBRIDGE_ODOO_Y_OFFSET_DOTS (0)
```

→ `[client.odoo]` (`enabled, url, api_key, printer_name`, optional `label_width_mm / label_height_mm / dpi`, `rotate_180 / x_offset_dots / y_offset_dots`, `poll_interval_secs` 5, `heartbeat_interval_secs` 30). Layout (#95): the PNG is rotated 180° and centred on the SIZE canvas like BarTender's job (576 × 879 → `BITMAP 2,0`); offsets move it in READING orientation (+ = right / down), a part pushed off the roll is cropped with a WARN; every label logs its ink box as read and WARNs (never rejects) inside BarTender's safe margins L13/T47/R13/B17. Absent keys = those defaults, so no config rewrite is needed. The block is written only WHOLE: layout/size env without URL + key + printer name is ignored (post-install warns), and re-giving the block (e.g. key rotation) must repeat every layout/size value to keep it. On an upgrade that keeps config.toml the block is ADDED, or REPLACED when the env is given again (key rotation). Config + `config.toml.*` snapshots are then restricted to SYSTEM + Administrators (the tray app, running as the user, then cannot read the dashboard port and uses its 9120 default). An invalid `[client.odoo]` refuses the service start (like any config error); a runtime failure of the Odoo source is logged and the gRPC path keeps printing. The client prints each batch as ONE TSPL spooler document (EventID 307 byte count), keeps a durable ledger `odoo-ledger.db` (a sent line is never reprinted), and acks every line (`empty png` / `size` / spooler error text). **Roll per batch (#108):** Odoo may name a batch's roll at BATCH level — `label_width_mm` / `label_height_mm`, optional `dpi` (e.g. the 50 × 30 mm crate QR labels on the same TSC as the bakery labels).
- That batch prints with `SIZE` from it. A batch without it (absent / `false` / `null` / `0`, Odoo's unset float) prints on the config roll, unchanged.
- An invalid size acks every line `size: …` and sends nothing: half-given, not a number, width ∉ 10–108 mm, height ∉ 10–300 mm, or dpi ∉ {203, 300}.
- Each batch logs `label size <w>x<h> mm (source: batch|config)`. The heartbeat reports the roll of the last document as `paper_size`.
- After a roll swap the store must gap-calibrate the TSC.

**Odoo device jobs of the pokladňa (#99)** — eKasa receipts + card payments that Odoo queues for the store's bridge (`POST /pokladna/bridge/next|ack|pending|heartbeat`, JSON-RPC 2.0, Bearer key of scope `pokladna_bridge`), each run AT MOST ONCE:

```powershell
$env:DEVBRIDGE_POKLADNA_URL = "https://erp.slovnormal.sk"             # Odoo base URL
$env:DEVBRIDGE_POKLADNA_API_KEY = "<pokladna_bridge key of the store's bridge user>"  # env only, never on a command line, never logged
$env:DEVBRIDGE_POKLADNA_WAIT_S = "25"                                   # optional long-poll 0..55 (0 = pure short polling)
```

→ `[client.pokladna]` (`enabled, url, api_key, wait_s` 25, `heartbeat_interval_secs` 30; device mapping `[[client.pokladna.kasy]] id / portos_url / lan_key / terminal_ip / terminal_tid` for the Portos / TIM executors and the LAN API — `id / portos_url / lan_key` written by the installer from `DEVBRIDGE_POKLADNA_KASY` since #101, the TIM fields not yet). Written whole, added or replaced (key rotation) on a preserved config like `[client.odoo]`, config files restricted to SYSTEM + Administrators. Independent of the print backend. Durable journal `device-journal.db` in the data dir — NEVER delete it on a store (a pending job missing from it is acked `not_received` = "nothing ran"). `fiscal.*` jobs go to the till's Portos (#100); `card.*` is refused `unsupported` (nothing runs) until the TIM executor (#102) exists — no store gets `[client.pokladna]` before the owner approves a pilot. `test_executor = true` is E2E only (no real device).

**LAN API of the tills (#101)** — the Android pokladňa sends its `fiscal.*` jobs straight to the store PC over HTTPS (`POST /v1/jobs`, `GET /v1/jobs/{kasa_id}/{client_uuid}`, `GET /v1/health`, port 9443, Bearer `lan_key` per till), also while the internet / Odoo is down; same journal / executor / queue / externalId as the Odoo channel, so a receipt is never registered twice, and every finished LAN job is reported to Odoo (`POST /pokladna/bridge/report_offline`, proposed contract). `card.*` is refused `unsupported`. Contract: `docs/lan-api.md`.

```powershell
$env:DEVBRIDGE_POKLADNA_KASY = '[{"id": 3, "portos_url": "http://localhost:3010", "lan_key": "<32-256 chars>"}]'  # env only, never logged
$env:DEVBRIDGE_LAN_API_ENABLED = "true"
$env:DEVBRIDGE_LAN_API_PORT = "9443"                       # optional
$env:DEVBRIDGE_LAN_API_HOSTNAME = "<client_id>.<zone>"     # the certificate's name
```

→ `[[client.pokladna.kasy]]` (every till replaced) + `[client.pokladna.lan_api]` (`enabled, port, hostname`, `acme` true); firewall: `DevBridge-LanApi` allows the port on Domain / Private, `DevBridge-LanApi-Public` BLOCKS it on Public (the `DevBridge-Service` program rule covers every profile), both removed by `DEVBRIDGE_LAN_API_ENABLED=false` — the store LAN must be a Private network. The installer refuses, before any change, a result the service would refuse (hostname ≠ `<client_id>.<zone>`, no till key, a till key = the bridge key). The certificate is a public Let's Encrypt one per store, ordered by pz-server (ACME DNS-01, Cloudflare) from the client's CSR over gRPC (`RequestCertificate`, only from the address of the client's job stream — a binding, not authentication) — pz-server needs `[server.acme]`, written by the installer in server mode from `DEVBRIDGE_ACME_ZONE`, optional `DEVBRIDGE_ACME_CONTACT_EMAIL`, `DEVBRIDGE_CLOUDFLARE_ZONE_ID` and `DEVBRIDGE_CLOUDFLARE_API_TOKEN` (env only, never printed; the config is then restricted to SYSTEM + Administrators); without it the LAN API never listens. NOT set up on any store or on pz-server yet — the owner picks the zone and approves a pilot.

NEVER manually write config/certs/tasks. If the installer doesn't handle something, fix the installer.
