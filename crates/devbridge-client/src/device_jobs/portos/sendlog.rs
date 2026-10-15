//! The Portos executor's send log (#100) — `portos-sent.db` in the data dir.
//!
//! One row per externalId, written (committed, `synchronous = FULL`) BEFORE
//! its register request leaves this PC. It is what makes "nothing ran"
//! PROVABLE for a receipt whose outcome is unknown: Portos has no receipt
//! with the externalId AND this log has no row for it → devbridge never sent
//! it (e.g. the process stopped while the job was still queued or looking
//! the receipt up). With a row, a lookup miss proves nothing (the request may
//! still be inside Portos, or Portos' index table may be incomplete) and the
//! job stays `indeterminate` for a person.
//!
//! NEVER delete this file on a store — like the device journal.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

/// Rows older than this are pruned (the journal's own retention).
pub const PRUNE_AFTER_DAYS: i64 = 400;

pub struct SendLog {
    conn: Mutex<Connection>,
}

impl SendLog {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open Portos send log {}", path.display()))?;
        Self::init(conn)
    }

    /// In-memory send log (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS portos_sent (
                 external_id TEXT PRIMARY KEY,
                 job_id      INTEGER NOT NULL,
                 endpoint    TEXT NOT NULL,
                 sent_at     TEXT NOT NULL
             );",
        )
        .context("create portos_sent table")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record that the register request of `external_id` is about to be sent
    /// (committed before this returns). A second record keeps the first.
    pub fn record(&self, external_id: &str, job_id: i64, endpoint: &str) -> Result<()> {
        self.conn()
            .execute(
                "INSERT OR IGNORE INTO portos_sent (external_id, job_id, endpoint, sent_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![external_id, job_id, endpoint, Utc::now().to_rfc3339()],
            )
            .with_context(|| format!("Portos send log: record {external_id}"))?;
        Ok(())
    }

    /// Whether a register request of `external_id` was ever (about to be) sent.
    pub fn was_sent(&self, external_id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM portos_sent WHERE external_id = ?1",
                params![external_id],
                |_| Ok(()),
            )
            .optional()
            .with_context(|| format!("Portos send log: read {external_id}"))?
            .is_some())
    }

    /// When `external_id` was recorded (RFC 3339), if ever.
    pub fn sent_at(&self, external_id: &str) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT sent_at FROM portos_sent WHERE external_id = ?1",
                params![external_id],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| format!("Portos send log: read {external_id}"))
    }

    /// Drop rows older than [`PRUNE_AFTER_DAYS`].
    pub fn prune(&self) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS)).to_rfc3339();
        Ok(self.conn().execute(
            "DELETE FROM portos_sent WHERE sent_at < ?1",
            params![cutoff],
        )?)
    }

    /// Test helper: backdate a row (prune tests).
    #[cfg(test)]
    fn set_sent_at(&self, external_id: &str, ts: &str) {
        self.conn()
            .execute(
                "UPDATE portos_sent SET sent_at = ?2 WHERE external_id = ?1",
                params![external_id, ts],
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_was_sent_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("portos-sent.db");
        {
            let log = SendLog::open(&path).unwrap();
            assert!(!log.was_sent("dbj-3-a").unwrap());
            assert_eq!(log.sent_at("dbj-3-a").unwrap(), None);
            log.record("dbj-3-a", 7, "http://localhost:3010").unwrap();
            assert!(log.was_sent("dbj-3-a").unwrap());
            assert!(!log.was_sent("dbj-3-b").unwrap());
        }
        let log = SendLog::open(&path).unwrap();
        assert!(log.was_sent("dbj-3-a").unwrap(), "durable");
        let first = log.sent_at("dbj-3-a").unwrap().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(&first).is_ok(),
            "{first}"
        );
        log.set_sent_at("dbj-3-a", "2026-01-01T00:00:00+00:00");
        log.record("dbj-3-a", 8, "http://other:3010").unwrap();
        assert_eq!(
            log.sent_at("dbj-3-a").unwrap().as_deref(),
            Some("2026-01-01T00:00:00+00:00"),
            "a second record keeps the first"
        );
    }

    #[test]
    fn test_prune_drops_only_old_rows() {
        let log = SendLog::open_in_memory().unwrap();
        log.record("old", 1, "e").unwrap();
        log.record("new", 2, "e").unwrap();
        let old = (Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS + 1)).to_rfc3339();
        log.set_sent_at("old", &old);
        assert_eq!(log.prune().unwrap(), 1);
        assert!(!log.was_sent("old").unwrap());
        assert!(log.was_sent("new").unwrap());
        assert_eq!(log.prune().unwrap(), 0);
    }
}
