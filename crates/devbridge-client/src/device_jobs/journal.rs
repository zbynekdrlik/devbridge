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
     report, reconcile_attempts, received_at, updated_at";

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
    })
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
                 updated_at         TEXT NOT NULL
             );",
        )
        .context("create device_jobs table")?;
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
mod tests {
    use super::*;
    use serde_json::json;

    fn job(id: i64, job_type: &str) -> DeviceJob {
        DeviceJob {
            id,
            job_type: job_type.into(),
            client_uuid: format!("uuid-{id}"),
            kasa_id: Some(3),
            kasa_name: "Pokladňa Bastion 1".into(),
            pos_order: json!({"id": 812, "name": "Bastion/0042"}),
            payload: json!({"amount": 12.40, "currency": "EUR"}),
            created_at: "2026-10-14 09:12:05".into(),
            claimed_at: "2026-10-14 09:12:06".into(),
        }
    }

    #[test]
    fn test_fail_unstarted_only_touches_received_jobs() {
        let j = Journal::open_in_memory().unwrap();
        j.insert_received(&job(1, "fiscal.cash_register")).unwrap();
        assert!(
            j.fail_unstarted(1, "expired_local", "waited too long - nothing ran")
                .unwrap()
        );
        let e = j.get(1).unwrap().unwrap();
        assert_eq!(e.state, JobState::Failed);
        assert_eq!(e.error_code.as_deref(), Some("expired_local"));
        assert_eq!(
            e.error_text.as_deref(),
            Some("waited too long - nothing ran")
        );
        assert!(!e.acked);
        assert!(chrono::DateTime::parse_from_rfc3339(&e.updated_at).is_ok());
        assert_eq!(
            e.owed_ack(),
            Some(AckRequest::Failed {
                job_id: 1,
                code: "expired_local".into(),
                text: "waited too long - nothing ran".into()
            })
        );
        assert!(
            !j.fail_unstarted(1, "again", "x").unwrap(),
            "already failed"
        );
        assert_eq!(
            j.get(1).unwrap().unwrap().error_code.as_deref(),
            Some("expired_local")
        );

        // a job its worker already started is never failed this way
        j.insert_received(&job(2, "fiscal.cash_register")).unwrap();
        assert!(j.start_execution(2).unwrap());
        assert!(!j.fail_unstarted(2, "expired_local", "x").unwrap());
        assert_eq!(j.get(2).unwrap().unwrap().state, JobState::Executing);
        assert!(
            !j.fail_unstarted(99, "expired_local", "x").unwrap(),
            "unknown job"
        );
    }

    #[test]
    fn test_insert_received_round_trips_and_known_is_never_new() {
        let j = Journal::open_in_memory().unwrap();
        let original = job(4711, "card.purchase");
        assert_eq!(j.insert_received(&original).unwrap(), Inserted::New);
        let e = j.get(4711).unwrap().unwrap();
        assert_eq!(e.job, original);
        assert_eq!(e.state, JobState::Received);
        assert_eq!(
            (e.acked, e.report, e.reconcile_attempts),
            (false, Report::None, 0)
        );
        assert_eq!(e.kind(), Some(DeviceKind::Card));
        assert!(e.result.is_none() && e.error_code.is_none());

        // Odoo handing the same job out again: kept as it is.
        j.start_execution(4711).unwrap();
        let mut again = original.clone();
        again.payload = json!({"amount": 99});
        match j.insert_received(&again).unwrap() {
            Inserted::Known(k) => {
                assert_eq!(k.state, JobState::Executing);
                assert_eq!(k.job.payload, original.payload);
            }
            other => panic!("{other:?}"),
        }
        assert!(j.get(1).unwrap().is_none());
    }

    #[test]
    fn test_start_execution_happens_only_once_and_only_from_received() {
        let j = Journal::open_in_memory().unwrap();
        j.insert_received(&job(1, "fiscal.cash_register")).unwrap();
        assert!(j.start_execution(1).unwrap());
        assert!(
            !j.start_execution(1).unwrap(),
            "second start must be refused"
        );
        assert!(!j.start_execution(2).unwrap(), "unknown job");
        j.insert_received(&job(3, "fiscal.cash_register")).unwrap();
        j.record_outcome(3, &Outcome::failed("bad_amount", "x"))
            .unwrap();
        assert!(!j.start_execution(3).unwrap(), "a refused job never starts");
        assert_eq!(j.get(1).unwrap().unwrap().state, JobState::Executing);
    }

