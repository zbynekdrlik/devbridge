use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::Utc;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use devbridge_core::client_registration::{ClientRegistration, PairingState};
use devbridge_core::job::JobState;
use devbridge_core::job_event::{PrintJobEvent, PrintStage};
use devbridge_core::proto::print_bridge_server::{PrintBridge, PrintBridgeServer};
use devbridge_core::proto::{
    CertificateReply, CertificateRequest, ClientIdentity, CompletionAck, JobCompletion,
    JobStatusUpdate, PayloadChunk, PayloadRequest, Ping, Pong, PrintJob, SerialAck, SerialData,
    StatusAck,
};

use crate::cert::{Caller, CertificateIssuer, disabled_reply, same_peer};
use crate::queue::JobQueue;
use crate::serial_bridge::SerialBridgeManager;

const CHUNK_SIZE: usize = 64 * 1024; // 64 KB

/// gRPC service implementing the PrintBridge protocol.
pub struct DispatchService {
    queue: Arc<JobQueue>,
    #[allow(dead_code)]
    spool_dir: PathBuf,
    connected_clients: Arc<AtomicU64>,
    max_retries: u32,
    retry_delay_secs: u64,
    serial_bridge: Arc<SerialBridgeManager>,
    /// Issues the stores' LAN API certificates (`[server.acme]`, #101).
    certificates: Option<Arc<CertificateIssuer>>,
    /// The peer address of every client's current job stream (#101): a
    /// certificate request must come from it.
    stream_peers: Arc<StreamPeers>,
}

/// machine_id → (connection_id, peer address) of the current
/// `SubscribeJobs` stream: a reconnect replaces it, its cleanup (once the
/// server notices the stream ended) forgets it.
#[derive(Default)]
pub struct StreamPeers(Mutex<HashMap<String, StreamPeer>>);

/// (connection_id, peer address) of one client's current job stream.
type StreamPeer = (String, Option<IpAddr>);

impl StreamPeers {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, StreamPeer>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `connection_id` of `machine_id` is now live from `peer`.
    pub fn connected(&self, machine_id: &str, connection_id: &str, peer: Option<IpAddr>) {
        self.map()
            .insert(machine_id.to_string(), (connection_id.to_string(), peer));
    }

    /// `connection_id` ended — forgotten unless a newer one replaced it.
    pub fn disconnected(&self, machine_id: &str, connection_id: &str) {
        let mut map = self.map();
        if map
            .get(machine_id)
            .is_some_and(|(live, _)| live == connection_id)
        {
            map.remove(machine_id);
        }
    }

    /// The peer address of `machine_id`'s live stream.
    pub fn peer_of(&self, machine_id: &str) -> Option<IpAddr> {
        self.map().get(machine_id).and_then(|(_, peer)| *peer)
    }
}

impl DispatchService {
    pub fn new(
        queue: Arc<JobQueue>,
        spool_dir: PathBuf,
        connected_clients: Arc<AtomicU64>,
        max_retries: u32,
        retry_delay_secs: u64,
        serial_bridge: Arc<SerialBridgeManager>,
    ) -> Self {
        Self {
            queue,
            spool_dir,
            connected_clients,
            max_retries,
            retry_delay_secs,
            serial_bridge,
            certificates: None,
            stream_peers: Arc::new(StreamPeers::default()),
        }
    }

