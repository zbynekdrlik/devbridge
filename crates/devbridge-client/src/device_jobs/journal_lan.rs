//! The `lan_jobs` table of the device journal (#101): jobs a till sent to the
//! store's LAN API — Odoo did not create them, so they have no Odoo job id.
//!
//! They share `device_jobs`' state machine: the state-machine methods of
//! [`Journal`] route a NEGATIVE device-job id here ([`super::lan_job_id`]).
//! What is LAN-only lives in this file: the insert keyed by
//! `(kasa_id, client_uuid)` (UNIQUE — a replay never creates a second row),
//! the look-ups a replay / poll needs, and the `report_offline` bookkeeping
//! (`reported`, `report_note`, `odoo_job_id`) that replaces the acks.
//!
//! `updated_at` of a final row is frozen — nothing writes it after the
//! outcome (marking the row reported does not) — so it is the report's
//! `executed_at`.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use super::{JobState, Journal, JournalEntry, Report, json_column, lan_job_id, now};
use crate::device_jobs::api::DeviceJob;

const LAN_COLUMNS: &str = "id, kasa_id, client_uuid, job_type, pos_order, payload, state, \
     result, error_code, error_text, reported, report_note, odoo_job_id, reconcile_attempts, \
     received_at, updated_at, device_contact_at";

/// `lan_jobs` — created with the journal (an older journal just gains it).
pub(super) fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lan_jobs (
             id                 INTEGER PRIMARY KEY AUTOINCREMENT,
             kasa_id            INTEGER NOT NULL,
             client_uuid        TEXT NOT NULL,
             job_type           TEXT NOT NULL,
             pos_order          TEXT NOT NULL,
             payload            TEXT NOT NULL,
             state              TEXT NOT NULL,
             result             TEXT,
             error_code         TEXT,
             error_text         TEXT,
             reported           INTEGER NOT NULL DEFAULT 0,
             report_note        TEXT,
             odoo_job_id        INTEGER,
             reconcile_attempts INTEGER NOT NULL DEFAULT 0,
             received_at        TEXT NOT NULL,
             updated_at         TEXT NOT NULL,
             device_contact_at  TEXT,
             UNIQUE (kasa_id, client_uuid)
         );",
    )
    .context("create lan_jobs table")
}

/// One `lan_jobs` row: the shared [`JournalEntry`] — its `job.id` is the
/// negative device-job id, `acked` / `ack_note` mirror `reported` /
/// `report_note` — plus the report bookkeeping.
#[derive(Debug, Clone, PartialEq)]
pub struct LanJob {
    pub entry: JournalEntry,
    /// Odoo answered the `report_offline` (accepted it, or refused it for
    /// good).
    pub reported: bool,
    /// `created`, `existing` or `refused: …`.
    pub report_note: Option<String>,
    /// Odoo's job id for it, once reported.
    pub odoo_job_id: Option<i64>,
}

impl LanJob {
    /// The (negative) device-job id.
    pub fn job_id(&self) -> i64 {
        self.entry.job.id
    }

    pub fn state(&self) -> JobState {
        self.entry.state
    }

    /// Final and not reported yet — the reporter owes Odoo this job. An
    /// `indeterminate` job is reported only once it is resolved.
    pub fn report_owed(&self) -> bool {
        self.entry.state.is_final() && !self.reported
    }
}

/// Result of [`Journal::insert_lan_received`].
#[derive(Debug, Clone, PartialEq)]
pub enum LanInserted {
    /// Newly journaled under this (negative) device-job id — the caller may
    /// hand it to its device.
    New(i64),
    /// `(kasa_id, client_uuid)` is already journaled — NEVER executed again.
    Known(Box<LanJob>),
}