    #[test]
    fn test_outcomes_and_owed_acks() {
        let j = Journal::open_in_memory().unwrap();
        for id in 1..=4 {
            j.insert_received(&job(id, "fiscal.cash_register")).unwrap();
            j.start_execution(id).unwrap();
        }
        assert!(
            j.record_outcome(1, &Outcome::Done(json!({"uid": "O-1"})))
                .unwrap()
        );
        assert!(
            j.record_outcome(2, &Outcome::failed("declined", "zamietnuté"))
                .unwrap()
        );
        assert!(
            j.record_outcome(3, &Outcome::Indeterminate("timeout".into()))
                .unwrap()
        );
        // 4 stays executing: nothing owed for it
        assert_eq!(
            j.owed_acks().unwrap(),
            vec![
                AckRequest::Done {
                    job_id: 1,
                    result: json!({"uid": "O-1"})
                },
                AckRequest::Failed {
                    job_id: 2,
                    code: "declined".into(),
                    text: "zamietnuté".into()
                },
                AckRequest::Indeterminate {
                    job_id: 3,
                    reason: "timeout".into()
                },
            ]
        );
        assert!(j.mark_acked(1, "ok").unwrap());
        assert!(j.mark_acked(2, "duplicate").unwrap());
        assert!(j.mark_reported(3).unwrap());
        assert!(j.owed_acks().unwrap().is_empty());
        let e1 = j.get(1).unwrap().unwrap();
        assert!(e1.acked);
        assert_eq!(e1.ack_note.as_deref(), Some("ok"));
        assert_eq!(e1.result, Some(json!({"uid": "O-1"})));
        assert_eq!(j.get(3).unwrap().unwrap().report, Report::Sent);
        // mark_acked / mark_reported only touch rows in the matching state
        assert!(!j.mark_acked(3, "ok").unwrap());
        assert!(!j.mark_reported(1).unwrap());
        assert!(!j.mark_acked(4, "ok").unwrap());
    }

    /// A row in a given state, read back from the journal (owed_ack is
    /// checked on the entry itself, not through the owed_acks SQL filter).
    fn entry_in(j: &Journal, id: i64, outcome: Option<Outcome>) -> JournalEntry {
        j.insert_received(&job(id, "card.purchase")).unwrap();
        j.start_execution(id).unwrap();
        if let Some(o) = outcome {
            j.record_outcome(id, &o).unwrap();
        }
        j.get(id).unwrap().unwrap()
    }

    #[test]
    fn test_owed_ack_guards_on_the_entry_itself() {
        let j = Journal::open_in_memory().unwrap();
        // done: owed until acked, never after
        let done = entry_in(&j, 1, Some(Outcome::Done(json!({"auth": "A1"}))));
        assert_eq!(
            done.owed_ack(),
            Some(AckRequest::Done {
                job_id: 1,
                result: json!({"auth": "A1"})
            })
        );
        let acked_done = JournalEntry {
            acked: true,
            ..done.clone()
        };
        assert_eq!(
            acked_done.owed_ack(),
            None,
            "an accepted done ack is not owed again"
        );
        // failed: the same
        let failed = entry_in(&j, 2, Some(Outcome::failed("declined", "zamietnutá")));
        assert_eq!(
            failed.owed_ack(),
            Some(AckRequest::Failed {
                job_id: 2,
                code: "declined".into(),
                text: "zamietnutá".into()
            })
        );
        let acked_failed = JournalEntry {
            acked: true,
            ..failed
        };
        assert_eq!(
            acked_failed.owed_ack(),
            None,
            "an accepted failed ack is not owed again"
        );
        // indeterminate: owed ONLY while the report is owed
        let unknown = entry_in(&j, 3, Some(Outcome::Indeterminate("no answer".into())));
        assert_eq!(unknown.report, Report::Owed);
        assert_eq!(
            unknown.owed_ack(),
            Some(AckRequest::Indeterminate {
                job_id: 3,
                reason: "no answer".into()
            })
        );
        for report in [Report::None, Report::Sent] {
            let quiet = JournalEntry {
                report,
                ..unknown.clone()
            };
            assert_eq!(quiet.owed_ack(), None, "{report:?}");
        }
        // in flight: nothing owed, whatever the flags say
        let running = entry_in(&j, 4, None);
        assert_eq!(running.state, JobState::Executing);
        for (acked, report) in [
            (false, Report::Owed),
            (true, Report::Owed),
            (false, Report::None),
        ] {
            let e = JournalEntry {
                acked,
                report,
                ..running.clone()
            };
            assert_eq!(e.owed_ack(), None);
        }
        // a received row never owes an ack
        let odd = JournalEntry {
            state: JobState::Received,
            ..done
        };
        assert_eq!(odd.owed_ack(), None);
    }

