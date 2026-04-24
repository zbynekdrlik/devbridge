use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::{Notify, broadcast, mpsc};
use tracing::{debug, error, info};

use devbridge_core::client_registration::ClientRegistration;
use devbridge_core::job::{JobEvent, JobMetadata, JobState};
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
    /// Optional broadcast sender for job events (consumed by WebSocket clients).
    job_events: Option<broadcast::Sender<JobEvent>>,
    /// Notifies waiters when any client's pairing state changes.
    pairing_notify: Arc<Notify>,
    /// Optional signal fired when the virtual-printer list changes.
    /// Consumed by the printer reconciler task (best-effort via try_send).
    reconciler_signal: Option<mpsc::Sender<()>>,
}

impl JobQueue {
    /// Create a new queue, pre-loading any pending UNPAIRED jobs from storage.
    /// Paired jobs stay in storage and are delivered via `drain_pending_for_client`
    /// when the target client connects — they must NEVER enter the default queue.
    pub fn new(storage: Storage) -> Result<Self> {
        let pending_jobs = storage.get_pending_jobs()?;
        let mut deque = VecDeque::new();
        let mut held = 0usize;
        for job in &pending_jobs {
            if job.target_client_id.is_some() {
                held += 1; // Paired job — held for target client, not in default queue
            } else {
                deque.push_back(job.job_id.clone());
            }
        }
        info!(
            default_queue = deque.len(),
            held_for_clients = held,
            "loaded pending jobs from storage"
        );

        Ok(Self {
            storage: Mutex::new(storage),
            client_channels: Mutex::new(HashMap::new()),
            default_pending: Arc::new(Mutex::new(deque)),
            default_notify: Arc::new(Notify::new()),
            job_events: None,
            pairing_notify: Arc::new(Notify::new()),
            reconciler_signal: None,
        })
    }

    /// Set the broadcast sender for job events.
    pub fn set_job_events(&mut self, sender: broadcast::Sender<JobEvent>) {
        self.job_events = Some(sender);
    }

    /// Wire the reconciler signal sender. After this is set,
    /// `insert_virtual_printer` and `update_virtual_printer` will fire a
    /// `try_send(())` to notify the reconciler that the virtual-printer
    /// list changed. Best-effort: a full channel drops the signal (the
    /// reconciler is already busy processing a previous burst).
    pub fn set_reconciler_signal(&mut self, tx: mpsc::Sender<()>) {
        self.reconciler_signal = Some(tx);
    }

    /// Emit a job event to all WebSocket subscribers.
    fn emit_event(&self, event: JobEvent) {
        if let Some(ref tx) = self.job_events {
            let _ = tx.send(event);
        }
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
        let mut channels = self.client_channels.lock().expect("queue lock poisoned");
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

        let storage = self.storage.lock().expect("queue lock poisoned");

        // Resolve target_client_id from virtual printer pairing
        if !meta.target_printer.is_empty()
            && let Ok(Some(vp)) = storage.get_virtual_printer_by_ipp_name(&meta.target_printer)
            && let Some(ref client_id) = vp.paired_client_id
        {
            meta.target_client_id = Some(client_id.clone());
        }

        storage.insert_job(&meta, &spool_path)?;
        drop(storage);

        self.emit_event(JobEvent::Created {
            job_id: job_id.clone(),
            document_name: meta.document_name.clone(),
            requesting_user: meta.requesting_user.clone(),
            target_printer: meta.target_printer.clone(),
        });

        // Emit routed event for audit trail
        let route_detail = if let Some(ref client_id) = meta.target_client_id {
            format!("{} → {}", meta.target_printer, client_id)
        } else {
            format!("{} → default queue", meta.target_printer)
        };
        let routed_event = devbridge_core::job_event::PrintJobEvent::ok(
            &meta.job_id,
            devbridge_core::job_event::PrintStage::Routed,
            &route_detail,
        );
        let _ = self.insert_job_event(&routed_event);

        // Route to specific client or hold until they connect
        if let Some(ref target_client) = meta.target_client_id {
            let channels = self.client_channels.lock().expect("queue lock poisoned");
            if let Some((_, tx)) = channels.get(target_client) {
                let _ = tx.send(job_id.clone());
                debug!(job_id = %job_id, client = %target_client, "job routed to client");
            } else {
                // Client not connected — hold in storage, do NOT put in default queue.
                // The job will be delivered when the target client reconnects and
                // calls drain_pending_for_client().
                debug!(job_id = %job_id, client = %target_client, "target client not connected, holding for reconnect");
            }
            return Ok(());
        }

        // Default queue — only for unpaired jobs (no target_client_id)
        {
            let mut q = self.default_pending.lock().expect("queue lock poisoned");
            q.push_back(job_id.clone());
        }
        debug!(job_id = %job_id, "job pushed to default queue (unpaired)");
        self.default_notify.notify_waiters();
        Ok(())
    }

