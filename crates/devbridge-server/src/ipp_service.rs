use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use ippper::server::{serve_http, wrap_as_http_service};
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
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build printer info: {e}"))?;

        let handler = JobHandler {
            spool_dir: self.spool_dir.clone(),
            queue: Arc::clone(&self.queue),
            ipp_name: vp.ipp_name.clone(),
        };

        let service = Arc::new(SimpleIppService::new(printer_info, handler));
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

        let http_service = wrap_as_http_service(default_service);
        let addr = format!("0.0.0.0:{port}").parse()?;
        serve_http(addr, http_service).await?;

        Ok(())
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

        // Read the full payload (IppPayload implements std::io::Read)
        let payload = {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut reader = document.payload;
            reader.read_to_end(&mut buf)?;
            buf
        };

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
            created_at: now,
            updated_at: now,
        };

        let spool_str = spool_path.to_string_lossy().to_string();
        self.queue.push(meta, spool_str)?;

        info!(job_id = %job_id, ipp_name = %self.ipp_name, size = payload_size, "IPP job received and queued");
        Ok(())
    }
}