    #[test]
    fn test_timestamps_are_rfc3339_now_and_move_with_each_transition() {
        let j = Journal::open_in_memory().unwrap();
        let before = Utc::now() - chrono::Duration::seconds(5);
        j.insert_received(&job(9, "fiscal.cash_register")).unwrap();
        let e = j.get(9).unwrap().unwrap();
        let received =
            chrono::DateTime::parse_from_rfc3339(&e.received_at).expect("received_at is RFC 3339");
        let updated =
            chrono::DateTime::parse_from_rfc3339(&e.updated_at).expect("updated_at is RFC 3339");
        assert!(received >= before && received <= Utc::now(), "{received}");
        assert_eq!(received, updated);
        std::thread::sleep(std::time::Duration::from_millis(5));
        j.start_execution(9).unwrap();
        let moved = j.get(9).unwrap().unwrap();
        let moved_at = chrono::DateTime::parse_from_rfc3339(&moved.updated_at).unwrap();
        assert!(moved_at > updated, "updated_at follows the transition");
        assert_eq!(moved.received_at, e.received_at, "received_at stays");
        // the test device log and the prune cutoff compare these as text:
        // RFC 3339 UTC strings sort like the instants they name
        assert!(e.updated_at < moved.updated_at);
    }

    #[test]
    fn test_first_final_outcome_wins() {
        let j = Journal::open_in_memory().unwrap();
        j.insert_received(&job(1, "card.purchase")).unwrap();
        j.start_execution(1).unwrap();
        j.record_outcome(1, &Outcome::Done(json!({"auth": "A1"})))
            .unwrap();
        assert!(!j.record_outcome(1, &Outcome::failed("x", "y")).unwrap());
        assert!(
            !j.record_outcome(1, &Outcome::Indeterminate("late".into()))
                .unwrap()
        );
        assert!(!j.record_outcome(1, &Outcome::Done(json!({}))).unwrap());
        assert!(!j.record_still_unknown(1, "late").unwrap());
        let e = j.get(1).unwrap().unwrap();
        assert_eq!(e.state, JobState::Done);
        assert_eq!(e.result, Some(json!({"auth": "A1"})));
        assert!(!j.record_outcome(99, &Outcome::Done(json!({}))).unwrap());
    }

    #[test]
    fn test_indeterminate_resolves_late_and_report_is_owed_once() {
        let j = Journal::open_in_memory().unwrap();
        j.insert_received(&job(1, "card.purchase")).unwrap();
        j.start_execution(1).unwrap();
        j.record_outcome(1, &Outcome::Indeterminate("no answer".into()))
            .unwrap();
        assert_eq!(j.get(1).unwrap().unwrap().report, Report::Owed);
        j.mark_reported(1).unwrap();
        // a still-unknown reconcile keeps the sent report, counts the attempt
        assert!(j.record_still_unknown(1, "terminal offline").unwrap());
        let e = j.get(1).unwrap().unwrap();
        assert_eq!(e.report, Report::Sent);
        assert_eq!(e.reconcile_attempts, 1);
        assert_eq!(e.error_text.as_deref(), Some("terminal offline"));
        assert!(j.owed_acks().unwrap().is_empty());
        // another indeterminate outcome does not re-owe a sent report
        j.record_outcome(1, &Outcome::Indeterminate("again".into()))
            .unwrap();
        assert_eq!(j.get(1).unwrap().unwrap().report, Report::Sent);
        assert_eq!(j.indeterminate().unwrap().len(), 1);
        // the late outcome
        assert!(
            j.record_outcome(1, &Outcome::Done(json!({"auth": "A9"})))
                .unwrap()
        );
        assert_eq!(
            j.owed_acks().unwrap(),
            vec![AckRequest::Done {
                job_id: 1,
                result: json!({"auth": "A9"})
            }]
        );
        assert!(j.indeterminate().unwrap().is_empty());
        assert!(!j.record_still_unknown(2, "x").unwrap());
    }

