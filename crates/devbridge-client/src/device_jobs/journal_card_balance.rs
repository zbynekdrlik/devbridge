//! The terminal's day closings in the device journal (#102 lane D3) — table
//! `card_balances`: every `card.balance` job and every 23:50 safety balance
//! that reached a terminal, by the store's LOCAL date.
//!
//! A row is committed BEFORE the terminal is asked (`contact_at`), so a
//! safety balance is never run twice for one terminal and day — also across
//! a restart after 23:50 (a partial UNIQUE index on `(terminal_ip,
//! local_date)` for `source = 'safety'`). `outcome` is `done` / `failed` once
//! known, NULL while unknown. Odoo has no channel for the safety balance yet
//! (odoo-erp issue 7598 lane O2 — its own code calls it invisible): its
//! result lives here and in the log.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::{Journal, PRUNE_AFTER_DAYS, now};

/// `source` of a balance row.
pub const SOURCE_JOB: &str = "job";
pub const SOURCE_SAFETY: &str = "safety";

pub(super) fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS card_balances (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             terminal_ip  TEXT NOT NULL,
             local_date   TEXT NOT NULL,
             source       TEXT NOT NULL,
             job_id       INTEGER,
             contact_at   TEXT NOT NULL,
             outcome      TEXT,
             result       TEXT
         );
         CREATE UNIQUE INDEX IF NOT EXISTS card_balances_one_safety
             ON card_balances (terminal_ip, local_date) WHERE source = 'safety';",
    )
    .context("create card_balances table")
}

/// One `card_balances` row.
#[derive(Debug, Clone, PartialEq)]
pub struct CardBalance {
    pub id: i64,
    pub terminal_ip: String,
    pub local_date: String,
    pub source: String,
    pub job_id: Option<i64>,
    pub contact_at: String,
    pub outcome: Option<String>,
    pub result: Option<Value>,
}

fn row_to_balance(row: &rusqlite::Row<'_>) -> rusqlite::Result<CardBalance> {
    let result: Option<String> = row.get(7)?;
    Ok(CardBalance {
        id: row.get(0)?,
        terminal_ip: row.get(1)?,
        local_date: row.get(2)?,
        source: row.get(3)?,
        job_id: row.get(4)?,
        contact_at: row.get(5)?,
        outcome: row.get(6)?,
        result: result.and_then(|t| serde_json::from_str(&t).ok()),
    })
}

const BALANCE_COLUMNS: &str =
    "id, terminal_ip, local_date, source, job_id, contact_at, outcome, result";

impl Journal {
    /// A balance is about to be sent to `terminal_ip` (committed before
    /// this returns). `None` = a safety balance of that terminal and day is
    /// already journaled — it must NOT run again.
    pub fn card_balance_contact(
        &self,
        terminal_ip: &str,
        local_date: &str,
        source: &str,
        job_id: Option<i64>,
    ) -> Result<Option<i64>> {
        let conn = self.conn();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO card_balances (terminal_ip, local_date, source, job_id,
                     contact_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![terminal_ip, local_date, source, job_id, now()],
            )
            .with_context(|| format!("journal: balance contact of {terminal_ip}"))?;
        Ok((inserted == 1).then(|| conn.last_insert_rowid()))
    }

