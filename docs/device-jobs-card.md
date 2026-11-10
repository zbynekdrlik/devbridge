# Card payments on the Worldline terminal — DevBridge side (#102 lanes D2 + D3, lane V prep)

Design: odoo-erp issue 7598 (draft issuecomment-6618078139, ROZHODNUTÉ issuecomment-6618106435;
POS-ID ROZHODNUTÉ devbridge #102 issuecomment-6632469305). Odoo's side of this contract is
odoo-erp `addons/company_pokladna_api/docs/device_job_card.md` (lane O1); the generic job channel
(`next` / `ack` / `pending`, the journal rules) is `device_job_api.md` there and
`docs/device-jobs-fiscal.md` §1 here.

DevBridge runs the till's terminal through the Worldline TIM C library (`[client.pokladna.tim]`,
loaded at runtime) with **AutoCommit = false**. It runs `card.purchase`, `card.commit`,
`card.rollback` (lane D2) and `card.credit`, `card.reversal`, `card.activate`, `card.deactivate`,
`card.balance`, `card.print_copy` plus a 23:50 safety balance (lane D3, §8–§12); any other
`card.*` type is refused `unsupported` before it is journaled. The LAN API of the tills never
RUNS a `card.*` job (POST 422 `unsupported`) but READS one (§13). The terminal prints every slip
on its own printer (§5). A card-only store (`[client.pokladna] fiscal = false`) runs `card.*`
without any Portos (§14).

## 1. Job types

