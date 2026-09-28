//! Durable per-line ledger for the Odoo source (#90).
//!
//! Odoo's `/food/print/next` has no lease: it returns the same lines until
//! they are acked. So the CLIENT must remember what it already sent to the
//! spooler, or a restart / lost ack would print a label twice. Every line is
//! written here BEFORE its bytes reach the spooler:
//!
//! - `sending` — about to be / being spooled; outcome unknown yet.
//! - `done`    — outcome known (`printed_qty` + optional `error`), with an
//!   `acked` flag that flips once Odoo accepted the ack.
//!
//! A line found in the ledger is NEVER printed again. A `sending` row left by
//! a previous process (crash / kill mid-print) cannot be proven printed or
//! not, so it is closed as an error ("interrupted …") for a person to resolve
//! in Odoo — reprinting could double a label, which is the worse failure.
//!
//! Stored in its own SQLite file (`odoo-ledger.db` in the data dir) with the
//! same rusqlite stack the client job history uses; no schema is shared with
//! the gRPC job DB.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

/// Ack error text for a line a previous process left mid-print.
pub const INTERRUPTED_REASON: &str = "interrupted: devbridge client restarted before print verification - check the printer, reprint manually if missing";

/// Acked rows older than this are pruned (Odoo never returns them again).
const PRUNE_AFTER_DAYS: i64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineState {
    Sending,
    Done,
}

/// Where the ack of a `done` line stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckState {
    /// Not yet accepted by Odoo — (re-)sent every cycle.
    Owed,
    /// Odoo stored the result (`ok` or `duplicate: true`). If Odoo offers the
    /// line again after this, a person re-queued it (e.g. an edited line).
    Accepted,
    /// Odoo answered `result.error`: it did NOT store the result.
    Refused,
}

/// `ack_note` values written by the source.
pub const ACK_NOTE_OK: &str = "ok";
pub const ACK_NOTE_DUPLICATE: &str = "duplicate";
/// Prefix of the note stored for a refused ack.
pub const ACK_NOTE_REFUSED_PREFIX: &str = "refused: ";

/// One ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub line_id: i64,
    pub batch_id: i64,
    pub state: LineState,
    pub printed_qty: i64,
    pub error: Option<String>,
    pub ack: AckState,
}

