# Client Pairing, Installer Hardening & Bug Fixes

## Problem

Every new DevBridge client setup is ad-hoc. Config is written manually, certs are copied by hand, SumatraPDF gets forgotten, virtual printers require server restarts. Each deployment is different, and each one breaks in a new way.

Root causes:
1. TLS is dead code — gRPC runs plaintext, but cert infrastructure exists and must be maintained
2. Virtual printer API has bugs — create/delete don't update IPP service in-memory
3. No authorization — any machine can connect and receive jobs
4. `install.ps1` doesn't call `post-install.ps1` — the one-liner install is incomplete
5. CLAUDE.md has no production inventory or deployment rules

## Solution

Single `irm | iex` command deploys a client. Admin approves on server dashboard. Everything auto-provisioned.

## 1. Client Pairing Flow

Clients have a pairing lifecycle:

```
[unknown] --> connects --> [pending] --> admin approves --> [approved] --> receives jobs
                                     --> admin rejects --> [rejected] --> connection refused
```

1. Client runs `irm | iex` with params (ServerHost, ClientId, TargetPrinter, PrintBackend, VirtualPrinterName)
2. Installer downloads NSIS, installs, calls post-install.ps1, service starts
3. Client connects to server, sends `ClientIdentity` with new `virtual_printer_name` field
4. Server stores client as `pending` — no jobs sent
5. Admin opens server dashboard, sees "Pending Clients" card, clicks Approve
6. On approval: client set to `approved`, virtual printer auto-created, Windows IPP printer auto-registered, IPP service updated in-memory, client starts receiving jobs

## 2. Virtual Printer API Bug Fixes

**Bug 1:** `POST /api/virtual-printers` saves to DB but doesn't call `ipp.add_printer()`. Requires server restart. Fix: add `ipp.add_printer()` after DB insert.

**Bug 2:** `POST /api/virtual-printers` doesn't accept `paired_client_id`. Fix: add optional field to `CreateRequest`.

**Bug 3:** `DELETE /api/virtual-printers/{id}` only deletes from DB. Leaves orphan IPP service entry and Windows printer. Fix: call `ipp.remove_printer()` and remove Windows printer.

## 3. Installer Hardening

**`install.ps1` change:** After NSIS install, locate and call `post-install.ps1` with all passthrough parameters. Single command does everything.

**`post-install.ps1` changes:**
- Add `-VirtualPrinterName` parameter, written to config.toml
- Remove `-CertsSource` parameter and all cert-copying logic (dead code)
- Prerequisites (VC++ Runtime, SumatraPDF, Ghostscript) already handled — just needs testing

**Delete:** `installer/generate-certs.ps1` (dead code, certs never used by gRPC)

## 4. TLS Cleanup

gRPC server (`dispatch.rs`) runs plaintext `http://`. Client (`receiver.rs`) connects plaintext. TLS config fields are parsed but never used. WireGuard provides encryption.

**Remove from new configs:** `[server.tls]` and `[client.tls]` sections from templates and default.toml.

**Keep in code:** `TlsConfig` struct with `#[serde(default)]` so old config.toml files on deployed machines still parse. Harmless dead fields.

**Keep:** `printer_tls` field in ClientConfig (Epson IPPS for direct_ipp, not gRPC).

## 5. Proto Change

Add `virtual_printer_name` to `ClientIdentity`:

```protobuf
message ClientIdentity {
  string machine_id = 1;
  string hostname = 2;
  repeated string printer_names = 3;
  string client_version = 4;
  string virtual_printer_name = 5;
}
```

Client sends this from config. Set during `irm | iex` install via `-VirtualPrinterName` param.

## 6. Database Changes

**`clients` table — add column:**
- `pairing_state TEXT NOT NULL DEFAULT 'pending'` — values: pending, approved, rejected
- `virtual_printer_name TEXT` — requested display name for server-side printer

**Migration:** Existing clients (pjsnvs, pjpos-client, holla-client) set to `approved`.

## 7. Dashboard API Additions

- `POST /api/clients/{id}/approve` — set approved, create VP + Windows printer, activate job channel
- `POST /api/clients/{id}/reject` — set rejected, drop connection
- `GET /api/clients` — add `pairing_state` and `virtual_printer_name` to response

## 8. Server Dashboard UI

"Pending Clients" section on server dashboard:
- Shows clients with `pairing_state == "pending"`
- Each card: client_id, hostname, requested printer name
- Approve (green) / Reject (red) buttons
- On approve: card disappears, virtual printer appears in printers list

## 9. CLAUDE.md Updates

Add production machine inventory table (pz-server, pz-snv, pjpos, pz-holla with IPs, MCP servers, client IDs, printers).

Add deployment rule: "NEVER manually write config.toml or install prerequisites by hand. Always use `irm | iex`. If the installer doesn't handle something, fix the installer."

## Files Modified

| File | Change |
|------|--------|
| `proto/devbridge.proto` | Add `virtual_printer_name` to ClientIdentity |
| `crates/devbridge-core/src/client_registration.rs` | Add PairingState enum, fields |
| `crates/devbridge-core/src/config.rs` | Add `virtual_printer_name` to ClientConfig |
| `crates/devbridge-server/src/storage.rs` | Migration, pairing_state CRUD |
| `crates/devbridge-server/src/dispatch.rs` | Check pairing_state on connect, remove auto-pair |
| `crates/devbridge-dashboard/src/api/virtual_printers.rs` | Bug fixes (create/delete) |
| `crates/devbridge-dashboard/src/api/clients.rs` | Approve/reject endpoints |
| `crates/devbridge-ui/src/pages/dashboard.rs` | Pending clients UI |
| `installer/install.ps1` | Call post-install.ps1 with params |
| `installer/post-install.ps1` | Add VirtualPrinterName, remove cert logic |
| `config/default.toml` | Remove TLS sections |
| `deploy/config-templates/*.toml` | Remove TLS sections |
| `CLAUDE.md` | Production inventory, deployment rules |

## Testing

**Unit tests:**
- Virtual printer create registers in IPP service
- Virtual printer delete cleans up IPP + Windows printer
- Pairing state transitions (pending -> approved, pending -> rejected)
- Approved client receives jobs, pending does not

**E2E tests:**
- New client appears as pending on `/api/clients`
- Approve creates virtual printer
- Test print through approved client completes

**Post-deploy verification:**
- Existing clients still work (migration to approved)
- Server dashboard shows pending section
- `irm | iex` on fresh machine -> approve -> print works
