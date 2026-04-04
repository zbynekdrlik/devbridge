//! CUPS print backend for macOS/Linux.
//!
//! Uses the `lp` command to submit PDF files directly to CUPS.
//! CUPS handles PDF rendering natively — no Ghostscript needed.

use std::path::Path;

use anyhow::Result;
use devbridge_core::job_event::{EventEmitter, PrintStage};

use crate::print_backend::{PrintBackend, PrintJobInfo};

pub struct CupsBackend {
    target_printer: String,
}

impl CupsBackend {
    pub fn new(target_printer: String) -> Self {
        Self { target_printer }
    }
}

impl PrintBackend for CupsBackend {
    fn name(&self) -> &str {
        "cups"
    }

    fn print(&self, job: &PrintJobInfo, pdf_path: &Path, events: &EventEmitter) -> Result<()> {
        let printer = &self.target_printer;
        let display = job.printer_display_name.as_deref().unwrap_or(printer);

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("CUPS → {}", display),
        );

        // Check printer is ready (warn but continue if check fails)
        if let Err(e) = crate::printer::check_printer_ready(printer) {
            tracing::warn!(printer, error = %e, "printer readiness check failed, attempting print anyway");
        }

        // Submit PDF via CUPS lp command
        crate::printer::print_pdf(printer, pdf_path)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!("Submitted to CUPS for {}", display),
        );

        // Verify print completion
        let verification = crate::printer::verify_print_completion(printer, 60)?;

        if verification.success {
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("Printed via CUPS on {}", display),
            );
        } else {
            events.emit_fail(
                &job.job_id,
                PrintStage::Failed,
                format!(
                    "spooler {}: {}",
                    verification.spooler_status, verification.detail
                ),
            );
            anyhow::bail!(
                "spooler {}: {} (printer: {})",
                verification.spooler_status,
                verification.detail,
                printer
            );
        }

        Ok(())
    }
}
