# The `file.write` contract (#130)

Copies of odoo-erp's contract for the `file.write` device job. The devbridge
gh identity cannot read odoo-erp, so the odoo-erp session posted both on
devbridge issue 130 (issuecomment-6877729983) and they are copied from there
byte-for-byte (the markdown fence and the JSON block of that comment).

| file | odoo-erp source (branch `main` @ `920d3c02c36a878a26012c3676e90b4ebc158471`) | sha256 |
|---|---|---|
| `device_job_file_write.md` | `addons/company_pokladna_api/docs/device_job_file_write.md`, last changed in `d6c0a5979c91f391a460a95f47ac7dc7d95f979d` | `ffbc32ebeb1e1763ab847c7df9d701aeb9d4c41201123e2c043fcb8243d641dc` |
| `next_file_write_golden.json` | the `/pokladna/bridge/next` answer asserted by `addons/company_pokladna_api/tests/test_device_job_file_write_5235.py` (`id`, `created_at`, `claimed_at` are placeholders there) | `bda8da1f82281b1669defe0052575e53f3c32d1f4a84739cb2ea4ff1e63961af` |

`contract_tests.rs` runs them through the REAL executor: the filename
pattern, the payload keys and the `file_exists` code of the document, and the
golden job (its cp1250 content `HDR objednávka Žilina\r\n`, 23 bytes) through
`parse_job` → `check` → `execute`. When odoo-erp changes the contract, one of
those tests goes red.

Never edit them by hand. A refresh is a deliberate, reviewed contract change:
ask the odoo-erp session for the new text (or, with read access to odoo-erp,
`gh api "repos/zbynekdrlik/odoo-erp/contents/addons/company_pokladna_api/docs/device_job_file_write.md?ref=main" --jq .content | base64 -d`),
then update the commit and sha256 columns above.
