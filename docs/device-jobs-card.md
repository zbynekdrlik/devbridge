# Card payments on the Worldline terminal — DevBridge side (#102 lane D2)

Design: odoo-erp issue 7598 (draft issuecomment-6618078139, ROZHODNUTÉ issuecomment-6618106435;
POS-ID ROZHODNUTÉ devbridge #102 issuecomment-6632469305). Odoo's side of this contract is
odoo-erp `addons/company_pokladna_api/docs/device_job_card.md` (lane O1); the generic job channel
(`next` / `ack` / `pending`, the journal rules) is `device_job_api.md` there and
`docs/device-jobs-fiscal.md` §1 here.

DevBridge runs the till's terminal through the Worldline TIM C library (`[client.pokladna.tim]`,
loaded at runtime) with **AutoCommit = false**. Only `card.purchase`, `card.commit` and
`card.rollback` run; every other `card.*` type (`card.reversal`, `card.credit`, `card.balance`,
`card.activate`, `card.deactivate`) is refused `unsupported` before it is journaled (lane D3). The
LAN API of the tills refuses every `card.*` type.

## 1. Job types

| type | payload | queue | what DevBridge does |
|---|---|---|---|
| `card.purchase` | `amount` (EUR, a whole number of cents > 0), optional `currency` (`"EUR"` only) | the till's terminal (`terminal_ip`) | authorise, then hold the terminal session until commit / rollback / the deadline |
| `card.commit` | `card_job_id` (the purchase), optional `trans_seq`, `amount` (Odoo composes them) | the terminal's settle queue (`<ip>#settle`) | hand `commit` to the held purchase |
| `card.rollback` | `card_job_id`, optional `trans_seq`, `amount` (the till's void sends `{card_job_id}` only) | the settle queue | hand `rollback` to the held purchase |

Refused before journaling ("nothing ran", acked `failed`): `bad_amount` (not > 0 or not whole
cents), `bad_currency` (not EUR), `no_terminal` (the till has no `terminal_ip`), `tim_unavailable`
(the TIM library is not loaded — no transaction is possible), `bad_payload` (a settle job without a
positive integer `card_job_id`), `unsupported`.

## 2. `card.purchase` — one held session

1. connect → login → terminal id (checked against `terminal_tid`) → `transaction_info_request`: the
   terminal's last `trans_seq` is the **baseline** (no transaction yet = 0; a last transaction
   without a response — a decline has none — falls back to the highest `trans_seq` DevBridge stored
   for that terminal; else unknown).
2. The journal commits the card row (terminal, amount in cents, baseline) and `device_contact_at`
   in ONE transaction — BEFORE the transaction is sent. A job whose terminal was already asked is
   never transacted again (acked `indeterminate`, reconciled).
3. `ta_terminal_transaction(purchase, amount)`.
4. Authorised → the result (§4, `state: "authorized"`) is journaled and acked at once with
   `phase: "authorized"`; the cardholder slip is printed (§5). The SAME session then waits — the
   simulator refuses a logout while the terminal waits for the commit and the disconnect then
   hangs, so the connection that authorised is the one that commits:
   - `card.commit` → `ta_terminal_commit` → purchase `done` (`state: "committed"`), commit `done`;
   - `card.rollback` → `ta_terminal_rollback` → purchase `failed` `rolled_back`, rollback `done`;
   - Odoo answered the authorised ack `too_late` (a refusal) → rollback at once;
   - **deadline**: `rollback_deadline_s` (default 45, 41 ..= `commit_timeout_s` − 5) after the
     authorisation without a decision → rollback → purchase `failed` `rolled_back`.
   Then logout → disconnect. Timeouts: Odoo 30 s < DevBridge 45 s < the terminal's CommitTimeout
   (60 s).
5. An authorisation that cannot pay the job (another amount, no `trans_seq`, not EUR) is rolled
   back at once and never offered to Odoo.

The authorised ack is owed only while the payment is still held: a payment decided before Odoo saw
it (Odoo unreachable) is acked with its final outcome alone, and a restart never re-sends a stale
authorisation.

## 3. Outcome codes

`card.purchase` failed (no money taken):

| code | when |
|---|---|
| `terminal_unreachable` | the connect failed — provably nothing sent (also when another ECR holds the terminal) |
| `terminal_error` | login / terminal id / creating the terminal object failed — nothing sent |
| `wrong_terminal` | the terminal at `terminal_ip` is not `terminal_tid` — nothing sent |
| `cancelled` | rc 2 ECR cancel (incl. the library's own ~60 s card-wait timeout), 53 Stop on the terminal, 54 cardholder timeout, 55 card removed |
| `declined` | rc 75, 77–88, 91–93, 97–118, 123–138, 152, 154–156, 158–161 |
| `card_error` | rc 32–48, 74, 76 (card reader / card) |
| `terminal_busy` | rc 69, 70, 162 |
| `rolled_back` | authorised, then rolled back (`card.rollback`, the deadline, Odoo's refusal, a commit after the terminal's CommitTimeout — rc 89), or the terminal reports it failed after a restart |
| `not_executed` | restart reconcile: the terminal was never asked |
| `bridge_error` | the journal could not record the contact — the transaction was not sent |

Every other result code of the transaction (connection lost, timeouts, 151
`host_timeout_outcome_unknown`, …), a commit / rollback error other than rc 89 and a crashed
thread are `indeterminate` (reconciled, never retried).

`card.commit`: `done` `{state: "committed", card_job_id}`; `failed` `already_rolled_back` (the
purchase is not charged), `unknown_purchase` (not a `card.purchase` of this till that this bridge
received), `settle_mismatch` (`trans_seq` / `amount` differ from the authorisation); `indeterminate`
while the purchase is unresolved — its reconcile follows the purchase.
`card.rollback`: `done` `{state: "rolled_back", card_job_id, detail}`; `failed` `already_committed`,
`unknown_purchase`, `settle_mismatch`; `indeterminate` as for the commit. A settle job whose
purchase is no longer held answers from the purchase's journal state (`done` → committed, `failed`
→ not charged).