fn optional_json(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Value>> {
    let text: Option<String> = row.get(idx)?;
    text.map(|t| {
        serde_json::from_str(&t).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

fn row_to_lan(row: &Row<'_>) -> rusqlite::Result<LanJob> {
    let state_text: String = row.get(6)?;
    let state = JobState::parse(&state_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            format!("unknown lan job state '{state_text}'").into(),
        )
    })?;
    let reported = row.get::<_, i64>(10)? != 0;
    let report_note: Option<String> = row.get(11)?;
    let received_at: String = row.get(14)?;
    Ok(LanJob {
        entry: JournalEntry {
            job: DeviceJob {
                id: lan_job_id(row.get(0)?),
                job_type: row.get(3)?,
                client_uuid: row.get(2)?,
                kasa_id: Some(row.get(1)?),
                kasa_name: String::new(),
                pos_order: json_column(row, 4)?,
                payload: json_column(row, 5)?,
                created_at: received_at.clone(),
                claimed_at: String::new(),
            },
            state,
            result: optional_json(row, 7)?,
            error_code: row.get(8)?,
            error_text: row.get(9)?,
            acked: reported,
            ack_note: report_note.clone(),
            report: Report::None,
            reconcile_attempts: row.get(13)?,
            received_at,
            updated_at: row.get(15)?,
            device_contact_at: row.get(16)?,
        },
        reported,
        report_note,
        odoo_job_id: row.get(12)?,
    })
}

impl Journal {
    /// Journal a LAN API job as `received` (committed before this returns).
    /// Its `(kasa_id, client_uuid)` already journaled → [`LanInserted::Known`]
    /// with the stored row, untouched — the caller must never execute it
    /// again (it compares the operation: replay or conflict).
    pub fn insert_lan_received(&self, job: &DeviceJob) -> Result<LanInserted> {
        let kasa_id = job
            .kasa_id
            .context("journal: a LAN API job must name its till")?;
        let inserted = {
            let conn = self.conn();
            let changed = conn
                .execute(
                    "INSERT OR IGNORE INTO lan_jobs (kasa_id, client_uuid, job_type, pos_order,
                         payload, state, received_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'received', ?6, ?6)",
                    params![
                        kasa_id,
                        job.client_uuid,
                        job.job_type,
                        job.pos_order.to_string(),
                        job.payload.to_string(),
                        now()
                    ],
                )
                .with_context(|| {
                    format!(
                        "journal: insert LAN job (till {kasa_id}, client_uuid {})",
                        job.client_uuid
                    )
                })?;
            (changed == 1).then(|| conn.last_insert_rowid())
        };
        if let Some(row_id) = inserted {
            return Ok(LanInserted::New(lan_job_id(row_id)));
        }
        let known = self.lan_job(kasa_id, &job.client_uuid)?.with_context(|| {
            format!(
                "journal: LAN job (till {kasa_id}, client_uuid {}) neither inserted nor present",
                job.client_uuid
            )
        })?;
        Ok(LanInserted::Known(Box::new(known)))
    }

    /// The LAN job of till `kasa_id` with `client_uuid`.
    pub fn lan_job(&self, kasa_id: i64, client_uuid: &str) -> Result<Option<LanJob>> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {LAN_COLUMNS} FROM lan_jobs WHERE kasa_id = ?1 AND client_uuid = ?2"
                ),
                params![kasa_id, client_uuid],
                row_to_lan,
            )
            .optional()
            .with_context(|| format!("journal: read LAN job (till {kasa_id}, {client_uuid})"))
    }

    /// The LAN job of the (negative) device-job id `job_id`.
    pub fn lan_job_by_id(&self, job_id: i64) -> Result<Option<LanJob>> {
        self.conn()
            .query_row(
                &format!("SELECT {LAN_COLUMNS} FROM lan_jobs WHERE id = ?1"),
                params![lan_job_id(job_id)],
                row_to_lan,
            )
            .optional()
            .with_context(|| format!("journal: read LAN job {job_id}"))
    }

    /// LAN jobs waiting for reconciliation, oldest first.
    pub(super) fn lan_indeterminate(&self) -> Result<Vec<JournalEntry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {LAN_COLUMNS} FROM lan_jobs WHERE state = 'indeterminate' ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([], row_to_lan)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|lan| lan.entry).collect())
    }

    /// Every LAN job whose report Odoo is owed (final, not reported), oldest
    /// first.
    pub fn owed_lan_reports(&self) -> Result<Vec<LanJob>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {LAN_COLUMNS} FROM lan_jobs
             WHERE state IN ('done', 'failed') AND reported = 0 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([], row_to_lan)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().filter(LanJob::report_owed).collect())
    }

    /// How many final LAN jobs Odoo has not been told about yet.
    pub fn unreported_lan_jobs(&self) -> Result<i64> {
        self.conn()
            .query_row(
                "SELECT COUNT(*) FROM lan_jobs WHERE state IN ('done', 'failed') AND reported = 0",
                [],
                |r| r.get(0),
            )
            .context("journal: count unreported LAN jobs")
    }

    /// Odoo answered the report of the final LAN job `job_id`: its own job
    /// id (when it named one) and `note` (`created` / `existing` /
    /// `refused: …`). `updated_at` stays the time of the outcome.
    pub fn mark_lan_reported(
        &self,
        job_id: i64,
        odoo_job_id: Option<i64>,
        note: &str,
    ) -> Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE lan_jobs SET reported = 1, report_note = ?3,
                     odoo_job_id = COALESCE(?2, odoo_job_id)
                 WHERE id = ?1 AND state IN ('done', 'failed')",
                params![lan_job_id(job_id), odoo_job_id, note],
            )
            .with_context(|| format!("journal: mark LAN job {job_id} reported"))?;
        Ok(changed == 1)
    }
}
