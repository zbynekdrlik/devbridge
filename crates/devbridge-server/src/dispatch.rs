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

use devbridge_core::client_registration::ClientRegistration;
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
        };
        if let Err(e) = self.queue.upsert_client(&reg) {
            error!(error = %e, "failed to register client");
        }
        if let Err(e) = self.queue.set_client_online(&machine_id, true) {
            error!(error = %e, "failed to set client online");
        }

        // Generate a unique ID for this connection to prevent race conditions
        // when a client reconnects before the old cleanup task runs.
        let connection_id = Uuid::new_v4().to_string();

        // Increment connected count
        self.connected_clients.fetch_add(1, Ordering::Relaxed);

        // Register for per-client job channel
        let mut client_rx = self.queue.register_client(&machine_id, &connection_id);

        let (tx, rx) = mpsc::channel(32);
        let queue = Arc::clone(&self.queue);
        let connected = Arc::clone(&self.connected_clients);
        let mid = machine_id.clone();
        let cid = connection_id.clone();

        tokio::spawn(async move {
            loop {
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

        if completion.success {
            info!(
                job_id = %completion.job_id,
                pages = completion.pages_printed,
                "job completed successfully"
            );
            self.queue
                .update_state(&completion.job_id, JobState::Completed)
                .map_err(|e| Status::internal(format!("failed to complete job: {e}")))?;
            return Ok(Response::new(CompletionAck {}));
        }

        // Failed: check if we should retry
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
