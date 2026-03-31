use std::path::Path;

use anyhow::Result;
use tracing::info;

use devbridge_core::job_event::{EventEmitter, PrintStage};

use crate::print_backend::{PrintBackend, PrintJobInfo};

/// Direct RAW backend — Ghostscript renders PDF to raster, streams via TCP port 9100.
pub struct DirectRaw {
    address: String,
    gs_device: String,
    gs_resolution: u32,
}

impl DirectRaw {
    pub fn new(address: String, gs_device: String, gs_resolution: u32) -> Self {
        Self {
            address,
            gs_device,
            gs_resolution,
        }
    }
}

impl PrintBackend for DirectRaw {
    fn name(&self) -> &str {
        "direct_raw"
    }

    fn print(&self, job: &PrintJobInfo, pdf_path: &Path, events: &EventEmitter) -> Result<()> {
        let display = job
            .printer_display_name
            .as_deref()
            .unwrap_or(&job.printer_name);

        // Step 1: Render PDF → raster via Ghostscript
        let output_path = pdf_path.with_extension("raw");

        let render_result = crate::ghostscript::render(
            pdf_path,
            &output_path,
            &self.gs_device,
            self.gs_resolution,
            &job.job_id,
            events,
        )?;

        // Step 2: Stream raster to printer via TCP
        let data = std::fs::read(&output_path)?;
        let data_size = data.len();

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!(
                "RAW TCP → {} ({}), {:.1}MB",
                display,
                self.address,
                data_size as f64 / (1024.0 * 1024.0)
            ),
        );

        use std::io::Write;
        let mut stream = std::net::TcpStream::connect(&self.address)?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
        stream.write_all(&data)?;
        stream.flush()?;
        stream.shutdown(std::net::Shutdown::Write)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!(
                "{} — {:.1}MB delivered, socket closed cleanly",
                display,
                data_size as f64 / (1024.0 * 1024.0)
            ),
        );

        events.emit_ok(
            &job.job_id,
            PrintStage::Completed,
            format!("{} — delivered (no ACK via RAW)", display),
        );

        info!(
            job_id = %job.job_id,
            address = %self.address,
            pages = render_result.pages,
            data_size,
            "RAW print complete"
        );

        // Clean up temp raster file
        let _ = std::fs::remove_file(&output_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direct_raw_name() {
        let backend = DirectRaw::new("10.78.5.9:9100".into(), "ppmraw".into(), 600);
        assert_eq!(backend.name(), "direct_raw");
    }

    #[test]
    fn test_direct_raw_create() {
        let backend = DirectRaw::new("10.78.5.9:9100".into(), "ppmraw".into(), 300);
        assert_eq!(backend.address, "10.78.5.9:9100");
        assert_eq!(backend.gs_device, "ppmraw");
        assert_eq!(backend.gs_resolution, 300);
    }
}
