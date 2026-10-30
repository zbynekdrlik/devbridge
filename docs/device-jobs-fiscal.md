# Fiscal device jobs (`fiscal.*`) — the Portos eKasa executor

devbridge #100. The channel (claim / ack / pending / heartbeat) is the Odoo
device-job contract (odoo-erp `addons/company_pokladna_api/docs/device_job_api.md`).
This document is the part devbridge owns: **the `payload` and the `result` of every
`fiscal.*` job type**, the error codes, and when a job ends `done`, `failed` or
`indeterminate`. The Odoo side copies the payload / result sections into its contract
(O4).

The device is the store's **Portos eKasa API** (NineDigit, Windows service
`NineDigit.eKasa.API`, HTTP port 3010, no authentication). Written against the
**Portos 7.0.4** swagger (the pilot store's version) and the vendor documentation
(ekasa.ninedigit.sk/docs); the CI job `portos-live` runs it against a real 7.0.4
(§8).

## 1. Which Portos runs a job

| config | meaning |
|---|---|
| `[[client.pokladna.kasy]] id = 3, portos_url = "http://10.78.2.10:3010"` | jobs of till (`pos.config`) 3 go to that Portos |
| a till without `portos_url`, or not listed | `http://localhost:3010` (Portos on the devbridge PC) |

- With `[client.pokladna] enabled = true` (and `test_executor = false`), every `fiscal.*`
  job goes to Portos. `card.*` jobs stay `unsupported` until the TIM executor (#102).
- Every Portos address has its **own serial queue**: one job (or reconcile) at a time per
  Portos, and a slow till never blocks another. Write one Portos the same way
  everywhere (`http://localhost:3010` and `http://127.0.0.1:3010` would be two queues
  driving one Portos).
- A job that waits in its queue longer than **120 s** is not run any more: it is acked
  `failed` / `expired_local` ("nothing ran").

## 2. The exactly-once key

devbridge gives every receipt the Portos `externalId`

```
externalId = "dbj-" + hex(SHA-256(UTF-8("<kasa_id>:<client_uuid>")))[0..32]
```

- `<kasa_id>` = the till's Odoo `pos.config` id in decimal, `<client_uuid>` = the job's
  `client_uuid` exactly as Odoo sends it, joined by one `:`; the hex is lower-case; the
  result is always **36 characters** (Portos accepts at most 40 — vendor library
  `NineDigit.eKasa.ID`; a real 7.0.4 answers a longer one HTTP 400 −900).
- Test vectors (any implementation, e.g. #101's offline LAN API, must reproduce them):
  - kasa 3, `6f1c2a0e-7b1d-4c55-9a77-1f0e2d3c4b5a` → `dbj-b254a45f6c31a2b6b7815da564522aec`
  - kasa 4, the same `client_uuid` → `dbj-7f4a84ac850eafcdf72d54f836ca18f5`
  - kasa 12, `a` → `dbj-4278b9eba36bd346856170cca2665a57`
- Decided on #100 (owner/main decision on the Design-question): a readable
  `dbj-<kasa_id>-<client_uuid>` did not fit the 40-character limit. The job `result`
  carries `external_id`, and devbridge logs it next to `kasa_id` + `client_uuid`, so a
  receipt can be found in Portos WebAdmin from the Odoo job.

It is never taken from the payload (a payload with an `externalId` key is refused).
Before any register request devbridge asks Portos for this externalId; if the receipt
exists it is reported `done` and **nothing is registered again**. Per the vendor
documentation ("Ochrana proti duplicitnému zaevidovaniu dokladu") Portos also returns the
original registration when an identical receipt is registered again under the same
externalId. So a till that re-sends the same operation must reuse its `client_uuid` (the
Odoo contract already requires this).

## 3. Payloads

Every payload is checked by devbridge BEFORE Portos is contacted. Any problem → the job
is acked `failed` with `error_code: "bad_payload"` and a text naming each field and rule
(never a value), e.g. `payload.data.items[0].price must have at most 2 decimal places -
Portos was not contacted, nothing ran`.

Field names and enums are Portos' own (swagger 7.0.4). Numbers must be written with at
most the decimals eKasa keeps (the `ekasa-clientphp` rounding rules) — round BEFORE
sending:

| field | decimals | range |
|---|---|---|
| `price`, `amount`, `roundingAmount`, `payments[].amount` | 2 | −10 000 000 … 10 000 000 |
| `unitPrice` | 6 | −10 000 000 … 10 000 000 |
| `quantity.amount` | 4 | 0 … 10 000 000 |
| `vatRate` | 2 | number, or `null` = not subject to VAT |

Only the shape and the decimals are devbridge's checks. eKasa's business rules (VAT
rates by date, item signs, `price = unitPrice × quantity`, discounts ≤ sales,
`roundingAmount` within ±0.04, …) are Portos' own: a breach is answered HTTP 400 →
`failed` / `portos_validation` with Portos' text ("nothing was registered").

### 3.1 `fiscal.cash_register` — a sale / return

```json
{
  "data": {
    "cashRegisterCode": "88812345678900001",
    "items": [
      {"type": "Positive", "name": "Chlieb kváskový 500 g",
       "quantity": {"amount": 2, "unit": "ks"}, "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0},
      {"type": "Returned", "name": "Rožok", "quantity": {"amount": 1},
       "unitPrice": -0.12, "price": -0.12, "vatRate": 5.0,
       "referenceReceiptId": "O-7DBCDA8A56EE426DBCDA8A56EE426D1A"}
    ],
    "payments": [{"name": "Hotovosť", "amount": 2.5}],
    "roundingAmount": 0.04,
    "customer": {"id": "12345678", "type": "ICO"},
    "headerText": "…", "footerText": "…"
  },
  "print": {"printerName": "pos", "options": {"OpenDrawer": true}}
}
```

- `data` = Portos `request.data` (`CashRegisterReceiptDTO`). Required: `cashRegisterCode`
  (16–17 digits, the till's ORP code) and `items` (non-empty).
- `items[]`: required `type` (`Positive`, `ReturnedContainer`, `Returned`, `Correction`,
  `Discount`, `Advance`, `Voucher`), `name` (1–255 printable characters), `price`,
  `unitPrice`, `quantity.amount`; optional `quantity.unit` (1–3 characters), `vatRate`,
  `referenceReceiptId` (the original receipt's `uid`, or its `okp` when it was offline —
  required by Portos for `Returned` / `Correction`), `specialRegulation`, `voucherNumber`,
  `seller` (`{"id", "type": "DIC"|"ICDPH"}`), `description` (printed only).
- `payments[]` (optional): `{"name", "amount"}`; their sum must cover the receipt.
  Cash rounding to 0.05 € is expressed by `roundingAmount` (and a payment line, e.g.
  `"Výdavok"`), computed by Odoo.
- `customer` (optional): `{"id", "type": "ICO"|"DIC"|"ICDPH"|"Other"}` — the buyer's
  identifier. Portos 7.x accepts it but has not sent it to eKasa since 5.22 (the
  Constitutional Court struck the buyer identifier down), and Portos 8 removed it — leave
  it out unless the pilot shows a need.
- `receiptType`, if given, must be `CashRegister`.
- `print` (optional; default `pos` = paper): `{"printerName": "pos"|"pdf"|"email",
  "options": {…}}` — Portos' print context (`OpenDrawer`, `PrintLogo`, email `To`, …, per
  the vendor documentation; the 7.0.4 swagger lists only `printerName`, the live CI test
  registers a receipt with `options`). An offline receipt is always printed on paper
  (eKasa rule).

### 3.2 `fiscal.invalid` — a training receipt ("Neplatný doklad")

Same as `cash_register` without `customer`; `receiptType`, if given, `Invalid`.

### 3.3 `fiscal.deposit` / `fiscal.withdraw` — cash in / out (VKLAD / VÝBER)

```json
{"data": {"cashRegisterCode": "88812345678900001", "amount": 100.00}}
```

`amount` (2 decimals) required; optional `headerText`, `footerText`, `receiptType`
(`Deposit` / `Withdraw`). Portos recommends a negative amount for a withdrawal.

> **Portos 9 removed Deposit / Withdraw / Invalid** ("Registrácia vkladu, výberu a
> neplatného dokladu nie je v tejto verzii podporovaná"; the legal duty to register them
> ended on 1.1.2026). Portos 7.x (the pilot) still registers them. On a Portos 9 the job
> fails with Portos' answer — never retried, never a second receipt.

### 3.4 `fiscal.print_copy` — a copy of a registered receipt

```json
{"cashRegisterCode": "88812345678900001", "externalId": "dbj-3-6f1c2a0e-7b1d"}
```

Exactly ONE way to name the receipt: `externalId` (the `external_id` of its `done`
result), or `id` (its `portos_request_id`), or `year` + `month` + `receiptNumber`.

### 3.5 `fiscal.open_drawer`

`{}` (Portos' configured pin) or `{"pin": "Pin2"|"Pin5"}`.

### 3.6 `fiscal.paragon` — a written paragón registered later (#115)

A paragón is the paper receipt the store writes by hand while the eKasa register cannot
work. Zákon 384/2025 Z.z. § 11 ods. 3: its data must be registered in that register
within ten days after it works again; § 9 ods. 3: the registration carries the items,
VAT and total, „poradové číslo paragónu, dátum a čas vyhotovenia paragónu". Portos
endpoint `POST /api/v1/requests/receipts/paragon` (7.0.4 swagger: `ParagonReceiptDTO`).

```json
{
  "data": {
    "cashRegisterCode": "88812345678900001",
    "issueDate": "2026-10-23T19:00:22+02:00",
    "paragonNumber": 17,
    "items": [
      {"type": "Positive", "name": "Chlieb kváskový 500 g",
       "quantity": {"amount": 2, "unit": "ks"}, "unitPrice": 1.29, "price": 2.58, "vatRate": 19.0}
    ],
    "payments": [{"name": "Hotovosť", "amount": 2.6}],
    "roundingAmount": 0.02,
    "headerText": "…", "footerText": "…"
  }
}
```

- `data` keys: those of a sale (§3.1) — `cashRegisterCode`, `items`, `payments`,
  `roundingAmount`, `headerText`, `footerText`, `receiptType` (if given, `Paragon`) —
  plus two REQUIRED ones:
  - `issueDate`: when the paper paragón was written, RFC 3339 WITH its UTC offset
    (`2026-10-23T19:00:22+02:00` or `…Z`); never later than the clock of the devbridge
    PC;
  - `paragonNumber`: the number written on the paragón, a whole number 1 … 4 294 967 295
    (vendor client; the 7.0.4 swagger names it `int32` — a number Portos itself cannot
    take is answered HTTP 400 → `failed` / `portos_validation`, nothing registered).
- Refused (`bad_payload`, Portos not contacted, "nothing ran"): a missing or malformed
  `issueDate` / `paragonNumber`, an `issueDate` in the future, `customer`, and any
  top-level `print` — a paragón takes no print context: its print method cannot be
  chosen (vendor documentation; the 7.0.4 schema lists both, Odoo never sends them).
  The refusal text says exactly that, `payload.print is not allowed for a paragon (a
  paragón takes no print context)`; whether Portos prints a paragón at all was never
  observed (the CI Portos is InMemory, no printer).
- Run exactly like a sale: the externalId lookup BEFORE the POST (a retry never registers
  twice), the same per-Portos queue, the same reconcile (§5). Whether Portos registers a
  paragón older than ten days is Portos' own rule: refused → `portos_validation`.
- What a real Portos 7.0.4 answered (CI `portos-live`, InMemory / Playground, run
  40223449996): a paragón is registered online with `receiptType` `Paragon`, its
  `paragonNumber` echoed and `issueDate` = the time it was WRITTEN (`createDate` /
  `process_date` = the registration); a paragón written **15 days** ago is registered
  (7.0.4 does not enforce the ten days); `paragonNumber` **3 000 000 000** (above int32) is
  registered and echoed. Not testable there: a paragón while Portos is offline from
  eKasa (InMemory / Playground cannot be put offline without a service restart, which
  wipes the store) and the printed paper (InMemory has no printer).

### 3.7 `fiscal.print_text` — a non-fiscal text (#120)

A text Odoo composes (the priebežná uzávierka X at a shift handover), printed on the
Portos printer. Portos 7.0.4 has no X / Z report endpoint; the vendor's own advice is to
print closings as a non-fiscal document. No receipt, no eKasa record, no externalId.

```json
{"title": "PRIEBEŽNÁ UZÁVIERKA (X)",
 "lines": ["NEFIŠKÁLNY DOKLAD –", "NIE JE DOKLAD O PREDAJI", "", "Hotovosť: 12,40 €"],
 "cut": true}
```

- Exactly these three keys. `title`: required, 1–40 characters, not blank. `lines`: 1–80
  strings of at most 40 characters (an empty string = a blank row). `cut`: required,
  `true` or `false`. No Unicode control or format character (Cc / Cf) anywhere.
- Any other key is refused `bad_payload`, Portos not contacted — `items`, `payments`,
  `receiptType`, `cashRegisterCode`, `data`, `print` included. Odoo's mirror of this
  contract is `check_print_text_payload` in its `tests/devbridge_receipt_contract.py`.
- The ONLY request: `POST /api/v1/printers/print` with
  `{"text": "<title>\n<line>\n…<pageBreak token>", "contentFlags": []}`. `cut: true` appends
  Portos' page-break token, `0x1A pagebreak 0x1A` as the vendor's examples write it, which
  triggers the paper cutter at the end of a non-fiscal document. `contentFlags: []` = no price of goods; a Portos 8+ prints
  NEPLATNÝ DOKLAD over a text without it. `cashRegisterCode` is never sent (it would print
  the seller's block first).
- The LAN API refuses this type (`unsupported`): Odoo sends it only over its device-job
  channel.
- Outcome: §5. A real 7.0.4 (CI `portos-live`, InMemory) takes this body with the token and
  answers `printed: true`. Not observed: the paper and the real cut (the CI Portos has no
  paper printer; a test print on the pilot printer needs the owner's OK).

## 4. Results (`done`)

### Receipts (`cash_register`, `invalid`, `deposit`, `withdraw`, `paragon`)

```json
{
  "device": "portos",
  "via": "register",
  "external_id": "dbj-3-6f1c2a0e-7b1d",
  "state": "online",
  "is_offline": false,
  "uid": "O-7DBCDA8A56EE426DBCDA8A56EE426D1A",
  "okp": "04eacca4-6ff07dea-c2c85513-28181bb6-f46edcb0",
  "receipt_number": 42,
  "issue_date": "2026-10-15T09:56:59+02:00",
  "receipt_type": "CashRegister",
  "amount": 2.58,
  "rounding_amount": 0.02,
  "cash_register_code": "88812345678900001",
  "portos_request_id": "2d419057-807c-41be-a5d6-aafea8eba13e",
  "process_date": "2026-10-15T09:57:00+02:00",
  "ekasa_error": null
}
```

| field | meaning |
|---|---|
| `state` | `online` — eKasa confirmed it (`uid` set); `offline` — stored in the CHDÚ and printed, sent to eKasa later by Portos (a valid receipt, `uid` null); `rejected` — stored and numbered, but eKasa refused it (`ekasa_error` = `{message, code}`): **a person must look at it** |
| `via` | `register` (this job registered it), `lookup` (it already existed — nothing was registered), `reconcile` (found after an unknown outcome) |
| `uid` | eKasa's receipt id (`response.data.id`) — the `referenceReceiptId` of a later return; with an offline receipt use `okp` |
| `receipt_number` + `issue_date` | Portos' number within the month of `issue_date` (`print_copy` by `year`/`month`/`receiptNumber`) |
| `paragon_number` | ONLY on a registered paragón (§3.6, `receipt_type` `Paragon`): its number as Portos echoes it; `issue_date` is then when the paper paragón was written |

No receipt items, prices or payments are echoed back.

### `print_copy` / `open_drawer` / `print_text`

`{"device": "portos", "printed": true, "printer_ready": true}` /
`{"device": "portos", "opened": true, "printer_ready": true}` — `printer_ready: false`
(HTTP 202) means Portos accepted the request while the printer was not ready (some CHDÚ
models print / open later); `printed` / `opened` is Portos' answer (`null` if absent).
A `print_text` is `done` ONLY with `printed: true` (§5).

## 5. When a job is `failed`, `indeterminate` or `done`

The rule: **`failed` only when Portos proves nothing was registered.** Anything unclear
after the request may have reached Portos is `indeterminate` and reconciled by the
externalId — never registered again.

### Receipts — executing

| what happened | outcome |
|---|---|
| payload / till / `client_uuid` invalid | `failed` `bad_payload` / `no_kasa` / `bad_client_uuid` (Portos not contacted) |
| waited > 120 s in the queue | `failed` `expired_local` |
| lookup finds the externalId | `done` (`via: lookup`) — not registered again |
| lookup: Portos not reachable (connection refused) | `failed` `portos_unreachable` (the receipt was not sent) |
| lookup: other error / no answer (incl. a bare 404 — no such route) | `failed` `portos_lookup_failed` (the receipt was not sent) |
| the device contact cannot be journaled | `failed` `bridge_error` (the receipt was not sent) |
| register: connection refused | `failed` `portos_unreachable` |
| register: HTTP 200 / 202 with the receipt | `done` (`state` online / offline / rejected) |
| register: HTTP 400 / 422 (validation, code −900) | `failed` `portos_validation` + Portos' text |
| register: HTTP 401 | `failed` `portos_unauthenticated` |
| register: HTTP 404 / 405 without a Portos code | `failed` `portos_endpoint_missing` (this Portos version has no such receipt type) |
| register: timeout, reset, any 5xx (e.g. −502 "print failed" — AFTER registration), any other answer | `indeterminate` → reconcile |

### Receipts — reconcile (every 30 s until it answers)

| Portos says | outcome |
|---|---|
| a receipt that names this externalId exists | `done` (`via: reconcile`), late ack |
| no such receipt, and the journal has no device contact for the job (the request never left — e.g. a restart while the job was queued or looking up) | `failed` `not_executed` — provably nothing ran |
| no such receipt, request sent, Portos lists it as unprocessed | still unknown — waiting for Portos |
| no such receipt, request sent, not listed | still unknown — **a person checks Portos WebAdmin** for the externalId (see below) |
| Portos unreachable / an error | still unknown — asked again |

Why "sent but not found" is not `failed`: Portos stores a receipt in the CHDÚ before it
answers, but its documentation does not prove that a request whose answer was lost was
never stored — it may still be inside Portos (a blocked CHDÚ / printer), and the lookup
goes through Portos' index table, which the vendor documents can be incomplete
(`index_table/status`, `index_table/restore`). Never register such a sale again under a
NEW `client_uuid` until a person has checked.

### `open_drawer` / `print_copy` — the one exception

They leave no fiscal record. Connection refused / 4xx → `failed` ("nothing ran":
`portos_unreachable`, `receipt_not_found` for a copy of an unknown receipt (−104),
`portos_validation`, `portos_unauthenticated`, `portos_endpoint_missing`,
`portos_refused`). An unknown outcome (timeout, 5xx) is `indeterminate`, then reconciled
to **`failed` `outcome_unknown_retry_safe`**: devbridge cannot tell whether the drawer
opened / the copy printed, and repeating it is harmless — the till may simply retry.
This is the ONLY `failed` code that does not prove "nothing ran".

`fiscal.print_text` (§3.7) follows the same table, and ends `failed`
`outcome_unknown_retry_safe` AT ONCE in two cases:

| Portos answers | why |
|---|---|
| HTTP 2xx without `printed: true` (202 = the printer was not ready) | `done` would tell the cashier the X printed; some CHDÚ models print later. The text names the HTTP status |
| HTTP 5xx | a printer error, e.g. „Tlačiareň nebola nájdená"; the text carries Portos' words, which a reconcile would lose |

A lost answer stays `indeterminate`, then reconciled like the drawer. devbridge never
sends a text twice; a new print is a new job from Odoo.

## 6. Heartbeat `device_status`

`portos=<state>; terminal=…`, probed every 15 s (`GET /api/v1/product/info` +
`GET /api/v1/printers/status`); with several Portos the worst one:

| state | meaning |
|---|---|
| `ok` | Portos answers, printer `Ready`, paper not `Empty` / `CoverOpen` |
| `degraded(printer=<state>,paper=<state>)` | Portos answers, the printer is not ready |
| `offline` | `product/info` does not answer |
| `unknown` | not probed yet |

## 7. Files and logs

- `device-journal.db` (client data dir, #99) also holds each job's `device_contact_at`,
  written (committed) before its register request leaves the PC: it is what tells a lost
  answer apart from a request that was never sent. **Never delete it on a store.** Rows
  that were in flight when a #99 journal was migrated are marked "contact unknown" and
  never read as "never sent".
- Logs name the job id, externalId, Portos address, receipt number and state — never
  items, prices or payments.

## 8. Tests

- Unit: payload checks per type, the decision tables above, result mapping, status.
- Integration (`crates/devbridge-client/tests/device_jobs/portos.rs`): the full
  device-job loop against a fake Odoo and a fake Portos 7.0.4 — lookup before register,
  a lost answer and a crash between POST and answer (restart) reconciled by externalId
  and POSTed exactly once, refusals, per-Portos queues, queue expiry, heartbeat.
- Real Portos (`portos_live.rs`, CI job `portos-live`): a FRESH GitHub-hosted
  `windows-latest` runner installs the pinned Portos **7.0.4** MSI headless
  (`portos-ekasa-api-win-x64-v7.0.4.msi`, SHA256 pinned), switches it to **InMemory**
  storage + **Playground** environment, and registers every `fiscal.*` type, a lost
  answer (TCP proxy drops Portos' answer) and a process crash between POST and answer —
  each reconciled, never registered twice (`next_number` counts the receipts). Never a
  store's Portos.
- Odoo's documents (#110): the sale and return receipts Odoo composes, vendored
  byte-for-byte from odoo-erp (`crates/devbridge-client/src/device_jobs/portos/testdata/odoo/`,
  source commits in its README), run through the real checker (unit) and registered by
  the real 7.0.4 (`portos_live.rs`: the sale, then its return with the sale's `uid`).
  odoo-erp checks the same files against its mirror of this contract, so a drift on
  either side turns one of the two parity tests red.
- `fiscal.paragon` (#115, `portos_live_paragon.rs`): registered once, a retry answered
  from the lookup; a paragón 15 days old and a number above int32 print what 7.0.4
  answers, and devbridge's outcome must match what Portos stored.
- `fiscal.print_text` (#120): `portos.rs` (only `printers/print`, no receipt path, a bad
  payload never reaches Portos, a printer error fails once with Portos' words),
  `portos_live_print_text.rs` (a real 7.0.4 answers `printed: true`, `next_number`
  unchanged).
- Note: the InMemory test identity expires on **15.11.2026**; the job prints the
  certificate's expiry and warns 30 days ahead — after that date the job fails until the
  identity is renewed. Pinning a newer MSI does NOT help: every MSI the vendor offers
  (7.0.4, 7.2.2, 8.0.1, 9.0.6) bundles the SAME identity (a base64 PKCS#12 inside
  `NineDigit.eKasa.API.dll` of the .NET single-file bundle, sha256
  `660a119e1ecfc5a34b9566917246526a2c46a269e801559428282c980400c423`, cash register
  `88812345678900001`). The way forward is a renewed identity from NineDigit — a new
  release, or a keystore stored as a GitHub secret and loaded by
  `deploy/ci-portos-inmemory.ps1` through `POST /api/v1/certificates` +
  `POST /api/v1/identities` (Portos 7.0.4 has both). Evidence: #101, comment 6197650284.
