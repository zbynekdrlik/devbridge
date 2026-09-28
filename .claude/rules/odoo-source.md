---
paths:
  - "crates/devbridge-client/src/odoo_source/**"
  - "crates/devbridge-client/tests/odoo_source.rs"
  - "crates/devbridge-e2e/src/odoo_source.rs"
---

# Odoo label source (#90) — invariants and gotchas

- **Contract** (#90 comment of stream david4, 2026-09-28): `POST /food/print/next|ack|heartbeat`, JSON-RPC 2.0 envelope, `Authorization: Bearer <key>`. BOTH error kinds come with HTTP 200: a top-level `error` (auth/exception) first, then `result.error` (business). Empty queue = `{"lines": []}`. Odoo sends `false` for empty fields (text AND ids). `/next` has NO lease: it re-offers a line until acked.
- **Print-once invariant (ledger `sent`):** a `line_id` whose bytes reached the spooler is NEVER printed again, whatever Odoo sends — its stored result is re-acked. To reprint, Odoo must issue a NEW line. Only a line rejected BEFORE any send (`empty png`, `size`, `invalid line: …`) is processed anew once Odoo stored that verdict. Do not add a "re-queued → print again" path without a per-cycle token from Odoo (re-review 🔴: an Odoo state bug would reprint every second).
- **Ack failures:** a transport error fails the poll (backoff ≤ 60 s); a JSON-RPC/protocol error on ONE line keeps only that ack owed (`AcksOwed`) — it must never block `/next` or other lines. A timed-out batch is acked `outcome unknown … check the printer before reprinting`, never "failed".
- **TSPL** mirrors the captured BarTender bytes: `SIZE 72.7 mm, 110.1 mm` (with the space), CRLF, `BITMAP` 1-bit = WHITE, no GAP/DENSITY/SPEED. Label dots = `round(mm × dpi / 25.4)` (floor would reject Odoo's 880-px height). Fixture: `src/odoo_source/fixtures/bartender_bitmap_7x80.bin`.
- **api_key** only in the `Authorization` header and `config.toml` (redacting `Debug`, never on an installer command line — post-install reads `$env:DEVBRIDGE_ODOO_API_KEY`).
- Integration tests use a fake Odoo (axum) + a fake spooler `PrintBackend`; the real spooler path is E2E step 35 on the RAW E2E client (fake Odoo on pz-server :9230).
