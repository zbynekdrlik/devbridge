---
paths:
  - "crates/devbridge-server/src/storage.rs"
  - "crates/devbridge-server/src/queue.rs"
  - "crates/devbridge-server/src/dispatch.rs"
  - "crates/devbridge-server/src/dispatch_tests.rs"
  - "crates/devbridge-service/src/runtime.rs"
  - "crates/devbridge-dashboard/src/api/jobs.rs"
---

# Server job states (#114) — the finished-job invariant

- **Finished = `completed`, `cancelled`, `unconfirmed`** (+ any state text this version cannot read). A finished job is NEVER requeued, reopened or re-sent. `unconfirmed` above all: the printer accepted it, a resend could print it twice (owner ruling „neposielať znova").
- **The rule lives in the SQL of the writes, never in a read before the write** — the status stream, `complete_job`, the delayed retry and the stale reaper all race each other (the reaper SELECTs, then requeues; the retry sleeps 30 s+ before requeueing):
  - `Storage::requeue_job` → `AND state IN ('failed', 'downloading', 'printing')`, returns `bool`; 0 rows on an existing job = INFO `requeue skipped: job is terminal or already queued` (a `queued` job is already waiting — requeueing it would route it twice). `JobQueue::requeue_job` routes ONLY a requeued job.
  - `Storage::update_job_state_unless_terminal` → `AND state IN ('queued', 'downloading', 'printing', 'failed')`, returns `bool`; `JobQueue::update_state_unless_terminal` sends `StateChanged` only for a real change. Used by `apply_status_update` (every client status update) and by `complete_job`'s failure branch (a finished job is neither marked failed nor retried; its failure event stays).
  - A missing job is an ERROR in both, never "terminal".
- **Unconditional writes on purpose:** `complete_job`'s success (`completed`) and unconfirmed (`unconfirmed`) branches — the client's final verdict.
- **Reconnect delivers only `state = 'queued'`** (`Storage::get_queued_jobs_for_client`, filtered in SQL). An unknown state text decodes to `Unconfirmed` + WARN (`str_to_state`), never `Queued`.
- **Manual Reprint creates a NEW job** (`devbridge-dashboard` `reprint_job`) — the guards never block it.
- A new state: decide finished vs in progress and put it in BOTH SQL lists (and `state_to_str` / `str_to_state`); `test_update_unless_terminal_keeps_exactly_the_finished_states` walks every variant.
- Test fixtures that requeue must start from `failed` (the only way production gets there) — a `queued` job is not requeued.
- `report_status` (gRPC stream) is mutation-excluded; its per-update body `apply_status_update` is mutated and unit-tested (`dispatch_tests.rs`).
