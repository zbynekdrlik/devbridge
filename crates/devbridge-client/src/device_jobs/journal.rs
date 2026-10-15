//! Durable device-job journal (#99) — `device-journal.db` in the data dir.
//!
//! The money/fiscal guarantee of the device-job channel lives here: a job is
//! written as `received` (committed, `synchronous = FULL`) BEFORE any device
//! contact, and the executor is called only by the one caller whose
//! `received → executing` transition succeeded. A journaled job is therefore
//! executed AT MOST ONCE, across crashes and restarts:
//!
//! ```text
//! received ──start_execution──> executing ──outcome──> done | failed   (final, ack owed)
//!    │                              └──────unknown──> indeterminate ──reconcile──> done | failed
//!    └── refused before any device contact (bad_amount / unsupported) ──> failed
//! restart: received | executing ──> indeterminate (reconciled through the device, never re-run)
//! ```
//!
//! `acked` flips once Odoo accepted the final ack; `report` tracks the
//! `indeterminate` ack of a job whose outcome is still unknown (none → owed →
//! sent). The first final outcome wins — a later contradicting one is
//! refused and logged by the caller. Rows are pruned 400 days after their
//! final ack (Odoo's own retention).
//!
//! NEVER delete this file on a store: a `pending` job missing from it is acked
//! `not_received` ("nothing ran"), so a lost journal could hide a payment the
//! terminal really made.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use super::api::{AckRequest, DeviceJob};
use super::executor::{DeviceKind, Outcome};

/// Acked final rows older than this are pruned (Odoo keeps jobs 400 days).
pub const PRUNE_AFTER_DAYS: i64 = 400;

/// Where a journaled job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Journaled, no device contact yet.
    Received,
    /// Handed to the executor — the device may be running it.
    Executing,
    /// The device ran it (final).
    Done,
    /// Nothing ran (final).
    Failed,
    /// Unknown whether it ran — waiting for reconciliation.
    Indeterminate,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Executing => "executing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Indeterminate => "indeterminate",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "received" => Self::Received,
            "executing" => Self::Executing,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "indeterminate" => Self::Indeterminate,
            _ => return None,
        })
    }

    /// `done` / `failed` — never changes again.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// The `indeterminate` ack of a job whose outcome is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// Not owed (yet): e.g. a restart-recovered row is reconciled first.
    None,
    /// Odoo must be told the outcome is unknown.
    Owed,
    /// Odoo accepted it (the job is `unknown` there until the late ack).
    Sent,
}

impl Report {
    /// The `report` column value.
    pub fn code(self) -> i64 {
        match self {
            Self::None => 0,
            Self::Owed => 1,
            Self::Sent => 2,
        }
    }

    pub fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Owed,
            2 => Self::Sent,
            _ => Self::None,
        }
    }
}

/// One journal row.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub job: DeviceJob,
    pub state: JobState,
    /// The device's answer (`done`).
    pub result: Option<Value>,
    pub error_code: Option<String>,
    /// `failed` text, or the last reason the outcome is unknown.
    pub error_text: Option<String>,
    /// Odoo accepted the final ack.
    pub acked: bool,
    pub ack_note: Option<String>,
    pub report: Report,
    pub reconcile_attempts: i64,
    pub received_at: String,
    pub updated_at: String,
    /// When the executor was about to send the job to its device (#100):
    /// set once, BEFORE the request leaves the PC. `None` after a restart
    /// proves the device was never asked — reconcile may then answer
    /// "nothing ran" when the device has no record either.
    pub device_contact_at: Option<String>,
}

impl JournalEntry {
    /// The device kind the job needs (`None` for a refused unknown type).
    pub fn kind(&self) -> Option<DeviceKind> {
        DeviceKind::for_job_type(&self.job.job_type)
    }

    /// The ack this row owes Odoo right now, if any.
    pub fn owed_ack(&self) -> Option<AckRequest> {
        let job_id = self.job.id;
        let text = || self.error_text.clone().unwrap_or_default();
        match self.state {
            JobState::Done if !self.acked => Some(AckRequest::Done {
                job_id,
                result: self.result.clone().unwrap_or(Value::Null),
            }),
            JobState::Failed if !self.acked => Some(AckRequest::Failed {
                job_id,
                code: self.error_code.clone().unwrap_or_default(),
                text: text(),
            }),
            JobState::Indeterminate if self.report == Report::Owed => {
                Some(AckRequest::Indeterminate {
                    job_id,
                    reason: text(),
                })
            }
            _ => None,
        }
    }
}

/// Result of [`Journal::insert_received`].
#[derive(Debug, Clone, PartialEq)]
pub enum Inserted {
    /// Newly journaled — this caller may hand it to its device.
    New,
    /// Already journaled — NEVER executed again.
    Known(Box<JournalEntry>),
}

