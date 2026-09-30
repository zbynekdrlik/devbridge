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
- **Orientation = the golden BarTender job (#95).** Rendered in printer coordinates, the known-good BarTender job `fixtures/bartender_label_468e3256.tspl` is the readable label ROTATED 180° with the same `DIRECTION 0,0` header — so `layout::place_label` rotates the bitmap itself (rows AND dots reversed, padding stays right/white) and centres it (`BITMAP 2,0` for 576 × 879). Never "fix" orientation with `DIRECTION 1` (untested firmware semantics) or by asking Odoo for a pre-rotated PNG (printer language stays in devbridge). Offsets are in READING orientation. Any layout change is proven by the test-only renderer in `tspl_golden.rs` (dot-for-dot vs the BarTender render, real Odoo label 4 fixture) — no store test prints.
- **The renderer convention:** `render(doc).rotated_180()` = the label as read. Measured: reference canvas 581 × 880, printer ink (29,17)-(558,833), as read (23,47)-(552,863); label 4 as read (26,29)-(549,752), document 63 406 B. The renderer errors on any unknown drawing command or `DIRECTION` ≠ 0,0 — extend it (with a test) rather than letting a command pass silently.
- **api_key** only in the `Authorization` header and `config.toml` (redacting `Debug`, never on an installer command line — post-install reads `$env:DEVBRIDGE_ODOO_API_KEY`).
- Integration tests use a fake Odoo (axum) + a fake spooler `PrintBackend`; the real spooler path is E2E step 35 on the RAW E2E client (fake Odoo on pz-server :9230).