    /// The balance `id` ended: `done` with its result, or `failed`.
    pub fn card_balance_outcome(&self, id: i64, outcome: &str, result: &Value) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE card_balances SET outcome = ?2, result = ?3 WHERE id = ?1",
                params![id, outcome, result.to_string()],
            )
            .with_context(|| format!("journal: balance {id} outcome"))?;
        Ok(changed == 1)
    }

    /// The balance `id` never reached the terminal (the session failed before
    /// the call — nothing ran): drop its row, so the safety balance of the
    /// day is tried again.
    pub fn card_balance_forget(&self, id: i64) -> Result<bool> {
        let changed = self
            .conn()
            .execute("DELETE FROM card_balances WHERE id = ?1", params![id])
            .with_context(|| format!("journal: forget balance {id}"))?;
        Ok(changed == 1)
    }

    /// Whether `terminal_ip` needs no safety balance on `local_date`: a
    /// balance job of that day ended `done`, or a safety balance of that
    /// day was already sent (whatever its outcome).
    pub fn card_balance_ran(&self, terminal_ip: &str, local_date: &str) -> Result<bool> {
        self.conn()
            .prepare(
                "SELECT 1 FROM card_balances WHERE terminal_ip = ?1 AND local_date = ?2
                   AND (source = 'safety' OR outcome = 'done')",
            )?
            .exists(params![terminal_ip, local_date])
            .context("journal: balance of the day")
    }

    /// Every balance row of `terminal_ip`, oldest first.
    pub fn card_balances(&self, terminal_ip: &str) -> Result<Vec<CardBalance>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {BALANCE_COLUMNS} FROM card_balances WHERE terminal_ip = ?1 ORDER BY id"
        ))?;
        stmt.query_map(params![terminal_ip], row_to_balance)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("journal: balances")
    }

    /// The balance row of the `card.balance` job `job_id`.
    pub fn card_balance_of_job(&self, job_id: i64) -> Result<Option<CardBalance>> {
        self.conn()
            .query_row(
                &format!("SELECT {BALANCE_COLUMNS} FROM card_balances WHERE job_id = ?1"),
                params![job_id],
                row_to_balance,
            )
            .optional()
            .context("journal: balance of a job")
    }

    /// Whether a `card.activate` / `card.deactivate` of till `kasa_id` came
    /// after the job `job_id` (Odoo's ids grow): the later one governs the
    /// terminal's shift.
    pub fn card_later_shift_job(&self, job_id: i64, kasa_id: Option<i64>) -> Result<bool> {
        self.conn()
            .prepare(
                "SELECT 1 FROM device_jobs WHERE kasa_id IS ?1 AND job_id > ?2
                   AND job_type IN ('card.activate', 'card.deactivate')",
            )?
            .exists(params![kasa_id, job_id])
            .context("journal: later shift job")
    }

    /// Balance rows older than the journal's retention.
    pub(super) fn prune_card_balances(conn: &Connection) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS)).to_rfc3339();
        Ok(conn.execute(
            "DELETE FROM card_balances WHERE contact_at < ?1",
            params![cutoff],
        )?)
    }

    /// Test helper: move every balance row `days` into the past.
    #[cfg(test)]
    pub fn card_balances_backdate(&self, days: i64) {
        let at = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        self.conn()
            .execute("UPDATE card_balances SET contact_at = ?1", params![at])
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::device_jobs::api::DeviceJob;

    #[test]
    fn test_one_safety_balance_per_terminal_and_day() {
        let j = Journal::open_in_memory().unwrap();
        let ip = "10.78.2.8";
        assert!(!j.card_balance_ran(ip, "2026-11-04").unwrap());
        let first = j
            .card_balance_contact(ip, "2026-11-04", SOURCE_SAFETY, None)
            .unwrap();
        assert!(first.is_some());
        // a second one the same day is refused — also after a restart
        assert_eq!(
            j.card_balance_contact(ip, "2026-11-04", SOURCE_SAFETY, None)
                .unwrap(),
            None
        );
        // a sent safety balance counts whatever its outcome
        assert!(j.card_balance_ran(ip, "2026-11-04").unwrap());
        // another day / another terminal are their own
        assert!(!j.card_balance_ran(ip, "2026-11-05").unwrap());
        assert!(!j.card_balance_ran("10.78.2.18", "2026-11-04").unwrap());
        let next = j
            .card_balance_contact(ip, "2026-11-05", SOURCE_SAFETY, None)
            .unwrap()
            .unwrap();
        // a safety balance that never reached the terminal is forgotten
        assert!(j.card_balance_forget(next).unwrap());
        assert!(!j.card_balance_forget(next).unwrap());
        assert!(!j.card_balance_ran(ip, "2026-11-05").unwrap());
    }

    #[test]
    fn test_a_balance_job_counts_only_once_done() {
        let j = Journal::open_in_memory().unwrap();
        let ip = "10.78.2.8";
        let id = j
            .card_balance_contact(ip, "2026-11-04", SOURCE_JOB, Some(9))
            .unwrap()
            .unwrap();
        assert!(!j.card_balance_ran(ip, "2026-11-04").unwrap());
        // two jobs a day are both journaled (no unique key for jobs)
        let other = j
            .card_balance_contact(ip, "2026-11-04", SOURCE_JOB, Some(10))
            .unwrap()
            .unwrap();
        assert!(j.card_balance_outcome(other, "failed", &json!({})).unwrap());
        assert!(!j.card_balance_ran(ip, "2026-11-04").unwrap());
        assert!(
            j.card_balance_outcome(id, "done", &json!({"total": 1}))
                .unwrap()
        );
        assert!(j.card_balance_ran(ip, "2026-11-04").unwrap());
        assert!(!j.card_balance_outcome(999, "done", &json!({})).unwrap());
        let row = j.card_balance_of_job(9).unwrap().unwrap();
        assert_eq!(row.outcome.as_deref(), Some("done"));
        assert_eq!(row.result, Some(json!({"total": 1})));
        assert_eq!(row.source, SOURCE_JOB);
        assert_eq!(row.local_date, "2026-11-04");
        assert_eq!(j.card_balances(ip).unwrap().len(), 2);
        assert!(j.card_balance_of_job(11).unwrap().is_none());
    }

    #[test]
    fn test_balance_rows_are_pruned_after_the_retention() {
        let j = Journal::open_in_memory().unwrap();
        j.card_balance_contact("10.78.2.8", "2025-01-01", SOURCE_SAFETY, None)
            .unwrap();
        j.prune().unwrap();
        assert_eq!(j.card_balances("10.78.2.8").unwrap().len(), 1);
        j.card_balances_backdate(PRUNE_AFTER_DAYS + 1);
        j.prune().unwrap();
        assert!(j.card_balances("10.78.2.8").unwrap().is_empty());
    }

    fn shift_job(id: i64, job_type: &str, kasa_id: i64) -> DeviceJob {
        DeviceJob {
            id,
            job_type: job_type.into(),
            client_uuid: format!("cd:{id}"),
            kasa_id: Some(kasa_id),
            kasa_name: String::new(),
            pos_order: Value::Null,
            payload: json!({"session_id": 1}),
            created_at: String::new(),
            claimed_at: String::new(),
        }
    }

    #[test]
    fn test_later_shift_job_of_the_same_till() {
        let j = Journal::open_in_memory().unwrap();
        j.insert_received(&shift_job(5, "card.deactivate", 3))
            .unwrap();
        assert!(!j.card_later_shift_job(5, Some(3)).unwrap());
        // another till's, or an older one, does not count
        j.insert_received(&shift_job(6, "card.activate", 4))
            .unwrap();
        j.insert_received(&shift_job(4, "card.activate", 3))
            .unwrap();
        j.insert_received(&shift_job(7, "card.balance", 3)).unwrap();
        assert!(!j.card_later_shift_job(5, Some(3)).unwrap());
        j.insert_received(&shift_job(8, "card.activate", 3))
            .unwrap();
        assert!(j.card_later_shift_job(5, Some(3)).unwrap());
        assert!(!j.card_later_shift_job(8, Some(3)).unwrap());
    }
}