    #[test]
    fn test_restart_recovery_never_reexecutes_and_reconciles_first() {
        let j = Journal::open_in_memory().unwrap();
        for id in 1..=5 {
            j.insert_received(&job(id, "fiscal.cash_register")).unwrap();
        }
        j.start_execution(2).unwrap();
        j.start_execution(3).unwrap();
        j.record_outcome(3, &Outcome::Done(json!({}))).unwrap();
        j.start_execution(4).unwrap();
        j.record_outcome(4, &Outcome::Indeterminate("x".into()))
            .unwrap();
        j.record_outcome(5, &Outcome::failed("unsupported", "no executor"))
            .unwrap();

        let recovered = j.recover_after_restart().unwrap();
        assert_eq!(
            recovered,
            vec![(1, JobState::Received), (2, JobState::Executing)]
        );
        for id in [1, 2] {
            let e = j.get(id).unwrap().unwrap();
            assert_eq!(e.state, JobState::Indeterminate);
            // reconciled through the device FIRST: no indeterminate ack yet
            assert_eq!(e.report, Report::None);
            assert!(!j.start_execution(id).unwrap(), "never executed again");
        }
        assert_eq!(
            j.get(1).unwrap().unwrap().error_text.as_deref(),
            Some("devbridge restarted while the job was received")
        );
        assert_eq!(
            j.get(2).unwrap().unwrap().error_text.as_deref(),
            Some("devbridge restarted while the job was executing")
        );
        assert_eq!(j.get(3).unwrap().unwrap().state, JobState::Done);
        assert_eq!(j.get(5).unwrap().unwrap().state, JobState::Failed);
        assert_eq!(j.indeterminate().unwrap().len(), 3);
        assert!(j.recover_after_restart().unwrap().is_empty());
    }

    #[test]
    fn test_reopen_ack_only_for_acked_final_rows() {
        let j = Journal::open_in_memory().unwrap();
        j.insert_received(&job(1, "card.purchase")).unwrap();
        assert!(!j.reopen_ack(1).unwrap());
        j.record_outcome(1, &Outcome::failed("bad_amount", "x"))
            .unwrap();
        assert!(!j.reopen_ack(1).unwrap(), "not acked yet");
        j.mark_acked(1, "ok").unwrap();
        assert!(j.reopen_ack(1).unwrap());
        assert_eq!(j.owed_acks().unwrap().len(), 1);
        assert!(!j.get(1).unwrap().unwrap().acked);
    }

    #[test]
    fn test_insert_refused_is_final_in_one_write_and_never_starts() {
        let j = Journal::open_in_memory().unwrap();
        let refused = job(5, "card.purchase");
        assert_eq!(
            j.insert_refused(&refused, "bad_amount", "nothing ran")
                .unwrap(),
            Inserted::New
        );
        let e = j.get(5).unwrap().unwrap();
        assert_eq!(e.state, JobState::Failed);
        assert_eq!(e.error_code.as_deref(), Some("bad_amount"));
        assert_eq!(e.error_text.as_deref(), Some("nothing ran"));
        assert!(!e.acked);
        assert!(!j.start_execution(5).unwrap());
        assert!(
            j.recover_after_restart().unwrap().is_empty(),
            "not in flight"
        );
        assert_eq!(
            j.owed_acks().unwrap(),
            vec![AckRequest::Failed {
                job_id: 5,
                code: "bad_amount".into(),
                text: "nothing ran".into()
            }]
        );
        // an id already journaled stays as it is
        j.insert_received(&job(6, "fiscal.cash_register")).unwrap();
        assert!(matches!(
            j.insert_refused(&job(6, "fiscal.cash_register"), "unsupported", "x")
                .unwrap(),
            Inserted::Known(_)
        ));
        assert_eq!(j.get(6).unwrap().unwrap().state, JobState::Received);
        let fresh = j.get(6).unwrap().unwrap();
        assert!(fresh.error_code.is_none() && fresh.error_text.is_none());
    }

