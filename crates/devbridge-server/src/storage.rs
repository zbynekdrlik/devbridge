use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use tracing::{debug, info};

use devbridge_core::client_registration::ClientRegistration;
use devbridge_core::job::{JobMetadata, JobState};
use devbridge_core::virtual_printer::VirtualPrinter;

/// SQLite-backed storage for print jobs, virtual printers, and clients.
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
                job_id           TEXT PRIMARY KEY,
                document_name    TEXT NOT NULL,
                target_printer   TEXT NOT NULL,
                target_client_id TEXT,
                copies           INTEGER NOT NULL,
                paper_size       TEXT NOT NULL,
                duplex           INTEGER NOT NULL,
                color            INTEGER NOT NULL,
                payload_size     INTEGER NOT NULL,
                payload_sha256   TEXT NOT NULL,
                state            TEXT NOT NULL,
                spool_path       TEXT NOT NULL,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS virtual_printers (
                id               TEXT PRIMARY KEY,
                display_name     TEXT NOT NULL,
                ipp_name         TEXT NOT NULL UNIQUE,
                paired_client_id TEXT,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS clients (
                machine_id       TEXT PRIMARY KEY,
                hostname         TEXT NOT NULL,
                printer_names    TEXT NOT NULL,
                client_version   TEXT NOT NULL,
                last_seen        TEXT NOT NULL,
                is_online        INTEGER NOT NULL DEFAULT 0
            );",
        )
        .context("failed to create tables")?;

        // Migration: add target_client_id column if missing (upgrades from old schema)
        let has_col: bool = conn
            .prepare("SELECT target_client_id FROM jobs LIMIT 0")
            .is_ok();
        if !has_col {
            let _ = conn.execute_batch("ALTER TABLE jobs ADD COLUMN target_client_id TEXT;");
        }

        // Migration: add retry_count and error_detail columns
        if conn
            .prepare("SELECT retry_count FROM jobs LIMIT 0")
            .is_err()
        {
            let _ = conn.execute_batch(
                "ALTER TABLE jobs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;",
            );
        }
        if conn
            .prepare("SELECT error_detail FROM jobs LIMIT 0")
            .is_err()
        {
            let _ = conn.execute_batch(
                "ALTER TABLE jobs ADD COLUMN error_detail TEXT NOT NULL DEFAULT '';",
            );
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS job_events (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id    TEXT NOT NULL,
                stage     TEXT NOT NULL,
                success   INTEGER NOT NULL,
                detail    TEXT NOT NULL DEFAULT '',
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_job_events_job_id ON job_events(job_id);",
        )
        .context("failed to create job_events table")?;

        // Migration: add pairing_state column to clients (default 'approved' for existing rows)
        if conn
            .prepare("SELECT pairing_state FROM clients LIMIT 0")
            .is_err()
        {
            let _ = conn.execute_batch(
                "ALTER TABLE clients ADD COLUMN pairing_state TEXT NOT NULL DEFAULT 'approved';",
            );
        }

        // Migration: add virtual_printer_name column to clients
        if conn
            .prepare("SELECT virtual_printer_name FROM clients LIMIT 0")
            .is_err()
        {
            let _ = conn.execute_batch("ALTER TABLE clients ADD COLUMN virtual_printer_name TEXT;");
        }

        info!("storage opened at {}", path.display());
        Ok(Self { conn })
    }

    // -----------------------------------------------------------------------
    // Jobs
    // -----------------------------------------------------------------------

    /// Insert a new job record.
    pub fn insert_job(&self, meta: &JobMetadata, spool_path: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO jobs (
                    job_id, document_name, target_printer, target_client_id,
                    copies, paper_size, duplex, color, payload_size, payload_sha256,
                    state, retry_count, error_detail, spool_path, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    meta.job_id,
                    meta.document_name,
                    meta.target_printer,
                    meta.target_client_id,
                    meta.copies,
                    meta.paper_size,
                    meta.duplex as i32,
                    meta.color as i32,
                    meta.payload_size as i64,
                    meta.payload_sha256,
                    state_to_str(meta.state),
                    meta.retry_count as i64,
                    meta.error_detail,
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

    /// Requeue a failed/stale job for retry: reset state to Queued and increment retry_count.
    pub fn requeue_job(&self, job_id: &str, error_detail: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .execute(
                "UPDATE jobs SET state = 'queued', retry_count = retry_count + 1, error_detail = ?1, updated_at = ?2 WHERE job_id = ?3",
                params![error_detail, now, job_id],
            )
            .with_context(|| format!("failed to requeue job {job_id}"))?;

        if rows == 0 {
            anyhow::bail!("job {job_id} not found");
        }
        debug!(job_id, "job requeued for retry");
        Ok(())
    }

    /// Find failed jobs eligible for retry (retry_count < max_retries).
    pub fn get_retriable_jobs(&self, max_retries: u32) -> Result<Vec<JobMetadata>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT * FROM jobs WHERE state = 'failed' AND retry_count < ?1 ORDER BY updated_at ASC",
            )
            .context("failed to prepare retriable-jobs query")?;

        let jobs = stmt
            .query_map(params![max_retries as i64], row_to_job)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read retriable jobs")?;

        Ok(jobs)
    }

    /// Find stale jobs stuck in downloading/printing for too long.
    pub fn get_stale_jobs(&self, older_than: DateTime<Utc>) -> Result<Vec<JobMetadata>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT * FROM jobs WHERE state IN ('downloading', 'printing') AND updated_at < ?1 ORDER BY updated_at ASC",
            )
            .context("failed to prepare stale-jobs query")?;

        let jobs = stmt
            .query_map(params![older_than.to_rfc3339()], row_to_job)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read stale jobs")?;

        Ok(jobs)
    }

    /// Set the target_client_id for a job.
    pub fn set_job_target_client(&self, job_id: &str, client_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE jobs SET target_client_id = ?1 WHERE job_id = ?2",
                params![client_id, job_id],
            )
            .with_context(|| format!("failed to set target client for job {job_id}"))?;
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

    /// Count jobs created today (UTC).
    pub fn count_jobs_today(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE date(created_at) = date('now')",
                [],
                |row| row.get(0),
            )
            .context("failed to count today's jobs")?;
        Ok(count as u64)
    }

    /// Delete all jobs and their events.
    pub fn clear_jobs(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM job_events; DELETE FROM jobs;")
            .context("failed to clear jobs")?;
        Ok(())
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

    // -----------------------------------------------------------------------
    // Virtual Printers
    // -----------------------------------------------------------------------

    /// Insert a new virtual printer.
    pub fn insert_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO virtual_printers (id, display_name, ipp_name, paired_client_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    vp.id,
                    vp.display_name,
                    vp.ipp_name,
                    vp.paired_client_id,
                    vp.created_at.to_rfc3339(),
                    vp.updated_at.to_rfc3339(),
                ],
            )
            .with_context(|| format!("failed to insert virtual printer {}", vp.id))?;
        Ok(())
    }

    /// Get a virtual printer by ID.
    pub fn get_virtual_printer(&self, id: &str) -> Result<Option<VirtualPrinter>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM virtual_printers WHERE id = ?1")
            .context("failed to prepare get-vp query")?;

        let mut rows = stmt
            .query_map(params![id], row_to_virtual_printer)
            .context("failed to query virtual printer")?;

        match rows.next() {
            Some(Ok(vp)) => Ok(Some(vp)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Look up a virtual printer by its IPP name (slug).
    pub fn get_virtual_printer_by_ipp_name(
        &self,
        ipp_name: &str,
    ) -> Result<Option<VirtualPrinter>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM virtual_printers WHERE ipp_name = ?1")
            .context("failed to prepare get-vp-by-ipp query")?;

        let mut rows = stmt
            .query_map(params![ipp_name], row_to_virtual_printer)
            .context("failed to query virtual printer by ipp_name")?;

        match rows.next() {
            Some(Ok(vp)) => Ok(Some(vp)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// List all virtual printers.
    pub fn list_virtual_printers(&self) -> Result<Vec<VirtualPrinter>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM virtual_printers ORDER BY created_at ASC")
            .context("failed to prepare list-vp query")?;

        let vps = stmt
            .query_map([], row_to_virtual_printer)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read virtual printers")?;

        Ok(vps)
    }

    /// Update a virtual printer's display_name, ipp_name, and pairing.
    pub fn update_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let rows = self
            .conn
            .execute(
                "UPDATE virtual_printers SET display_name = ?1, ipp_name = ?2, paired_client_id = ?3, updated_at = ?4 WHERE id = ?5",
                params![
                    vp.display_name,
                    vp.ipp_name,
                    vp.paired_client_id,
                    Utc::now().to_rfc3339(),
                    vp.id,
                ],
            )
            .with_context(|| format!("failed to update virtual printer {}", vp.id))?;

        if rows == 0 {
            anyhow::bail!("virtual printer {} not found", vp.id);
        }
        Ok(())
    }

    /// Delete a virtual printer by ID.
    pub fn delete_virtual_printer(&self, id: &str) -> Result<()> {
        let rows = self
            .conn
            .execute("DELETE FROM virtual_printers WHERE id = ?1", params![id])
            .with_context(|| format!("failed to delete virtual printer {id}"))?;

        if rows == 0 {
            anyhow::bail!("virtual printer {id} not found");
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Clients
    // -----------------------------------------------------------------------

    /// Insert or update a client registration.
    ///
    /// CRITICAL: The `ON CONFLICT DO UPDATE` intentionally does NOT overwrite
    /// `pairing_state` — a reconnecting approved client must stay approved.
    /// `virtual_printer_name` uses COALESCE to only update if the new value is non-null.
    pub fn upsert_client(&self, reg: &ClientRegistration) -> Result<()> {
        let printer_names_json = serde_json::to_string(&reg.printer_names)
            .context("failed to serialize printer names")?;

        self.conn
            .execute(
                "INSERT INTO clients (machine_id, hostname, printer_names, client_version, last_seen, is_online, pairing_state, virtual_printer_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(machine_id) DO UPDATE SET
                    hostname = excluded.hostname,
                    printer_names = excluded.printer_names,
                    client_version = excluded.client_version,
                    last_seen = excluded.last_seen,
                    is_online = excluded.is_online,
                    virtual_printer_name = COALESCE(excluded.virtual_printer_name, clients.virtual_printer_name)",
                params![
                    reg.machine_id,
                    reg.hostname,
                    printer_names_json,
                    reg.client_version,
                    reg.last_seen.to_rfc3339(),
                    reg.is_online as i32,
                    reg.pairing_state.to_string(),
                    reg.virtual_printer_name,
                ],
            )
            .with_context(|| format!("failed to upsert client {}", reg.machine_id))?;
        Ok(())
    }

    /// List all registered clients.
    pub fn list_clients(&self) -> Result<Vec<ClientRegistration>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM clients ORDER BY last_seen DESC")
            .context("failed to prepare list-clients query")?;

        let clients = stmt
            .query_map([], row_to_client)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read clients")?;

        Ok(clients)
    }

    /// Set a client's online status.
    pub fn set_client_online(&self, machine_id: &str, online: bool) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE clients SET is_online = ?1, last_seen = ?2 WHERE machine_id = ?3",
                params![online as i32, now, machine_id],
            )
            .with_context(|| format!("failed to set client {machine_id} online={online}"))?;
        Ok(())
    }

    /// Set all clients offline (used on startup).
    pub fn set_all_clients_offline(&self) -> Result<()> {
        self.conn
            .execute("UPDATE clients SET is_online = 0", [])
            .context("failed to set all clients offline")?;
        Ok(())
    }

    /// Fetch a single client by machine_id.
    pub fn get_client(&self, machine_id: &str) -> Result<Option<ClientRegistration>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM clients WHERE machine_id = ?1")
            .context("failed to prepare get-client query")?;

        let mut rows = stmt
            .query_map(params![machine_id], row_to_client)
            .context("failed to query client")?;

        match rows.next() {
            Some(Ok(client)) => Ok(Some(client)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Update a client's pairing state.
    pub fn update_pairing_state(
        &self,
        machine_id: &str,
        state: devbridge_core::client_registration::PairingState,
    ) -> Result<()> {
        let rows = self
            .conn
            .execute(
                "UPDATE clients SET pairing_state = ?1 WHERE machine_id = ?2",
                params![state.to_string(), machine_id],
            )
            .with_context(|| format!("failed to update pairing state for client {machine_id}"))?;

        if rows == 0 {
            anyhow::bail!("client {machine_id} not found");
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Job Events
    // -----------------------------------------------------------------------

    /// Insert a print job audit event.
    pub fn insert_job_event(&self, event: &devbridge_core::job_event::PrintJobEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_events (job_id, stage, success, detail, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.job_id,
                serde_json::to_value(event.stage)
                    .unwrap()
                    .as_str()
                    .unwrap_or("unknown"),
                event.success as i32,
                event.detail,
                event.timestamp.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Return all audit events for a given job, ordered by insertion time.
    pub fn get_job_events(
        &self,
        job_id: &str,
    ) -> Result<Vec<devbridge_core::job_event::PrintJobEvent>> {
        use devbridge_core::job_event::{PrintJobEvent, PrintStage};

        let mut stmt = self.conn.prepare(
            "SELECT job_id, stage, success, detail, timestamp
             FROM job_events WHERE job_id = ?1 ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map(params![job_id], |row| {
                let stage_str: String = row.get(1)?;
                let stage: PrintStage = serde_json::from_str(&format!("\"{}\"", stage_str))
                    .unwrap_or(PrintStage::Failed);
                let ts_str: String = row.get(4)?;
                let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());

                Ok(PrintJobEvent {
                    job_id: row.get(0)?,
                    stage,
                    success: row.get::<_, i32>(2)? != 0,
                    detail: row.get(3)?,
                    timestamp,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(events)
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
        target_client_id: row
            .get::<_, Option<String>>("target_client_id")
            .unwrap_or(None),
        copies: row.get("copies")?,
        paper_size: row.get("paper_size")?,
        duplex: row.get::<_, i32>("duplex")? != 0,
        color: row.get::<_, i32>("color")? != 0,
        payload_size: row.get::<_, i64>("payload_size")? as u64,
        payload_sha256: row.get("payload_sha256")?,
        state: str_to_state(&state_str),
        retry_count: row.get::<_, i64>("retry_count").unwrap_or(0) as u32,
        error_detail: row.get::<_, String>("error_detail").unwrap_or_default(),
        created_at: created_str.parse::<DateTime<Utc>>().unwrap_or_default(),
        updated_at: updated_str.parse::<DateTime<Utc>>().unwrap_or_default(),
    })
}

fn row_to_virtual_printer(row: &rusqlite::Row) -> rusqlite::Result<VirtualPrinter> {
    let created_str: String = row.get("created_at")?;
    let updated_str: String = row.get("updated_at")?;

    Ok(VirtualPrinter {
        id: row.get("id")?,
        display_name: row.get("display_name")?,
        ipp_name: row.get("ipp_name")?,
        paired_client_id: row
            .get::<_, Option<String>>("paired_client_id")
            .unwrap_or(None),
        created_at: created_str.parse::<DateTime<Utc>>().unwrap_or_default(),
        updated_at: updated_str.parse::<DateTime<Utc>>().unwrap_or_default(),
    })
}

fn row_to_client(row: &rusqlite::Row) -> rusqlite::Result<ClientRegistration> {
    use devbridge_core::client_registration::PairingState;

    let last_seen_str: String = row.get("last_seen")?;
    let printer_names_str: String = row.get("printer_names")?;
    let printer_names: Vec<String> = serde_json::from_str(&printer_names_str).unwrap_or_default();
    let pairing_str: String = row
        .get::<_, String>("pairing_state")
        .unwrap_or_else(|_| "approved".to_string());

    Ok(ClientRegistration {
        machine_id: row.get("machine_id")?,
        hostname: row.get("hostname")?,
        printer_names,
        client_version: row.get("client_version")?,
        last_seen: last_seen_str.parse::<DateTime<Utc>>().unwrap_or_default(),
        is_online: row.get::<_, i32>("is_online")? != 0,
        pairing_state: PairingState::from_str_lossy(&pairing_str),
        virtual_printer_name: row
            .get::<_, Option<String>>("virtual_printer_name")
            .unwrap_or(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_job(id: &str) -> JobMetadata {
        JobMetadata {
            job_id: id.to_string(),
            document_name: "test.pdf".into(),
            target_printer: "Test Printer".into(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".into(),
            duplex: false,
            color: true,
            payload_size: 1024,
            payload_sha256: "abc123".into(),
            state: JobState::Queued,
            retry_count: 0,
            error_detail: String::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_insert_and_get_job() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let job = test_job("job-100");
        storage.insert_job(&job, "/tmp/spool/job-100.pdf").unwrap();

        let loaded = storage
            .get_job("job-100")
            .unwrap()
            .expect("job should exist");
        assert_eq!(loaded.job_id, "job-100");
        assert_eq!(loaded.document_name, "test.pdf");
        assert_eq!(loaded.target_printer, "Test Printer");
        assert!(loaded.target_client_id.is_none());
        assert_eq!(loaded.copies, 1);
        assert_eq!(loaded.paper_size, "A4");
        assert!(!loaded.duplex);
        assert!(loaded.color);
        assert_eq!(loaded.payload_size, 1024);
        assert_eq!(loaded.payload_sha256, "abc123");
        assert_eq!(loaded.state, JobState::Queued);
    }

    #[test]
    fn test_insert_job_with_target_client_id() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let mut job = test_job("job-101");
        job.target_client_id = Some("client-abc".into());
        storage.insert_job(&job, "/tmp/spool/job-101.pdf").unwrap();

        let loaded = storage.get_job("job-101").unwrap().unwrap();
        assert_eq!(loaded.target_client_id, Some("client-abc".into()));
    }

    #[test]
    fn test_set_job_target_client() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let job = test_job("job-102");
        storage.insert_job(&job, "/tmp/spool/job-102.pdf").unwrap();

        storage
            .set_job_target_client("job-102", "client-xyz")
            .unwrap();

        let loaded = storage.get_job("job-102").unwrap().unwrap();
        assert_eq!(loaded.target_client_id, Some("client-xyz".into()));
    }

    #[test]
    fn test_update_job_state() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let job = test_job("job-200");
        storage.insert_job(&job, "/tmp/spool/job-200.pdf").unwrap();

        storage
            .update_job_state("job-200", JobState::Completed)
            .unwrap();

        let loaded = storage
            .get_job("job-200")
            .unwrap()
            .expect("job should exist");
        assert_eq!(loaded.state, JobState::Completed);
    }

    #[test]
    fn test_get_pending_jobs_filters_completed() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let queued_job = test_job("job-300");
        storage
            .insert_job(&queued_job, "/tmp/spool/job-300.pdf")
            .unwrap();

        let mut completed_job = test_job("job-301");
        completed_job.state = JobState::Completed;
        storage
            .insert_job(&completed_job, "/tmp/spool/job-301.pdf")
            .unwrap();
        storage
            .update_job_state("job-301", JobState::Completed)
            .unwrap();

        let pending = storage.get_pending_jobs().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].job_id, "job-300");
    }

    #[test]
    fn test_count_jobs_today_returns_zero_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let count = storage.count_jobs_today().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_count_jobs_today_counts_todays_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let job = test_job("job-today");
        storage
            .insert_job(&job, "/tmp/spool/job-today.pdf")
            .unwrap();

        let count = storage.count_jobs_today().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_get_spool_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let job = test_job("job-400");
        storage.insert_job(&job, "/tmp/spool/job-400.pdf").unwrap();

        let path = storage
            .get_spool_path("job-400")
            .unwrap()
            .expect("spool path should exist");
        assert_eq!(path, "/tmp/spool/job-400.pdf");
    }

    // -----------------------------------------------------------------------
    // Virtual Printer tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_virtual_printer_crud() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-1".into(),
            display_name: "Store A Receipt".into(),
            ipp_name: "store-a-receipt".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        storage.insert_virtual_printer(&vp).unwrap();

        // Get by ID
        let loaded = storage.get_virtual_printer("vp-1").unwrap().unwrap();
        assert_eq!(loaded.display_name, "Store A Receipt");
        assert_eq!(loaded.ipp_name, "store-a-receipt");
        assert!(loaded.paired_client_id.is_none());

        // Get by ipp_name
        let loaded = storage
            .get_virtual_printer_by_ipp_name("store-a-receipt")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id, "vp-1");

        // List
        let list = storage.list_virtual_printers().unwrap();
        assert_eq!(list.len(), 1);

        // Update
        let mut updated = loaded;
        updated.display_name = "Store A - Updated".into();
        updated.paired_client_id = Some("client-1".into());
        storage.update_virtual_printer(&updated).unwrap();
        let loaded = storage.get_virtual_printer("vp-1").unwrap().unwrap();
        assert_eq!(loaded.display_name, "Store A - Updated");
        assert_eq!(loaded.paired_client_id, Some("client-1".into()));

        // Delete
        storage.delete_virtual_printer("vp-1").unwrap();
        assert!(storage.get_virtual_printer("vp-1").unwrap().is_none());
    }

    #[test]
    fn test_virtual_printer_unique_ipp_name() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let now = Utc::now();
        let vp1 = VirtualPrinter {
            id: "vp-1".into(),
            display_name: "First".into(),
            ipp_name: "same-name".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        storage.insert_virtual_printer(&vp1).unwrap();

        let vp2 = VirtualPrinter {
            id: "vp-2".into(),
            display_name: "Second".into(),
            ipp_name: "same-name".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        assert!(storage.insert_virtual_printer(&vp2).is_err());
    }

    // -----------------------------------------------------------------------
    // Client tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_client_upsert_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let reg = ClientRegistration {
            machine_id: "mc-1".into(),
            hostname: "store-a".into(),
            printer_names: vec!["EPSON L3270".into()],
            client_version: "0.1.0".into(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Approved,
            virtual_printer_name: None,
        };
        storage.upsert_client(&reg).unwrap();

        let clients = storage.list_clients().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].machine_id, "mc-1");
        assert_eq!(clients[0].hostname, "store-a");
        assert_eq!(clients[0].printer_names, vec!["EPSON L3270"]);
        assert!(clients[0].is_online);

        // Upsert again with updated hostname
        let reg2 = ClientRegistration {
            machine_id: "mc-1".into(),
            hostname: "store-a-new".into(),
            printer_names: vec!["EPSON L3270".into(), "Canon MG3600".into()],
            client_version: "0.2.0".into(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Approved,
            virtual_printer_name: None,
        };
        storage.upsert_client(&reg2).unwrap();

        let clients = storage.list_clients().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].hostname, "store-a-new");
        assert_eq!(clients[0].printer_names.len(), 2);
        assert_eq!(clients[0].client_version, "0.2.0");
    }

    #[test]
    fn test_client_online_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let reg = ClientRegistration {
            machine_id: "mc-2".into(),
            hostname: "store-b".into(),
            printer_names: vec![],
            client_version: "0.1.0".into(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Approved,
            virtual_printer_name: None,
        };
        storage.upsert_client(&reg).unwrap();

        storage.set_client_online("mc-2", false).unwrap();
        let clients = storage.list_clients().unwrap();
        assert!(!clients[0].is_online);

        storage.set_client_online("mc-2", true).unwrap();
        let clients = storage.list_clients().unwrap();
        assert!(clients[0].is_online);
    }

    #[test]
    fn test_client_pairing_state_default() {
        use devbridge_core::client_registration::PairingState;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        // Insert a pending client
        let reg = ClientRegistration {
            machine_id: "mc-default".into(),
            hostname: "host".into(),
            printer_names: vec![],
            client_version: "0.1.0".into(),
            last_seen: Utc::now(),
            is_online: false,
            pairing_state: PairingState::Pending,
            virtual_printer_name: None,
        };
        storage.upsert_client(&reg).unwrap();

        let client = storage.get_client("mc-default").unwrap().unwrap();
        assert_eq!(client.pairing_state, PairingState::Pending);

        // Insert an approved client with virtual_printer_name
        let reg2 = ClientRegistration {
            machine_id: "mc-approved".into(),
            hostname: "host2".into(),
            printer_names: vec![],
            client_version: "0.1.0".into(),
            last_seen: Utc::now(),
            is_online: false,
            pairing_state: PairingState::Approved,
            virtual_printer_name: Some("store-receipt".into()),
        };
        storage.upsert_client(&reg2).unwrap();

        let client = storage.get_client("mc-approved").unwrap().unwrap();
        assert_eq!(client.pairing_state, PairingState::Approved);
        assert_eq!(client.virtual_printer_name, Some("store-receipt".into()));
    }

    #[test]
    fn test_update_pairing_state() {
        use devbridge_core::client_registration::PairingState;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let reg = ClientRegistration {
            machine_id: "mc-pair".into(),
            hostname: "host".into(),
            printer_names: vec![],
            client_version: "0.1.0".into(),
            last_seen: Utc::now(),
            is_online: false,
            pairing_state: PairingState::Pending,
            virtual_printer_name: None,
        };
        storage.upsert_client(&reg).unwrap();

        storage
            .update_pairing_state("mc-pair", PairingState::Approved)
            .unwrap();
        let client = storage.get_client("mc-pair").unwrap().unwrap();
        assert_eq!(client.pairing_state, PairingState::Approved);

        storage
            .update_pairing_state("mc-pair", PairingState::Rejected)
            .unwrap();
        let client = storage.get_client("mc-pair").unwrap().unwrap();
        assert_eq!(client.pairing_state, PairingState::Rejected);

        assert!(
            storage
                .update_pairing_state("nonexistent", PairingState::Approved)
                .is_err()
        );
    }

    #[test]
    fn test_upsert_does_not_overwrite_pairing_state() {
        use devbridge_core::client_registration::PairingState;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        // Insert as Pending, then approve
        let reg = ClientRegistration {
            machine_id: "mc-keep".into(),
            hostname: "host".into(),
            printer_names: vec!["Printer1".into()],
            client_version: "0.1.0".into(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: PairingState::Pending,
            virtual_printer_name: Some("store-a".into()),
        };
        storage.upsert_client(&reg).unwrap();

        storage
            .update_pairing_state("mc-keep", PairingState::Approved)
            .unwrap();

        // Client reconnects with Pending (default for new connections)
        let reg2 = ClientRegistration {
            machine_id: "mc-keep".into(),
            hostname: "host-updated".into(),
            printer_names: vec!["Printer1".into(), "Printer2".into()],
            client_version: "0.2.0".into(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: PairingState::Pending,
            virtual_printer_name: None,
        };
        storage.upsert_client(&reg2).unwrap();

        // pairing_state must remain Approved (not overwritten by upsert)
        let client = storage.get_client("mc-keep").unwrap().unwrap();
        assert_eq!(client.pairing_state, PairingState::Approved);
        assert_eq!(client.hostname, "host-updated");
        // virtual_printer_name preserved via COALESCE (new value was NULL)
        assert_eq!(client.virtual_printer_name, Some("store-a".into()));
    }

    // -----------------------------------------------------------------------
    // Job Events tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_insert_and_query_job_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        // Insert a job first
        let now = Utc::now();
        let meta = JobMetadata {
            job_id: "evt-job-1".to_string(),
            document_name: "test.pdf".to_string(),
            target_printer: "printer".to_string(),
            target_client_id: None,
            copies: 1,
            paper_size: "A4".to_string(),
            duplex: false,
            color: true,
            payload_size: 1024,
            payload_sha256: "abc".to_string(),
            state: JobState::Queued,
            retry_count: 0,
            error_detail: String::new(),
            created_at: now,
            updated_at: now,
        };
        storage.insert_job(&meta, "/tmp/spool/test.pdf").unwrap();

        use devbridge_core::job_event::{PrintJobEvent, PrintStage};
        let e1 = PrintJobEvent::ok("evt-job-1", PrintStage::Received, "234KB");
        let e2 = PrintJobEvent::ok(
            "evt-job-1",
            PrintStage::Rendering,
            "Ghostscript ppmraw 600dpi",
        );
        let e3 = PrintJobEvent::ok("evt-job-1", PrintStage::Rendered, "3 pages, 4.2MB, 1.3s");
        storage.insert_job_event(&e1).unwrap();
        storage.insert_job_event(&e2).unwrap();
        storage.insert_job_event(&e3).unwrap();

        let events = storage.get_job_events("evt-job-1").unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].stage, PrintStage::Received);
        assert_eq!(events[1].stage, PrintStage::Rendering);
        assert_eq!(events[2].stage, PrintStage::Rendered);
        assert_eq!(events[0].detail, "234KB");
    }

    #[test]
    fn test_get_job_events_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        let events = storage.get_job_events("nonexistent").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_set_all_clients_offline() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();

        for i in 0..3 {
            let reg = ClientRegistration {
                machine_id: format!("mc-{i}"),
                hostname: format!("host-{i}"),
                printer_names: vec![],
                client_version: "0.1.0".into(),
                last_seen: Utc::now(),
                is_online: true,
                pairing_state: devbridge_core::client_registration::PairingState::Approved,
                virtual_printer_name: None,
            };
            storage.upsert_client(&reg).unwrap();
        }

        storage.set_all_clients_offline().unwrap();
        let clients = storage.list_clients().unwrap();
        assert!(clients.iter().all(|c| !c.is_online));
    }
}
