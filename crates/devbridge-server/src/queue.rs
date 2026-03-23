use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::{Notify, mpsc};
use tracing::{debug, info};

use devbridge_core::client_registration::ClientRegistration;
use devbridge_core::job::{JobMetadata, JobState};
use devbridge_core::virtual_printer::VirtualPrinter;

use crate::storage::Storage;

/// In-memory job queue backed by persistent SQLite storage.
///
/// Supports per-client routing: jobs targeting a paired client are sent
/// directly to that client's channel, while unrouted jobs go to the
/// default queue for any connected client.
pub struct JobQueue {
    storage: Mutex<Storage>,
    /// Per-client channels for targeted job delivery.
    /// Value is (connection_id, sender) to prevent race conditions on reconnect.
    client_channels: Mutex<HashMap<String, (String, mpsc::UnboundedSender<String>)>>,
    /// Default queue for unrouted jobs (backward compat).
    default_pending: Arc<Mutex<VecDeque<String>>>,
    default_notify: Arc<Notify>,
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
            client_channels: Mutex::new(HashMap::new()),
            default_pending: Arc::new(Mutex::new(deque)),
            default_notify: Arc::new(Notify::new()),
        })
    }

    /// Register a client to receive targeted jobs. Returns a receiver for job IDs.
    ///
    /// Each registration includes a unique `connection_id` so that stale cleanup
    /// tasks from a previous connection don't accidentally remove a newer one.
    pub fn register_client(
        &self,
        machine_id: &str,
        connection_id: &str,
    ) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.client_channels
            .lock()
            .unwrap()
            .insert(machine_id.to_string(), (connection_id.to_string(), tx));
        debug!(
            machine_id,
            connection_id, "client registered for job routing"
        );
        rx
    }

    /// Unregister a client's channel, but only if the stored connection_id matches.
    ///
    /// This prevents a stale cleanup task from removing a newer connection's channel.
    pub fn unregister_client(&self, machine_id: &str, connection_id: &str) {
        let mut channels = self.client_channels.lock().unwrap();
        if let Some((stored_id, _)) = channels.get(machine_id) {
            if stored_id == connection_id {
                channels.remove(machine_id);
                debug!(
                    machine_id,
                    connection_id, "client unregistered from job routing"
                );
            } else {
                debug!(
                    machine_id,
                    connection_id,
                    stored_id = %stored_id,
                    "skipping unregister — newer connection exists"
                );
            }
        }
    }

    /// Check whether the given connection_id is still the active one for a machine.
    pub fn is_active_connection(&self, machine_id: &str, connection_id: &str) -> bool {
        self.client_channels
            .lock()
            .unwrap()
            .get(machine_id)
            .is_some_and(|(stored_id, _)| stored_id == connection_id)
    }

    /// Insert a job into persistent storage and route it.
    ///
    /// Routing logic:
    /// 1. Look up `target_printer` in virtual_printers table
    /// 2. If paired to a client, set `target_client_id` and send to that client's channel
    /// 3. Otherwise, push to the default queue for any connected client
    pub fn push(&self, mut meta: JobMetadata, spool_path: String) -> Result<()> {
        let job_id = meta.job_id.clone();

        let storage = self.storage.lock().unwrap();

        // Resolve target_client_id from virtual printer pairing
        if !meta.target_printer.is_empty()
            && let Ok(Some(vp)) = storage.get_virtual_printer_by_ipp_name(&meta.target_printer)
            && let Some(ref client_id) = vp.paired_client_id
        {
            meta.target_client_id = Some(client_id.clone());
        }

        storage.insert_job(&meta, &spool_path)?;
        drop(storage);

        // Route to specific client or default queue
        if let Some(ref target_client) = meta.target_client_id {
            let channels = self.client_channels.lock().unwrap();
            if let Some((_, tx)) = channels.get(target_client) {
                let _ = tx.send(job_id.clone());
                debug!(job_id = %job_id, client = %target_client, "job routed to client");
                return Ok(());
            }
            // Client not connected; fall through to default queue
            debug!(job_id = %job_id, client = %target_client, "target client not connected, queuing to default");
        }

        // Default queue
        {
            let mut q = self.default_pending.lock().unwrap();
            q.push_back(job_id.clone());
        }
        debug!(job_id = %job_id, "job pushed to default queue");
        self.default_notify.notify_waiters();
        Ok(())
    }

    /// Pop the next pending job ID from the default queue, if any.
    pub fn next_job(&self) -> Option<String> {
        let mut q = self.default_pending.lock().unwrap();
        q.pop_front()
    }

    /// Async wait until a new job is pushed to the default queue.
    pub async fn wait_for_job(&self) {
        self.default_notify.notified().await;
    }

    /// Get a future that completes when the next notification fires.
    /// Call this BEFORE checking next_job() to avoid missing notifications.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.default_notify.notified()
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

    /// Count jobs created today.
    pub fn count_jobs_today(&self) -> Result<u64> {
        let storage = self.storage.lock().unwrap();
        storage.count_jobs_today()
    }

    /// Get spool path for a job.
    pub fn get_spool_path(&self, job_id: &str) -> Result<Option<String>> {
        let storage = self.storage.lock().unwrap();
        storage.get_spool_path(job_id)
    }

    // -----------------------------------------------------------------------
    // Virtual Printer delegation
    // -----------------------------------------------------------------------

    pub fn insert_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.insert_virtual_printer(vp)
    }

    pub fn get_virtual_printer(&self, id: &str) -> Result<Option<VirtualPrinter>> {
        let storage = self.storage.lock().unwrap();
        storage.get_virtual_printer(id)
    }

    pub fn get_virtual_printer_by_ipp_name(
        &self,
        ipp_name: &str,
    ) -> Result<Option<VirtualPrinter>> {
        let storage = self.storage.lock().unwrap();
        storage.get_virtual_printer_by_ipp_name(ipp_name)
    }

    pub fn list_virtual_printers(&self) -> Result<Vec<VirtualPrinter>> {
        let storage = self.storage.lock().unwrap();
        storage.list_virtual_printers()
    }

    pub fn update_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.update_virtual_printer(vp)
    }

    pub fn delete_virtual_printer(&self, id: &str) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.delete_virtual_printer(id)
    }

    // -----------------------------------------------------------------------
    // Client delegation
    // -----------------------------------------------------------------------

    pub fn upsert_client(&self, reg: &ClientRegistration) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.upsert_client(reg)
    }

    pub fn list_clients(&self) -> Result<Vec<ClientRegistration>> {
        let storage = self.storage.lock().unwrap();
        storage.list_clients()
    }

    pub fn set_client_online(&self, machine_id: &str, online: bool) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.set_client_online(machine_id, online)
    }

    pub fn set_all_clients_offline(&self) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.set_all_clients_offline()
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

    #[tokio::test]
    async fn test_targeted_job_routed_to_client() {
        let (_dir, queue) = temp_queue();

        // Create a virtual printer paired to a client
        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-1".into(),
            display_name: "Store A".into(),
            ipp_name: "store-a".into(),
            paired_client_id: Some("client-1".into()),
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();

        // Register the client
        let mut rx = queue.register_client("client-1", "conn-1");

        // Push a job targeting the virtual printer
        let mut job = test_job("job-routed");
        job.target_printer = "store-a".into();
        queue.push(job, "/tmp/routed.pdf".into()).unwrap();

        // Client should receive it via channel
        let received = rx.try_recv().unwrap();
        assert_eq!(received, "job-routed");

        // Default queue should be empty
        assert!(queue.next_job().is_none());
    }

    #[test]
    fn test_unrouted_job_goes_to_default_queue() {
        let (_dir, queue) = temp_queue();

        // Push a job with no virtual printer match
        let job = test_job("job-default");
        queue.push(job, "/tmp/default.pdf".into()).unwrap();

        // Should be in default queue
        assert_eq!(queue.next_job().unwrap(), "job-default");
    }

    #[tokio::test]
    async fn test_register_unregister_client() {
        let (_dir, queue) = temp_queue();

        let rx = queue.register_client("client-x", "conn-1");
        assert!(
            queue
                .client_channels
                .lock()
                .unwrap()
                .contains_key("client-x")
        );

        queue.unregister_client("client-x", "conn-1");
        assert!(
            !queue
                .client_channels
                .lock()
                .unwrap()
                .contains_key("client-x")
        );

        drop(rx);
    }

    #[test]
    fn test_unregister_stale_connection_preserves_new() {
        let (_dir, queue) = temp_queue();

        // First connection registers
        let _rx1 = queue.register_client("client-x", "conn-old");

        // New connection replaces it
        let _rx2 = queue.register_client("client-x", "conn-new");

        // Old connection tries to unregister — should be ignored
        queue.unregister_client("client-x", "conn-old");

        // New connection should still be registered
        assert!(queue.is_active_connection("client-x", "conn-new"));
        assert!(
            queue
                .client_channels
                .lock()
                .unwrap()
                .contains_key("client-x")
        );
    }

    #[test]
    fn test_is_active_connection() {
        let (_dir, queue) = temp_queue();

        let _rx = queue.register_client("client-x", "conn-1");
        assert!(queue.is_active_connection("client-x", "conn-1"));
        assert!(!queue.is_active_connection("client-x", "conn-other"));
        assert!(!queue.is_active_connection("unknown", "conn-1"));
    }
}
