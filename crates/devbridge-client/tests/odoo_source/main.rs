//! Integration tests for the Odoo label source (#90) against a fake Odoo
//! HTTP server that speaks the agreed contract (JSON-RPC 2.0 envelope,
//! Bearer key, both error kinds with HTTP 200, `{"lines": []}` when empty,
//! idempotent ack with `duplicate: true`).
//!
//! The Windows spooler is the one external system replaced here (it does not
//! exist on the Linux CI runner): `FakeSpooler` records the exact bytes the
//! source hands to the print backend and reports an EventID 307-shaped
//! verification. The real spooler path is covered by the E2E step on pz-snv.
//!
//! Layout: [`fake`] = the fake Odoo, the fake spooler and the rig;
//! [`batches`] = batches on the `[client.odoo]` roll, the print-once ledger,
//! ack failures, timeouts, restarts and the run loop; [`label_size`] = the
//! batch's own roll (#108: 50 × 30 mm crate labels, the fallback to the
//! config roll, invalid sizes, the heartbeat `paper_size`).

mod batches;
mod fake;
mod label_size;
