use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use chrono::Utc;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info};
use uuid::Uuid;

use devbridge_core::client_registration::{ClientRegistration, PairingState};
use devbridge_core::job::JobState;
use devbridge_core::proto::print_bridge_server::{PrintBridge, PrintBridgeServer};
use devbridge_core::proto::{
    ClientIdentity, CompletionAck, JobCompletion, JobStatusUpdate, PayloadChunk, PayloadRequest,
    Ping, Pong, PrintJob, StatusAck,
};

use crate::queue::JobQueue;

const CHUNK_SIZE: usize = 64 * 1024; // 64 KB

/// gRPC service implementing the PrintBridge protocol.
pub struct DispatchService {
    queue: Arc<JobQueue>,
    #[allow(dead_code)]
    spool_dir: PathBuf,
    connected_clients: Arc<AtomicU64>,
    max_retries: u32,
}

impl DispatchService {
    pub fn new(
        queue: Arc<JobQueue>,
        spool_dir: PathBuf,
        connected_clients: Arc<AtomicU64>,
        max_retries: u32,
    ) -> Self {
        Self {
            queue,
            spool_dir,
            connected_clients,
            max_retries,
        }
    }

    /// Start the tonic gRPC server on the given port.
    pub async fn run(self, port: u16) -> Result<()> {
        let addr = format!("0.0.0.0:{port}").parse()?;
        info!(%addr, "starting gRPC dispatch server");

        tonic::transport::Server::builder()
            .add_service(PrintBridgeServer::new(self))
            .serve(addr)
            .await?;

        Ok(())
    }
}

#[tonic::async_trait]
impl PrintBridge for DispatchService {
    type SubscribeJobsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<PrintJob, Status>> + Send>>;