pub struct Ledger {
    conn: Mutex<Connection>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open Odoo ledger {}", path.display()))?;
        Self::init(conn)
    }

    /// In-memory ledger (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS odoo_lines (
                 line_id     INTEGER PRIMARY KEY,
                 batch_id    INTEGER NOT NULL,
                 state       TEXT NOT NULL,
                 printed_qty INTEGER NOT NULL DEFAULT 0,
                 error       TEXT,
                 acked       INTEGER NOT NULL DEFAULT 0,
                 ack_note    TEXT,
                 created_at  TEXT NOT NULL,
                 updated_at  TEXT NOT NULL
             );",
        )
        .context("create odoo_lines table")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, line_id: i64) -> Result<Option<LedgerEntry>> {
        self.conn()
            .query_row(
                "SELECT line_id, batch_id, state, printed_qty, error, acked, ack_note FROM odoo_lines WHERE line_id = ?1",
                params![line_id],
                row_to_entry,
            )
            .optional()
            .context("read Odoo ledger row")
    }

    /// Record `line_ids` of `batch_id` as `sending`, atomically, BEFORE
    /// the spooler submit. A line already present is an error: the caller
    /// must never re-send a known line.
    pub fn mark_sending(&self, batch_id: i64, line_ids: &[i64]) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for line_id in line_ids {
            tx.execute(
                "INSERT INTO odoo_lines (line_id, batch_id, state, created_at, updated_at)
                 VALUES (?1, ?2, 'sending', ?3, ?3)",
                params![line_id, batch_id, now],
            )
            .with_context(|| format!("ledger: line {line_id} already recorded"))?;
        }
        tx.commit().context("commit ledger sending rows")
    }

    /// Final outcome of a line (inserting the row when it was rejected
    /// before any send). The ack is still owed (`acked = 0`).
    pub fn record_result(
        &self,
        batch_id: i64,
        line_id: i64,
        printed_qty: i64,
        error: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn()
            .execute(
                "INSERT INTO odoo_lines (line_id, batch_id, state, printed_qty, error, acked, created_at, updated_at)
                 VALUES (?1, ?2, 'done', ?3, ?4, 0, ?5, ?5)
                 ON CONFLICT(line_id) DO UPDATE SET
                     state = 'done', printed_qty = ?3, error = ?4, acked = 0, ack_note = NULL, updated_at = ?5",
                params![line_id, batch_id, printed_qty, error, now],
            )
            .with_context(|| format!("ledger: record result of line {line_id}"))?;
        Ok(())
    }

    /// Odoo answered the ack (`note`: [`ACK_NOTE_OK`], [`ACK_NOTE_DUPLICATE`]
    /// or [`ACK_NOTE_REFUSED_PREFIX`] + Odoo's text).
    pub fn mark_acked(&self, line_id: i64, note: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn()
            .execute(
                "UPDATE odoo_lines SET acked = 1, ack_note = ?2, updated_at = ?3 WHERE line_id = ?1",
                params![line_id, note, now],
            )
            .with_context(|| format!("ledger: mark line {line_id} acked"))?;
        Ok(())
    }

    /// Mark an already-acked line as owing its ack again (Odoo returned it
    /// from `/next`, so it never stored our result).
    pub fn reopen_ack(&self, line_id: i64) -> Result<()> {
        self.conn()
            .execute(
                "UPDATE odoo_lines SET acked = 0 WHERE line_id = ?1 AND state = 'done'",
                params![line_id],
            )
            .with_context(|| format!("ledger: reopen ack of line {line_id}"))?;
        Ok(())
    }

    /// Drop a line whose result Odoo ACCEPTED and then offered again: a person
    /// re-queued it, so it starts a new print cycle.
    pub fn forget(&self, line_id: i64) -> Result<()> {
        self.conn()
            .execute(
                "DELETE FROM odoo_lines WHERE line_id = ?1",
                params![line_id],
            )
            .with_context(|| format!("ledger: forget line {line_id}"))?;
        Ok(())
    }

    /// Lines with a known outcome whose ack Odoo has not accepted yet.
    pub fn pending_acks(&self) -> Result<Vec<LedgerEntry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT line_id, batch_id, state, printed_qty, error, acked, ack_note FROM odoo_lines
             WHERE state = 'done' AND acked = 0 ORDER BY batch_id, line_id",
        )?;
        let rows = stmt
            .query_map([], row_to_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Startup recovery: every `sending` row belongs to a previous process
    /// that died mid-print. Close it as an error (ack owed), never reprint.
    /// Returns the affected line ids.
    pub fn recover_interrupted(&self) -> Result<Vec<i64>> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let ids = {
            let mut stmt = tx.prepare("SELECT line_id FROM odoo_lines WHERE state = 'sending'")?;
            stmt.query_map([], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "UPDATE odoo_lines SET state = 'done', printed_qty = 0, error = ?1, acked = 0, updated_at = ?2
             WHERE state = 'sending'",
            params![INTERRUPTED_REASON, now],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    /// Drop acked rows older than 90 days. Returns how many were removed.
    pub fn prune(&self) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS)).to_rfc3339();
        let n = self.conn().execute(
            "DELETE FROM odoo_lines WHERE acked = 1 AND updated_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<LedgerEntry> {
    let state: String = r.get(2)?;
    Ok(LedgerEntry {
        line_id: r.get(0)?,
        batch_id: r.get(1)?,
        state: if state == "sending" {
            LineState::Sending
        } else {
            LineState::Done
        },
        printed_qty: r.get(3)?,
        error: r.get(4)?,
        ack: ack_state(
            r.get::<_, i64>(5)? != 0,
            r.get::<_, Option<String>>(6)?.as_deref(),
        ),
    })
}

fn ack_state(acked: bool, note: Option<&str>) -> AckState {
    match (acked, note) {
        (false, _) => AckState::Owed,
        (true, Some(n)) if n.starts_with(ACK_NOTE_REFUSED_PREFIX) => AckState::Refused,
        (true, _) => AckState::Accepted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("devbridge-ledger-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("odoo-ledger.db")
    }

    #[test]
    fn test_unknown_line_is_absent() {
        let l = Ledger::open_in_memory().unwrap();
        assert_eq!(l.get(1).unwrap(), None);
        assert!(l.pending_acks().unwrap().is_empty());
    }

    #[test]
    fn test_sending_then_result_then_ack_lifecycle() {
        let l = Ledger::open_in_memory().unwrap();
        l.mark_sending(12, &[345, 346]).unwrap();
        let e = l.get(345).unwrap().unwrap();
        assert_eq!(
            (e.state, e.batch_id, e.ack),
            (LineState::Sending, 12, AckState::Owed)
        );
        // sending rows owe no ack yet (outcome unknown)
        assert!(l.pending_acks().unwrap().is_empty());

        l.record_result(12, 345, 40, None).unwrap();
        l.record_result(12, 346, 0, Some("EventID 842")).unwrap();
        let pending = l.pending_acks().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].printed_qty, 40);
        assert_eq!(pending[0].error, None);
        assert_eq!(pending[1].error.as_deref(), Some("EventID 842"));

        l.mark_acked(345, ACK_NOTE_OK).unwrap();
        let pending = l.pending_acks().unwrap();
        assert_eq!(
            pending.iter().map(|e| e.line_id).collect::<Vec<_>>(),
            vec![346]
        );
        assert_eq!(l.get(345).unwrap().unwrap().ack, AckState::Accepted);
        l.mark_acked(
            346,
            &format!("{ACK_NOTE_REFUSED_PREFIX}Dávka nie je v stave Tlač."),
        )
        .unwrap();
        assert_eq!(l.get(346).unwrap().unwrap().ack, AckState::Refused);
        l.mark_acked(346, ACK_NOTE_DUPLICATE).unwrap();
        assert_eq!(l.get(346).unwrap().unwrap().ack, AckState::Accepted);
        l.reopen_ack(346).unwrap();

        l.reopen_ack(345).unwrap();
        assert_eq!(l.pending_acks().unwrap().len(), 2);
    }

    #[test]
    fn test_mark_sending_refuses_a_known_line_and_is_atomic() {
        let l = Ledger::open_in_memory().unwrap();
        l.mark_sending(1, &[10]).unwrap();
        let err = l.mark_sending(2, &[11, 10]).unwrap_err();
        assert!(
            format!("{err:#}").contains("line 10 already recorded"),
            "{err:#}"
        );
        // line 11 must not have been left behind by the failed transaction
        assert_eq!(l.get(11).unwrap(), None);
    }

    #[test]
    fn test_rejected_line_recorded_without_send() {
        let l = Ledger::open_in_memory().unwrap();
        l.record_result(7, 70, 0, Some("empty png")).unwrap();
        let e = l.get(70).unwrap().unwrap();
        assert_eq!(e.state, LineState::Done);
        assert_eq!(e.error.as_deref(), Some("empty png"));
        assert_eq!(e.ack, AckState::Owed);
    }

    #[test]
    fn test_forget_starts_a_new_cycle() {
        let l = Ledger::open_in_memory().unwrap();
        l.record_result(1, 10, 2, None).unwrap();
        l.mark_acked(10, ACK_NOTE_OK).unwrap();
        l.forget(10).unwrap();
        assert_eq!(l.get(10).unwrap(), None);
        l.mark_sending(2, &[10])
            .expect("a forgotten line can be sent again");
    }

    #[test]
    fn test_ack_state_mapping() {
        assert_eq!(ack_state(false, None), AckState::Owed);
        assert_eq!(ack_state(false, Some("ok")), AckState::Owed);
        assert_eq!(ack_state(true, Some("ok")), AckState::Accepted);
        assert_eq!(ack_state(true, None), AckState::Accepted);
        assert_eq!(ack_state(true, Some("refused: x")), AckState::Refused);
    }

    #[test]
    fn test_reopen_ack_ignores_sending_rows() {
        let l = Ledger::open_in_memory().unwrap();
        l.mark_sending(1, &[5]).unwrap();
        l.reopen_ack(5).unwrap();
        assert_eq!(l.get(5).unwrap().unwrap().state, LineState::Sending);
        assert!(l.pending_acks().unwrap().is_empty());
    }

    #[test]
    fn test_survives_restart_and_interrupted_line_is_never_reprinted() {
        let path = temp_db("restart");
        {
            let l = Ledger::open(&path).unwrap();
            l.mark_sending(12, &[345, 346]).unwrap();
            l.record_result(12, 345, 40, None).unwrap();
            // process dies here: 345 printed but not acked, 346 mid-print
        }
        let l = Ledger::open(&path).unwrap();
        let recovered = l.recover_interrupted().unwrap();
        assert_eq!(recovered, vec![346]);
        let e346 = l.get(346).unwrap().unwrap();
        assert_eq!(e346.state, LineState::Done);
        assert_eq!(e346.printed_qty, 0);
        assert_eq!(e346.error.as_deref(), Some(INTERRUPTED_REASON));
        // both acks are owed after the restart; 345 keeps its printed result
        let pending = l.pending_acks().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending[0],
            LedgerEntry {
                line_id: 345,
                batch_id: 12,
                state: LineState::Done,
                printed_qty: 40,
                error: None,
                ack: AckState::Owed
            }
        );
        assert!(l.recover_interrupted().unwrap().is_empty(), "idempotent");
        assert!(INTERRUPTED_REASON.chars().count() <= 200);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_prune_drops_only_old_acked_rows() {
        let l = Ledger::open_in_memory().unwrap();
        l.record_result(1, 1, 1, None).unwrap();
        l.mark_acked(1, ACK_NOTE_OK).unwrap();
        l.record_result(1, 2, 1, None).unwrap(); // not acked
        l.record_result(1, 3, 1, None).unwrap();
        l.mark_acked(3, ACK_NOTE_OK).unwrap();
        let old = (Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS + 1)).to_rfc3339();
        l.conn()
            .execute(
                "UPDATE odoo_lines SET updated_at = ?1 WHERE line_id IN (1, 2)",
                params![old],
            )
            .unwrap();
        assert_eq!(l.prune().unwrap(), 1);
        assert_eq!(l.get(1).unwrap(), None);
        assert!(l.get(2).unwrap().is_some(), "unacked row must be kept");
        assert!(l.get(3).unwrap().is_some(), "recent row must be kept");
    }
}
