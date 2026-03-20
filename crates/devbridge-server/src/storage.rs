use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use tracing::{debug, info};

use devbridge_core::job::{JobMetadata, JobState};

/// SQLite-backed storage for print jobs.
pub struct Storage {
    conn: Connection,
}

impl Storage {
    /// Open (or create) the SQLite database at `path` and ensure the schema exists.
    pub fn new(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                job_id         TEXT PRIMARY KEY,
                document_name  TEXT NOT NULL,
                target_printer TEXT NOT NULL,
                copies         INTEGER NOT NULL,
                paper_size     TEXT NOT NULL,
                duplex         INTEGER NOT NULL,
                color          INTEGER NOT NULL,
                payload_size   INTEGER NOT NULL,
                payload_sha256 TEXT NOT NULL,
                state          TEXT NOT NULL,
                spool_path     TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            );",
        )
        .context("failed to create jobs table")?;

        info!("storage opened at {}", path.display());
        Ok(Self { conn })
    }

    /// Insert a new job record.
    pub fn insert_job(&self, meta: &JobMetadata, spool_path: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO jobs (
                    job_id, document_name, target_printer, copies, paper_size,
                    duplex, color, payload_size, payload_sha256, state,
                    spool_path, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    meta.job_id,
                    meta.document_name,
                    meta.target_printer,
                    meta.copies,
                    meta.paper_size,
                    meta.duplex as i32,
                    meta.color as i32,
                    meta.payload_size as i64,
                    meta.payload_sha256,
                    state_to_str(meta.state),
                    spool_path,
                    meta.created_at.to_rfc3339(),
                    meta.updated_at.to_rfc3339(),
                ],
            )
            .with_context(|| format!("failed to insert job {}", meta.job_id))?;

        debug!(job_id = %meta.job_id, "job inserted into storage");
        Ok(())
    }

    /// Update the state of an existing job.
    pub fn update_job_state(&self, job_id: &str, state: JobState) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .execute(
                "UPDATE jobs SET state = ?1, updated_at = ?2 WHERE job_id = ?3",
                params![state_to_str(state), now, job_id],
            )
            .with_context(|| format!("failed to update job {job_id}"))?;

        if rows == 0 {
            anyhow::bail!("job {job_id} not found");
        }
        debug!(job_id, ?state, "job state updated");
        Ok(())
    }

    /// Return all jobs currently in the `Queued` state.
    pub fn get_pending_jobs(&self) -> Result<Vec<JobMetadata>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM jobs WHERE state = 'queued' ORDER BY created_at ASC")
            .context("failed to prepare pending-jobs query")?;

        let jobs = stmt
            .query_map([], row_to_job)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read pending jobs")?;

        debug!(count = jobs.len(), "loaded pending jobs");
        Ok(jobs)
    }

    /// Fetch a single job by ID.
    pub fn get_job(&self, job_id: &str) -> Result<Option<JobMetadata>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM jobs WHERE job_id = ?1")
            .context("failed to prepare get-job query")?;

        let mut rows = stmt
            .query_map(params![job_id], row_to_job)
            .context("failed to query job")?;

        match rows.next() {
            Some(Ok(job)) => Ok(Some(job)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Return every job in the database.
    pub fn get_all_jobs(&self) -> Result<Vec<JobMetadata>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM jobs ORDER BY created_at DESC")
            .context("failed to prepare all-jobs query")?;

        let jobs = stmt
            .query_map([], row_to_job)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read all jobs")?;

        Ok(jobs)
    }

    /// Return the spool path for a given job.
    pub fn get_spool_path(&self, job_id: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT spool_path FROM jobs WHERE job_id = ?1")
            .context("failed to prepare spool-path query")?;

        let mut rows = stmt
            .query_map(params![job_id], |row| row.get::<_, String>(0))
            .context("failed to query spool path")?;

        match rows.next() {
            Some(Ok(p)) => Ok(Some(p)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn state_to_str(s: JobState) -> &'static str {
    match s {
        JobState::Queued => "queued",
        JobState::Downloading => "downloading",
        JobState::Printing => "printing",
        JobState::Completed => "completed",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

fn str_to_state(s: &str) -> JobState {
    match s {
        "queued" => JobState::Queued,
        "downloading" => JobState::Downloading,
        "printing" => JobState::Printing,
        "completed" => JobState::Completed,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        _ => JobState::Queued,
    }
}

fn row_to_job(row: &rusqlite::Row) -> rusqlite::Result<JobMetadata> {
    let state_str: String = row.get("state")?;
    let created_str: String = row.get("created_at")?;
    let updated_str: String = row.get("updated_at")?;

    Ok(JobMetadata {
        job_id: row.get("job_id")?,
        document_name: row.get("document_name")?,
        target_printer: row.get("target_printer")?,
        copies: row.get("copies")?,
        paper_size: row.get("paper_size")?,
        duplex: row.get::<_, i32>("duplex")? != 0,
        color: row.get::<_, i32>("color")? != 0,
        payload_size: row.get::<_, i64>("payload_size")? as u64,
        payload_sha256: row.get("payload_sha256")?,
        state: str_to_state(&state_str),
        created_at: created_str.parse::<DateTime<Utc>>().unwrap_or_default(),
        updated_at: updated_str.parse::<DateTime<Utc>>().unwrap_or_default(),
    })
}
