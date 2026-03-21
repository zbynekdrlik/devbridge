use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info};

use devbridge_core::job::JobState;
use devbridge_core::proto::print_bridge_server::{PrintBridge, PrintBridgeServer};
use devbridge_core::proto::{
    ClientIdentity, CompletionAck, JobCompletion, JobStatusUpdate, PayloadChunk, PayloadRequest,
    Ping, Pong, PrintJob, StatusAck,
};

use crate::queue::JobQueue;

const CHUNK_SIZE: usize = 64 * 1024; // 64 KB

/// gRPC service implementing the PrintBridge protocol.
#[allow(dead_code)]
pub struct DispatchService {
    queue: Arc<JobQueue>,
    spool_dir: PathBuf,
}

impl DispatchService {
    pub fn new(queue: Arc<JobQueue>, spool_dir: PathBuf) -> Self {
        Self { queue, spool_dir }
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
        info!(
            machine_id = %identity.machine_id,
            hostname = %identity.hostname,
            "client subscribed for jobs"
        );

        let (tx, rx) = mpsc::channel(32);
        let queue = Arc::clone(&self.queue);

        tokio::spawn(async move {
            loop {
                // Register for notification BEFORE checking the queue to avoid
                // race where push() notifies between next_job() and wait.
                let notified = queue.notified();

                // Try to pop a pending job
                if let Some(job_id) = queue.next_job() {
                    match queue.get_job(&job_id) {
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
                                break;
                            }
                        }
                        Ok(None) => {
                            debug!(job_id, "job not found in storage, skipping");
                        }
                        Err(e) => {
                            error!(error = %e, job_id, "failed to load job from storage");
                        }
                    }
                } else {
                    // Wait for new jobs to arrive (using pre-registered permit)
                    notified.await;
                }
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

    /// Mark a job as completed or failed.
    async fn complete_job(
        &self,
        request: Request<JobCompletion>,
    ) -> Result<Response<CompletionAck>, Status> {
        let completion = request.into_inner();

        let state = if completion.success {
            JobState::Completed
        } else {
            JobState::Failed
        };

        info!(
            job_id = %completion.job_id,
            success = completion.success,
            pages = completion.pages_printed,
            "job completion reported"
        );

        self.queue
            .update_state(&completion.job_id, state)
            .map_err(|e| Status::internal(format!("failed to complete job: {e}")))?;

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