    /// Answer `RequestCertificate` with `issuer` (#101); without it every
    /// request is answered `acme_disabled`.
    pub fn with_certificates(mut self, issuer: Arc<CertificateIssuer>) -> Self {
        self.certificates = Some(issuer);
        self
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
        let peer = request.remote_addr().map(|addr| addr.ip());
        let identity = request.into_inner();
        let machine_id = identity.machine_id.clone();

        info!(
            machine_id = %identity.machine_id,
            hostname = %identity.hostname,
            virtual_printer_driver = %identity.virtual_printer_driver,
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
            virtual_printer_driver: if identity.virtual_printer_driver.is_empty() {
                None
            } else {
                Some(identity.virtual_printer_driver.clone())
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
        self.stream_peers
            .connected(&machine_id, &connection_id, peer);
        info!(machine_id = %machine_id, connection_id = %connection_id, peer = ?peer, "job stream connected");

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
        let stream_peers = Arc::clone(&self.stream_peers);

        // Drain any jobs held while this client was disconnected
        self.queue.drain_pending_for_client(&machine_id);

        tokio::spawn(async move {
            loop {
                // Gate job delivery on pairing approval — re-check each iteration
                // so approval takes effect immediately without a reconnect.
                let registration = queue.get_client(&mid).ok().flatten();
                let is_approved = registration
                    .as_ref()
                    .is_some_and(|c| c.pairing_state == PairingState::Approved);
                // A vendor-driver (RAW label) client gets ONLY its paired
                // printer's jobs, never default-queue PDFs (#88).
                let takes_default_queue = registration
                    .as_ref()
                    .is_none_or(ClientRegistration::takes_default_queue);
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
                if !takes_default_queue {
                    debug!(machine_id = %mid, "RAW client — skipping default queue (#88)");
                } else if let Some(job_id) = queue.next_job() {
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
            stream_peers.disconnected(&mid, &cid);
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
            apply_status_update(&self.queue, &update?)?;
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
        if let Ok(Some(job)) = self.queue.get_job(&completion.job_id)
            && let Some(ref expected_client) = job.target_client_id
            && !completion.client_id.is_empty()
            && completion.client_id != *expected_client
        {
            info!(
                job_id = %completion.job_id,
                expected = %expected_client,
                actual = %completion.client_id,
                "job completed by different client than routed"
            );
            let warning = devbridge_core::job_event::PrintJobEvent {
                job_id: completion.job_id.clone(),
                requesting_user: None,
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
                requesting_user: None,
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

        if completion.unconfirmed {
            // #114 (owner ruling: neposielať znova): the printer ACCEPTED the
            // job, its end is unknown — a resend could print it twice.
            // Terminal, never retried; the store reprints by hand if a page
            // is really missing.
            warn!(
                job_id = %completion.job_id,
                client = %completion.client_id,
                detail = %completion.error_detail,
                "job UNCONFIRMED: the printer accepted it but its end was never confirmed - NOT resent (#114)"
            );
            let event = PrintJobEvent {
                job_id: completion.job_id.clone(),
                requesting_user: None,
                stage: PrintStage::Unconfirmed,
                success: false,
                detail: completion.error_detail.clone(),
                verification_method: completion.verification_method,
                verification_evidence: completion.verification_evidence,
                timestamp: chrono::Utc::now(),
            };
            let _ = self.queue.insert_job_event(&event);
            self.queue
                .update_state(&completion.job_id, JobState::Unconfirmed)
                .map_err(|e| Status::internal(format!("failed to mark job unconfirmed: {e}")))?;
            return Ok(Response::new(CompletionAck {}));
        }

        // Failed: store failure event with evidence
        let event = devbridge_core::job_event::PrintJobEvent {
            job_id: completion.job_id.clone(),
            requesting_user: None,
            stage: devbridge_core::job_event::PrintStage::Failed,
            success: false,
            detail: completion.error_detail.clone(),
            verification_method: completion.verification_method,
            verification_evidence: completion.verification_evidence,
            timestamp: chrono::Utc::now(),
        };
        let _ = self.queue.insert_job_event(&event);

        // Mark it Failed — unless it already FINISHED (#114): a late failure
        // of a completed / cancelled / unconfirmed job (e.g. the abandoned
        // backend task's result after the job ended unconfirmed) neither
        // reopens it nor schedules a retry that would resend a job the
        // printer may have printed. One conditional write decides it; the
        // failure event above stays on record.
        //
        // State transition otherwise: Downloading/Printing -> Failed (now) ->
        // Queued (when the spawned task's backoff elapses). Marking Failed
        // immediately keeps the stale-job reaper (`get_stale_jobs` matches
        // state IN ('downloading','printing')) from double-requeueing us
        // while the backoff sleeps. Once the retry delay exceeds the
        // stale_timeout (retry_count >= 4 at default config), without this
        // both this spawned task and the reaper would requeue the same job,
        // burning two retry slots per failure.
        let marked_failed = self
            .queue
            .update_state_unless_terminal(&completion.job_id, JobState::Failed)
            .map_err(|e| Status::internal(format!("failed to mark job failed: {e}")))?;
        if !marked_failed {
            info!(
                job_id = %completion.job_id,
                error = %completion.error_detail,
                "job failure NOT retried: the job is already finished (#114)"
            );
            return Ok(Response::new(CompletionAck {}));
        }

        // Check if we should retry (fold the lookup so we don't hit
        // storage twice on the retry path).
        let existing_job = self.queue.get_job(&completion.job_id).ok().flatten();
        let should_retry = existing_job
            .as_ref()
            .is_some_and(|job| job.retry_count < self.max_retries);

        if should_retry {
            // Exponential backoff: delay = retry_delay * 2^retry_count.
            // Requeue happens in a spawned task so CompleteJob RPC returns
            // immediately. Without this, a failing job (e.g. Canon busy
            // with 0x0507) would get hammered with 4 retries in 30s —
            // keeping the printer stuck and wasting cycles. The requeue
            // itself is conditional (`Storage::requeue_job`): a job that
            // finishes during the backoff is never resent.
            let current_retry = existing_job.as_ref().map(|j| j.retry_count).unwrap_or(0);
            let delay = std::time::Duration::from_secs(
                self.retry_delay_secs
                    .saturating_mul(1u64 << current_retry.min(6)),
            );
            info!(
                job_id = %completion.job_id,
                error = %completion.error_detail,
                max_retries = self.max_retries,
                retry_in_secs = delay.as_secs(),
                "job failed, scheduling retry with backoff"
            );
            let queue = Arc::clone(&self.queue);
            let job_id = completion.job_id.clone();
            let err_detail = completion.error_detail.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Err(e) = queue.requeue_job(&job_id, &err_detail) {
                    tracing::warn!(job_id = %job_id, error = %e, "delayed requeue failed");
                }
            });
        } else {
            info!(
                job_id = %completion.job_id,
                error = %completion.error_detail,
                "job failed permanently (retry limit reached)"
            );
        }

        Ok(Response::new(CompletionAck {}))
    }

    type StreamSerialDataStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<SerialAck, Status>> + Send>>;

    /// Receive a stream of serial data (barcode scans) from a client and
    /// acknowledge each message. Forwarding to virtual COM ports is handled
    /// by the serial bridge manager (wired in from the service layer).
    async fn stream_serial_data(
        &self,
        request: Request<Streaming<SerialData>>,
    ) -> Result<Response<Self::StreamSerialDataStream>, Status> {
        info!("StreamSerialData: new client stream established");
        let mut stream = request.into_inner();
        let serial_bridge = Arc::clone(&self.serial_bridge);
        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            let mut msg_count = 0u64;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(data) => {
                        msg_count += 1;
                        let ok = match serial_bridge.write(&data.client_id, &data.data).await {
                            Ok(()) => {
                                info!(
                                    client = %data.client_id,
                                    bytes = data.data.len(),
                                    total_msg = msg_count,
                                    "serial data forwarded to virtual COM port"
                                );
                                true
                            }
                            Err(e) => {
                                warn!(
                                    client = %data.client_id,
                                    error = %e,
                                    total_msg = msg_count,
                                    "serial bridge write failed"
                                );
                                false
                            }
                        };
                        let _ = tx.send(Ok(SerialAck { ok })).await;
                    }
                    Err(e) => {
                        info!(
                            error = %e,
                            total_msg = msg_count,
                            "StreamSerialData stream ended (client disconnected or error)"
                        );
                        break;
                    }
                }
            }
            info!(total_msg = msg_count, "StreamSerialData handler exiting");
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    type HeartbeatStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<Pong, Status>> + Send>>;

    /// Echo a Pong for each Ping received, and update last_seen for the
    /// client identified by machine_id. This keeps the dashboard's
    /// "last_seen" fresh even for idle clients (no job traffic), so the
    /// online indicator reflects reality instead of showing the last
    /// subscribe_jobs timestamp.
    async fn heartbeat(
        &self,
        request: Request<Streaming<Ping>>,
    ) -> Result<Response<Self::HeartbeatStream>, Status> {
        let mut stream = request.into_inner();
        let queue = Arc::clone(&self.queue);
        let (tx, rx) = mpsc::channel(8);

        tokio::spawn(async move {
            while let Some(ping) = stream.next().await {
                match ping {
                    Ok(p) => {
                        if !p.machine_id.is_empty()
                            && let Err(e) = queue.touch_client(&p.machine_id)
                        {
                            debug!(machine_id = %p.machine_id, error = %e, "heartbeat touch_client failed");
                        }
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

    /// LAN API certificate of a store (#101): only an APPROVED client, only
    /// from the address of its live job stream, only for its own name
    /// (`crate::cert`).
    async fn request_certificate(
        &self,
        request: Request<CertificateRequest>,
    ) -> Result<Response<CertificateReply>, Status> {
        let peer = request.remote_addr().map(|addr| addr.ip());
        let request = request.into_inner();
        let Some(issuer) = &self.certificates else {
            warn!(client_id = %request.client_id, peer = ?peer, "certificate requested, but [server.acme] is not enabled");
            return Ok(Response::new(disabled_reply()));
        };
        let stream_peer = self.stream_peers.peer_of(&request.client_id);
        info!(client_id = %request.client_id, hostname = %request.hostname, peer = ?peer, stream_peer = ?stream_peer, "LAN API certificate requested");
        let caller = Caller {
            approved: matches!(
                self.queue.get_client(&request.client_id),
                Ok(Some(client)) if client.pairing_state == PairingState::Approved
            ),
            on_its_stream: same_peer(stream_peer, peer),
        };
        Ok(Response::new(issuer.answer(&request, caller).await))
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
                // Source of truth for the client dashboard (issue #52). The
                // client mirrors this into its stored JobMetadata so its own
                // idempotent insert_job upsert no longer overwrites the count
                // with 0 during a server-driven retry storm.
                retry_count: meta.retry_count,
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

/// One message of a client's status stream (`report_status`): the job's new
/// state and the audit event (`PrintJobEvent` JSON) the message carries.
///
/// The status stream races CompleteJob, so a FINISHED job (completed,
/// cancelled, unconfirmed) keeps its state (#114): a late `printing` would
/// hand it to the stale reaper, a late `failed` to a retry — a job the
/// printer may have printed sent again. The condition is in the UPDATE
/// (`update_state_unless_terminal`), never a read before the write. The audit
/// event is stored either way.
fn apply_status_update(queue: &JobQueue, update: &JobStatusUpdate) -> Result<(), Status> {
    let event = if update.message.is_empty() {
        None
    } else {
        serde_json::from_str::<PrintJobEvent>(&update.message).ok()
    };
    let state = status_update_state(
        proto_state_to_core(update.state()),
        event.as_ref().map(|e| e.stage),
    );
    let applied = queue
        .update_state_unless_terminal(&update.job_id, state)
        .map_err(|e| Status::internal(format!("failed to update state: {e}")))?;
    debug!(
        job_id = %update.job_id,
        ?state,
        applied,
        message = %update.message,
        "received status update"
    );

    // Store the audit event
    if let Some(event) = &event {
        let _ = queue.insert_job_event(event);
    }
    Ok(())
}

/// The state a client's status update stores (#114). The client's
/// `unconfirmed` event travels as FAILED on the wire (a server without #114
/// would read a new value as queued and re-dispatch the job); this server
/// reads the event's stage and stores `unconfirmed` at once — never a
/// transient `failed` (a "Print Failed" tray notification, an error toast).
fn status_update_state(wire: JobState, stage: Option<PrintStage>) -> JobState {
    if stage == Some(PrintStage::Unconfirmed) {
        JobState::Unconfirmed
    } else {
        wire
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
        devbridge_core::proto::JobState::Rendering => JobState::Printing,
        devbridge_core::proto::JobState::Sending => JobState::Printing,
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
