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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_job(id: &str) -> JobMetadata {
        JobMetadata {
            job_id: id.to_string(),
            document_name: "test.pdf".into(),
            target_printer: "Test Printer".into(),
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

    fn temp_queue() -> (tempfile::TempDir, JobQueue) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();
        let queue = JobQueue::new(storage).unwrap();
        (dir, queue)
    }

    #[test]
    fn test_push_and_pop_ordering() {
        let (_dir, queue) = temp_queue();

        queue.push(test_job("job-a"), "/tmp/a.pdf".into()).unwrap();
        queue.push(test_job("job-b"), "/tmp/b.pdf".into()).unwrap();
        queue.push(test_job("job-c"), "/tmp/c.pdf".into()).unwrap();

        assert_eq!(queue.next_job().unwrap(), "job-a");
        assert_eq!(queue.next_job().unwrap(), "job-b");
        assert_eq!(queue.next_job().unwrap(), "job-c");
        assert!(queue.next_job().is_none());
    }

    #[tokio::test]
    async fn test_wait_for_job_notification() {
        let (_dir, queue) = temp_queue();
        let queue = Arc::new(queue);
        let queue_clone = Arc::clone(&queue);

        let handle = tokio::spawn(async move {
            queue_clone.wait_for_job().await;
            queue_clone.next_job()
        });

        // Give the waiter time to register
        tokio::task::yield_now().await;

        queue
            .push(test_job("job-notify"), "/tmp/notify.pdf".into())
            .unwrap();

        let result = handle.await.unwrap();
        assert_eq!(result.unwrap(), "job-notify");
    }

    #[test]
    fn test_preload_pending_from_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Insert jobs directly into storage first
        {
            let storage = Storage::new(&db_path).unwrap();
            storage
                .insert_job(&test_job("job-pre-1"), "/tmp/pre1.pdf")
                .unwrap();
            storage
                .insert_job(&test_job("job-pre-2"), "/tmp/pre2.pdf")
                .unwrap();
        }

        // Create a new queue -- it should preload pending jobs
        let storage = Storage::new(&db_path).unwrap();
        let queue = JobQueue::new(storage).unwrap();

        assert_eq!(queue.next_job().unwrap(), "job-pre-1");
        assert_eq!(queue.next_job().unwrap(), "job-pre-2");
        assert!(queue.next_job().is_none());
    }
}
