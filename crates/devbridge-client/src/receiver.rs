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

use devbridge_core::config::ClientConfig;
use devbridge_core::job::{JobMetadata, JobState};
use devbridge_core::job_event::PrintStage;
use devbridge_core::proto::print_bridge_client::PrintBridgeClient;
use devbridge_core::proto::{
    ClientIdentity, JobCompletion, JobStatusUpdate, PayloadRequest, PrintJob,
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
    printer_display_name: Option<String>,
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
            printer_display_name: config.printer_display_name.clone(),
        }
    }

    async fn connect(&self) -> Result<PrintBridgeClient<Channel>> {
        let endpoint = format!("http://{}", self.server_address);
        info!(endpoint = %endpoint, "connecting to server");
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

            warn!(delay = ?backoff, "reconnecting after delay");
            tokio::time::sleep(backoff).await;
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

            // Download payload
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
                    let printer_display_name = self.printer_display_name.clone();

                    // Create event emitter for audit trail
                    let (event_tx, _) = tokio::sync::broadcast::channel::<
                        devbridge_core::job_event::PrintJobEvent,
                    >(64);
                    let event_emitter =
                        devbridge_core::job_event::EventEmitter::new(event_tx.clone());

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
                            let update = JobStatusUpdate {
                                job_id: event.job_id.clone(),
                                state: print_stage_to_proto_state(event.stage),
                                message: serde_json::to_string(&event).unwrap_or_default(),
                                timestamp: Some(prost_types::Timestamp {
                                    seconds: event.timestamp.timestamp(),
                                    nanos: event.timestamp.timestamp_subsec_nanos() as i32,
                                }),
                            };
                            let _ = status_sender.send(update).await;
                        }
                    });

                    let print_result = tokio::task::spawn_blocking(move || {
                        let backend = crate::print_backend::create_backend(
                            &backend_type,
                            printer_addr.as_deref(),
                            &gs_device,
                            gs_resolution,
                            &print_printer,
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

                        backend.print(&job_info, &pdf, &event_emitter)
                    })
                    .await
                    .unwrap_or_else(|e| Err(anyhow::anyhow!("print task panicked: {e}")));

                    // Stop event persistence
                    drop(event_tx);
                    let _ = event_persist_task.await;

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

                    // Report completion with backend info
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
                    };
                    let _ = client.complete_job(completion).await;
                }
            }

            // Clean up spool file
            let _ = tokio::fs::remove_file(&dest).await;
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

/// Map `PrintStage` to the proto `JobState` enum integer.
fn print_stage_to_proto_state(stage: PrintStage) -> i32 {
    match stage {
        PrintStage::Received | PrintStage::Routed => 1, // QUEUED
        PrintStage::Downloading | PrintStage::Downloaded => 2, // DOWNLOADING
        PrintStage::Rendering | PrintStage::Rendered => 7, // RENDERING
        PrintStage::Sending | PrintStage::Sent | PrintStage::Acknowledged => 8, // SENDING
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
        created_at,
        updated_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devbridge_core::config::{ClientConfig, TlsConfig};

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
            printer_display_name: None,
            tls: TlsConfig {
                cert_file: "".into(),
                key_file: "".into(),
                ca_file: "".into(),
            },
        }
    }

    #[test]
    fn test_machine_id_deterministic() {
        let config = test_config();
        let receiver = Receiver::new(&config);

        // machine_id should be a 16-char hex string
        assert_eq!(receiver.machine_id.len(), 16);
        assert!(receiver.machine_id.chars().all(|c| c.is_ascii_hexdigit()));

        // Creating another receiver on the same machine should produce the same id
        let receiver2 = Receiver::new(&config);
        assert_eq!(receiver.machine_id, receiver2.machine_id);
    }

    #[test]
    fn test_explicit_client_id_overrides_hostname() {
        let mut config = test_config();
        config.client_id = Some("pjpos-client-01".into());

        let receiver = Receiver::new(&config);
        assert_eq!(receiver.machine_id, "pjpos-client-01");
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

        // COMPLETED = 4
        assert_eq!(print_stage_to_proto_state(PrintStage::Completed), 4);

        // FAILED = 5
        assert_eq!(print_stage_to_proto_state(PrintStage::Failed), 5);
    }
}
