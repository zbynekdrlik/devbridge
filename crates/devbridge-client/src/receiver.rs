use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use devbridge_core::config::{ClientConfig, SerialBridgeClientConfig};
use devbridge_core::job::{JobMetadata, JobState};
use devbridge_core::job_event::PrintStage;
use devbridge_core::proto::print_bridge_client::PrintBridgeClient;
use devbridge_core::proto::{
    ClientIdentity, JobCompletion, JobStatusUpdate, PayloadRequest, PrintJob, SerialData,
};
use devbridge_server::queue::JobQueue;

/// gRPC client that subscribes to print jobs from the server.
pub struct Receiver {
    server_address: String,
    machine_id: String,
    hostname: String,
    reconnect_interval: Duration,
    max_reconnect_interval: Duration,
    print_backend: String,
    printer_address: Option<String>,
    ghostscript_device: String,
    ghostscript_resolution: u32,
    printer_tls: bool,
    printer_display_name: Option<String>,
    virtual_printer_name: Option<String>,
    print_proxy_url: Option<String>,
    serial_bridge_config: SerialBridgeClientConfig,
}

impl Receiver {
    pub fn new(config: &ClientConfig) -> Self {
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".into());

        let machine_id = if let Some(ref id) = config.client_id {
            id.clone()
        } else {
            let mut hasher = Sha256::new();
            hasher.update(hostname.as_bytes());
            format!("{:x}", hasher.finalize())[..16].to_string()
        };

        Self {
            server_address: config.server_address.clone(),
            machine_id,
            hostname,
            reconnect_interval: Duration::from_secs(config.reconnect_interval_secs),
            max_reconnect_interval: Duration::from_secs(config.max_reconnect_interval_secs),
            print_backend: config.print_backend.clone(),
            printer_address: config.printer_address.clone(),
            ghostscript_device: config.ghostscript_device.clone(),
            ghostscript_resolution: config.ghostscript_resolution,
            printer_tls: config.printer_tls,
            printer_display_name: config.printer_display_name.clone(),
            virtual_printer_name: config.virtual_printer_name.clone(),
            print_proxy_url: config.print_proxy_url.clone(),
            serial_bridge_config: config.serial_bridge.clone(),
        }
    }

    async fn connect(&self) -> Result<PrintBridgeClient<Channel>> {
        let endpoint = Channel::from_shared(format!("http://{}", self.server_address))?
            .connect_timeout(std::time::Duration::from_secs(10));
        info!(server = %self.server_address, "connecting to server");
        let client = PrintBridgeClient::connect(endpoint).await?;
        Ok(client)
    }

    /// Main loop: connect, subscribe, download and print jobs. Reconnects on failure.
    pub async fn run(
        self,
        spool_dir: PathBuf,
        target_printer: Arc<RwLock<String>>,
        queue: Option<Arc<JobQueue>>,
    ) -> Result<()> {
        let mut backoff = self.reconnect_interval;

        loop {
            match self
                .run_inner(&spool_dir, Arc::clone(&target_printer), queue.as_ref())
                .await
            {
                Ok(()) => {
                    info!("connection closed gracefully");
                    backoff = self.reconnect_interval;
                }
                Err(e) => {
                    error!(error = %e, "connection error");
                }
            }

            // Enforce a minimum backoff of 1s regardless of config so the
            // serial bridge reader has time to release COM5 after an error
            // exit. The SerialCleanup guard signals shutdown synchronously
            // on drop, but the reader's blocking read has a 100ms timeout
            // and may take up to ~200ms to actually let go of the port.
            // If a user sets reconnect_interval_secs=0, the next connect
            // attempt would race and the new reader gets "Access denied".
            let effective_delay = backoff.max(Duration::from_secs(1));
            warn!(delay = ?effective_delay, "reconnecting after delay");
            tokio::time::sleep(effective_delay).await;
            backoff = (backoff * 2).min(self.max_reconnect_interval);
        }
    }

