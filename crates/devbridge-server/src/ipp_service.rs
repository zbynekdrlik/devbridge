use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use ippper::server::{serve_http, wrap_as_http_service};
use ippper::service::simple::{
    PrinterInfoBuilder, SimpleIppDocument, SimpleIppService, SimpleIppServiceHandler,
};
use sha2::{Digest, Sha256};
use tracing::info;
use uuid::Uuid;

use devbridge_core::config::ServerConfig;
use devbridge_core::job::{JobMetadata, JobState};

use crate::queue::JobQueue;

/// IPP server that accepts print jobs and feeds them into the queue.
pub struct IppServer {
    config: ServerConfig,
    queue: Arc<JobQueue>,
    spool_dir: PathBuf,
}

impl IppServer {
    pub fn new(config: ServerConfig, queue: Arc<JobQueue>) -> Self {
        let spool_dir = PathBuf::from(&config.spool_dir);
        Self {
            config,
            queue,
            spool_dir,
        }
    }

    /// Start the IPP listener on the configured port.
    pub async fn run(&self) -> Result<()> {
        let port = self.config.ipp_port;
        let spool_dir = self.spool_dir.clone();
        let queue = Arc::clone(&self.queue);
        let printer_name = self.config.printer_name.clone();

        // Ensure spool directory exists
        tokio::fs::create_dir_all(&spool_dir).await?;

        info!(port, printer_name = %printer_name, "starting IPP server");

        let printer_info = PrinterInfoBuilder::default()
            .name(printer_name)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build printer info: {e}"))?;

        let handler = JobHandler { spool_dir, queue };

        let service = SimpleIppService::new(printer_info, handler);
        let http_service = wrap_as_http_service(Arc::new(service));

        let addr = format!("0.0.0.0:{port}").parse()?;
        serve_http(addr, http_service).await?;

        Ok(())
    }
}

/// Handler that receives IPP documents and queues them as print jobs.
struct JobHandler {
    spool_dir: PathBuf,
    queue: Arc<JobQueue>,
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
            target_printer: String::new(), // assigned during dispatch
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

        info!(job_id = %job_id, size = payload_size, "IPP job received and queued");
        Ok(())
    }
}
