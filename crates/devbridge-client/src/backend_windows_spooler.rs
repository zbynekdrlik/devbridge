use std::path::Path;

use anyhow::Result;
use devbridge_core::job_event::{EventEmitter, PrintJobEvent, PrintStage};

use crate::print_backend::{PrintBackend, PrintJobInfo};

pub struct WindowsSpooler {
    #[allow(dead_code)]
    target_printer: String,
}

impl WindowsSpooler {
    pub fn new(target_printer: String) -> Self {
        Self { target_printer }
    }

    /// Verify physical delivery via Windows Print Service EventID 307.
    /// Only runs on Windows; on other platforms, falls back to spooler verification.
    #[cfg(target_os = "windows")]
    fn verify_eventid_307(
        &self,
        printer: &str,
        display: &str,
        job_id: &str,
        events: &EventEmitter,
    ) -> Result<()> {
        use std::process::Command;
        use std::time::{Duration, Instant};

        // Ensure the PrintService Operational log is enabled (idempotent)
        let _ = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "wevtutil sl 'Microsoft-Windows-PrintService/Operational' /e:true",
            ])
            .output();

        let deadline = Instant::now() + Duration::from_secs(60);
        let poll_interval = Duration::from_secs(2);
        let start_time = chrono::Utc::now();

        loop {
            let ps_script = format!(
                r#"Get-WinEvent -LogName 'Microsoft-Windows-PrintService/Operational' -MaxEvents 20 -ErrorAction SilentlyContinue | Where-Object {{ $_.Id -eq 307 -and $_.TimeCreated -ge '{start}' -and $_.Message -match '{printer}' }} | Select-Object -First 1 -ExpandProperty Message"#,
                start = start_time.format("%Y-%m-%dT%H:%M:%S"),
                printer = printer.replace('\'', "''"),
            );

            let output = Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps_script])
                .output()?;

            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if !stdout.is_empty() {
                let evidence = format!(
                    "EventID 307: {}",
                    stdout.chars().take(200).collect::<String>()
                );
                tracing::info!(
                    job_id,
                    printer,
                    "physical delivery confirmed via EventID 307"
                );
                events.emit_verified(job_id, "eventid_307", &evidence);
                events.emit_ok(
                    job_id,
                    PrintStage::Completed,
                    format!("Printed on {} (EventID 307 confirmed)", display),
                );
                return Ok(());
            }

            if Instant::now() > deadline {
                // Check for error events before failing
                let error_ps = format!(
                    r#"Get-WinEvent -LogName 'Microsoft-Windows-PrintService/Operational' -MaxEvents 10 -ErrorAction SilentlyContinue | Where-Object {{ ($_.Id -eq 372 -or $_.Id -eq 842) -and $_.TimeCreated -ge '{start}' -and $_.Message -match '{printer}' }} | Select-Object -First 1 -ExpandProperty Message"#,
                    start = start_time.format("%Y-%m-%dT%H:%M:%S"),
                    printer = printer.replace('\'', "''"),
                );
                let error_output = Command::new("powershell")
                    .args(["-NoProfile", "-Command", &error_ps])
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                let error_detail = if error_output.is_empty() {
                    format!("No EventID 307 within 60s for {}", printer)
                } else {
                    format!(
                        "No EventID 307 within 60s for {}. Error: {}",
                        printer,
                        error_output.chars().take(200).collect::<String>()
                    )
                };

                let mut fail_event = PrintJobEvent::fail(job_id, PrintStage::Failed, &error_detail);
                fail_event.verification_method = "eventid_307".into();
                fail_event.verification_evidence = error_detail.clone();
                events.emit(fail_event);
                anyhow::bail!("{}", error_detail);
            }

            std::thread::sleep(poll_interval);
        }
    }

    /// Non-Windows fallback: use spooler queue verification.
    #[cfg(not(target_os = "windows"))]
    fn verify_eventid_307(
        &self,
        printer: &str,
        display: &str,
        job_id: &str,
        events: &EventEmitter,
    ) -> Result<()> {
        let verification = crate::printer::verify_print_completion(printer, 60)?;
        if verification.success {
            events.emit_verified(
                job_id,
                "spooler_queue",
                format!(
                    "Spooler queue cleared for {} (non-Windows fallback)",
                    display
                ),
            );
            events.emit_ok(
                job_id,
                PrintStage::Completed,
                format!("Printed via spooler on {}", display),
            );
            Ok(())
        } else {
            let error_detail = format!(
                "spooler {}: {} (printer: {})",
                verification.spooler_status, verification.detail, printer
            );
            let mut fail_event = PrintJobEvent::fail(job_id, PrintStage::Failed, &error_detail);
            fail_event.verification_method = "spooler_queue".into();
            fail_event.verification_evidence = error_detail.clone();
            events.emit(fail_event);
            anyhow::bail!("{}", error_detail);
        }
    }
}

impl PrintBackend for WindowsSpooler {
    fn name(&self) -> &str {
        "windows_spooler"
    }

    fn print(&self, job: &PrintJobInfo, pdf_path: &Path, events: &EventEmitter) -> Result<()> {
        let printer = &job.printer_name;
        let display = job.printer_display_name.as_deref().unwrap_or(printer);

        events.emit_ok(
            &job.job_id,
            PrintStage::Sending,
            format!("Windows spooler → {}", display),
        );

        crate::printer::print_pdf(printer, pdf_path, job.copies)?;

        events.emit_ok(
            &job.job_id,
            PrintStage::Sent,
            format!("Submitted to Windows spooler for {}", display),
        );

        let lc = printer.to_lowercase();
        let is_virtual = lc.contains("pdf")
            || lc.contains("xps")
            || lc.contains("onenote")
            || lc.contains("fax")
            || lc.contains("null");

        if is_virtual {
            events.emit_verified(
                &job.job_id,
                "virtual_printer",
                format!("Virtual printer {} — no physical delivery", display),
            );
            events.emit_ok(
                &job.job_id,
                PrintStage::Completed,
                format!("Virtual printer {}", display),
            );
            return Ok(());
        }

        // Physical printer: verify with EventID 307 (Windows) or spooler (Linux/macOS)
        self.verify_eventid_307(printer, display, &job.job_id, events)
    }
}