    async fn run_inner(
        &self,
        spool_dir: &Path,
        target_printer: Arc<RwLock<String>>,
        queue: Option<&Arc<JobQueue>>,
    ) -> Result<()> {
        let mut client = self.connect().await?;

        let printer_names = match crate::printer::list_printers() {
            Ok(printers) => printers.iter().map(|p| p.name.clone()).collect(),
            Err(e) => {
                warn!(error = %e, "failed to list printers, sending target only");
                vec![target_printer.read().await.clone()]
            }
        };

        let identity = ClientIdentity {
            machine_id: self.machine_id.clone(),
            hostname: self.hostname.clone(),
            printer_names,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            virtual_printer_name: self.virtual_printer_name.clone().unwrap_or_default(),
        };

        info!("subscribing to jobs");
        let mut stream = client.subscribe_jobs(identity).await?.into_inner();

        // Open ReportStatus stream for sending audit events to server
        let (status_tx, status_rx) = tokio::sync::mpsc::channel::<JobStatusUpdate>(64);
        let status_stream = tokio_stream::wrappers::ReceiverStream::new(status_rx);
        let mut report_client = client.clone();
        tokio::spawn(async move {
            if let Err(e) = report_client.report_status(status_stream).await {
                debug!(error = %e, "ReportStatus stream ended");
            }
        });

        // Open Heartbeat stream — every 15s we send a Ping with our
        // machine_id, server updates last_seen on receipt. This keeps the
        // dashboard's "last seen" field fresh for idle clients (no job
        // traffic) instead of showing only the initial subscribe time.
        let (ping_tx, ping_rx) = tokio::sync::mpsc::channel::<devbridge_core::proto::Ping>(4);
        let ping_stream = tokio_stream::wrappers::ReceiverStream::new(ping_rx);
        let mut heartbeat_client = client.clone();
        tokio::spawn(async move {
            match heartbeat_client.heartbeat(ping_stream).await {
                Ok(resp) => {
                    let mut pongs = resp.into_inner();
                    while pongs.message().await.is_ok() {
                        // Pong received — connection alive.
                    }
                    debug!("Heartbeat stream closed");
                }
                Err(e) => {
                    debug!(error = %e, "Heartbeat RPC ended");
                }
            }
        });
        let heartbeat_machine_id = self.machine_id.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15));
            ticker.tick().await; // first tick fires immediately; skip so we wait 15s
            loop {
                ticker.tick().await;
                let now = chrono::Utc::now();
                let ping = devbridge_core::proto::Ping {
                    timestamp: Some(prost_types::Timestamp {
                        seconds: now.timestamp(),
                        nanos: now.timestamp_subsec_nanos() as i32,
                    }),
                    machine_id: heartbeat_machine_id.clone(),
                };
                if ping_tx.send(ping).await.is_err() {
                    debug!("heartbeat channel closed, stopping pinger");
                    break;
                }
            }
        });

        // Scope guard ensures the serial bridge reader releases the COM
        // port and the gRPC task is aborted when `run_inner` returns —
        // including on error paths (e.g. server restart bubbling up via
        // `?`). Without this, zombie readers keep holding the COM port
        // and subsequent reconnect attempts fail silently with "Access
        // denied" forever. Bit us today when pz-server restarted at
        // 07:12:14 and pjkeb silently lost its serial bridge.
        struct SerialCleanup {
            shutdown_flag: Option<crate::serial_bridge::ShutdownFlag>,
            grpc_abort: Option<tokio::task::AbortHandle>,
        }
        impl Drop for SerialCleanup {
            fn drop(&mut self) {
                if let Some(flag) = self.shutdown_flag.take() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(handle) = self.grpc_abort.take() {
                    handle.abort();
                }
            }
        }

        // Spawn serial bridge reader if enabled
        let (serial_task, _serial_cleanup) = if self.serial_bridge_config.enabled {
            info!(
                port = %self.serial_bridge_config.port,
                baud = self.serial_bridge_config.baud_rate,
                "serial bridge enabled, starting reader and gRPC stream"
            );
            let (serial_tx, serial_rx) = tokio::sync::mpsc::channel::<SerialData>(64);
            let serial_stream = tokio_stream::wrappers::ReceiverStream::new(serial_rx);
            let mut serial_client = client.clone();
            let serial_handle = tokio::spawn(async move {
                match serial_client.stream_serial_data(serial_stream).await {
                    Ok(resp) => {
                        info!("StreamSerialData RPC established, awaiting server acks");
                        let mut acks = resp.into_inner();
                        let mut ack_count = 0u64;
                        while let Some(ack) = acks.message().await.unwrap_or(None) {
                            ack_count += 1;
                            if ack.ok {
                                info!(
                                    ok = true,
                                    total_acks = ack_count,
                                    "serial bridge: server confirmed barcode forwarded to virtual COM"
                                );
                            } else {
                                warn!(
                                    ok = false,
                                    total_acks = ack_count,
                                    "serial bridge: server rejected barcode write"
                                );
                            }
                        }
                        warn!(total_acks = ack_count, "StreamSerialData ack stream ended");
                    }
                    Err(e) => {
                        warn!(error = %e, "StreamSerialData RPC ended");
                    }
                }
            });
            let (reader_handle, shutdown_flag) = crate::serial_bridge::spawn_reader(
                self.serial_bridge_config.clone(),
                self.machine_id.clone(),
                serial_tx,
            );
            let cleanup = SerialCleanup {
                shutdown_flag: Some(Arc::clone(&shutdown_flag)),
                grpc_abort: Some(serial_handle.abort_handle()),
            };
            (
                Some((serial_handle, reader_handle, shutdown_flag)),
                Some(cleanup),
            )
        } else {
            (None, None)
        };

        while let Some(job) = stream.message().await? {
            info!(
                job_id = %job.job_id,
                document = %job.document_name,
                size = job.payload_size,
                "received job"
            );

            let dest = spool_dir.join(format!("{}.pdf", job.job_id));

            // Read target printer fresh for this job
            let printer = target_printer.read().await.clone();

            // Record job in local history before processing
            if let Some(q) = queue {
                let meta = job_to_metadata(&job, &printer);
                if let Err(e) = q.record_job(&meta, &dest.to_string_lossy()) {
                    warn!(job_id = %job.job_id, error = %e, "failed to record job in history");
                }
            }

            // Create event emitter for audit trail (before download so it covers the full lifecycle)
            let (event_tx, _) =
                tokio::sync::broadcast::channel::<devbridge_core::job_event::PrintJobEvent>(64);
            let event_emitter = devbridge_core::job_event::EventEmitter::new(event_tx.clone());

            // Persist events to local queue AND stream to server
            let event_queue = queue.cloned();
            let mut event_rx = event_tx.subscribe();
            let status_sender = status_tx.clone();
            let event_persist_task = tokio::spawn(async move {
                while let Ok(event) = event_rx.recv().await {
                    // Store locally
                    if let Some(q) = &event_queue {
                        let _ = q.insert_job_event(&event);
                    }
                    // Stream to server via ReportStatus
                    // Skip received/routed — server already has its own from ipp_service/queue
                    if event.stage != PrintStage::Received && event.stage != PrintStage::Routed {
                        let update = JobStatusUpdate {
                            job_id: event.job_id.clone(),
                            state: print_stage_to_proto_state(event.stage),
                            message: serde_json::to_string(&event).unwrap_or_default(),
                            timestamp: Some(prost_types::Timestamp {
                                seconds: event.timestamp.timestamp(),
                                nanos: event.timestamp.timestamp_subsec_nanos() as i32,
                            }),
                        };
                        // Use try_send to avoid blocking if the gRPC stream is slow/broken.
                        // Dropping events is acceptable — the server has its own event sources.
                        let _ = status_sender.try_send(update);
                    }
                }
            });

            // Emit server-side events locally so client dashboard has the full timeline
            event_emitter.emit_ok(
                &job.job_id,
                PrintStage::Received,
                format!(
                    "Print job received ({})",
                    format_download_size(job.payload_size)
                ),
            );
            event_emitter.emit_ok(
                &job.job_id,
                PrintStage::Routed,
                format!("{} → {}", job.target_printer, self.machine_id),
            );

            // Download payload
            event_emitter.emit_ok(
                &job.job_id,
                PrintStage::Downloading,
                "Client started payload download",
            );
            match self
                .download_payload(
                    &mut client,
                    &job.job_id,
                    job.payload_size,
                    &job.payload_sha256,
                    &dest,
                )
                .await
            {
                Ok(()) => {
                    let file_size = tokio::fs::metadata(&dest)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                    event_emitter.emit_ok(
                        &job.job_id,
                        PrintStage::Downloaded,
                        format!("SHA256 verified ({})", format_download_size(file_size)),
                    );
                    debug!(job_id = %job.job_id, "payload downloaded");

                    if let Some(q) = queue {
                        let _ = q.update_job_state(&job.job_id, JobState::Printing);
                    }

                    // Print via configured backend
                    let print_printer = printer.clone();
                    let pdf = dest.clone();
                    let job_id_for_print = job.job_id.clone();
                    let doc_name = job.document_name.clone();
                    let copies = job.copies;
                    let backend_type = self.print_backend.clone();
                    let printer_addr = self.printer_address.clone();
                    let gs_device = self.ghostscript_device.clone();
                    let gs_resolution = self.ghostscript_resolution;
                    let printer_tls = self.printer_tls;
                    let printer_display_name = self.printer_display_name.clone();
                    let proxy_url = self.print_proxy_url.clone();

                    let print_emitter = event_emitter.clone();
                    let print_handle = tokio::task::spawn_blocking(move || {
                        let backend = crate::print_backend::create_backend(
                            &backend_type,
                            printer_addr.as_deref(),
                            &gs_device,
                            gs_resolution,
                            &print_printer,
                            printer_tls,
                            proxy_url.as_deref(),
                        )?;

                        info!(
                            job_id = %job_id_for_print,
                            backend = backend.name(),
                            printer = %print_printer,
                            "printing via {} backend",
                            backend.name()
                        );

                        let job_info = crate::print_backend::PrintJobInfo {
                            job_id: job_id_for_print,
                            document_name: doc_name,
                            copies,
                            duplex: false,
                            color: true,
                            printer_name: print_printer,
                            printer_display_name,
                        };

                        backend.print(&job_info, &pdf, &print_emitter)
                    });

                    // Timeout: if the print task hangs beyond 120s, abort it.
                    // SumatraPDF + verify should complete within 90s max.
                    let print_result =
                        match tokio::time::timeout(Duration::from_secs(120), print_handle).await {
                            Ok(join_result) => join_result.unwrap_or_else(|e| {
                                Err(anyhow::anyhow!("print task panicked: {e}"))
                            }),
                            Err(_) => {
                                error!(
                                    job_id = %job.job_id,
                                    "print task timed out after 120s — backend or spooler hung"
                                );
                                Err(anyhow::anyhow!(
                                    "print task timed out after 120s — backend or spooler hung"
                                ))
                            }
                        };

                    // Stop event persistence (with timeout to avoid blocking on slow gRPC)
                    drop(event_tx);
                    let _ = tokio::time::timeout(Duration::from_secs(5), event_persist_task).await;

                    let (success, error_detail) = match &print_result {
                        Ok(()) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    };

                    if let Some(q) = queue {
                        let state = if success {
                            JobState::Completed
                        } else {
                            JobState::Failed
                        };
                        let _ = q.update_job_state(&job.job_id, state);
                    }

                    // Get verification evidence from the event emitter
                    let (ver_method, ver_evidence) = event_emitter.last_verification();

                    // Report completion with backend info and verification
                    let completion = JobCompletion {
                        job_id: job.job_id.clone(),
                        success,
                        error_detail,
                        pages_printed: if success { job.copies } else { 0 },
                        printer_status: if success {
                            "delivered".into()
                        } else {
                            "error".into()
                        },
                        spooler_status: self.print_backend.clone(),
                        verification_method: ver_method,
                        verification_evidence: ver_evidence,
                        client_id: self.machine_id.clone(),
                    };
                    match client.complete_job(completion).await {
                        Ok(_) => {
                            info!(job_id = %job.job_id, success, backend = %self.print_backend, "job completed")
                        }
                        Err(e) => {
                            error!(job_id = %job.job_id, error = %e, "failed to report completion")
                        }
                    }
                }
                Err(e) => {
                    error!(job_id = %job.job_id, error = %e, "payload download failed");
                    event_emitter.emit_fail(
                        &job.job_id,
                        PrintStage::Failed,
                        format!("Download failed: {e}"),
                    );

                    // Stop event persistence (with timeout to avoid blocking on slow gRPC)
                    drop(event_tx);
                    let _ = tokio::time::timeout(Duration::from_secs(5), event_persist_task).await;

                    if let Some(q) = queue {
                        let _ = q.update_job_state(&job.job_id, JobState::Failed);
                    }
                    let completion = JobCompletion {
                        job_id: job.job_id.clone(),
                        success: false,
                        error_detail: e.to_string(),
                        pages_printed: 0,
                        printer_status: String::new(),
                        spooler_status: "download_failed".into(),
                        verification_method: String::new(),
                        verification_evidence: String::new(),
                        client_id: self.machine_id.clone(),
                    };
                    let _ = client.complete_job(completion).await;
                }
            }

            // Clean up spool file
            let _ = tokio::fs::remove_file(&dest).await;
        }

        // Graceful path: signal shutdown explicitly now, then wait for
        // the reader to release COM before returning (so the next
        // reconnect doesn't race the old reader). The SerialCleanup guard
        // is the belt-and-suspenders: if we exit via `?` above, it fires
        // the same shutdown on drop so reconnects don't inherit a zombie.
        if let Some((grpc_handle, reader_handle, shutdown_flag)) = serial_task {
            info!("signaling serial bridge reader to shut down");
            shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            grpc_handle.abort();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), reader_handle).await;
        }

        Ok(())
    }

    async fn download_payload(
        &self,
        client: &mut PrintBridgeClient<Channel>,
        job_id: &str,
        _payload_size: u64,
        expected_sha256: &str,
        dest: &Path,
    ) -> Result<()> {
        // Check for partial download from a previous attempt (resume support)
        let existing_size = match tokio::fs::metadata(dest).await {
            Ok(m) => m.len(),
            Err(_) => 0,
        };

        let mut hasher = Sha256::new();
        let mut file = if existing_size > 0 {
            // Resume: hash existing bytes for SHA256 continuity, open in append mode
            info!(
                job_id,
                existing_bytes = existing_size,
                "resuming download from offset"
            );
            let existing_data = tokio::fs::read(dest).await?;
            hasher.update(&existing_data);
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(dest)
                .await?
        } else {
            tokio::fs::File::create(dest).await?
        };

        let request = PayloadRequest {
            job_id: job_id.to_string(),
            offset: existing_size,
        };

        let mut stream = client.download_payload(request).await?.into_inner();

        while let Some(chunk) = stream.message().await? {
            hasher.update(&chunk.data);
            file.write_all(&chunk.data).await?;
        }

        file.flush().await?;

        let actual_sha256 = format!("{:x}", hasher.finalize());
        if actual_sha256 != expected_sha256 {
            // SHA256 mismatch — delete the partial file so next attempt starts fresh
            let _ = tokio::fs::remove_file(dest).await;
            anyhow::bail!("SHA256 mismatch: expected {expected_sha256}, got {actual_sha256}");
        }

        Ok(())
    }
}