    /// Pop the next pending job ID from the default queue, if any.
    /// Only returns unpaired jobs — paired jobs are delivered via per-client channels.
    pub fn next_job(&self) -> Option<String> {
        let mut q = self.default_pending.lock().expect("queue lock poisoned");
        q.pop_front()
    }

    /// Deliver any held jobs that are paired to this client but were queued
    /// while the client was disconnected. Called when a client reconnects.
    pub fn drain_pending_for_client(&self, client_id: &str) {
        let storage = self.storage.lock().expect("queue lock poisoned");
        let jobs = match storage.get_jobs_for_client(client_id) {
            Ok(jobs) => jobs,
            Err(e) => {
                error!(error = %e, client_id, "failed to query held jobs for client");
                return;
            }
        };
        drop(storage);

        let channels = self.client_channels.lock().expect("queue lock poisoned");
        if let Some((_, tx)) = channels.get(client_id) {
            for job in &jobs {
                if job.state == JobState::Queued {
                    let _ = tx.send(job.job_id.clone());
                    debug!(job_id = %job.job_id, client_id, "delivered held job to reconnected client");
                }
            }
        }
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

    /// Returns a future that completes when any client's pairing state changes.
    pub fn pairing_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.pairing_notify.notified()
    }

    /// Update a job's state in storage.
    pub fn update_state(&self, job_id: &str, state: JobState) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.update_job_state(job_id, state)?;
        // Look up requesting_user so the WebSocket event carries it for
        // per-user filtering on the tray app side.
        let requesting_user = storage
            .get_job(job_id)
            .ok()
            .flatten()
            .and_then(|j| j.requesting_user);
        drop(storage);

