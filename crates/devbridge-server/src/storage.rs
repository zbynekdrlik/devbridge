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
                    state, spool_path, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
    pub fn upsert_client(&self, reg: &ClientRegistration) -> Result<()> {
        let printer_names_json = serde_json::to_string(&reg.printer_names)
            .context("failed to serialize printer names")?;

        self.conn
            .execute(
                "INSERT INTO clients (machine_id, hostname, printer_names, client_version, last_seen, is_online)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(machine_id) DO UPDATE SET
                    hostname = excluded.hostname,
                    printer_names = excluded.printer_names,
                    client_version = excluded.client_version,
                    last_seen = excluded.last_seen,
                    is_online = excluded.is_online",
                params![
                    reg.machine_id,
                    reg.hostname,
                    printer_names_json,
                    reg.client_version,
                    reg.last_seen.to_rfc3339(),
                    reg.is_online as i32,
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
    let last_seen_str: String = row.get("last_seen")?;
    let printer_names_str: String = row.get("printer_names")?;
    let printer_names: Vec<String> = serde_json::from_str(&printer_names_str).unwrap_or_default();

    Ok(ClientRegistration {
        machine_id: row.get("machine_id")?,
        hostname: row.get("hostname")?,
        printer_names,
        client_version: row.get("client_version")?,
        last_seen: last_seen_str.parse::<DateTime<Utc>>().unwrap_or_default(),
        is_online: row.get::<_, i32>("is_online")? != 0,
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
            };
            storage.upsert_client(&reg).unwrap();
        }

        storage.set_all_clients_offline().unwrap();
        let clients = storage.list_clients().unwrap();
        assert!(clients.iter().all(|c| !c.is_online));
    }
}
