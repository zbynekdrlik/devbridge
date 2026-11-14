# Files for a server-side bridge (`file.write`, #5235 G5-EDI)

Part of the device-job channel ([device_job_api.md](device_job_api.md)); design #5235 issuecomment-6841967397.

A DevBridge on a server WITHOUT a till (the Codex server) writes files Odoo composes — the first
one is the Codex EDI ORDER of a store's order to the central company (`company_pokladna_sklad`).

- **Addressing.** The job carries `target_bridge_user_id` = ONE bridge user. `bridge/next` hands it
  to that bridge only (within the bridge user's companies), whether the bridge serves tills or none;
  the bridge of the till that ordered never claims it. A bridge with no till long-polls `next` like
  any bridge once a file job was ever addressed to it; one never addressed gets `job: null` at once.
  `ack` / `pending` work as for every job. The job's `kasa` and `pos_order` are `null` — the
  ordering till only scopes the job's company in Odoo.
- **The job as `next` returns it** (golden shape, `test_device_job_file_write_5235`):

  ```json
  {"id": 812, "type": "file.write", "client_uuid": "edi-4711-1", "kasa": null, "pos_order": null,
   "payload": {...}, "created_at": "2026-11-13 14:15:02", "claimed_at": "2026-11-13 14:15:03"}
  ```

- **Payload** (`job.payload`) — a plain file NAME and the content, never a folder:

  ```json
  {"filename": "ORDER_SNV408_P00012_20261113141502.txt", "encoding": "cp1250",
   "content_b64": "<base64 of the exact bytes>", "size": 2321, "sha256": "<hex of the bytes>"}
  ```

  `filename` matches `^[A-Za-z0-9][A-Za-z0-9_.-]{0,99}$`. The FOLDER lives in the DevBridge config
  only (gk security shape, #5235 issuecomment-6806671564: a folder in the payload would be a
  path-traversal hole).
- **The bridge** decodes `content_b64`, checks `size` + `sha256`, writes it under a temporary name in
  its configured folder and renames it to `filename` (Codex reads every file of the folder in one go,
  so a half-written file must never carry the final name), then acks `done` (any `result` object).
  It never moves or deletes the file afterwards (Codex archives it). A name that already exists is a
  failure (`error_code: "file_exists"`), never an overwrite.
- **Queue TTL** `device_job_file_queue_ttl_s` (default 1 h): a file job nobody claimed in that time
  expires (`failed` / `expired`) and the sender may queue it again — for the EDI ORDER the order number
  stays the same, so Codex refuses a second import of it.
- **A resend is a NEW file with a NEW name** (gk E-🔵3, ACCEPT issuecomment-6844789063). The EDI ORDER
  is named `ORDER_<store>_<PO name>_<UTC yyyymmddHHMMSS>.txt` (`purchase_order_edi._pokladna_edi_file`)
  and the sender queues it again only after the last job `failed` / went `unknown`. The name must be
  unique: the bridge refuses an existing name (`file_exists`, above), Codex moves an imported file into
  `data\in\archCodex\`, and codex-bridge#840 (issuecomment-6841727623) asks for a unique file name. What
  keeps a resend from booking the order twice is the ORDER NUMBER (HDR pos 19 = the PO name), the same
  in every file of the order: Codex refuses a repeated number „už bola importovaná" (codex-bridge#840).
  The scope of that check, read from Codex's own EDI history (`raw.sp002`, `AOPERATOR='EDI'`, 23 356
  orders, 13.11.2026): one number recurs across delivery points of a customer (e.g. `883/26` for ten
  `IICODOPL`) but never twice for the same customer + delivery point — consistent with a check per
  customer + delivery point (an inference from the history, not Codex's script). A resend writes the
  store's CURRENT delivery EAN (HDR pos 285, read from the store partner on every attempt, not frozen
  on the order): an EAN the office changes between an `unknown` file and its resend escapes Codex's
  twin check. NOT yet confirmed: what Codex does with the refused file (archived or left in
  `data\in`) — an activation step of #5235 (first real file verified in Codex) together with the
  executor, zbynekdrlik/devbridge#130. Two attempts of one order in the same second would share a
  name; the bridge never overwrites (`file_exists` while the first file still sits in the folder) and a
  later resend gets a new stamp.
