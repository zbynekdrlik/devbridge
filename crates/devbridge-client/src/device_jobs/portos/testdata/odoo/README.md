# Odoo's golden `fiscal.cash_register` documents (#110)

Byte-for-byte copies of the receipt documents Odoo composes for the
`fiscal.cash_register` device job. The SAME files are the golden reference of
odoo-erp's own parity test (`addons/company_pokladna_api/tests/devbridge_receipt_contract.py`,
a mirror of `payload.rs`). devbridge runs them through its REAL checker
(`payload_tests.rs`: `check_payload` / `check_register` / `check_payments`)
and through a real Portos 7.0.4 (`tests/device_jobs/portos_live.rs`, CI job
`portos-live`). When either side changes the contract, one of these tests goes
red.

| file | odoo-erp path (branch `main`) | source commit | sha256 |
|---|---|---|---|
| `devbridge_cash_register_receipt_9243.json` | `addons/company_pokladna_api/tests/data/devbridge_cash_register_receipt_9243.json` | `19f0110121573c59321c6dde2c3950dcd1ff1b12` (odoo-erp #9243, a sale paid in cash with change: a negative `Výdavok` payment) | `c4f659cfcac7c939f6ff52605e14cbccaa6e2a80c825b309b499fb3a67cc8d8e` |
| `devbridge_cash_register_return_9419.json` | `addons/company_pokladna_api/tests/data/devbridge_cash_register_return_9419.json` | `eb28507d46b98b53adda9fcae59ce02bbf969704` (odoo-erp #9419, the return of that sale: `Returned` items with `referenceReceiptId`, one negative `Hotovosť` payment) | `70817a9d8e26ca87d55e208a164fd8c8579b1c2172918a30e2e1bd18b1ffc2b4` |

Never edit them by hand. A refresh is a deliberate, reviewed contract change:

```sh
for f in devbridge_cash_register_receipt_9243.json devbridge_cash_register_return_9419.json; do
  p=addons/company_pokladna_api/tests/data/$f
  gh api "repos/zbynekdrlik/odoo-erp/contents/$p?ref=main" --jq .content | base64 -d > "$f"
  gh api "repos/zbynekdrlik/odoo-erp/commits?path=$p&sha=main&per_page=1" --jq '.[0].sha'
done
sha256sum *.json
```

Then update the commit and sha256 columns above, and say in the commit what
changed in the contract.
