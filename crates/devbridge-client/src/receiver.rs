use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use devbridge_core::config::{ClientConfig, JobsConfig, SerialBridgeClientConfig};
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
    /// Per-job hard timeout for the print task (sourced from
    /// [jobs].print_timeout_secs). Was hardcoded 120 s before 0.8.23 —
    /// see default_print_timeout_secs in devbridge_core::config.
    print_timeout: Duration,
    /// Tracks which job_ids currently have a live print task. Survives
    /// reconnects (the abandoned blocking task from a timed-out print may
    /// outlive the gRPC stream that started it), so a requeued retry on a
    /// fresh connection is still refused while the first task drains. Issue
    /// #51 defense-in-depth against the double-dispatch.
    inflight: crate::inflight::InFlightJobs,
}

impl Receiver {
    pub fn new(config: &ClientConfig, jobs: &JobsConfig) -> Self {
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
            print_timeout: Duration::from_secs(jobs.print_timeout_secs),
            inflight: crate::inflight::InFlightJobs::new(),
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
                    let print_timeout = self.print_timeout;

                    let print_emitter = event_emitter.clone();
                    let job_id_for_dispatch = job.job_id.clone();
                    let timeout_secs = print_timeout.as_secs();

                    // Build the blocking print closure. The CancellationToken
                    // is passed in by run_print_task_with_timeout; the backend
                    // polls it and bails (killing child processes / dropping
                    // in-flight connections) when the outer timeout fires.
                    let make_print = move |cancel: CancellationToken| -> Result<()> {
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

                        backend.print(&job_info, &pdf, &print_emitter, &cancel)
                    };

                    // Run under the per-job outer timeout, with cancellation +
                    // an in-flight double-dispatch guard (issue #51). When the
                    // timeout fires the token is signalled so the abandoned
                    // blocking task stops touching the printer; while it drains,
                    // a server-requeued retry for the same job_id is suppressed.
                    let dispatch = run_print_task_with_timeout(
                        &self.inflight,
                        &job_id_for_dispatch,
                        print_timeout,
                        CancellationToken::new(),
                        make_print,
                    )
                    .await;

                    let print_result = match dispatch {
                        PrintDispatch::Completed(result) => result,
                        PrintDispatch::TimedOut => {
                            error!(
                                job_id = %job.job_id,
                                timeout_secs,
                                "print task timed out — backend or spooler hung; \
                                 cancellation signalled to stop the in-flight task"
                            );
                            Err(anyhow::anyhow!(
                                "print task timed out after {timeout_secs}s — backend or spooler hung"
                            ))
                        }
                        PrintDispatch::DuplicateSuppressed => {
                            // A prior print task for this job_id is still
                            // draining. We did NOT touch the printer — report
                            // failure so the server's retry bookkeeping stays
                            // accurate, but no double IPP stream was sent.
                            warn!(
                                job_id = %job.job_id,
                                "duplicate print dispatch suppressed — prior task \
                                 for this job_id still in flight (issue #51)"
                            );
                            Err(anyhow::anyhow!(
                                "duplicate print suppressed — prior task for this job_id still in flight"
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

/// Outcome of [`run_print_task_with_timeout`].
#[derive(Debug)]
pub(crate) enum PrintDispatch {
    /// The print task ran to completion within the timeout. Carries the
    /// backend's own `Result`.
    Completed(Result<()>),
    /// The print task exceeded the configured per-job timeout. The
    /// cancellation token has been signalled so the (now abandoned) blocking
    /// task stops touching the printer; the receiver reports failure to the
    /// server.
    TimedOut,
    /// A print task for this `job_id` was already in flight. The second
    /// dispatch was suppressed (issue #51 in-flight guard) and NOTHING was
    /// sent to the printer — no double-stream.
    DuplicateSuppressed,
}

/// Run one print task under the per-job outer timeout, with cancellation and a
/// double-dispatch guard (issue #51).
///
/// Behaviour:
/// 1. Acquire the in-flight slot for `job_id`. If a print for this `job_id` is
///    already running, return [`PrintDispatch::DuplicateSuppressed`] WITHOUT
///    spawning anything — the receiver must never start a second concurrent
///    print for the same job (two racing IPP streams = partial duplicates).
/// 2. Spawn the blocking print work, passing it the `CancellationToken`.
/// 3. Wait up to `print_timeout`. If the task finishes first, return its
///    result. If the timeout fires, CANCEL the token so the abandoned blocking
///    thread observes cancellation and stops, and return
///    [`PrintDispatch::TimedOut`].
///
/// `make_print` builds and runs the blocking print; it receives the token so
/// the backend can poll it. The returned [`InFlightGuard`] is moved into the
/// spawned task so the slot is released exactly when the (possibly cancelled)
/// task finishes unwinding.
pub(crate) async fn run_print_task_with_timeout<F>(
    inflight: &crate::inflight::InFlightJobs,
    job_id: &str,
    print_timeout: Duration,
    cancel: CancellationToken,
    make_print: F,
) -> PrintDispatch
where
    F: FnOnce(CancellationToken) -> Result<()> + Send + 'static,
{
    let guard = match inflight.try_begin(job_id) {
        Some(g) => g,
        None => {
            // A prior task for this job_id is still alive (e.g. an abandoned,
            // cancelled-but-not-yet-unwound task). Refuse the second dispatch
            // — defense-in-depth against the server-requeue double-print. The
            // try_begin call already logs the suppression with the job_id.
            return PrintDispatch::DuplicateSuppressed;
        }
    };

    let task_cancel = cancel.clone();
    let print_handle = tokio::task::spawn_blocking(move || {
        // Hold the guard for the whole life of the blocking task (incl. an
        // abandoned/cancelled one) so the in-flight slot is freed only when
        // this task actually stops touching the printer.
        let _guard = guard;
        make_print(task_cancel)
    });

    match tokio::time::timeout(print_timeout, print_handle).await {
        Ok(join_result) => {
            let result =
                join_result.unwrap_or_else(|e| Err(anyhow::anyhow!("print task panicked: {e}")));
            PrintDispatch::Completed(result)
        }
        Err(_) => {
            // Outer timeout fired. The JoinHandle is dropped, but the blocking
            // thread keeps running until it returns on its own — so SIGNAL the
            // token. Cancellation-aware backends (ghostscript child kill,
            // direct_ipp connection drop, spooler/IPP verify loops) observe it
            // and bail, releasing the printer and the in-flight slot promptly.
            warn!(
                job_id,
                timeout_secs = print_timeout.as_secs(),
                "print task timed out — cancelling in-flight task so it stops \
                 touching the printer (issue #51)"
            );
            cancel.cancel();
            PrintDispatch::TimedOut
        }
    }
}

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
        // Mirror the server's retry count (source of truth) so the CLIENT
        // dashboard shows the real count during a server-driven retry storm.
        // Hardcoding 0 here let the idempotent insert_job upsert (PR #50)
        // overwrite the stored count with 0 — issue #52.
        retry_count: job.retry_count,
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

    /// Locks the runtime semantics: when `tokio::time::timeout` is fed the
    /// `Receiver::print_timeout` field and the inner future never resolves,
    /// the timeout MUST fire after exactly the configured duration. This
    /// guards against a regression that re-hardcodes a different `Duration`
    /// at the timeout call site — the struct-storage tests above only
    /// guarantee the field is populated, not that it is the value passed
    /// to `tokio::time::timeout`. Uses a short real-time duration so the
    /// test completes in milliseconds without requiring tokio's test-util
    /// feature.
    #[tokio::test]
    async fn test_receiver_print_timeout_actually_fires_at_configured_value() {
        let cfg = test_config();
        let mut jobs = test_jobs_config();
        // 0 secs forces immediate elapse; the `as_secs` is what we read,
        // so we override the struct field after construction to a sub-second
        // value that real wall-clock can hit fast in CI.
        jobs.print_timeout_secs = 1;
        let mut receiver = Receiver::new(&cfg, &jobs);
        receiver.print_timeout = Duration::from_millis(50);

        let never_completes = std::future::pending::<Result<(), anyhow::Error>>();
        let outcome = tokio::time::timeout(receiver.print_timeout, never_completes).await;
        assert!(
            outcome.is_err(),
            "tokio::time::timeout with receiver.print_timeout MUST fire when the inner future hangs"
        );
    }

    // ----------------------------------------------------------------------
    // Issue #51 regression: a backend that genuinely hangs (printer offline,
    // ghostscript stuck on a malformed PDF, IPP TLS deadlock) must, when the
    // outer per-job timeout fires:
    //   (1) have its in-flight blocking task CANCELLED (the CancellationToken
    //       set so the abandoned task stops touching the printer), and
    //   (2) NOT allow a second concurrent print for the same job_id to start
    //       (the requeue double-dispatch the server would otherwise trigger).
    // Before the fix, the outer timeout dropped the JoinHandle but never
    // signalled the task, and there was no in-flight guard — so a requeued
    // retry raced the still-running first task on the same physical printer.
    // ----------------------------------------------------------------------

    /// RED→GREEN: outer-timeout on a hung backend must cancel the in-flight
    /// task. A fake backend blocks until the cancellation token fires
    /// (simulating an EXTERNAL hang — the printer/child never returning). The
    /// dispatch helper must (a) report `TimedOut`, (b) leave the token
    /// cancelled, and (c) let the blocking task observe the cancel and exit so
    /// the in-flight slot is released.
    #[tokio::test]
    async fn test_hung_backend_is_cancelled_on_outer_timeout() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let inflight = crate::inflight::InFlightJobs::new();
        let cancel = CancellationToken::new();
        let observed_cancel = Arc::new(AtomicBool::new(false));
        let observed_for_task = Arc::clone(&observed_cancel);

        // Fake backend that hangs until cancelled — the issue's exact failure
        // mode (a wedged backend that never returns on its own).
        let make_print = move |token: CancellationToken| -> Result<()> {
            // Spin-wait on the cancel flag with a hard ceiling so a regression
            // (token never set) fails the test by timeout rather than hanging.
            let start = std::time::Instant::now();
            while !token.is_cancelled() {
                if start.elapsed() > Duration::from_secs(5) {
                    return Ok(()); // token was never set → bug not reproduced
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            observed_for_task.store(true, Ordering::SeqCst);
            anyhow::bail!("print cancelled by outer timeout")
        };

        let dispatch = run_print_task_with_timeout(
            &inflight,
            "job-hung",
            Duration::from_millis(50),
            cancel.clone(),
            make_print,
        )
        .await;

        assert!(
            matches!(dispatch, PrintDispatch::TimedOut),
            "a hung backend exceeding the outer timeout must report TimedOut, got {dispatch:?}"
        );
        assert!(
            cancel.is_cancelled(),
            "outer timeout MUST cancel the token so the abandoned task stops \
             touching the printer (issue #51)"
        );

        // The blocking task must observe the cancel and exit, releasing the slot.
        for _ in 0..200 {
            if inflight.is_empty() && observed_cancel.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            observed_cancel.load(Ordering::SeqCst),
            "the cancelled backend must observe the token and stop"
        );
        assert!(
            inflight.is_empty(),
            "the in-flight slot must be released once the cancelled task exits"
        );
    }

    /// RED→GREEN: while a print for a job_id is still in flight, a second
    /// dispatch for the SAME job_id (the requeue double-print) must be
    /// suppressed without sending anything to the printer.
    #[tokio::test]
    async fn test_requeue_double_dispatch_is_suppressed_while_first_in_flight() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let inflight = crate::inflight::InFlightJobs::new();
        let prints_started = Arc::new(AtomicUsize::new(0));

        // First task: hangs until cancelled, holding the in-flight slot.
        let started1 = Arc::clone(&prints_started);
        let first = run_print_task_with_timeout(
            &inflight,
            "job-dup",
            Duration::from_millis(40),
            CancellationToken::new(),
            move |token: CancellationToken| -> Result<()> {
                started1.fetch_add(1, Ordering::SeqCst);
                let start = std::time::Instant::now();
                while !token.is_cancelled() {
                    if start.elapsed() > Duration::from_secs(2) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                anyhow::bail!("first task cancelled")
            },
        );

        // Concurrently, simulate the server requeue re-dispatching the SAME
        // job_id while the first task is still alive. It must be refused.
        let started2 = Arc::clone(&prints_started);
        let inflight2 = inflight.clone();
        let second = async move {
            // Give the first task a beat to register its in-flight slot.
            tokio::time::sleep(Duration::from_millis(5)).await;
            run_print_task_with_timeout(
                &inflight2,
                "job-dup",
                Duration::from_secs(5),
                CancellationToken::new(),
                move |_token: CancellationToken| -> Result<()> {
                    started2.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
        };

        let (first_outcome, second_outcome) = tokio::join!(first, second);

        assert!(
            matches!(first_outcome, PrintDispatch::TimedOut),
            "first (hung) task should time out, got {first_outcome:?}"
        );
        assert!(
            matches!(second_outcome, PrintDispatch::DuplicateSuppressed),
            "a second dispatch for a job_id already in flight MUST be suppressed, \
             got {second_outcome:?}"
        );
        assert_eq!(
            prints_started.load(Ordering::SeqCst),
            1,
            "only the FIRST print body may run — the duplicate must never reach \
             the printer (no double IPP stream)"
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

    // ----------------------------------------------------------------------
    // Regression tests for issue #52: the CLIENT dashboard showed
    // retry_count=0 for every server-driven retry. `job_to_metadata`
    // hardcoded `retry_count: 0`; since PR #50 made Storage::insert_job
    // idempotent (`ON CONFLICT(job_id) DO UPDATE SET retry_count = ...`),
    // the client's own record_job path overwrote the stored count with 0.
    // Fix (Option 1, server is source of truth): plumb `retry_count` through
    // the PrintJob proto and have `job_to_metadata` mirror it.
    // ----------------------------------------------------------------------

    /// Build a minimal `PrintJob` proto message carrying the given retry_count.
    fn print_job_with_retry_count(retry_count: u32) -> PrintJob {
        PrintJob {
            job_id: "job-52".into(),
            target_printer: "ignored-server-side-printer".into(),
            document_name: "doc.pdf".into(),
            copies: 1,
            paper_size: "A4".into(),
            duplex: false,
            color: true,
            payload_size: 2048,
            payload_sha256: "deadbeef".into(),
            created_at: None,
            retry_count,
        }
    }

    #[test]
    fn test_job_to_metadata_surfaces_server_retry_count() {
        // Server is mid-retry-storm: retry_count = 7. The client MUST mirror
        // that into the JobMetadata it stores, so the client dashboard shows
        // the real count instead of 0.
        let job = print_job_with_retry_count(7);
        let meta = job_to_metadata(&job, "Local Printer");
        assert_eq!(
            meta.retry_count, 7,
            "job_to_metadata MUST surface the server's PrintJob.retry_count \
             (issue #52) — hardcoding 0 makes the client dashboard lie during \
             a server-driven retry storm"
        );
    }

    #[test]
    fn test_job_to_metadata_preserves_zero_retry_count() {
        // A fresh job (no retries yet) must still map to 0 — proves the value
        // is read from the message, not coincidentally hardcoded. Kills the
        // mutant that would always return the same constant.
        let job = print_job_with_retry_count(0);
        let meta = job_to_metadata(&job, "Local Printer");
        assert_eq!(
            meta.retry_count, 0,
            "a job with zero server-side retries must map to retry_count = 0"
        );
    }
}