## 4. Result (`phase: "authorized"` and the final `done`)

```json
{"state": "authorized",
 "terminal": {"tid": "12345678"},
 "transaction": {"trans_seq": 100, "trm_trans_ref": "0012345678100", "trans_ref": "200",
                 "auth_code": "300", "amount": 1.0, "currency": "EUR", "pos_entry_mode": "18",
                 "cvm": "pin"},
 "card": {"brand_name": "VISA", "card_number_printable": "XXXXXXXXXXXX5240", "last4": "5240"},
 "receipts": {"merchant": "<slip text>", "cardholder": "<slip text>"}}
```

- `state`: `authorized` (the phase ack), `committed` (the final `done`), `successful` +
  `"reconciled": true` (a restart reconcile found the payment charged).
- `cvm`: `undefined` / `pin` / `pin_signature` / `signature` / `on_device` / `no_cvm` / `other`.
- `card_number_printable` is masked by DevBridge to its last four digits; the full PAN and
  `card_ref` are never read from the library, so they are neither stored nor logged. Logs carry
  `trans_seq`, amount and codes, never slip text.
- Odoo maps (design §6): `transaction_id` ← `trm_trans_ref`, `payment_ref_no` ← `trans_ref`,
  `payment_method_authcode` ← `auth_code`, `card_brand` ← `brand_name`, `card_no` ← `last4`,
  `payment_method_payment_mode` ← `pos_entry_mode`, `ticket` ← `receipts.merchant`,
  `pokladna_card_trans_seq` ← `trans_seq`, `pokladna_card_tid` ← `terminal.tid`.

## 5. Slips

Right after the authorisation the cardholder slip goes to the till's Portos (`portos_url`) through
the same checked `printers/print` request as a `fiscal.print_text` job (title + ≤ 80 lines × 40
characters, wider lines wrapped, control characters blanked, cut) — directly, not through the
fiscal device queue (a receipt registration there can hold it for a minute). The merchant slip is
printed only for CVM `signature` / `pin_signature`. Response → print request ≤ 3 s (asserted on the
log timestamps of `TIM transaction response: authorised` and `card slip print request sent`). A
print failure is a WARN and never changes the payment; both slips are in the result.

## 6. Restart reconcile

A `card.purchase` that was in flight when DevBridge stopped is `indeterminate` (journal restart
recovery). Its reconcile:

1. no `device_contact_at` → `failed` `not_executed`;
2. left for a person before → `unknown`, the terminal is NOT asked again;
3. waits until the latest of (contact, authorisation, end of the execute, executor start) +
   `commit_timeout_s` + 10 s — by then the terminal has decided an uncommitted authorisation;
4. ONE session with `transaction_info_request`: the payment is ours only when the last
   transaction is a purchase with `trans_seq` > the stored baseline AND the job's amount — outcome
   successful → `done` (`state: "successful"`), failed → `failed` `rolled_back`. Anything else (no
   baseline, no response, a lower / equal `trans_seq`, another amount, outcome partial / undefined)
   is never guessed: the job stays `unknown` for a person (the terminal returns only its LATEST
   transaction — another purchase on it, e.g. by Codex, hides ours). A terminal that cannot be
   reached is asked again next round.

The auto-reversal after CommitTimeout is never assumed: the EftSimulator does not model it (a purchase
killed before its commit still reads `outcome=1`) — on a real terminal it is checked on the ETU (lane
V). A crash right after the authorised ack can therefore read `successful` with no sale bound to it:
Odoo answers that `done` with `not_authorized_first` only when it never saw the authorisation; with
it, the payment is flagged for a Vedúca on Odoo's side.

## 7. Configuration

`[client.pokladna.tim]`: `library_path`, `integrator_id`, `commit_timeout_s` (50–300, default 60),
`rollback_deadline_s` (41 ..= `commit_timeout_s` − 5, default 45; not written by the installer —
absent = default). `[[client.pokladna.kasy]] terminal_ip / terminal_tid`. The POS-ID is the plain
till id (`3`) — the SDK allows a number of at most six digits (EP2), so a till with a terminal needs
an id 1 ..= 999 999 (startup validation; the installer refuses it too).
