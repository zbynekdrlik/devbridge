use std::path::Path;

use anyhow::Result;
use devbridge_core::job_event::{EventEmitter, PrintStage};
use tracing::warn;

use crate::print_backend::{PrintBackend, PrintJobInfo};

pub struct WindowsSpooler {
    #[allow(dead_code)]
    target_printer: String,
}

impl WindowsSpooler {
    pub fn new(target_printer: String) -> Self {
        Self { target_printer }
    }
}

impl PrintBackend for WindowsSpooler {
    fn name(&self) -> &str {
        "windows_spooler"
    }

    fn print(&self, job: &PrintJobInfo, pdf_path: &Path, events: &EventEmitter) -> Result<()> {
        let printer = &job.printer_name;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("Windows spooler to {}", printer),
        );

        if let Err(e) = crate::printer::check_printer_ready(printer) {
            warn!(printer, error = %e, "printer readiness check failed, attempting print anyway");
        }

        crate::printer::print_pdf(printer, pdf_path)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!("submitted to Windows spooler for {}", printer),
        );

        let is_virtual = printer.to_lowercase().contains("pdf")
            || printer.to_lowercase().contains("xps")
            || printer.to_lowercase().contains("onenote")
            || printer.to_lowercase().contains("fax");

        let verification = crate::printer::verify_print_completion(printer, 60)?;
        if !verification.success {
            if is_virtual {
                warn!(
                    printer,
                    spooler_status = %verification.spooler_status,
                    detail = %verification.detail,
                    "spooler issue on virtual printer (advisory)"
                );
                events.emit_ok(
                    &job.job_id,
                    PrintStage::Completed,
                    format!(
                        "virtual printer {} (advisory: {})",
                        printer, verification.detail
                    ),
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
        } else {
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("printed via Windows spooler to {}", printer),
            );
        }

        Ok(())
    }
}