use devbridge_core::format_size as format_download_size;

/// Map `PrintStage` to the proto `JobState` enum integer.
fn print_stage_to_proto_state(stage: PrintStage) -> i32 {
    match stage {
        PrintStage::Received | PrintStage::Routed => 1, // QUEUED
        PrintStage::Downloading | PrintStage::Downloaded => 2, // DOWNLOADING
        PrintStage::Rendering | PrintStage::Rendered => 7, // RENDERING
        PrintStage::Sending
        | PrintStage::Sent
        | PrintStage::Acknowledged
        | PrintStage::Verified => 8, // SENDING
        PrintStage::Completed => 4,                     // COMPLETED
        PrintStage::Failed => 5,                        // FAILED
        PrintStage::Retrying => 1,                      // QUEUED
    }
}

/// Convert a gRPC PrintJob message to a JobMetadata struct for local storage.
fn job_to_metadata(job: &PrintJob, target_printer: &str) -> JobMetadata {
    let created_at = job
        .created_at
        .as_ref()
        .and_then(|ts| DateTime::from_timestamp(ts.seconds, ts.nanos as u32))
        .unwrap_or_else(Utc::now);

    JobMetadata {
        job_id: job.job_id.clone(),
        document_name: job.document_name.clone(),
        target_printer: target_printer.to_string(),
        target_client_id: None,
        copies: job.copies,
        paper_size: job.paper_size.clone(),
        duplex: job.duplex,
        color: job.color,
        payload_size: job.payload_size,
        payload_sha256: job.payload_sha256.clone(),
        state: JobState::Downloading,
        retry_count: 0,
        error_detail: String::new(),
        requesting_user: None,
        created_at,
        updated_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devbridge_core::config::{ClientConfig, JobsConfig, TlsConfig};

    fn test_config() -> ClientConfig {
        ClientConfig {
            server_address: "127.0.0.1:50051".into(),
            target_printer: "Test".into(),
            dashboard_port: 9120,
            reconnect_interval_secs: 5,
            max_reconnect_interval_secs: 60,
            client_id: None,
            print_backend: "windows_spooler".into(),
            printer_address: None,
            ghostscript_device: "ppmraw".into(),
            ghostscript_resolution: 600,
            printer_tls: false,
            printer_display_name: None,
            virtual_printer_name: None,
            print_proxy_url: None,
            tls: TlsConfig {
                cert_file: "".into(),
                key_file: "".into(),
                ca_file: "".into(),
            },
            serial_bridge: Default::default(),
        }
    }

    fn test_jobs_config() -> JobsConfig {
        JobsConfig {
            max_retries: 3,
            retry_delay_secs: 10,
            job_expiry_hours: 24,
            max_payload_size_mb: 50,
            print_timeout_secs: 1800,
        }
    }

    #[test]
    fn test_machine_id_deterministic() {
        let config = test_config();
        let jobs = test_jobs_config();
        let receiver = Receiver::new(&config, &jobs);

        // machine_id should be a 16-char hex string
        assert_eq!(receiver.machine_id.len(), 16);
        assert!(receiver.machine_id.chars().all(|c| c.is_ascii_hexdigit()));

        // Creating another receiver on the same machine should produce the same id
        let receiver2 = Receiver::new(&config, &jobs);
        assert_eq!(receiver.machine_id, receiver2.machine_id);
    }

    #[test]
    fn test_explicit_client_id_overrides_hostname() {
        let mut config = test_config();
        config.client_id = Some("pjpos-client-01".into());

        let receiver = Receiver::new(&config, &test_jobs_config());
        assert_eq!(receiver.machine_id, "pjpos-client-01");
    }

    // ----------------------------------------------------------------------
    // Regression tests for issue: 120s hardcoded receiver-side print-task
    // timeout (receiver.rs:426) killed multi-page IPP jobs on slow consumer
    // printers (Epson L3260 ~30s/page → 7-page label sheet > 120s → loop
    // of partial reprints). Fix exposes the timeout as a configurable
    // [jobs].print_timeout_secs with a generous 30 min default.
    // ----------------------------------------------------------------------

    #[test]
    fn test_receiver_uses_configured_print_timeout() {
        let cfg = test_config();
        let mut jobs = test_jobs_config();
        jobs.print_timeout_secs = 900;
        let receiver = Receiver::new(&cfg, &jobs);
        assert_eq!(
            receiver.print_timeout,
            Duration::from_secs(900),
            "Receiver must honour [jobs].print_timeout_secs from config"
        );
    }

    #[test]
    fn test_receiver_default_print_timeout_is_1800s() {
        let cfg = test_config();
        let jobs = test_jobs_config();
        let receiver = Receiver::new(&cfg, &jobs);
        assert_eq!(
            receiver.print_timeout,
            Duration::from_secs(1800),
            "Default print_timeout (1800s = 30 min) must propagate to the Receiver \
             so multi-page label sheets on slow Epson/Canon printers don't time out"
        );
    }

    #[test]
    fn test_print_stage_to_proto_state_mapping() {
        use devbridge_core::job_event::PrintStage;

        // QUEUED = 1
        assert_eq!(print_stage_to_proto_state(PrintStage::Received), 1);
        assert_eq!(print_stage_to_proto_state(PrintStage::Routed), 1);
        assert_eq!(print_stage_to_proto_state(PrintStage::Retrying), 1);

        // DOWNLOADING = 2
        assert_eq!(print_stage_to_proto_state(PrintStage::Downloading), 2);
        assert_eq!(print_stage_to_proto_state(PrintStage::Downloaded), 2);

        // RENDERING = 7
        assert_eq!(print_stage_to_proto_state(PrintStage::Rendering), 7);
        assert_eq!(print_stage_to_proto_state(PrintStage::Rendered), 7);

        // SENDING = 8
        assert_eq!(print_stage_to_proto_state(PrintStage::Sending), 8);
        assert_eq!(print_stage_to_proto_state(PrintStage::Sent), 8);
        assert_eq!(print_stage_to_proto_state(PrintStage::Acknowledged), 8);
        assert_eq!(print_stage_to_proto_state(PrintStage::Verified), 8);

        // COMPLETED = 4
        assert_eq!(print_stage_to_proto_state(PrintStage::Completed), 4);

        // FAILED = 5
        assert_eq!(print_stage_to_proto_state(PrintStage::Failed), 5);
    }

    #[test]
    fn test_format_download_size() {
        assert_eq!(format_download_size(0), "0B");
        assert_eq!(format_download_size(512), "512B");
        assert_eq!(format_download_size(1023), "1023B");
        assert_eq!(format_download_size(1024), "1.0KB");
        assert_eq!(format_download_size(1536), "1.5KB");
        assert_eq!(format_download_size(1048576), "1.0MB");
        assert_eq!(format_download_size(2621440), "2.5MB");
    }
}
