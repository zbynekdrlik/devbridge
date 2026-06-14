use std::path::Path;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

use devbridge_core::job_event::{EventEmitter, PrintStage};

use crate::print_backend::{PrintBackend, PrintJobInfo, bail_if_cancelled};

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

    fn send_raw(
        &self,
        job: &PrintJobInfo,
        output_path: &Path,
        render_result: &crate::ghostscript::RenderResult,
        display: &str,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Step 2: Stream raster to printer via TCP. The JetDirect raw-9100
        // protocol has no copies attribute — to produce N copies we send the
        // rendered payload N times in a single TCP session chain. See #37.
        let data = std::fs::read(output_path)?;
        let data_size = data.len();
        let copies = job.copies.max(1);

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!(
                "RAW TCP → {} ({}), {:.1}MB × {} copies",
                display,
                self.address,
                data_size as f64 / (1024.0 * 1024.0),
                copies
            ),
        );

        use std::io::Write;
        for copy_index in 1..=copies {
            // Check cancellation before each copy — a hung/slow RAW socket that
            // overran the outer timeout must not keep sending copies (issue #51).
            bail_if_cancelled(cancel, &job.job_id, events, "before RAW copy send")?;
            let mut stream = std::net::TcpStream::connect(&self.address)?;
            stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
            stream.write_all(&data)?;
            stream.flush()?;
            stream.shutdown(std::net::Shutdown::Write)?;
            info!(
                job_id = %job.job_id,
                copy = copy_index,
                total = copies,
                "RAW copy delivered"
            );
        }

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!(
                "{} — {:.1}MB × {} copies delivered, socket closed cleanly",
                display,
                data_size as f64 / (1024.0 * 1024.0),
                copies
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
            copies,
            data_size,
            "RAW print complete"
        );

        Ok(())
    }
}

impl PrintBackend for DirectRaw {
    fn name(&self) -> &str {
        "direct_raw"
    }

    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let display = job
            .printer_display_name
            .as_deref()
            .unwrap_or(&job.printer_name);

        // Step 1: Render PDF → raster via Ghostscript (kills GS child on cancel)
        let output_path = pdf_path.with_extension("raw");

        let render_result = crate::ghostscript::render(
            pdf_path,
            &output_path,
            &self.gs_device,
            self.gs_resolution,
            &job.job_id,
            events,
            cancel,
        )?;

        // Steps 2+: Send raw data and report completion
        let result = self.send_raw(job, &output_path, &render_result, display, events, cancel);

        // Clean up temp raster file regardless of success or failure
        let _ = std::fs::remove_file(&output_path);

        result
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
