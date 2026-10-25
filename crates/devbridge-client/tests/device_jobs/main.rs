//! Integration tests for the Odoo device-job source (#99) against a fake Odoo
//! that implements the bridge side of the device-job contract (odoo-erp
//! `addons/company_pokladna_api/docs/device_job_api.md`): JSON-RPC 2.0
//! envelope, Bearer key, `{"ok": false, "error": {code, message}}`
//! refusals, a long-polling `next` that claims a job at most once, `pending`
//! (claimed + unknown, no payload) and an `ack` that follows the contract's
//! tables — first final ack wins (`duplicate`), `not_received` refused with
//! `too_early` inside the claim window and `after_indeterminate` after an
//! `indeterminate` report, late acks after `unknown`.
//!
//! The devices are the external systems replaced here: the library's own
//! `test` executor (with its durable device log, as in E2E) and a gated
//! executor for the serial-queue tests. Every fake also records whether a
//! `pending` (or a second `next`) ever arrived while a `next` was in flight.
//!
//! Layout: [`fake`] = the fake Odoo, the scripted devices and the harness;
//! [`exactly_once`] = journal / restart / lost-response scenarios;
//! [`acks_and_loop`] = acks, reconcile, loop timing, serial devices, heartbeat;
//! [`fake_portos`] + [`portos`] = the Portos eKasa executor (#100) against a
//! fake Portos 7.0.4 (exactly-once receipts, reconcile table, per-Portos
//! queues, queue expiry, status); [`lan`] = the tills' LAN API over HTTPS
//! (#101: auth, replay / conflict, restart reconcile, one queue for both
//! channels, `report_offline`); [`lan_cert`] = its certificate issued by
//! the real gRPC `RequestCertificate` with a test CA.

mod acks_and_loop;
mod exactly_once;
mod fake;
mod fake_portos;
mod lan;
mod lan_cert;
mod portos;
// Against a REAL Portos (InMemory + Playground) — the CI `portos-live` job.
#[cfg(feature = "portos-live")]
mod portos_live;
// fiscal.paragon (#115) against the same REAL Portos.
#[cfg(feature = "portos-live")]
mod portos_live_paragon;
