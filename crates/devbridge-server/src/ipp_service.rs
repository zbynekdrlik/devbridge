use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use hyper_util::rt::{TokioExecutor, TokioIo};
use ippper::handler::handle_ipp_via_http;
use ippper::service::simple::{
    PrinterInfoBuilder, SimpleIppDocument, SimpleIppService, SimpleIppServiceHandler,
};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use devbridge_core::job::{JobMetadata, JobState};
use devbridge_core::virtual_printer::VirtualPrinter;

use crate::queue::JobQueue;

/// IPP server that accepts print jobs and feeds them into the queue.
///
/// Supports multiple virtual printers via URI-based routing:
/// - `/printers/<ipp_name>` routes to the named virtual printer
/// - `/ipp/print` (legacy) routes to the first/default virtual printer
pub struct IppServer {
    port: u16,
    queue: Arc<JobQueue>,
    spool_dir: PathBuf,
    /// Map from ipp_name to SimpleIppService instance
    printers: Arc<RwLock<HashMap<String, Arc<SimpleIppService<JobHandler>>>>>,
}

impl IppServer {
    pub fn new(port: u16, queue: Arc<JobQueue>, spool_dir: PathBuf) -> Self {
        Self {
            port,
            queue,
            spool_dir,
            printers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a virtual printer to the IPP service.
    pub async fn add_printer(&self, vp: &VirtualPrinter) -> Result<()> {
        let printer_info = PrinterInfoBuilder::default()
            .name(vp.display_name.clone())
            .info(Some(format!(
                "DevBridge Virtual Printer - {}",
                vp.display_name
            )))
            .make_and_model(Some("DevBridge Virtual Printer".to_string()))
            .media_supported(vec![
                "na_letter_8.5x11in".to_string(),
                "iso_a4_210x297mm".to_string(),
            ])
            .media_default("na_letter_8.5x11in".to_string())
            .document_format_supported(vec![
                "application/pdf".to_string(),
                "application/octet-stream".to_string(),
                "application/postscript".to_string(),
                "application/vnd.ms-xpsdocument".to_string(),
                "image/pwg-raster".to_string(),
                "image/urf".to_string(),
            ])
            .sides_supported(vec![
                "one-sided".to_string(),
                "two-sided-long-edge".to_string(),
                "two-sided-short-edge".to_string(),
            ])
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build printer info: {e}"))?;

        let handler = JobHandler {
            spool_dir: self.spool_dir.clone(),
            queue: Arc::clone(&self.queue),
            ipp_name: vp.ipp_name.clone(),
        };

        let mut service = SimpleIppService::new(printer_info, handler);
        service.set_basepath("/ipp/print");
        let service = Arc::new(service);
        self.printers
            .write()
            .await
            .insert(vp.ipp_name.clone(), service);

        info!(ipp_name = %vp.ipp_name, display_name = %vp.display_name, "added virtual printer");
        Ok(())
    }

    /// Remove a virtual printer from the IPP service.
    pub async fn remove_printer(&self, ipp_name: &str) {
        self.printers.write().await.remove(ipp_name);
        info!(ipp_name, "removed virtual printer");
    }

    /// Start the IPP listener on the configured port.
    ///
    /// Uses the first registered printer as the default handler for legacy
    /// `/ipp/print` URIs.
    pub async fn run(&self) -> Result<()> {
        let port = self.port;

        // Ensure spool directory exists
        tokio::fs::create_dir_all(&self.spool_dir).await?;

        let printers = self.printers.read().await;
        let default_service = printers
            .values()
            .next()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no virtual printers configured"))?;

        info!(
            port,
            count = printers.len(),
            "starting IPP server with virtual printers"
        );
        drop(printers);

        // Custom HTTP service wrapper that:
        // 1. Normalizes Content-Type for Windows IPP Class Driver compatibility
        // 2. Catches handler errors and returns HTTP 500 instead of dropping the connection
        let http_service =
            hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                let ipp_service = default_service.clone();
                async move {
                    let ct_value = req
                        .headers()
                        .get(hyper::header::CONTENT_TYPE)
                        .and_then(|ct| ct.to_str().ok())
                        .unwrap_or("<none>")
                        .to_string();
                    info!(
                        method = %req.method(),
                        uri = %req.uri(),
                        content_type = %ct_value,
                        "IPP HTTP request received"
                    );
                    // Windows inetpp.dll sends "application/ipp; charset=utf-8" but
                    // ippper requires exact "application/ipp".
                    let needs_normalize =
                        ct_value.to_ascii_lowercase().starts_with("application/ipp")
                            && ct_value != "application/ipp";
                    if needs_normalize {
                        info!(
                            original = %ct_value,
                            "normalizing Content-Type for Windows IPP compatibility"
                        );
                        req.headers_mut().insert(
                            hyper::header::CONTENT_TYPE,
                            "application/ipp".parse().unwrap(),
                        );
                    }
                    match handle_ipp_via_http(req, ipp_service.as_ref()).await {
                        Ok(resp) => Ok::<_, anyhow::Error>(resp),
                        Err(e) => {
                            tracing::error!(error = %e, "IPP handler error");
                            Ok(hyper::Response::builder()
                                .status(500)
                                .body(ippper::body::Body::from(format!(
                                    "500 Internal Server Error: {e}"
                                )))
                                .unwrap())
                        }
                    }
                }
            });

        // Custom TCP listener using HTTP/1.1 only. ippper's serve_http uses
        // auto::Builder which auto-detects HTTP/1 vs HTTP/2. Windows inetpp.dll
        // only speaks HTTP/1.1, and the auto-detection can cause connection hangs.
        let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse()?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(err) => {
                    tracing::error!(error = %err, "error accepting connection");
                    continue;
                }
            };
            let service = http_service.clone();
            tokio::task::spawn(async move {
                if let Err(err) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .http1_only()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    tracing::error!(error = %err, "error serving connection");
                }
            });
        }
    }
}