        self.emit_event(JobEvent::StateChanged {
            job_id: job_id.to_string(),
            new_state: state,
            requesting_user,
        });
        Ok(())
    }

    /// Requeue a failed job for retry. Resets state to Queued, increments retry_count,
    /// and pushes the job back into the default queue.
    pub fn requeue_job(&self, job_id: &str, error_detail: &str) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.requeue_job(job_id, error_detail)?;
        // Read back the job to check if it's paired to a specific client
        let target_client = storage
            .get_job(job_id)
            .ok()
            .flatten()
            .and_then(|j| j.target_client_id);
        drop(storage);

        // Route retries the same way as initial dispatch: paired → client channel, unpaired → default
        if let Some(ref client_id) = target_client {
            let channels = self.client_channels.lock().expect("queue lock poisoned");
            if let Some((_, tx)) = channels.get(client_id) {
                let _ = tx.send(job_id.to_string());
                debug!(job_id, client = %client_id, "requeued job routed to paired client");
            } else {
                debug!(job_id, client = %client_id, "requeued job held for disconnected paired client");
            }
        } else {
            let mut pending = self.default_pending.lock().expect("queue lock poisoned");
            pending.push_back(job_id.to_string());
            self.default_notify.notify_waiters();
        }
        Ok(())
    }

    /// Find stale jobs stuck in downloading/printing for too long.
    pub fn get_stale_jobs(&self, stale_timeout_secs: u64) -> Result<Vec<JobMetadata>> {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(stale_timeout_secs as i64);
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_stale_jobs(cutoff)
    }

    /// Retrieve a job by ID from storage.
    pub fn get_job(&self, job_id: &str) -> Result<Option<JobMetadata>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_job(job_id)
    }

    /// Return all jobs from storage.
    pub fn get_all_jobs(&self) -> Result<Vec<JobMetadata>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_all_jobs()
    }

    /// Record a job in storage without routing (used by the client to track
    /// jobs received from the server).
    pub fn record_job(&self, meta: &JobMetadata, spool_path: &str) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.insert_job(meta, spool_path)
    }

    /// Update the state of a job in storage.
    pub fn update_job_state(&self, job_id: &str, state: JobState) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.update_job_state(job_id, state)?;
        let requesting_user = storage
            .get_job(job_id)
            .ok()
            .flatten()
            .and_then(|j| j.requesting_user);
        drop(storage);

        self.emit_event(JobEvent::StateChanged {
            job_id: job_id.to_string(),
            new_state: state,
            requesting_user,
        });
        Ok(())
    }

    /// Count jobs created today.
    pub fn count_jobs_today(&self) -> Result<u64> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.count_jobs_today()
    }

    /// Get spool path for a job.
    pub fn get_spool_path(&self, job_id: &str) -> Result<Option<String>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_spool_path(job_id)
    }

    // -----------------------------------------------------------------------
    // Virtual Printer delegation
    // -----------------------------------------------------------------------

    pub fn insert_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.insert_virtual_printer(vp)?;
        if let Some(tx) = &self.reconciler_signal {
            let _ = tx.try_send(());
        }
        Ok(())
    }

    pub fn get_virtual_printer(&self, id: &str) -> Result<Option<VirtualPrinter>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_virtual_printer(id)
    }

    pub fn get_virtual_printer_by_ipp_name(
        &self,
        ipp_name: &str,
    ) -> Result<Option<VirtualPrinter>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_virtual_printer_by_ipp_name(ipp_name)
    }

    pub fn list_virtual_printers(&self) -> Result<Vec<VirtualPrinter>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.list_virtual_printers()
    }

    pub fn update_virtual_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.update_virtual_printer(vp)?;
        if let Some(tx) = &self.reconciler_signal {
            let _ = tx.try_send(());
        }
        Ok(())
    }

    pub fn delete_virtual_printer(&self, id: &str) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.delete_virtual_printer(id)
    }

    // -----------------------------------------------------------------------
    // Client delegation
    // -----------------------------------------------------------------------

    pub fn upsert_client(&self, reg: &ClientRegistration) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.upsert_client(reg)
    }

    pub fn list_clients(&self) -> Result<Vec<ClientRegistration>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.list_clients()
    }

    pub fn set_client_online(&self, machine_id: &str, online: bool) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.set_client_online(machine_id, online)
    }

    /// Update `last_seen` to now for the given client. Called on every
    /// heartbeat ping so the dashboard reflects real connection state.
    pub fn touch_client(&self, machine_id: &str) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.touch_client(machine_id)
    }

    pub fn set_all_clients_offline(&self) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.set_all_clients_offline()
    }

    pub fn get_client(&self, machine_id: &str) -> Result<Option<ClientRegistration>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_client(machine_id)
    }

    pub fn update_pairing_state(
        &self,
        machine_id: &str,
        state: devbridge_core::client_registration::PairingState,
    ) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.update_pairing_state(machine_id, state)?;
        drop(storage);
        self.pairing_notify.notify_waiters();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Job event (audit trail) delegation
    // -----------------------------------------------------------------------

    /// Get all audit events for a job.
    pub fn get_job_events(
        &self,
        job_id: &str,
    ) -> Result<Vec<devbridge_core::job_event::PrintJobEvent>> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_job_events(job_id)
    }

    pub fn get_all_job_events(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<devbridge_core::job_event::PrintJobEvent>>>
    {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.get_all_job_events()
    }

    /// Record a print pipeline event.
    pub fn insert_job_event(&self, event: &devbridge_core::job_event::PrintJobEvent) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.insert_job_event(event)
    }

    /// Delete all jobs and their events.
    pub fn clear_jobs(&self) -> Result<()> {
        let storage = self.storage.lock().expect("queue lock poisoned");
        storage.clear_jobs()
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
            retry_count: 0,
            error_detail: String::new(),
            requesting_user: None,
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
    fn test_preload_only_unpaired_jobs_into_default_queue() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Insert unpaired and paired jobs directly into storage
        {
            let storage = Storage::new(&db_path).unwrap();
            // Unpaired jobs → should go to default queue
            storage
                .insert_job(&test_job("job-unpaired-1"), "/tmp/u1.pdf")
                .unwrap();
            storage
                .insert_job(&test_job("job-unpaired-2"), "/tmp/u2.pdf")
                .unwrap();
            // Paired job → must NOT go to default queue
            let mut paired = test_job("job-paired-held");
            paired.target_client_id = Some("holla-client".into());
            storage.insert_job(&paired, "/tmp/paired.pdf").unwrap();
        }

        // Create a new queue — only unpaired should be in default queue
        let storage = Storage::new(&db_path).unwrap();
        let queue = JobQueue::new(storage).unwrap();

        assert_eq!(queue.next_job().unwrap(), "job-unpaired-1");
        assert_eq!(queue.next_job().unwrap(), "job-unpaired-2");
        assert!(
            queue.next_job().is_none(),
            "paired job must NOT be in default queue"
        );
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

    #[test]
    fn test_count_jobs_today_through_queue() {
        let (_dir, queue) = temp_queue();

        // Initially zero
        assert_eq!(queue.count_jobs_today().unwrap(), 0);

        // Insert a job, count should be 1
        queue
            .push(test_job("job-today-1"), "/tmp/t1.pdf".into())
            .unwrap();
        assert_eq!(queue.count_jobs_today().unwrap(), 1);

        // Insert another, count should be 2
        queue
            .push(test_job("job-today-2"), "/tmp/t2.pdf".into())
            .unwrap();
        assert_eq!(queue.count_jobs_today().unwrap(), 2);
    }

    #[test]
    fn test_list_virtual_printers_through_queue() {
        let (_dir, queue) = temp_queue();

        // Initially empty
        assert!(queue.list_virtual_printers().unwrap().is_empty());

        // Insert a VP and verify list returns it
        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-list".into(),
            display_name: "Test VP".into(),
            ipp_name: "test-vp".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();

        let vps = queue.list_virtual_printers().unwrap();
        assert_eq!(vps.len(), 1);
        assert_eq!(vps[0].id, "vp-list");
    }

    #[test]
    fn test_list_clients_through_queue() {
        let (_dir, queue) = temp_queue();

        // Initially empty
        assert!(queue.list_clients().unwrap().is_empty());

        // Insert a client and verify list returns it
        let reg = ClientRegistration {
            machine_id: "mc-list".into(),
            hostname: "host".into(),
            printer_names: vec!["Printer".into()],
            client_version: "0.1.0".into(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Approved,
            virtual_printer_name: None,
        };
        queue.upsert_client(&reg).unwrap();

        let clients = queue.list_clients().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].machine_id, "mc-list");
    }

    #[test]
    fn test_set_all_clients_offline_through_queue() {
        let (_dir, queue) = temp_queue();

        // Insert two online clients
        for i in 0..2 {
            let reg = ClientRegistration {
                machine_id: format!("mc-offline-{i}"),
                hostname: format!("host-{i}"),
                printer_names: vec![],
                client_version: "0.1.0".into(),
                last_seen: Utc::now(),
                is_online: true,
                pairing_state: devbridge_core::client_registration::PairingState::Approved,
                virtual_printer_name: None,
            };
            queue.upsert_client(&reg).unwrap();
        }

        // Verify they're online
        let clients = queue.list_clients().unwrap();
        assert!(clients.iter().all(|c| c.is_online));

        // Set all offline
        queue.set_all_clients_offline().unwrap();

        // Verify they're offline
        let clients = queue.list_clients().unwrap();
        assert!(clients.iter().all(|c| !c.is_online));
    }

    #[test]
    fn test_get_all_job_events_through_queue() {
        let (_dir, queue) = temp_queue();

        // Initially empty
        let map = queue.get_all_job_events().unwrap();
        assert!(map.is_empty());

        // Insert events for two jobs
        use devbridge_core::job_event::{PrintJobEvent, PrintStage};
        let e1 = PrintJobEvent::ok("job-evt-a", PrintStage::Received, "data-a");
        let e2 = PrintJobEvent::ok("job-evt-b", PrintStage::Received, "data-b");
        queue.insert_job_event(&e1).unwrap();
        queue.insert_job_event(&e2).unwrap();

        let map = queue.get_all_job_events().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("job-evt-a"));
        assert!(map.contains_key("job-evt-b"));
    }

    #[test]
    fn test_clear_jobs_through_queue() {
        let (_dir, queue) = temp_queue();

        // Insert jobs
        queue
            .push(test_job("job-clr-1"), "/tmp/c1.pdf".into())
            .unwrap();
        queue
            .push(test_job("job-clr-2"), "/tmp/c2.pdf".into())
            .unwrap();

        // Verify they exist
        assert_eq!(queue.get_all_jobs().unwrap().len(), 2);

        // Clear
        queue.clear_jobs().unwrap();

        // Verify gone
        assert_eq!(queue.get_all_jobs().unwrap().len(), 0);
    }

    #[test]
    fn test_get_stale_jobs_cutoff_direction() {
        let (_dir, queue) = temp_queue();

        // Insert a job and set it to downloading
        queue
            .push(test_job("job-stale-q"), "/tmp/stale.pdf".into())
            .unwrap();
        queue
            .update_state("job-stale-q", JobState::Downloading)
            .unwrap();

        // With a very large timeout (far in the past cutoff), no jobs should be stale
        // because the job was updated just now
        let stale = queue.get_stale_jobs(999_999).unwrap();
        assert!(
            stale.is_empty(),
            "recently updated job should not be stale with large timeout"
        );

        // With a zero-second timeout, job should be stale (cutoff = now)
        // Need to wait a tiny bit to ensure updated_at < cutoff
        std::thread::sleep(std::time::Duration::from_millis(50));
        let stale = queue.get_stale_jobs(0).unwrap();
        assert_eq!(stale.len(), 1, "job should be stale with 0s timeout");
        assert_eq!(stale[0].job_id, "job-stale-q");
    }

    #[test]
    fn test_push_emits_routed_event() {
        let (_dir, queue) = temp_queue();

        let job = test_job("job-routed-event");
        queue.push(job, "/tmp/routed-event.pdf".into()).unwrap();

        let events = queue.get_job_events("job-routed-event").unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.stage == devbridge_core::job_event::PrintStage::Routed),
            "expected a Routed event, got: {:?}",
            events
        );
    }

    #[test]
    fn test_requeue_paired_job_stays_with_paired_client() {
        let (_dir, queue) = temp_queue();

        // Register a client channel
        let mut rx = queue.register_client("client-a", "conn-1");

        // Push a paired job
        let mut job = test_job("job-paired-retry");
        job.target_client_id = Some("client-a".into());
        queue.push(job, "/tmp/paired-retry.pdf".into()).unwrap();

        // Drain the initial push from the channel
        assert!(rx.try_recv().is_ok());

        // Requeue — should go back to client-a's channel, NOT default queue
        queue
            .requeue_job("job-paired-retry", "transient error")
            .unwrap();

        // Client channel should have the requeued job
        let requeued = rx.try_recv();
        assert!(
            requeued.is_ok(),
            "requeued paired job should go to client channel"
        );
        assert_eq!(requeued.unwrap(), "job-paired-retry");

        // Default queue should be empty
        assert!(
            queue.next_job().is_none(),
            "default queue should be empty for paired retries"
        );
    }

    #[test]
    fn test_requeue_unpaired_job_goes_to_default_queue() {
        let (_dir, queue) = temp_queue();

        // Push an unpaired job
        queue
            .push(
                test_job("job-unpaired-retry"),
                "/tmp/unpaired-retry.pdf".into(),
            )
            .unwrap();

        // Drain from default queue
        assert!(queue.next_job().is_some());

        // Requeue — should go to default queue
        queue
            .requeue_job("job-unpaired-retry", "transient error")
            .unwrap();

        assert_eq!(
            queue.next_job().unwrap(),
            "job-unpaired-retry",
            "requeued unpaired job should go to default queue"
        );
    }

    #[tokio::test]
    async fn test_insert_virtual_printer_signals_reconciler() {
        let (_dir, mut queue) = temp_queue();
        let (tx, mut rx) = mpsc::channel::<()>(8);
        queue.set_reconciler_signal(tx);

        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-signal-1".into(),
            display_name: "Signal Test".into(),
            ipp_name: "signal-test".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();

        assert!(
            rx.try_recv().is_ok(),
            "expected one signal after insert_virtual_printer"
        );
    }

    #[tokio::test]
    async fn test_update_virtual_printer_signals_reconciler() {
        let (_dir, mut queue) = temp_queue();
        let (tx, mut rx) = mpsc::channel::<()>(8);
        queue.set_reconciler_signal(tx);

        let now = Utc::now();
        let mut vp = VirtualPrinter {
            id: "vp-signal-2".into(),
            display_name: "Signal Test 2".into(),
            ipp_name: "signal-test-2".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();
        // drain the insert signal
        let _ = rx.try_recv();

        vp.display_name = "Renamed".into();
        queue.update_virtual_printer(&vp).unwrap();
        assert!(
            rx.try_recv().is_ok(),
            "expected one signal after update_virtual_printer"
        );
    }

    #[tokio::test]
    async fn test_insert_virtual_printer_without_signal_does_not_panic() {
        let (_dir, queue) = temp_queue();

        let now = Utc::now();
        let vp = VirtualPrinter {
            id: "vp-nosignal".into(),
            display_name: "NoSignal".into(),
            ipp_name: "no-signal".into(),
            paired_client_id: None,
            created_at: now,
            updated_at: now,
        };
        queue.insert_virtual_printer(&vp).unwrap();
        // No reconciler wired — must not panic.
    }
}
