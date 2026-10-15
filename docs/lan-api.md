# LAN API of the tills (#101) — eKasa receipts without internet

The store's DevBridge client offers the Android pokladňa a local HTTPS API.
A till sends its `fiscal.*` jobs (cash receipt, invalid receipt, deposit,
withdraw, copy, open drawer) to it directly — also while the store's internet
(and so Odoo) is down. Portos registers the receipt offline (CHDÚ) and sends it
to the tax office later; DevBridge reports the job to Odoo once Odoo answers.

**One path:** every LAN job runs exactly like an Odoo job — the same executor
checks before any device contact, the journal (`device-journal.db`, table
`lan_jobs`, `received` committed BEFORE the device is asked), the same serial
queue per Portos (a LAN job and an Odoo job never drive one Portos at once) and
the same externalId `"dbj-" + 32 hex of SHA-256("<kasa_id>:<client_uuid>")`. A
receipt registered through the LAN API is therefore found by the
lookup-before-register if the same `client_uuid` later comes through Odoo
`next` — Portos is not asked to register it twice.

Card payments are out of scope (the terminal needs the bank online anyway):
`card.*` is refused `unsupported`.

## 1. Endpoint and authentication

- `https://<client_id>.<zone>:<port>` — default port **9443**. The till maps the
  name to the PC's LAN IP itself (Android OkHttp `Dns` override, the IP from its
  Odoo configuration) or a static DNS entry on the store router resolves it — no
  public A record is needed.
- `Authorization: Bearer <lan_key>` — every till has its own key
  (`[[client.pokladna.kasy]] lan_key`), valid for THAT till only. The Odoo bridge
  key is never accepted. Keys are compared as SHA-256 digests in constant time.
- The certificate is a public Let's Encrypt certificate (section 4): the till
  needs no own CA and no pinning.

| Status | When |
|---|---|
| 401 `unauthorized` | no / unknown key, or the Odoo bridge key |
| 403 `wrong_kasa` | the key belongs to another till than `kasa_id` |
| 400 `bad_request` | the body / path is not the agreed shape (the text names the field) |
| 422 `<code>` | refused BEFORE journaling — nothing ran; the same `client_uuid` may be sent again, corrected: `unsupported` (not `fiscal.*`), `bad_payload`, `no_kasa`, `bad_client_uuid` |
| 409 `client_uuid_conflict` | this `client_uuid` already names ANOTHER operation (type or payload) on this till — nothing ran |
| 503 `bridge_error` | DevBridge could not journal it — nothing ran |
| 200 | the job is final: `done` (it ran) or `failed` (nothing ran) |
| 202 | still `received` / `executing`, or `indeterminate` (unknown — being reconciled with Portos): poll |

Errors: `{"error": {"code": "...", "text": "..."}}` (409 adds the stored
`"job"`).

## 2. Requests

`POST /v1/jobs`

```json
{"client_uuid": "6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a", "kasa_id": 3,
 "job_type": "fiscal.cash_register", "payload": {...}, "pos_order_id": 812}
```

- `client_uuid`: `[A-Za-z0-9_-]{1,64}` — the idempotency key, unique per till.
- `kasa_id`: the till's Odoo `pos.config` id; must be the key's till.
- `job_type` / `payload`: exactly as in the Odoo channel
  (`docs/device-jobs-fiscal.md`).
- `pos_order_id`: optional (an offline sale may have no Odoo id yet).

The call waits for the job up to `lan_answer_wait` (90 s — above Portos'
register timeout), so a normal receipt is answered in one call; give the HTTP
client a timeout above 90 s. A replay of the same `client_uuid` with the same
`job_type` + `payload` never runs again: it answers the stored job
(`"replay": true`), waiting like the first call while it still runs.

`GET /v1/jobs/{kasa_id}/{client_uuid}` — the job (404 `not_found` when this
till never sent it).

`GET /v1/health` — `{"status": "ok", "version": "0.8.48", "portos": "ok",
"unreported": 0}` (any till's key).

A job answer:

```json
{"client_uuid": "6f1c...", "kasa_id": 3, "job_type": "fiscal.cash_register",
 "state": "done", "result": {...}, "error": null, "replay": false,
 "reported": false, "odoo_job_id": null,
 "received_at": "2026-10-15T10:00:00+00:00", "updated_at": "2026-10-15T10:00:02+00:00"}
```

`result` is the executor's result (the same object an Odoo job's `done` ack
carries); `error` is `{code, text}` when `failed` (`code` as in the fiscal
contract) or `{"code": "indeterminate", "text": ...}` while unknown.
`reported` / `odoo_job_id` show whether Odoo has the job yet.

## 3. Report to Odoo — `POST /pokladna/bridge/report_offline` (proposed contract)