const COLUMNS: &str = "job_id, job_type, client_uuid, kasa_id, kasa_name, pos_order, payload, \
     odoo_created_at, odoo_claimed_at, state, result, error_code, error_text, acked, ack_note, \
     report, reconcile_attempts, received_at, updated_at, device_contact_at";

/// States from which an outcome may still be recorded.
const OPEN_STATES: &str = "('received', 'executing', 'indeterminate')";

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn json_column(row: &Row<'_>, idx: usize) -> rusqlite::Result<Value> {
    let text: String = row.get(idx)?;
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<JournalEntry> {
    let state_text: String = row.get(9)?;
    let state = JobState::parse(&state_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            9,
            rusqlite::types::Type::Text,
            format!("unknown journal state '{state_text}'").into(),
        )
    })?;
    let result: Option<String> = row.get(10)?;
    let result = match result {
        Some(text) => Some(serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(JournalEntry {
        job: DeviceJob {
            id: row.get(0)?,
            job_type: row.get(1)?,
            client_uuid: row.get(2)?,
            kasa_id: row.get(3)?,
            kasa_name: row.get(4)?,
            pos_order: json_column(row, 5)?,
            payload: json_column(row, 6)?,
            created_at: row.get(7)?,
            claimed_at: row.get(8)?,
        },
        state,
        result,
        error_code: row.get(11)?,
        error_text: row.get(12)?,
        acked: row.get::<_, i64>(13)? != 0,
        ack_note: row.get(14)?,
        report: Report::from_code(row.get(15)?),
        reconcile_attempts: row.get(16)?,
        received_at: row.get(17)?,
        updated_at: row.get(18)?,
        device_contact_at: row.get(19)?,
    })
}

/// `device_contact_at` of rows that were in flight when a #99 journal was
/// migrated: whether their device was asked is unknown, so they never read
/// as "never contacted".
pub const CONTACT_UNKNOWN_BEFORE_MIGRATION: &str = "unknown: in flight before v0.8.47";

/// Incremental migration (#100): a #99 journal (v0.8.45/46) gets
/// `device_contact_at`; its rows still in flight are marked
/// [`CONTACT_UNKNOWN_BEFORE_MIGRATION`] (one transaction), final rows keep
/// `NULL` (never reconciled again).
fn add_device_contact_column(conn: &Connection) -> Result<()> {
    let has_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('device_jobs') WHERE name = 'device_contact_at'")?
        .exists([])?;
    if !has_column {
        conn.execute_batch(&format!(
            "BEGIN;
             ALTER TABLE device_jobs ADD COLUMN device_contact_at TEXT;
             UPDATE device_jobs SET device_contact_at = '{CONTACT_UNKNOWN_BEFORE_MIGRATION}'
                 WHERE state IN {OPEN_STATES};
             COMMIT;"
        ))
        .context("add device_contact_at to device_jobs")?;
    }
    Ok(())
}

pub struct Journal {
    conn: Mutex<Connection>,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open device journal {}", path.display()))?;
        Self::init(conn)
    }

    /// In-memory journal (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS device_jobs (
                 job_id             INTEGER PRIMARY KEY,
                 job_type           TEXT NOT NULL,
                 client_uuid        TEXT NOT NULL,
                 kasa_id            INTEGER,
                 kasa_name          TEXT NOT NULL,
                 pos_order          TEXT NOT NULL,
                 payload            TEXT NOT NULL,
                 odoo_created_at    TEXT NOT NULL,
                 odoo_claimed_at    TEXT NOT NULL,
                 state              TEXT NOT NULL,
                 result             TEXT,
                 error_code         TEXT,
                 error_text         TEXT,
                 acked              INTEGER NOT NULL DEFAULT 0,
                 ack_note           TEXT,
                 report             INTEGER NOT NULL DEFAULT 0,
                 reconcile_attempts INTEGER NOT NULL DEFAULT 0,
                 received_at        TEXT NOT NULL,
                 updated_at         TEXT NOT NULL,
                 device_contact_at  TEXT
             );",
        )
        .context("create device_jobs table")?;
        add_device_contact_column(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Journal a claimed job as `received` (committed before this returns).
    /// A job id already present is NOT touched and comes back as
    /// [`Inserted::Known`] — the caller must never execute it again.
    pub fn insert_received(&self, job: &DeviceJob) -> Result<Inserted> {
        self.insert(job, None)
    }

    /// Journal a claimed job that is refused before any device contact
    /// (`bad_amount`, `unsupported`) directly as `failed`, ack owed — in ONE
    /// write, so a crash can never leave it looking in flight.
    pub fn insert_refused(&self, job: &DeviceJob, code: &str, text: &str) -> Result<Inserted> {
        self.insert(job, Some((code, text)))
    }

    fn insert(&self, job: &DeviceJob, refused: Option<(&str, &str)>) -> Result<Inserted> {
        let ts = now();
        let (state, code, text) = match refused {
            Some((code, text)) => ("failed", Some(code), Some(text)),
            None => ("received", None, None),
        };
        let changed = self
            .conn()
            .execute(
                "INSERT OR IGNORE INTO device_jobs (job_id, job_type, client_uuid, kasa_id, kasa_name,
                     pos_order, payload, odoo_created_at, odoo_claimed_at, state, error_code,
                     error_text, received_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
                params![
                    job.id,
                    job.job_type,
                    job.client_uuid,
                    job.kasa_id,
                    job.kasa_name,
                    job.pos_order.to_string(),
                    job.payload.to_string(),
                    job.created_at,
                    job.claimed_at,
                    state,
                    code,
                    text,
                    ts
                ],
            )
            .with_context(|| format!("journal: insert job {}", job.id))?;
        if changed == 1 {
            return Ok(Inserted::New);
        }
        let known = self
            .get(job.id)?
            .with_context(|| format!("journal: job {} neither inserted nor present", job.id))?;
        Ok(Inserted::Known(Box::new(known)))
    }

    pub fn get(&self, job_id: i64) -> Result<Option<JournalEntry>> {
        self.conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM device_jobs WHERE job_id = ?1"),
                params![job_id],
                row_to_entry,
            )
            .optional()
            .with_context(|| format!("journal: read job {job_id}"))
    }

    /// `received → executing`, committed. `true` = the caller now owns the
    /// one and only execution of this job; `false` = it must not execute it.
    pub fn start_execution(&self, job_id: i64) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE device_jobs SET state = 'executing', updated_at = ?2
                 WHERE job_id = ?1 AND state = 'received'",
                params![job_id, now()],
            )
            .with_context(|| format!("journal: start execution of job {job_id}"))?;
        Ok(changed == 1)
    }

    /// Store an outcome of a job that is not final yet. `Done` / `Failed`
    /// make it final (ack owed); `Indeterminate` keeps it open and owes the
    /// `indeterminate` ack (unless already sent). `false` = the job already
    /// had a final outcome, which is kept (the first one wins).
    pub fn record_outcome(&self, job_id: i64, outcome: &Outcome) -> Result<bool> {
        let ts = now();
        let conn = self.conn();
        let changed = match outcome {
            Outcome::Done(result) => conn.execute(
                &format!(
                    "UPDATE device_jobs SET state = 'done', result = ?2, error_code = NULL,
                         error_text = NULL, acked = 0, ack_note = NULL, updated_at = ?3
                     WHERE job_id = ?1 AND state IN {OPEN_STATES}"
                ),
                params![job_id, result.to_string(), ts],
            ),
            Outcome::Failed { code, text } => conn.execute(
                &format!(
                    "UPDATE device_jobs SET state = 'failed', result = NULL, error_code = ?2,
                         error_text = ?3, acked = 0, ack_note = NULL, updated_at = ?4
                     WHERE job_id = ?1 AND state IN {OPEN_STATES}"
                ),
                params![job_id, code, text, ts],
            ),
            Outcome::Indeterminate(reason) => conn.execute(
                &format!(
                    "UPDATE device_jobs SET state = 'indeterminate', error_text = ?2,
                         report = CASE WHEN report = 2 THEN 2 ELSE 1 END, updated_at = ?3
                     WHERE job_id = ?1 AND state IN {OPEN_STATES}"
                ),
                params![job_id, reason, ts],
            ),
        }
        .with_context(|| format!("journal: record {} for job {job_id}", outcome.label()))?;
        Ok(changed == 1)
    }

    /// Refuse a job that is still `received` (never handed to its device) as
    /// `failed`, ack owed — e.g. it waited too long in its device queue.
    /// `false` = it is no longer `received` (left untouched).
    pub fn fail_unstarted(&self, job_id: i64, code: &str, text: &str) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE device_jobs SET state = 'failed', error_code = ?2, error_text = ?3,
                     acked = 0, ack_note = NULL, updated_at = ?4
                 WHERE job_id = ?1 AND state = 'received'",
                params![job_id, code, text, now()],
            )
            .with_context(|| format!("journal: fail unstarted job {job_id}"))?;
        Ok(changed == 1)
    }

    /// The executor is about to send `job_id` to its device (committed before
    /// this returns; the first time is kept). `false` = no such job, or
    /// already marked.
    pub fn mark_device_contact(&self, job_id: i64) -> Result<bool> {
        let ts = now();
        let changed = self
            .conn()
            .execute(
                "UPDATE device_jobs SET device_contact_at = ?2, updated_at = ?2
                 WHERE job_id = ?1 AND device_contact_at IS NULL",
                params![job_id, ts],
            )
            .with_context(|| format!("journal: mark device contact of job {job_id}"))?;
        Ok(changed == 1)
    }

    /// A reconcile attempt of an `indeterminate` job could not tell: count
    /// it, keep the reason and owe the `indeterminate` ack (once).
    pub fn record_still_unknown(&self, job_id: i64, reason: &str) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE device_jobs SET reconcile_attempts = reconcile_attempts + 1, error_text = ?2,
                     report = CASE WHEN report = 2 THEN 2 ELSE 1 END, updated_at = ?3
                 WHERE job_id = ?1 AND state = 'indeterminate'",
                params![job_id, reason, now()],
            )
            .with_context(|| format!("journal: record still-unknown for job {job_id}"))?;
        Ok(changed == 1)
    }

    /// Startup recovery: every `received` / `executing` row belongs to a
    /// previous process that stopped before the device's outcome was stored.
    /// It becomes `indeterminate` (to be reconciled through its device —
    /// NEVER executed again). Returns `(job_id, previous state)`.
    pub fn recover_after_restart(&self) -> Result<Vec<(i64, JobState)>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT job_id, state FROM device_jobs
                 WHERE state IN ('received', 'executing') ORDER BY job_id",
            )?;
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "UPDATE device_jobs SET
                 error_text = 'devbridge restarted while the job was ' || state,
                 state = 'indeterminate', updated_at = ?1
             WHERE state IN ('received', 'executing')",
            params![now()],
        )?;
        tx.commit().context("journal: commit restart recovery")?;
        Ok(rows
            .into_iter()
            .map(|(id, s)| (id, JobState::parse(&s).unwrap_or(JobState::Executing)))
            .collect())
    }

    /// Every ack Odoo is owed: final outcomes not accepted yet and
    /// `indeterminate` reports, oldest job first.
    pub fn owed_acks(&self) -> Result<Vec<AckRequest>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM device_jobs
             WHERE (state IN ('done', 'failed') AND acked = 0)
                OR (state = 'indeterminate' AND report = 1)
             ORDER BY job_id"
        ))?;
        let rows = stmt
            .query_map([], row_to_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.iter().filter_map(JournalEntry::owed_ack).collect())
    }

    /// Odoo answered the final ack (`note`: `ok`, `duplicate` or `refused: …`).
    pub fn mark_acked(&self, job_id: i64, note: &str) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE device_jobs SET acked = 1, ack_note = ?2, updated_at = ?3
                 WHERE job_id = ?1 AND state IN ('done', 'failed')",
                params![job_id, note, now()],
            )
            .with_context(|| format!("journal: mark job {job_id} acked"))?;
        Ok(changed == 1)
    }

    /// Odoo accepted the `indeterminate` report.
    pub fn mark_reported(&self, job_id: i64) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE device_jobs SET report = 2, updated_at = ?2
                 WHERE job_id = ?1 AND state = 'indeterminate'",
                params![job_id, now()],
            )
            .with_context(|| format!("journal: mark job {job_id} reported"))?;
        Ok(changed == 1)
    }

    /// Odoo still lists a job whose final ack we think it accepted — owe the
    /// ack again (a retry is safe: the first final ack wins, `duplicate`).
    pub fn reopen_ack(&self, job_id: i64) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE device_jobs SET acked = 0, updated_at = ?2
                 WHERE job_id = ?1 AND state IN ('done', 'failed') AND acked = 1",
                params![job_id, now()],
            )
            .with_context(|| format!("journal: reopen ack of job {job_id}"))?;
        Ok(changed == 1)
    }

    /// Rows waiting for reconciliation, oldest first.
    pub fn indeterminate(&self) -> Result<Vec<JournalEntry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM device_jobs WHERE state = 'indeterminate' ORDER BY job_id"
        ))?;
        let rows = stmt
            .query_map([], row_to_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop final, acked rows older than [`PRUNE_AFTER_DAYS`]. A row whose
    /// ack Odoo REFUSED is kept: Odoo may still list that job in `pending`,
    /// and without its row it would be acked `not_received` ("nothing ran").
    pub fn prune(&self) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS)).to_rfc3339();
        let n = self.conn().execute(
            "DELETE FROM device_jobs
             WHERE acked = 1 AND state IN ('done', 'failed') AND updated_at < ?1
               AND (ack_note IS NULL OR ack_note NOT LIKE 'refused:%')",
            params![cutoff],
        )?;
        Ok(n)
    }

    /// Test helper: backdate a row (prune tests).
    #[cfg(test)]
    fn set_updated_at(&self, job_id: i64, ts: &str) {
        self.conn()
            .execute(
                "UPDATE device_jobs SET updated_at = ?2 WHERE job_id = ?1",
                params![job_id, ts],
            )
            .unwrap();
    }
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
