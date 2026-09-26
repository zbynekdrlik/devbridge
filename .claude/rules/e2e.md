---
paths:
  - "crates/devbridge-e2e/**"
  - "deploy/e2e-*.ps1"
---

# E2E suite (devbridge-e2e + deploy/e2e-*.ps1) — gotchas

- **The CI E2E is an ISOLATED instance**, never the production one: data dir `C:\ProgramData\DevBridge-E2E`, gRPC 50152, dashboard 9220, `client_id = "e2e-client"`, task `DevBridgeE2E`. The setup rewrites its `config.toml` fresh every run. Never point an E2E step at `C:\ProgramData\DevBridge` (pz-snv is the live pjsnvs store POS).
- **To exercise a real installer function in E2E** (post-install helpers live in `installer/DevBridgeInstallerLib.ps1`, install.ps1's inline), dot-source `deploy/lib/Get-FunctionSourceFromScript.ps1` and extract it by AST — exactly like the Pester suites and the #70 serial-bridge merge in `e2e-setup-client-local.ps1`. Never run post-install/install for real in E2E: it hard-codes the production data dir. The ONE sanctioned run is `post-install.ps1 -ValidateOnly` (read-only, exits before the first change) in the `e2e-deploy-client` packaging gate (#80), which hashes the production `config.toml` before/after.
- **`src/main.rs` is ~2440 lines.** Put a new step in its own module (e.g. `src/serial_bridge.rs`: pure validators + unit tests, plus the async step) and only wire it in `main`. Renumber every `[N/total]` print and the final `All N E2E tests passed` line. The server-driven retry test (#56) must stay LAST (it points the client at a bad printer).
- **A step that waits for a client retry loop must wait longer than the loop's backoff cap.** By the time late steps run the client has been up for minutes, so any backoff (e.g. the serial reader's 30 s cap) is already maxed.
- **Formatting the workspace-excluded e2e crate from a nested worktree:** `cargo fmt --manifest-path crates/devbridge-e2e/Cargo.toml` fails ("believes it's in a workspace" — it resolves the parent checkout's Cargo.toml). Use `rustfmt --edition 2024 crates/devbridge-e2e/src/main.rs` (formats its modules too).
- **Quoting E2E evidence from CI:** `gh run view <id> --log` can prefix lines with the wrong job name (the E2E Test output also shows up under "Test"). For a specific job use `gh api repos/zbynekdrlik/devbridge/actions/jobs/<job-id>/logs`.
- **There are TWO isolated E2E clients on pz-snv since #88:** `e2e-client` (PDF, `DevBridge-NullPrinter`) and `e2e-raw-client` (`windows_spooler_raw`, data dir `DevBridge-E2E-Raw`, dashboard 9222, task `DevBridgeE2ERaw`, target `DevBridge-E2E-Raw` = Generic / Text Only on `NUL:`). `e2e-wait-ready.ps1` leaves the RAW one PENDING; step 34 (`src/raw_passthrough.rs`) approves it, prints, and REJECTS it on every path. Any step that picks "a client" must select `e2e-client` by machine_id, never `.first()`. Expected `connected_clients` = 2.