    #[test]
    fn test_prune_drops_only_old_acked_final_rows() {
        let j = Journal::open_in_memory().unwrap();
        let old = (Utc::now() - chrono::Duration::days(PRUNE_AFTER_DAYS + 1)).to_rfc3339();
        for id in 1..=4 {
            j.insert_received(&job(id, "fiscal.cash_register")).unwrap();
        }
        j.record_outcome(1, &Outcome::Done(json!({}))).unwrap();
        j.mark_acked(1, "ok").unwrap();
        j.record_outcome(2, &Outcome::Done(json!({}))).unwrap(); // ack still owed
        j.record_outcome(3, &Outcome::Indeterminate("x".into()))
            .unwrap();
        j.record_outcome(4, &Outcome::Done(json!({}))).unwrap();
        j.mark_acked(4, "ok").unwrap();
        j.insert_received(&job(5, "fiscal.cash_register")).unwrap();
        j.record_outcome(5, &Outcome::Done(json!({}))).unwrap();
        j.mark_acked(5, "refused: Odoo refused (not_found): x")
            .unwrap();
        for id in [1, 2, 3, 5] {
            j.set_updated_at(id, &old);
        }
        assert_eq!(j.prune().unwrap(), 1);
        assert!(j.get(1).unwrap().is_none());
        for id in 2..=5 {
            assert!(j.get(id).unwrap().is_some(), "{id} must stay");
        }
    }

    #[test]
    fn test_journal_survives_reopen_wal_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device-journal.db");
        {
            let j = Journal::open(&path).unwrap();
            j.insert_received(&job(7, "card.purchase")).unwrap();
            j.start_execution(7).unwrap();
            let conn = j.conn();
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            let sync: i64 = conn
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sync, 2, "synchronous = FULL");
        }
        let j = Journal::open(&path).unwrap();
        assert_eq!(j.get(7).unwrap().unwrap().state, JobState::Executing);
        assert_eq!(
            j.recover_after_restart().unwrap(),
            vec![(7, JobState::Executing)]
        );
    }

    #[test]
    fn test_state_and_report_codes() {
        for s in [
            JobState::Received,
            JobState::Executing,
            JobState::Done,
            JobState::Failed,
            JobState::Indeterminate,
        ] {
            assert_eq!(JobState::parse(s.as_str()), Some(s));
        }
        assert_eq!(JobState::parse("queued"), None);
        assert!(JobState::Done.is_final() && JobState::Failed.is_final());
        assert!(!JobState::Indeterminate.is_final() && !JobState::Executing.is_final());
        for r in [Report::None, Report::Owed, Report::Sent] {
            assert_eq!(Report::from_code(r.code()), r);
        }
        assert_eq!(Report::from_code(7), Report::None);
    }

    #[test]
    fn test_corrupt_row_is_an_error_not_a_guess() {
        let j = Journal::open_in_memory().unwrap();
        j.insert_received(&job(1, "card.purchase")).unwrap();
        j.conn()
            .execute(
                "UPDATE device_jobs SET state = 'weird' WHERE job_id = 1",
                [],
            )
            .unwrap();
        assert!(j.get(1).is_err());
        j.conn()
            .execute(
                "UPDATE device_jobs SET state = 'done', result = '{oops' WHERE job_id = 1",
                [],
            )
            .unwrap();
        assert!(j.get(1).is_err());
        j.conn()
            .execute(
                "UPDATE device_jobs SET result = NULL, payload = 'not json' WHERE job_id = 1",
                [],
            )
            .unwrap();
        assert!(j.get(1).is_err());
    }
}