/// Handler that receives IPP documents and queues them as print jobs.
struct JobHandler {
    spool_dir: PathBuf,
    queue: Arc<JobQueue>,
    ipp_name: String,
}

impl SimpleIppServiceHandler for JobHandler {
    async fn handle_document(&self, document: SimpleIppDocument) -> anyhow::Result<()> {
        let job_id = Uuid::new_v4().to_string();
        let spool_path = self.spool_dir.join(format!("{job_id}.pdf"));

        // Read the full payload using spawn_blocking to avoid blocking the
        // tokio runtime. IppPayload implements std::io::Read (synchronous),
        // and blocking reads on the HTTP body can deadlock the async runtime
        // when the Windows print spooler sends data slowly.
        let payload = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut reader = document.payload;
            reader.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await??;

        // Compute hash and size
        let mut hasher = Sha256::new();
        hasher.update(&payload);
        let sha256 = format!("{:x}", hasher.finalize());
        let payload_size = payload.len() as u64;

        // Write payload to spool
        tokio::fs::write(&spool_path, &payload).await?;

        let document_name = format!("job-{job_id}");

        let now = Utc::now();
        let meta = JobMetadata {
            job_id: job_id.clone(),
            document_name,
            target_printer: self.ipp_name.clone(),
            target_client_id: None, // resolved during push() via VP pairing
            copies: 1,
            paper_size: document.job_attributes.media.clone(),
            duplex: document.job_attributes.sides != "one-sided",
            color: document.job_attributes.print_color_mode != "monochrome",
            payload_size,
            payload_sha256: sha256,
            state: JobState::Queued,
            retry_count: 0,
            error_detail: String::new(),
            created_at: now,
            updated_at: now,
        };

        let spool_str = spool_path.to_string_lossy().to_string();
        self.queue.push(meta, spool_str)?;

        info!(job_id = %job_id, ipp_name = %self.ipp_name, size = payload_size, "IPP job received and queued");
        Ok(())
    }
}