| type | payload | queue | what DevBridge does |
|---|---|---|---|
| `card.purchase` | `amount` (EUR, a whole number of cents > 0), optional `currency` (`"EUR"` only) | the till's terminal (`terminal_ip`) | authorise, then hold the terminal session until commit / rollback / the deadline |
| `card.commit` | `card_job_id` (the purchase), optional `trans_seq`, `amount` (Odoo composes them) | the terminal's settle queue (`<ip>#settle`) | hand `commit` to the held purchase |
| `card.rollback` | `card_job_id`, optional `trans_seq`, `amount` (the till's void sends `{card_job_id}` only) | the settle queue | hand `rollback` to the held purchase |
| `card.credit` | `amount`, optional `currency` (Odoo: `{pos_payment_id, amount, currency}`) | the terminal | a refund to the card — exactly like `card.purchase` (§9) |
| `card.reversal` | `trans_seq` (int 0 ..= 2³¹ − 1), `amount` (whole cents > 0), optional `tid` (Odoo: `{pos_payment_id, trans_seq, amount, tid}`) | the terminal | the storno, ONLY the TransSeq sent, no commit (§8) |
| `card.activate` / `card.deactivate` | anything (Odoo: `{session_id}`) | the terminal | open / close the terminal's shift (§10) |
| `card.balance` | anything | the terminal | the day closing (§11) |
| `card.print_copy` | `card_job_id` (DevBridge's proposed shape — Odoo has no such type yet) | the terminal | the terminal reprints its latest receipt on its own printer (§12) |

Refused before journaling ("nothing ran", acked `failed`): `bad_amount` (not > 0 or not whole
cents), `bad_currency` (not EUR), `no_terminal` (the till has no `terminal_ip`), `tim_unavailable`
(the TIM library is not loaded — no transaction is possible), `bad_payload` (a settle job or a
`card.print_copy` without a positive integer `card_job_id`; a `card.reversal` without a usable
`trans_seq` + `amount`), `unsupported`. The amount / currency / terminal checks hold for
`card.credit` as for a purchase; `no_terminal` / `tim_unavailable` for every terminal job.

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
   `phase: "authorized"`; the terminal has printed the slips itself (§5). The SAME session then waits — the
   simulator refuses a logout while the terminal waits for the commit and the disconnect then
   hangs, so the connection that authorised is the one that commits:
   - `card.commit` → `ta_terminal_commit` → purchase `done` (`state: "committed"`), commit `done`;
   - `card.rollback` → `ta_terminal_rollback` → purchase `failed` `rolled_back`, rollback `done`;
   - Odoo answered the authorised ack `too_late` (a refusal) → rollback at once;
   - **deadline**: `rollback_deadline_s` (default 45, 45 ..= min(`commit_timeout_s` − 5, 90)) after
     the authorisation without a decision → rollback → purchase `failed` `rolled_back`.
   Then logout → disconnect. Timeouts: Odoo 30 s < DevBridge 45 s < the terminal's CommitTimeout
   (60 s). Odoo's clock starts when the authorised ack REACHES it, DevBridge's at the
   authorisation, so the slack (≥ 5 s above Odoo's largest 40 s) covers the ack and the commit's
   way back; a sale Odoo records after the deadline rolled back ends `already_rolled_back` (the
   payment is reversed, a To-Do for the Vedúca) — never a charge without a sale.
   An unknown end (a lost connection, a failed commit / rollback) first releases the terminal
   with rollbacks until it answers (or says nothing is pending, rc 68) for at most the terminal's
   CommitTimeout — a logout while it waits for a commit is refused and the disconnect hangs.
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

The TERMINAL prints the slips on its own printer (ROZHODNUTÉ #102 issuecomment-6766472016 —
DevBridge never prints a card slip anywhere else, the till's Portos included). Every session sets
the terminal's print options on its terminal object before the connect
(`ta_terminal_set_print_options`, a list of two `ta_print_option_create`): cardholder AND merchant,
format `ta_c_pfmt_on_device_with_receipt` (5: the terminal prints on its own printer AND hands the
receipt text back to DevBridge), width 40, no flags. So the terminal
prints and still returns the slip text: both slips stay in the result (`receipts.merchant` →
Odoo's `pos.payment.ticket`, kept ≥ 3 months). Which slips the terminal prints for which CVM is the
terminal's own rule. `tim-settings.cfg` lists the options; a terminal that refuses them fails the
session before any terminal contact. Whether the live ETU honours them is a lane V check
(ROZHODNUTÉ: if it prints nothing, the fallback is decided from the observed behaviour).

## 6. Restart reconcile

A `card.purchase` that was in flight when DevBridge stopped is `indeterminate` (journal restart
recovery). Its reconcile:

1. no `device_contact_at` → `failed` `not_executed`;
2. left for a person before → `unknown`, the terminal is NOT asked again;
3. waits until the latest of (contact, authorisation, end of the execute, executor start) +
   `commit_timeout_s` + 10 s — by then the terminal has decided an uncommitted authorisation;
4. ONE session with `transaction_info_request`: the payment is ours only when no later payment
   was sent to that terminal (its card row) and the last transaction is a purchase with exactly
   the journaled `trans_seq` (when the authorisation was stored; else `trans_seq` > the stored
   baseline) AND the job's amount — outcome
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
`rollback_deadline_s` (45 ..= min(`commit_timeout_s` − 5, 90), default 45; not written by the installer —
absent = default). `[[client.pokladna.kasy]] terminal_ip / terminal_tid`. The POS-ID is the plain
till id (`3`) — the SDK allows a number of at most six digits (EP2), so a till with a terminal needs
an id 1 ..= 999 999 (startup validation; the installer refuses it too).

## 8. `card.reversal` — the storno in the terminal's open period

ONE session on the terminal queue: connect → login → terminal id (`terminal_tid`, and the
payload's `tid` when given → `wrong_terminal`, nothing sent) → `transaction_info_request` (the
baseline, as for a purchase) → the card row (type reversal, amount, baseline) + `device_contact_at`
journaled in ONE transaction → `ta_terminal_transaction2(reversal, request)` whose request carries
**ONLY `transaction_data.trans_seq`** — no amount (Worldline FAQ: further arguments may make it
fail) → logout → disconnect. No commit (FAQ: "A Reversal does not need a Commit"; measured: the
simulator then logs out normally). Measured on the EftSimulator 3.34 (4.11.2026): it asks the
merchant for Accept, answers the ORIGINAL amount and its own `trans_seq` (the next one).

- `done`: the result of §4 with `state: "reversed"`, `transaction.type: "reversal"`, the
  reversal's own `trans_seq` / `trans_ref` / slips; the terminal printed the slips (§5). A
  reversal of another amount than the job names is reported as the terminal answered (WARN).
- A TransSeq is reversed AT MOST ONCE: before the contact (under the terminal's lock) a reversal
  whose TransSeq already has a `done` reversal is `failed` `already_reversed`, one whose earlier
  reversal is unresolved (executing / indeterminate) `failed` `reversal_unresolved` — nothing sent,
  a person decides (a failed earlier reversal reversed nothing and blocks nothing).
- `failed` (nothing reversed): rc 67 `sixml_unknown_reference_number` = `period_closed` (Odoo then
  offers `card.credit`) ONLY when a balance of this terminal ended `done` after this bridge sent the
  payment with that TransSeq (journal `card_balances`); otherwise `unknown_reference` — rc 67 also
  answers an unknown or an already reversed TransSeq (measured on the simulator), and reading it as
  a closed period could refund the card twice; the simulator does NOT close the period on a
  balance, what a real terminal answers after its own balance is checked on the ETU (lane V). The
  purchase refusal codes of §3 (`declined`, `cancelled`, `card_error`, `terminal_busy`),
  `terminal_unreachable`, `terminal_error`, `wrong_terminal`, `bridge_error`.
- every other rc / a lost session: `indeterminate`, never sent again; reconciled like a purchase
  (§6): after the wait the terminal's LAST transaction must be a reversal with a `trans_seq` above
  the baseline AND the job's amount AND no later card row on that terminal — else a person (a last
  `trans_seq` equal to the baseline is a person, never `not_executed`); a reversal the terminal
  reports failed is `failed` `not_reversed` (the payment is still charged).

## 9. `card.credit` — a refund after the period closed

Exactly the purchase's flow with transaction type credit: baseline + contact journaled before
`ta_terminal_transaction(credit, amount)`, the `phase: "authorized"` ack (Odoo accepts it for a
credit), the held session, `card.commit` / `card.rollback` with `card_job_id` = the credit job,
the deadline rollback, the slips, the reconcile (the last transaction must be a credit). Result
`transaction.type: "credit"`. Until Odoo's refund route composes the commit (odoo-erp lane F2), an
authorised credit is rolled back at the deadline — no money moves.

## 10. `card.activate` / `card.deactivate` — the terminal's shift

ONE session, `device_contact_at` journaled before `ta_terminal_activate` / `ta_terminal_deactivate`.
`done` `{state: "activated", act_seq_counter, receipts}` / `{state: "deactivated", shift, brands,
total, receipts}` (the shift's counters, §11 shape). An error after the call was sent is
`failed` with the purchase table's code when the terminal says it did not run it (`terminal_busy`,
`cancelled`, …), else `indeterminate`. Reconcile: no contact → `failed` `not_executed`; else the job is run AGAIN (the
SDK: "If the shift is already open / closed, no error is returned", measured twice on the
simulator) → `done` with `reconciled: true` — unless a LATER `card.activate` / `card.deactivate`
of the same till is journaled (that one governs the shift): then a person.

## 11. `card.balance` and the 23:50 safety balance

ONE session, `device_contact_at` and a `card_balances` row (terminal, LOCAL date, `job`) journaled
before `ta_terminal_balance`. Result:

```json
{"state": "closed", "terminal": {"tid": "12345678"}, "period": 4,
 "brands": [{"brand_name": "VISA", "acq_id": 2, "count": 4,
             "totals": [{"currency": "EUR", "count": 4, "amount": 0.2,
                         "details": [{"transaction_type": "purchase", "count": 2, "amount": 2.7},
                                     {"transaction_type": "reversal", "count": 1, "amount": 2.0},
                                     {"transaction_type": "credit", "count": 1, "amount": 0.5}]}]}],
 "total": {"currency": "EUR", "count": 4, "amount": 0.2},
 "receipts": {"merchant": "<daily closing slip>", "cardholder": null}}
```

A brand total's `amount` is the terminal's NET sum (purchases − reversals − credits, signed:
measured −0.70 for 2.40 − 2.40 − 0.70); `total` adds every brand's EUR totals; a detail's amount
is that type's own sum. A balance the terminal refuses (`terminal_busy`, …) is `failed` (nothing
ran, the row keeps `failed`); an interrupted one is `indeterminate` and stays with a person (the
terminal does not tell whether it ran; the next balance closes the period anyway).

**Safety balance.** Every 60 s the executor checks the store PC's LOCAL clock; from 23:50 to
midnight each terminal with no balance of that day — no `card.balance` that ended `done`, no
safety balance already sent — gets ONE balance on its own queue (after any held payment). Once the
terminal's lock is held the day is checked again (a balance job may have finished meanwhile) and
its `card_balances` row (`safety`, unique per terminal and day) is committed right BEFORE the call,
so a restart after 23:50 never runs a second one; a session that never reached the call (connect /
login failed) journals nothing, and a balance the terminal refused (nothing ran) drops its row —
both are tried again the next minute. An unknown answer keeps the row (no second balance). A
safety balance still waiting for the terminal at midnight is not sent for the old day. **Odoo has no
channel for it yet** (odoo-erp lane O2's `pokladna.card.balance` is not built; Odoo's refund choice
treats it as invisible — a reversal Odoo offers after it is then refused `period_closed` and
becomes a credit): its result is journaled (`card_balances.result`) and logged (`TIM safety balance
finished`, with the EUR total).

## 12. `card.print_copy` — the terminal's copy of a receipt

`{card_job_id}` names a `card.purchase` / `card.credit` / `card.reversal` of the same till that this
bridge ran and whose terminal answer is journaled (else `failed` `unknown_purchase`). ONE session
(`device_contact_at` journaled inside it, the §5 print options set): `transaction_info_request` —
the terminal reprints only its LATEST receipt, so its last `trans_seq` must be the named one (else
`failed` `not_last_transaction`, nothing printed) → `ta_terminal_receipt_request(reprint)`, which
the terminal prints on its own printer. `done` `{state: "printed", card_job_id, trans_seq,
receipts, receipts_returned}` (the copy's text as the terminal returned it; `receipts_returned:
false` = the terminal answered ok but returned no text — with `on_device_with_receipt` it should,
so that flags a terminal that did not honour the print options, logged); a refused reprint is
`failed` `terminal_error`. An interrupted copy reconciles `failed` `print_unknown` (no money; the till may
ask again).

## 13. The LAN API reads card jobs

`GET /v1/card-jobs/{kasa_id}/{client_uuid}` (`docs/lan-api.md` §2): the newest card job the till
submitted (never an Odoo-composed commit / rollback / shift job) with that key as this bridge
journaled it — `state`, `phase: "authorized"` +
`result` = the authorisation while a payment is held, the final `result` / `error` afterwards — so a
till whose Odoo is down learns that its payment was rolled back at the deadline. Read only: a
`card.*` POST stays 422 `unsupported`.

## 14. Card-only store — `[client.pokladna] fiscal = false`

For a store whose eKasa stays outside DevBridge (pjsnvs, owner ruling 26.10.2026: no real eKasa
receipt on its live Portos). `fiscal = false` (installer `DEVBRIDGE_POKLADNA_FISCAL=false`, sticky
like `test_executor`; default `true`) builds NO Portos executor: every `fiscal.*` job — from Odoo
and from the LAN API — is refused `unsupported` ("devbridge has no executor … nothing ran") before
it is journaled (Odoo: `insert_refused`, one `failed` ack; LAN: 422), so the till's Portos is never
contacted. `card.*` runs on the TIM executor as above (`[client.pokladna.tim]`); the heartbeat
reads `portos=none`. Without `[client.pokladna.tim]` every job is refused, and the LAN API (it runs
`fiscal.*` only) refuses everything — the service logs both at start. `test_executor = true` wins over it (both kinds then run on the test
executor).