The Odoo side does not exist yet (asked on #101); DevBridge implements the
proposal below against a fake Odoo, all of its shape in
`crates/devbridge-client/src/device_jobs/report_offline.rs`.

JSON-RPC 2.0, the bridge's Bearer key (scope `pokladna_bridge`):

```json
{"client_uuid": "…", "kasa_id": 3, "job_type": "fiscal.cash_register", "pos_order_id": 812,
 "payload": {…}, "state": "done", "result": {…}, "error": null,
 "executed_at": "2026-10-15T10:00:00Z", "origin": "lan"}
```

- `state` is `done` or `failed`; `error` is `{code, text}`; `executed_at` is
  when DevBridge journaled the outcome (UTC). An `indeterminate` job is reported
  only once it is resolved.
- Reply `{"ok": true, "job_id": 123, "created": true|false}` — idempotent by
  (`kasa_id`, `client_uuid`): the same report returns the same `job_id` with
  `created: false`.
- A `client_uuid` naming a different operation is refused
  `{"ok": false, "error": {"code": "client_uuid_conflict", ...}}`.
- "One sale, one receipt": a reported offline receipt for a sale that already
  has a live receipt must not be dropped — store and flag it (like
  `ran_after_not_received`), the receipt already exists at the tax office.

DevBridge sends every final, unreported LAN job on every outcome and on a 30 s
tick; a transport error (Odoo unreachable) retries with backoff (1 → 30 s), a
JSON-RPC / protocol error keeps that one owed, a refusal is final
(`report_note = "refused: …"`, ERROR log — a person reconciles it).

## 4. Certificate

A public Let's Encrypt certificate per store for `<client_id>.<zone>`, issued
by ACME DNS-01 — centrally on pz-server:

1. the store PC makes a NEW private key and a CSR for its
   `[client.pokladna.lan_api] hostname` — the key never leaves the PC
   (`<data_dir>\lan-api\key.pem`, SYSTEM + Administrators only);
2. the CSR goes to pz-server over the existing gRPC channel
   (`RequestCertificate`);
3. pz-server signs only for an APPROVED client, only `<client_id>.<zone>`, only
   a CSR naming exactly that name, one order per name at a time, at most one
   certificate per name per hour; it runs the ACME order with the
   `_acme-challenge` TXT record in Cloudflare (the DNS token lives on pz-server
   only) and returns the chain;
4. the client checks it (the name, its own key, not expired), installs it and
   reloads the listener. Renewal starts at 30 days left (checked every 12 h,
   retried 5 min → 6 h); the certificate keeps working offline until it expires.

`[server.acme]` on pz-server (none of this is set up yet — the owner chooses the
zone):

```toml
[server.acme]
enabled = true
zone = "pos.example.sk"            # names are <client_id>.<zone>
contact_email = "it@example.sk"     # optional
# directory_url = Let's Encrypt production by default
# propagation_secs = 30             # wait after creating the TXT record

[server.acme.cloudflare]
zone_id = "<Cloudflare zone id>"
# api_token here, or DEVBRIDGE_CLOUDFLARE_API_TOKEN in the service environment
# (Zone.DNS: Edit on that zone only)
```

Without `[server.acme]` every request is answered `acme_disabled` and the LAN
API waits (it listens only with a usable certificate).

## 5. Store configuration (installer)

```powershell
$env:DEVBRIDGE_POKLADNA_KASY = '[{"id": 3, "portos_url": "http://localhost:3010", "lan_key": "<key of till 3>"}]'
$env:DEVBRIDGE_LAN_API_ENABLED = "true"
$env:DEVBRIDGE_LAN_API_PORT = "9443"                          # optional, default 9443
$env:DEVBRIDGE_LAN_API_HOSTNAME = "pjsnvs.pos.example.sk"     # <client_id>.<zone>
```

The tills are written as WHOLE `[[client.pokladna.kasy]]` blocks (every existing
one is replaced); keys are read from the environment only and never printed.
`[client.pokladna.lan_api]` is added or replaced; the port is opened on the
Private / Domain firewall profiles. **No store gets `[client.pokladna]` / the
LAN API before the owner approves a pilot.**

## 6. Tests

- Unit: auth (wrong key, other till, bridge key), request validation, replay vs
  conflict, the `lan_jobs` state machine, the reporter's decisions, the
  certificate status / renewal (30 days), the CSR, the server's checks, the
  Cloudflare provider (fake API), the installer (Pester, pwsh 7 + 5.1).
- Integration (`crates/devbridge-client/tests/device_jobs/lan.rs`,
  `lan_cert.rs`): HTTPS with a test CA → LAN API → fake Portos; a lost Portos
  answer + restart → reconcile → exactly one POST; the same `client_uuid` via LAN
  then Odoo `next` → one receipt; one device queue for both channels; the
  reporter against a fake Odoo (down → up → reported once, a lost answer →
  `existing`, a refusal is final); the certificate from the real gRPC service
  with a test CA; an unapproved client gets none.
- ACME against Pebble (CI job `ACME (Pebble)`, `tests/acme_pebble.rs`).
- Real Portos 7.0.4 (CI `portos-live`): a LAN receipt registered once, its
  replay registers nothing, reported once.
- E2E step 37 on pz-snv (isolated RAW client, test executor, test certificate):
  with Odoo down a receipt over HTTPS runs once, replay, wrong / bridge /
  other-till key and a card payment refused, then the fake Odoo gets exactly one
  `report_offline`.