    /// Stream pending jobs to a connected client.
    async fn subscribe_jobs(
        &self,
        request: Request<ClientIdentity>,
    ) -> Result<Response<Self::SubscribeJobsStream>, Status> {
        let identity = request.into_inner();
        let machine_id = identity.machine_id.clone();

        info!(
            machine_id = %identity.machine_id,
            hostname = %identity.hostname,
            "client subscribed for jobs"
        );

        // Auto-register client in storage
        let reg = ClientRegistration {
            machine_id: identity.machine_id.clone(),
            hostname: identity.hostname.clone(),
            printer_names: identity.printer_names.clone(),
            client_version: identity.client_version.clone(),
            last_seen: Utc::now(),
            is_online: true,
            pairing_state: devbridge_core::client_registration::PairingState::Pending,
            virtual_printer_name: if identity.virtual_printer_name.is_empty() {
                None
            } else {
                Some(identity.virtual_printer_name.clone())
            },
        };
        if let Err(e) = self.queue.upsert_client(&reg) {
            error!(error = %e, "failed to register client");
        }

        // Check pairing state from DB (not from `reg` — existing approved clients keep their state)
        match self.queue.get_client(&machine_id) {
            Ok(Some(stored)) => match stored.pairing_state {
                PairingState::Rejected => {
                    return Err(Status::permission_denied("client has been rejected"));
                }
                PairingState::Pending | PairingState::Approved => {
                    // Pending: allow connection but gate job delivery below
                    // Approved: full access
                }
            },
            Ok(None) => {
                // Should not happen after upsert, but treat as Pending
            }
            Err(e) => {
                error!(error = %e, "failed to read client pairing state");
            }
        }

        // Generate a unique ID for this connection to prevent race conditions
        // when a client reconnects before the old cleanup task runs.
        let connection_id = Uuid::new_v4().to_string();

        // Register the channel BEFORE setting online — this ensures the old
        // connection's cleanup sees the new connection_id in is_active_connection()
        // and skips setting is_online=false.
        let mut client_rx = self.queue.register_client(&machine_id, &connection_id);

        // Now safe to set online — old connection cleanup won't race because
        // is_active_connection() already returns false for the old connection_id.
        if let Err(e) = self.queue.set_client_online(&machine_id, true) {
            error!(error = %e, "failed to set client online");
        }

        // Increment connected count
        self.connected_clients.fetch_add(1, Ordering::Relaxed);

        let (tx, rx) = mpsc::channel(32);
        let queue = Arc::clone(&self.queue);
        let connected = Arc::clone(&self.connected_clients);
        let mid = machine_id.clone();
        let cid = connection_id.clone();

        // Drain any jobs held while this client was disconnected
        self.queue.drain_pending_for_client(&machine_id);

        tokio::spawn(async move {
            loop {
                // Gate job delivery on pairing approval — re-check each iteration
                // so approval takes effect immediately without a reconnect.
                let is_approved = queue
                    .get_client(&mid)
                    .ok()
                    .flatten()
                    .is_some_and(|c| c.pairing_state == PairingState::Approved);
                if !is_approved {
                    queue.pairing_notified().await;
                    continue;
                }

                // Register for default queue notification BEFORE checking
                let notified = queue.notified();

                // Check per-client channel first (non-blocking)
                if let Ok(job_id) = client_rx.try_recv() {
                    if send_job(&tx, &queue, &job_id).await.is_err() {
                        break;
                    }
                    continue;
                }

                // Try to pop from default queue
                if let Some(job_id) = queue.next_job() {
                    if send_job(&tx, &queue, &job_id).await.is_err() {
                        break;
                    }
                    continue;
                }

                // Wait for either a routed job or a default queue notification
                tokio::select! {
                    Some(job_id) = client_rx.recv() => {
                        if send_job(&tx, &queue, &job_id).await.is_err() {
                            break;
                        }
                    }
                    _ = notified => {
                        // Default queue may have a job - loop back to check
                    }
                }
            }

            // Cleanup on disconnect.
            // Always decrement the counter — every connection that incremented must decrement.
            // Only unregister the channel and mark offline if this is still the active connection;
            // a newer connection may have already replaced us in the registry.
            info!(machine_id = %mid, connection_id = %cid, "client disconnected");
            connected.fetch_sub(1, Ordering::Relaxed);
            if queue.is_active_connection(&mid, &cid) {
                queue.unregister_client(&mid, &cid);
                if let Err(e) = queue.set_client_online(&mid, false) {
                    error!(error = %e, "failed to set client offline");
                }
            } else {
                debug!(
                    machine_id = %mid,
                    connection_id = %cid,
                    "stale connection cleanup — channel kept for newer connection"
                );
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    type DownloadPayloadStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<PayloadChunk, Status>> + Send>>;

    /// Stream the spool file for a job in 64 KB chunks.
    async fn download_payload(
        &self,
        request: Request<PayloadRequest>,
    ) -> Result<Response<Self::DownloadPayloadStream>, Status> {
        let req = request.into_inner();
        let job_id = req.job_id.clone();
        let start_offset = req.offset as usize;

        debug!(job_id = %job_id, offset = start_offset, "payload download requested");

        // Look up the spool path
        let spool_path = self
            .queue
            .get_spool_path(&job_id)
            .map_err(|e| Status::internal(format!("storage error: {e}")))?
            .ok_or_else(|| Status::not_found(format!("job {job_id} not found")))?;

        let data = tokio::fs::read(&spool_path)
            .await
            .map_err(|e| Status::internal(format!("failed to read spool file: {e}")))?;

        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            let total = data.len();
            let mut offset = start_offset;

            while offset < total {
                let end = (offset + CHUNK_SIZE).min(total);
                let chunk = PayloadChunk {
                    job_id: job_id.clone(),
                    offset: offset as u64,
                    data: data[offset..end].to_vec(),
                    is_last: end >= total,
                };
                offset = end;

                if tx.send(Ok(chunk)).await.is_err() {
                    debug!("client disconnected during payload download");
                    break;
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    /// Receive a stream of status updates from the client.
    async fn report_status(
        &self,
        request: Request<Streaming<JobStatusUpdate>>,
    ) -> Result<Response<StatusAck>, Status> {
        let mut stream = request.into_inner();

        while let Some(update) = stream.next().await {
            let update = update?;
            let state = proto_state_to_core(update.state());
            debug!(
                job_id = %update.job_id,
                ?state,
                message = %update.message,
                "received status update"
            );

            self.queue
                .update_state(&update.job_id, state)
                .map_err(|e| Status::internal(format!("failed to update state: {e}")))?;

            // Store audit event if message contains PrintJobEvent JSON
            if !update.message.is_empty()
                && let Ok(event) = serde_json::from_str::<devbridge_core::job_event::PrintJobEvent>(
                    &update.message,
                )
            {
                let _ = self.queue.insert_job_event(&event);
            }
        }

        Ok(Response::new(StatusAck {}))
    }

    /// Mark a job as completed or failed. Failed jobs are automatically
    /// requeued for retry if under the max_retries limit.
    async fn complete_job(
        &self,
        request: Request<JobCompletion>,
    ) -> Result<Response<CompletionAck>, Status> {
        let completion = request.into_inner();

        // Check for client mismatch on paired jobs
        if let Ok(Some(job)) = self.queue.get_job(&completion.job_id) {
            if let Some(ref expected_client) = job.target_client_id {
                if !completion.client_id.is_empty() && completion.client_id != *expected_client {
                    info!(
                        job_id = %completion.job_id,
                        expected = %expected_client,
                        actual = %completion.client_id,
                        "job completed by different client than routed"
                    );
                    let warning = devbridge_core::job_event::PrintJobEvent {
                        job_id: completion.job_id.clone(),
                        stage: devbridge_core::job_event::PrintStage::Completed,
                        success: true,
                        detail: format!(
                            "Completed by {} (originally routed to {})",
                            completion.client_id, expected_client
                        ),
                        verification_method: "client_mismatch".into(),
                        verification_evidence: format!(
                            "expected={}, actual={}",
                            expected_client, completion.client_id
                        ),
                        timestamp: chrono::Utc::now(),
                    };
                    let _ = self.queue.insert_job_event(&warning);
                }
            }
        }

        if completion.success {
            info!(
                job_id = %completion.job_id,
                pages = completion.pages_printed,
                verification = %completion.verification_method,
                "job completed successfully"
            );

            // Store completion event with verification evidence
            let event = devbridge_core::job_event::PrintJobEvent {
                job_id: completion.job_id.clone(),
                stage: devbridge_core::job_event::PrintStage::Completed,
                success: true,
                detail: format!(
                    "Completed ({})",
                    if completion.verification_method.is_empty() {
                        "no verification"
                    } else {
                        &completion.verification_method
                    }
                ),
                verification_method: completion.verification_method,
                verification_evidence: completion.verification_evidence,
                timestamp: chrono::Utc::now(),
            };
            let _ = self.queue.insert_job_event(&event);

            self.queue
                .update_state(&completion.job_id, JobState::Completed)
                .map_err(|e| Status::internal(format!("failed to complete job: {e}")))?;
            return Ok(Response::new(CompletionAck {}));
        }

        // Failed: store failure event with evidence
        let event = devbridge_core::job_event::PrintJobEvent {
            job_id: completion.job_id.clone(),
            stage: devbridge_core::job_event::PrintStage::Failed,
            success: false,
            detail: completion.error_detail.clone(),
            verification_method: completion.verification_method,
            verification_evidence: completion.verification_evidence,
            timestamp: chrono::Utc::now(),
        };
        let _ = self.queue.insert_job_event(&event);

        // Check if we should retry
        let should_retry = self
            .queue
            .get_job(&completion.job_id)
            .ok()
            .flatten()
            .is_some_and(|job| job.retry_count < self.max_retries);

        if should_retry {
            info!(
                job_id = %completion.job_id,
                error = %completion.error_detail,
                max_retries = self.max_retries,
                "job failed, requeuing for retry"
            );
            self.queue
                .requeue_job(&completion.job_id, &completion.error_detail)
                .map_err(|e| Status::internal(format!("failed to requeue job: {e}")))?;
        } else {
            info!(
                job_id = %completion.job_id,
                error = %completion.error_detail,
                "job failed permanently (retry limit reached)"
            );
            self.queue
                .update_state(&completion.job_id, JobState::Failed)
                .map_err(|e| Status::internal(format!("failed to mark job failed: {e}")))?;
        }

        Ok(Response::new(CompletionAck {}))
    }

    type HeartbeatStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<Pong, Status>> + Send>>;

    /// Echo a Pong for each Ping received.
    async fn heartbeat(
        &self,
        request: Request<Streaming<Ping>>,
    ) -> Result<Response<Self::HeartbeatStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(8);

        tokio::spawn(async move {
            while let Some(ping) = stream.next().await {
                match ping {
                    Ok(p) => {
                        let pong = Pong {
                            timestamp: p.timestamp,
                        };
                        if tx.send(Ok(pong)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "heartbeat stream error");
                        break;
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Send a job over the gRPC stream. Returns Err if client disconnected.
async fn send_job(
    tx: &mpsc::Sender<Result<PrintJob, Status>>,
    queue: &JobQueue,
    job_id: &str,
) -> Result<(), ()> {
    match queue.get_job(job_id) {
        Ok(Some(meta)) => {
            let created_at = Some(prost_types::Timestamp {
                seconds: meta.created_at.timestamp(),
                nanos: meta.created_at.timestamp_subsec_nanos() as i32,
            });

            let print_job = PrintJob {
                job_id: meta.job_id,
                target_printer: meta.target_printer,
                document_name: meta.document_name,
                copies: meta.copies,
                paper_size: meta.paper_size,
                duplex: meta.duplex,
                color: meta.color,
                payload_size: meta.payload_size,
                payload_sha256: meta.payload_sha256,
                created_at,
            };

            if tx.send(Ok(print_job)).await.is_err() {
                debug!("client disconnected, stopping job stream");
                return Err(());
            }
        }
        Ok(None) => {
            debug!(job_id, "job not found in storage, skipping");
        }
        Err(e) => {
            error!(error = %e, job_id, "failed to load job from storage");
        }
    }
    Ok(())
}

/// Map proto JobState enum to core JobState.
fn proto_state_to_core(s: devbridge_core::proto::JobState) -> JobState {
    match s {
        devbridge_core::proto::JobState::Unspecified => JobState::Queued,
        devbridge_core::proto::JobState::Queued => JobState::Queued,
        devbridge_core::proto::JobState::Downloading => JobState::Downloading,
        devbridge_core::proto::JobState::Printing => JobState::Printing,
        devbridge_core::proto::JobState::Completed => JobState::Completed,
        devbridge_core::proto::JobState::Failed => JobState::Failed,
        devbridge_core::proto::JobState::Cancelled => JobState::Cancelled,
        devbridge_core::proto::JobState::Rendering => JobState::Printing,
        devbridge_core::proto::JobState::Sending => JobState::Printing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::JobQueue;
    use crate::storage::Storage;
    use devbridge_core::job::JobMetadata;

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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn temp_dispatch(max_retries: u32) -> (tempfile::TempDir, Arc<JobQueue>, DispatchService) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::new(&db_path).unwrap();
        let queue = Arc::new(JobQueue::new(storage).unwrap());
        let spool_dir = dir.path().join("spool");
        std::fs::create_dir_all(&spool_dir).unwrap();
        let service = DispatchService::new(
            Arc::clone(&queue),
            spool_dir,
            Arc::new(AtomicU64::new(0)),
            max_retries,
        );
        (dir, queue, service)
    }

    #[test]
    fn test_chunk_size_is_64kb() {
        // Kill mutant: replace * with + in 64 * 1024
        // 64 * 1024 = 65536, but 64 + 1024 = 1088
        assert_eq!(CHUNK_SIZE, 65536);
    }

    #[test]
    fn test_proto_state_to_core_mapping() {
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Unspecified),
            JobState::Queued
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Queued),
            JobState::Queued
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Downloading),
            JobState::Downloading
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Printing),
            JobState::Printing
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Completed),
            JobState::Completed
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Failed),
            JobState::Failed
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Cancelled),
            JobState::Cancelled
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Rendering),
            JobState::Printing
        );
        assert_eq!(
            proto_state_to_core(devbridge_core::proto::JobState::Sending),
            JobState::Printing
        );
    }

    #[tokio::test]
    async fn test_complete_job_success_sets_completed() {
        let (_dir, queue, service) = temp_dispatch(3);

        // Insert a job
        queue
            .push(test_job("job-ok"), "/tmp/ok.pdf".into())
            .unwrap();

        let completion = JobCompletion {
            job_id: "job-ok".into(),
            success: true,
            pages_printed: 1,
            error_detail: String::new(),
            printer_status: String::new(),
            spooler_status: String::new(),
            verification_method: String::new(),
            verification_evidence: String::new(),
            client_id: String::new(),
        };
        let resp = service
            .complete_job(Request::new(completion))
            .await
            .unwrap();
        assert_eq!(resp.into_inner(), CompletionAck {});

        let job = queue.get_job("job-ok").unwrap().unwrap();
        assert_eq!(job.state, JobState::Completed);
    }

    #[tokio::test]
    async fn test_complete_job_failure_requeues_under_max_retries() {
        let (_dir, queue, service) = temp_dispatch(3);

        // Insert a job with retry_count = 0
        queue
            .push(test_job("job-retry"), "/tmp/retry.pdf".into())
            .unwrap();

        let completion = JobCompletion {
            job_id: "job-retry".into(),
            success: false,
            pages_printed: 0,
            error_detail: "printer jam".into(),
            printer_status: String::new(),
            spooler_status: String::new(),
            verification_method: String::new(),
            verification_evidence: String::new(),
            client_id: String::new(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let job = queue.get_job("job-retry").unwrap().unwrap();
        // Should be requeued (state = Queued, retry_count = 1)
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.retry_count, 1);
    }

    #[tokio::test]
    async fn test_complete_job_failure_at_max_retries_fails_permanently() {
        let (_dir, queue, service) = temp_dispatch(2);

        // Insert a job and requeue it twice to reach retry_count=2
        queue
            .push(test_job("job-maxretry"), "/tmp/maxretry.pdf".into())
            .unwrap();
        queue.requeue_job("job-maxretry", "err1").unwrap(); // retry_count = 1
        queue.requeue_job("job-maxretry", "err2").unwrap(); // retry_count = 2

        let completion = JobCompletion {
            job_id: "job-maxretry".into(),
            success: false,
            pages_printed: 0,
            error_detail: "permanent error".into(),
            printer_status: String::new(),
            spooler_status: String::new(),
            verification_method: String::new(),
            verification_evidence: String::new(),
            client_id: String::new(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let loaded = queue.get_job("job-maxretry").unwrap().unwrap();
        assert_eq!(loaded.state, JobState::Failed);
    }

    #[tokio::test]
    async fn test_complete_job_failure_one_below_max_retries_requeues() {
        let (_dir, queue, service) = temp_dispatch(3);

        // Job with retry_count = 2, max_retries = 3 → should requeue (2 < 3)
        queue
            .push(test_job("job-below-max"), "/tmp/below.pdf".into())
            .unwrap();
        queue.requeue_job("job-below-max", "err1").unwrap(); // retry_count = 1
        queue.requeue_job("job-below-max", "err2").unwrap(); // retry_count = 2

        let completion = JobCompletion {
            job_id: "job-below-max".into(),
            success: false,
            pages_printed: 0,
            error_detail: "transient".into(),
            printer_status: String::new(),
            spooler_status: String::new(),
            verification_method: String::new(),
            verification_evidence: String::new(),
            client_id: String::new(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let loaded = queue.get_job("job-below-max").unwrap().unwrap();
        assert_eq!(loaded.state, JobState::Queued);
        assert_eq!(loaded.retry_count, 3);
    }

    #[tokio::test]
    async fn test_complete_job_stores_verification_evidence() {
        let (_dir, queue, service) = temp_dispatch(3);
        queue
            .push(test_job("job-verified"), "/tmp/verified.pdf".into())
            .unwrap();

        let completion = JobCompletion {
            job_id: "job-verified".into(),
            success: true,
            pages_printed: 1,
            error_detail: String::new(),
            printer_status: "delivered".into(),
            spooler_status: "windows_spooler".into(),
            verification_method: "eventid_307".into(),
            verification_evidence: "EventID 307: Document 42, eholla printer, USB002, 245KB".into(),
            client_id: "holla-client".into(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let job = queue.get_job("job-verified").unwrap().unwrap();
        assert_eq!(job.state, JobState::Completed);

        let events = queue.get_job_events("job-verified").unwrap();
        let completed_event = events
            .iter()
            .find(|e| e.stage == devbridge_core::job_event::PrintStage::Completed);
        assert!(completed_event.is_some(), "should have a Completed event");
        let ev = completed_event.unwrap();
        assert_eq!(ev.verification_method, "eventid_307");
        assert!(ev.verification_evidence.contains("EventID 307"));
    }

    #[tokio::test]
    async fn test_complete_job_client_mismatch_emits_warning() {
        let (_dir, queue, service) = temp_dispatch(3);

        let mut job = test_job("job-mismatch");
        job.target_client_id = Some("holla-client".into());
        queue.push(job, "/tmp/mismatch.pdf".into()).unwrap();

        let completion = JobCompletion {
            job_id: "job-mismatch".into(),
            success: true,
            pages_printed: 1,
            error_detail: String::new(),
            printer_status: "delivered".into(),
            spooler_status: "direct_ipp".into(),
            verification_method: "ipp_job_state".into(),
            verification_evidence: "IPP job-state=9 (completed)".into(),
            client_id: "pjpos-client".into(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let events = queue.get_job_events("job-mismatch").unwrap();
        let mismatch_event = events
            .iter()
            .find(|e| e.verification_method == "client_mismatch");
        assert!(
            mismatch_event.is_some(),
            "should have a client_mismatch warning"
        );
        let ev = mismatch_event.unwrap();
        assert!(ev.detail.contains("pjpos-client"));
        assert!(ev.detail.contains("holla-client"));
    }

    #[tokio::test]
    async fn test_complete_job_unpaired_no_mismatch_warning() {
        let (_dir, queue, service) = temp_dispatch(3);
        queue
            .push(test_job("job-unpaired"), "/tmp/unpaired.pdf".into())
            .unwrap();

        let completion = JobCompletion {
            job_id: "job-unpaired".into(),
            success: true,
            pages_printed: 1,
            error_detail: String::new(),
            printer_status: "delivered".into(),
            spooler_status: "direct_ipp".into(),
            verification_method: "ipp_job_state".into(),
            verification_evidence: "IPP job-state=9 (completed)".into(),
            client_id: "any-client".into(),
        };
        service
            .complete_job(Request::new(completion))
            .await
            .unwrap();

        let events = queue.get_job_events("job-unpaired").unwrap();
        let mismatch = events
            .iter()
            .any(|e| e.verification_method == "client_mismatch");
        assert!(!mismatch, "unpaired jobs should not get mismatch warnings");
    }
}
