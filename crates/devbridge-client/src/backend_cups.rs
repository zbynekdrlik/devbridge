//! CUPS print backend for macOS/Linux.
//!
//! Uses the `lp` command to submit PDF files directly to CUPS.
//! CUPS handles PDF rendering natively — no Ghostscript needed.

use std::path::Path;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};

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

    fn print(
        &self,
        job: &PrintJobInfo,
        pdf_path: &Path,
        events: &EventEmitter,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _ = cancel;
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

        // Submit PDF via CUPS lp command (with job.copies for multi-copy jobs)
        crate::printer::print_pdf(printer, pdf_path, job.copies)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!("Submitted to CUPS for {}", display),
        );

        // Verify print completion
        let verification = crate::printer::verify_print_completion(printer, 180)?;

        if verification.success {
            events.emit_verified(
                &job.job_id,
                "cups_lpstat",
                format!("CUPS job completed on {}", display),
            );
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("Printed via CUPS on {}", display),
            );
        } else {
            let evidence = format!(
                "CUPS spooler {}: {} (printer: {})",
                verification.spooler_status, verification.detail, printer
            );
            let mut fail_event = PrintJobEvent::fail(&job.job_id, PrintStage::Failed, &evidence);
            fail_event.verification_method = "cups_lpstat".into();
            fail_event.verification_evidence = evidence.clone();
            events.emit(fail_event);
            anyhow::bail!("{}", evidence);
        }

        Ok(())
    }
}
