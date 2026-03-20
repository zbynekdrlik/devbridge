use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::Notify;
use tracing::{debug, info};

use devbridge_core::job::{JobMetadata, JobState};

use crate::storage::Storage;

/// In-memory job queue backed by persistent SQLite storage.
pub struct JobQueue {
    storage: Mutex<Storage>,
    pending: Arc<Mutex<VecDeque<String>>>,
    notify: Arc<Notify>,
}

impl JobQueue {
    /// Create a new queue, pre-loading any pending jobs from storage.
    pub fn new(storage: Storage) -> Result<Self> {
        let pending_jobs = storage.get_pending_jobs()?;
        let mut deque = VecDeque::with_capacity(pending_jobs.len());
        for job in &pending_jobs {
            deque.push_back(job.job_id.clone());
        }
        info!(
            count = pending_jobs.len(),
            "loaded pending jobs from storage"
        );

        Ok(Self {
            storage: Mutex::new(storage),
            pending: Arc::new(Mutex::new(deque)),
            notify: Arc::new(Notify::new()),
        })
    }

    /// Insert a job into persistent storage and the in-memory queue, then wake
    /// any waiters.
    pub fn push(&self, meta: JobMetadata, spool_path: String) -> Result<()> {
        let job_id = meta.job_id.clone();

        {
            let storage = self.storage.lock().unwrap();
            storage.insert_job(&meta, &spool_path)?;
        }

        {
            let mut q = self.pending.lock().unwrap();
            q.push_back(job_id.clone());
        }

        debug!(job_id = %job_id, "job pushed to queue");
        self.notify.notify_waiters();
        Ok(())
    }

    /// Pop the next pending job ID, if any.
    pub fn next_job(&self) -> Option<String> {
        let mut q = self.pending.lock().unwrap();
        q.pop_front()
    }

    /// Async wait until a new job is pushed.
    pub async fn wait_for_job(&self) {
        self.notify.notified().await;
    }

    /// Update a job's state in storage.
    pub fn update_state(&self, job_id: &str, state: JobState) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.update_job_state(job_id, state)
    }

    /// Retrieve a job by ID from storage.
    pub fn get_job(&self, job_id: &str) -> Result<Option<JobMetadata>> {
        let storage = self.storage.lock().unwrap();
        storage.get_job(job_id)
    }

    /// Return all jobs from storage.
    pub fn get_all_jobs(&self) -> Result<Vec<JobMetadata>> {
        let storage = self.storage.lock().unwrap();
        storage.get_all_jobs()
    }

    /// Get spool path for a job.
    pub fn get_spool_path(&self, job_id: &str) -> Result<Option<String>> {
        let storage = self.storage.lock().unwrap();
        storage.get_spool_path(job_id)
    }
}
